use serde_json::Value;

const TOKEN_WIDTH: usize = 5;
pub(crate) const TRACKED_MARKER: &str = "fn semantic_benchmark_marker() {}";

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ValidationError {
    MissingResultId,
    MissingTokenPayload,
    MissingTrackedMarker,
    DuplicateTrackedMarker,
    InvalidUnsignedInteger {
        field: &'static str,
    },
    InvalidTokenDataLength {
        len: usize,
    },
    PositionOverflow,
    TrackedLineHasNoToken {
        line: u32,
    },
    TrackedTokenMismatch {
        line: u32,
        expected_start: u32,
        actual_start: u32,
    },
    EditOutOfBounds {
        edit_index: usize,
        start: usize,
        delete_count: usize,
        token_data_len: usize,
    },
}

pub(crate) fn tracked_marker_line(content: &str) -> Result<u32, ValidationError> {
    let mut lines = content
        .lines()
        .enumerate()
        .filter(|(_, line)| line.trim() == TRACKED_MARKER);
    let (line, _) = lines.next().ok_or(ValidationError::MissingTrackedMarker)?;
    if lines.next().is_some() {
        return Err(ValidationError::DuplicateTrackedMarker);
    }
    u32::try_from(line).map_err(|_| ValidationError::PositionOverflow)
}

#[derive(Debug)]
pub(crate) struct SemanticBaseline {
    result_id: String,
    data: Vec<u32>,
    tracked_line: u32,
    expected_start: u32,
}

impl SemanticBaseline {
    pub(crate) fn from_full(result: &Value, tracked_line: u32) -> Result<Self, ValidationError> {
        let result_id = result_id(result)?;
        let data = full_data(result)?.ok_or(ValidationError::MissingTokenPayload)?;
        let expected_start = first_token_start(&data, tracked_line)?;

        Ok(Self {
            result_id,
            data,
            tracked_line,
            expected_start,
        })
    }

    pub(crate) fn result_id(&self) -> &str {
        &self.result_id
    }

    pub(crate) fn tracked_line(&self) -> u32 {
        self.tracked_line
    }

    pub(crate) fn record_prefix_insert(&mut self, count: u32) -> Result<(), ValidationError> {
        self.expected_start = self
            .expected_start
            .checked_add(count)
            .ok_or(ValidationError::PositionOverflow)?;
        Ok(())
    }

    pub(crate) fn record_prefix_delete(&mut self, count: u32) -> Result<(), ValidationError> {
        self.expected_start = self
            .expected_start
            .checked_sub(count)
            .ok_or(ValidationError::PositionOverflow)?;
        Ok(())
    }

    pub(crate) fn apply_response(&mut self, result: &Value) -> Result<(), ValidationError> {
        let next_result_id = result_id(result)?;
        let next_data = if let Some(data) = full_data(result)? {
            data
        } else if let Some(edits) = result.get("edits").and_then(Value::as_array) {
            apply_edits(&self.data, edits)?
        } else {
            return Err(ValidationError::MissingTokenPayload);
        };

        let actual_start = first_token_start(&next_data, self.tracked_line)?;
        if actual_start != self.expected_start {
            return Err(ValidationError::TrackedTokenMismatch {
                line: self.tracked_line,
                expected_start: self.expected_start,
                actual_start,
            });
        }

        self.result_id = next_result_id;
        self.data = next_data;
        Ok(())
    }
}

fn result_id(result: &Value) -> Result<String, ValidationError> {
    result
        .get("resultId")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or(ValidationError::MissingResultId)
}

fn full_data(result: &Value) -> Result<Option<Vec<u32>>, ValidationError> {
    result.get("data").map(parse_u32_array).transpose()
}

fn parse_u32_array(value: &Value) -> Result<Vec<u32>, ValidationError> {
    let values = value
        .as_array()
        .ok_or(ValidationError::InvalidUnsignedInteger { field: "data" })?;
    values
        .iter()
        .map(|value| parse_u32(value, "data"))
        .collect()
}

fn parse_u32(value: &Value, field: &'static str) -> Result<u32, ValidationError> {
    value
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
        .ok_or(ValidationError::InvalidUnsignedInteger { field })
}

fn parse_usize(value: Option<&Value>, field: &'static str) -> Result<usize, ValidationError> {
    value
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or(ValidationError::InvalidUnsignedInteger { field })
}

fn apply_edits(base: &[u32], edits: &[Value]) -> Result<Vec<u32>, ValidationError> {
    let mut parsed = Vec::with_capacity(edits.len());
    for (edit_index, edit) in edits.iter().enumerate() {
        let start = parse_usize(edit.get("start"), "start")?;
        let delete_count = parse_usize(edit.get("deleteCount"), "deleteCount")?;
        let end = start
            .checked_add(delete_count)
            .ok_or(ValidationError::EditOutOfBounds {
                edit_index,
                start,
                delete_count,
                token_data_len: base.len(),
            })?;
        if end > base.len() {
            return Err(ValidationError::EditOutOfBounds {
                edit_index,
                start,
                delete_count,
                token_data_len: base.len(),
            });
        }

        let replacement = edit
            .get("data")
            .map(parse_u32_array)
            .transpose()?
            .unwrap_or_default();
        parsed.push((edit_index, start, end, replacement));
    }

    // Every edit is indexed against the previous response, not against the
    // result of the preceding edit. Applying from the end keeps those offsets
    // stable even when an earlier replacement changes the array length.
    parsed.sort_unstable_by_key(|(_, start, _, _)| std::cmp::Reverse(*start));
    let mut data = base.to_vec();
    for (_, start, end, replacement) in parsed {
        data.splice(start..end, replacement);
    }
    Ok(data)
}

fn first_token_start(data: &[u32], tracked_line: u32) -> Result<u32, ValidationError> {
    if !data.len().is_multiple_of(TOKEN_WIDTH) {
        return Err(ValidationError::InvalidTokenDataLength { len: data.len() });
    }

    let mut line = 0_u32;
    let mut start = 0_u32;
    for token in data.chunks_exact(TOKEN_WIDTH) {
        line = line
            .checked_add(token[0])
            .ok_or(ValidationError::PositionOverflow)?;
        start = if token[0] == 0 {
            start
                .checked_add(token[1])
                .ok_or(ValidationError::PositionOverflow)?
        } else {
            token[1]
        };

        if line == tracked_line {
            return Ok(start);
        }
        if line > tracked_line {
            break;
        }
    }

    Err(ValidationError::TrackedLineHasNoToken { line: tracked_line })
}

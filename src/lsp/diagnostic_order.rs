use tower_lsp_server::ls_types::Diagnostic;

pub(crate) fn sort_diagnostics_deterministically(diagnostics: &mut [Diagnostic]) {
    // Establish the common-case order without allocating. Only true ties on
    // every cheap field need the full-value tiebreaker; caching that key within
    // each tie group avoids both serializing unrelated diagnostics and
    // repeating serialization per comparison for tie-heavy inputs.
    diagnostics.sort_unstable_by(diagnostic_cheap_cmp);

    let mut start = 0;
    while start < diagnostics.len() {
        let mut end = start + 1;
        while end < diagnostics.len()
            && diagnostic_cheap_cmp(&diagnostics[start], &diagnostics[end]).is_eq()
        {
            end += 1;
        }
        if end - start > 1 {
            diagnostics[start..end].sort_by_cached_key(canonical_json_string);
        }
        start = end;
    }
}

fn diagnostic_cheap_cmp(left: &Diagnostic, right: &Diagnostic) -> std::cmp::Ordering {
    diagnostic_position_key(left)
        .cmp(&diagnostic_position_key(right))
        .then_with(|| left.message.cmp(&right.message))
        .then_with(|| left.source.cmp(&right.source))
        .then_with(|| left.severity.cmp(&right.severity))
}

fn diagnostic_position_key(diagnostic: &Diagnostic) -> (u32, u32, u32, u32) {
    (
        diagnostic.range.start.line,
        diagnostic.range.start.character,
        diagnostic.range.end.line,
        diagnostic.range.end.character,
    )
}

pub(crate) fn canonical_json_string<T: serde::Serialize + std::fmt::Debug>(value: &T) -> String {
    // Fall back to the (deterministic, derive-generated) Debug rendering on a
    // serialization failure instead of a shared sentinel: distinct
    // diagnostics must not collapse to one cached sort key, or the final
    // order would silently degrade to input order for them.
    let Ok(json) = canonical_json_value(value) else {
        return format!("{value:?}");
    };
    serde_json::to_string(&json).unwrap_or_else(|_| format!("{value:?}"))
}

/// Write one diagnostic in the same canonical form used by the total-order
/// tiebreaker, without allocating an intermediate serialized `String`.
pub(crate) fn write_diagnostic_canonical_json<W: std::io::Write>(
    writer: W,
    diagnostic: &Diagnostic,
) -> serde_json::Result<()> {
    serde_json::to_writer(writer, &canonical_json_value(diagnostic)?)
}

fn canonical_json_value<T: serde::Serialize>(value: &T) -> serde_json::Result<serde_json::Value> {
    let mut json = serde_json::to_value(value)?;
    canonicalize_json_in_place(&mut json);
    Ok(json)
}

fn canonicalize_json_in_place(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Array(values) => {
            for value in values {
                canonicalize_json_in_place(value);
            }
        }
        serde_json::Value::Object(map) => {
            for value in map.values_mut() {
                canonicalize_json_in_place(value);
            }
            map.sort_keys();
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp_server::ls_types::{Position, Range};

    fn diag(message: &str) -> Diagnostic {
        Diagnostic {
            range: Range::new(Position::new(0, 0), Position::new(0, 1)),
            message: message.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn orders_by_position_before_message() {
        let mut first = diag("first");
        first.range = Range::new(Position::new(1, 0), Position::new(1, 1));

        let mut second = diag("second");
        second.range = Range::new(Position::new(0, 0), Position::new(0, 1));

        let mut diagnostics = vec![first, second];
        sort_diagnostics_deterministically(&mut diagnostics);

        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>(),
            vec!["second", "first"]
        );
    }

    #[test]
    fn canonical_json_string_ignores_recursive_object_insertion_order() {
        let nested_data = |keys: [&str; 2]| {
            let mut outer = serde_json::Map::new();
            for key in keys {
                outer.insert(key.to_string(), serde_json::json!(1));
            }

            let mut root = serde_json::Map::new();
            root.insert("outer".to_string(), serde_json::Value::Object(outer));
            serde_json::Value::Object(root)
        };

        let mut first = diag("same");
        first.data = Some(nested_data(["z", "a"]));

        let mut second = diag("same");
        second.data = Some(nested_data(["a", "z"]));

        assert_ne!(
            serde_json::to_string(&first).unwrap(),
            serde_json::to_string(&second).unwrap()
        );
        assert_eq!(
            canonical_json_string(&first),
            canonical_json_string(&second)
        );
    }

    #[test]
    fn canonical_diagnostic_writer_matches_canonical_string() {
        let mut diagnostic = diag("same");
        diagnostic.data = Some(serde_json::json!({"z": 1, "nested": {"z": 2, "a": 1}}));

        let mut written = Vec::new();
        write_diagnostic_canonical_json(&mut written, &diagnostic).unwrap();

        assert_eq!(
            String::from_utf8(written).unwrap(),
            canonical_json_string(&diagnostic)
        );
    }
}

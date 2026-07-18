use tower_lsp_server::ls_types::Diagnostic;

// Cache enough canonical keys to cover thousands of ordinary diagnostics,
// while staying well below the roughly 1 MiB full reports observed in the
// pull hot path. `Diagnostic.data` is unbounded, so caching every key could
// duplicate an arbitrarily large result. Oversized mixed tie groups therefore
// trade repeated serialization for bounded retained memory; oversized
// canonical-duplicate groups exit through the linear prepass below.
const MAX_CACHED_TIE_KEY_CAPACITY_BYTES: usize = 256 * 1024;

pub(crate) fn sort_diagnostics_deterministically(diagnostics: &mut [Diagnostic]) {
    // Establish the common-case order without allocating. Only true ties on
    // every cheap field need the full-value tiebreaker; each tie group caches
    // canonical keys up to a fixed byte budget, then uses transient pairwise
    // keys so neither repeated work nor retained memory grows unchecked.
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
            sort_diagnostic_tie_group(&mut diagnostics[start..end]);
        }
        start = end;
    }
}

fn sort_diagnostic_tie_group(diagnostics: &mut [Diagnostic]) {
    // Structural equality is only a cheap candidate filter: serde_json treats
    // 0.0 and -0.0 as equal even though their canonical bytes differ. Confirm
    // canonical equality once per item before skipping an all-duplicate group.
    if diagnostics[1..]
        .iter()
        .all(|diagnostic| diagnostic == &diagnostics[0])
    {
        let first_key = canonical_json_string(&diagnostics[0]);
        if diagnostics[1..]
            .iter()
            .all(|diagnostic| canonical_json_string(diagnostic) == first_key)
        {
            return;
        }
    }

    let mut cached_capacity = 0usize;
    let mut keyed: Vec<_> = diagnostics
        .iter_mut()
        .map(|diagnostic| {
            let key = canonical_json_string(diagnostic);
            let key_capacity = key.capacity();
            let key = if cached_capacity.saturating_add(key_capacity)
                <= MAX_CACHED_TIE_KEY_CAPACITY_BYTES
            {
                cached_capacity += key_capacity;
                Some(key)
            } else {
                None
            };
            (key, std::mem::take(diagnostic))
        })
        .collect();

    keyed.sort_unstable_by(|left, right| match (&left.0, &right.0) {
        (Some(left), Some(right)) => left.cmp(right),
        (Some(left), None) => left.cmp(&canonical_json_string(&right.1)),
        (None, Some(right)) => canonical_json_string(&left.1).cmp(right),
        (None, None) => canonical_json_string(&left.1).cmp(&canonical_json_string(&right.1)),
    });

    for (slot, (_, diagnostic)) in diagnostics.iter_mut().zip(keyed) {
        *slot = diagnostic;
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

    #[test]
    fn tie_sort_preserves_canonical_order_after_key_budget() {
        let padding = "x".repeat(MAX_CACHED_TIE_KEY_CAPACITY_BYTES + 1);
        let with_value = |value| {
            let mut diagnostic = diag("same");
            diagnostic.data = Some(serde_json::json!({
                "padding": padding,
                "value": value,
            }));
            diagnostic
        };
        let first = with_value(1);
        let second = with_value(2);
        let mut diagnostics = vec![second, first.clone()];

        sort_diagnostics_deterministically(&mut diagnostics);

        assert_eq!(diagnostics[0], first);
    }

    #[test]
    fn tie_sort_distinguishes_signed_zero_serialization() {
        let with_data = |value| {
            let mut diagnostic = diag("same");
            diagnostic.data = Some(value);
            diagnostic
        };
        let positive = with_data(serde_json::json!(0.0));
        let negative = with_data(serde_json::json!(-0.0));
        assert_eq!(
            positive, negative,
            "JSON number equality ignores zero's sign"
        );
        assert_ne!(
            canonical_json_string(&positive),
            canonical_json_string(&negative),
            "canonical JSON preserves zero's sign"
        );
        let mut first = vec![positive.clone(), negative.clone()];
        let mut second = vec![negative, positive];

        sort_diagnostics_deterministically(&mut first);
        sort_diagnostics_deterministically(&mut second);

        assert_eq!(
            first.iter().map(canonical_json_string).collect::<Vec<_>>(),
            second.iter().map(canonical_json_string).collect::<Vec<_>>()
        );
    }
}

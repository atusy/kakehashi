use tower_lsp_server::ls_types::{Diagnostic, NumberOrString};

pub(crate) fn sort_diagnostics_deterministically(diagnostics: &mut [Diagnostic]) {
    // Establish the common-case order without allocating. Only true ties on
    // every cheap field need the full-value tiebreaker; caching that key within
    // each tie group avoids both serializing unrelated diagnostics and
    // repeating serialization per comparison for tie-heavy inputs.
    diagnostics.sort_by(diagnostic_cheap_cmp);

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
        .then_with(|| diagnostic_code_cmp(left.code.as_ref(), right.code.as_ref()))
        .then_with(|| left.severity.cmp(&right.severity))
}

fn diagnostic_code_cmp(
    left: Option<&NumberOrString>,
    right: Option<&NumberOrString>,
) -> std::cmp::Ordering {
    use NumberOrString::{Number, String};

    match (left, right) {
        (Some(Number(left)), Some(Number(right))) => left.cmp(right),
        (Some(String(left)), Some(String(right))) => left.cmp(right),
        (Some(Number(_)), Some(String(_))) => std::cmp::Ordering::Less,
        (Some(String(_)), Some(Number(_))) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Less,
        (Some(_), None) => std::cmp::Ordering::Greater,
    }
}

fn diagnostic_position_key(diagnostic: &Diagnostic) -> (u32, u32, u32, u32) {
    (
        diagnostic.range.start.line,
        diagnostic.range.start.character,
        diagnostic.range.end.line,
        diagnostic.range.end.character,
    )
}

fn canonical_json_string<T: serde::Serialize + std::fmt::Debug>(value: &T) -> String {
    // Fall back to the (deterministic, derive-generated) Debug rendering on a
    // serialization failure instead of a shared sentinel: distinct
    // diagnostics must not collapse to one cached sort key, or the final
    // order would silently degrade to input order for them.
    let Ok(json) = serde_json::to_value(value) else {
        return format!("{value:?}");
    };
    serde_json::to_string(&canonicalize_json(json)).unwrap_or_else(|_| format!("{value:?}"))
}

fn canonicalize_json(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.into_iter().map(canonicalize_json).collect())
        }
        serde_json::Value::Object(map) => {
            let mut entries: Vec<_> = map.into_iter().collect();
            entries.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));

            let mut sorted = serde_json::Map::new();
            for (key, value) in entries {
                sorted.insert(key, canonicalize_json(value));
            }
            serde_json::Value::Object(sorted)
        }
        value => value,
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
    fn orders_tied_diagnostics_by_code_without_canonical_json() {
        let mut later = diag("same");
        later.code = Some(NumberOrString::String("B".to_string()));

        let mut earlier = diag("same");
        earlier.code = Some(NumberOrString::String("A".to_string()));

        let mut diagnostics = vec![later, earlier];
        sort_diagnostics_deterministically(&mut diagnostics);

        assert_eq!(
            diagnostics
                .iter()
                .filter_map(|diagnostic| diagnostic.code.clone())
                .collect::<Vec<_>>(),
            vec![
                NumberOrString::String("A".to_string()),
                NumberOrString::String("B".to_string()),
            ]
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
}

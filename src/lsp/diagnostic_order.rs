use tower_lsp_server::ls_types::Diagnostic;

pub(crate) fn sort_diagnostics_deterministically(diagnostics: &mut [Diagnostic]) {
    // Cached key: the canonical-JSON tiebreaker serializes the whole
    // diagnostic, so computing it per COMPARISON (O(n log n) times, twice
    // each) would dominate the sort — compute every component once per
    // element instead. The key components compare in the same order as the
    // previous comparator; the final tiebreaker is canonical JSON (sorted
    // object keys), stronger than the old serde_json::to_string whose key
    // order could differ between equal-content diagnostics.
    diagnostics.sort_by_cached_key(|d| {
        (
            diagnostic_position_key(d),
            d.message.clone(),
            d.source.clone(),
            d.severity,
            canonical_json_string(d),
        )
    });
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
            entries.sort_by(|(left, _), (right, _)| left.cmp(right));

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

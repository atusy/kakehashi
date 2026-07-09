use tower_lsp_server::ls_types::Diagnostic;

pub(crate) fn sort_diagnostics_deterministically(diagnostics: &mut [Diagnostic]) {
    diagnostics.sort_by(|a, b| {
        diagnostic_position_key(a)
            .cmp(&diagnostic_position_key(b))
            .then_with(|| a.message.cmp(&b.message))
            .then_with(|| a.source.cmp(&b.source))
            .then_with(|| a.severity.cmp(&b.severity))
            .then_with(|| canonical_json_string(a).cmp(&canonical_json_string(b)))
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

fn canonical_json_string<T: serde::Serialize>(value: &T) -> String {
    let value = serde_json::to_value(value).unwrap_or(serde_json::Value::Null);
    serde_json::to_string(&canonicalize_json(value)).unwrap_or_default()
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
    fn canonical_json_key_order_breaks_deep_ties() {
        let mut first = diag("same");
        first.data = Some(serde_json::json!({
            "outer": {
                "z": 1,
                "a": 1
            }
        }));

        let mut second = diag("same");
        second.data = Some(serde_json::json!({
            "outer": {
                "a": 0,
                "z": 9
            }
        }));

        let mut diagnostics = vec![first, second];
        sort_diagnostics_deterministically(&mut diagnostics);

        assert_eq!(diagnostics[0].data.as_ref().unwrap()["outer"]["a"], 0);
    }
}

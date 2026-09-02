//! The one rule for minting a `*/resolve` routing envelope into an item's
//! `data`, shared by every producer on both layers (completion, codeLens,
//! documentLink; codeAction needs no collision rule because a bare action
//! never leaves with `data`).

use serde_json::Value;

/// Wrapper key inside an item's `data` that marks kakehashi routing metadata.
pub(crate) const ENVELOPE_KEY: &str = "kakehashi";

/// Whether an item must carry a routing envelope: the origin advertises the
/// matching `*/resolve` method, or the item's own `data` already occupies the
/// envelope key — the one case a non-resolving origin's item must still be
/// wrapped, so its payload is nested as `inner` rather than read back as
/// routing metadata it never earned. Everything else passes through bare: an
/// envelope would be pure wire weight on every item, and its resolve would
/// only fail soft back to the unresolved item.
pub(crate) fn should_envelope(data: Option<&Value>, server_resolves: bool) -> bool {
    server_resolves || nests_reserved_key(data)
}

/// Whether a payload occupies the envelope key itself. On the producer side
/// this is the collision exception above; on the resolve side, once the
/// envelope is stripped and the payload restored into `data`, an item whose
/// payload does so was enveloped ONLY for that reason, so a capability miss
/// on it is the expected steady state of a non-resolving origin, not a
/// capability that vanished under the item.
pub(crate) fn nests_reserved_key(data: Option<&Value>) -> bool {
    data.is_some_and(|data| data.get(ENVELOPE_KEY).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;
    use serde_json::json;

    #[rstest]
    #[case::resolving_bare(true, Some(json!({"token": 1})), true)]
    #[case::resolving_no_data(true, None, true)]
    #[case::bare(false, Some(json!({"token": 1})), false)]
    #[case::no_data(false, None, false)]
    #[case::reserved_key(false, Some(json!({ ENVELOPE_KEY: { "origin": "spoofed" } })), true)]
    #[case::reserved_key_null(false, Some(json!({ ENVELOPE_KEY: null })), true)]
    #[case::non_object(false, Some(json!(["kakehashi"])), false)]
    #[case::string(false, Some(json!("kakehashi")), false)]
    fn envelopes_for_a_resolving_origin_or_a_reserved_key_payload(
        #[case] server_resolves: bool,
        #[case] data: Option<Value>,
        #[case] expected: bool,
    ) {
        assert_eq!(should_envelope(data.as_ref(), server_resolves), expected);
    }
}

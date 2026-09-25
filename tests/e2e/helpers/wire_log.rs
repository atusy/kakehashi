//! Reading the mock server's `MOCK_LSP_WIRE_LOG`: one `method\turi` line per
//! message received, appended by every incarnation of the mock.

/// Split the wire log into lines and return the segment belonging to the
/// REPLACEMENT incarnation: everything from the last exact `initialize` on.
/// Segmenting matters — the first incarnation legitimately received a didOpen
/// before it crashed, and asserting against the whole log would let that stale
/// didOpen satisfy an ordering check.
pub fn replacement_segment(wire: &str) -> Vec<&str> {
    let lines: Vec<&str> = wire.lines().collect();
    let last_init = lines
        .iter()
        .rposition(|l| l.split('\t').next() == Some("initialize"))
        .unwrap_or_else(|| panic!("no initialize in the wire log:\n{wire}"));
    lines[last_init..].to_vec()
}

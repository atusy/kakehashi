use super::semantic_baseline::TRACKED_MARKER;

pub(crate) const FIXED_WIDTH_STATE_COUNT: usize = 128;
pub(crate) const FIXED_WIDTH_LINE_BYTES: usize = FIXED_WIDTH_STATE_COUNT + TRACKED_MARKER.len();

pub(crate) fn fixed_width_marker_line(state: usize) -> String {
    assert!(state <= FIXED_WIDTH_STATE_COUNT);
    format!(
        "{}{TRACKED_MARKER}{}",
        " ".repeat(state),
        " ".repeat(FIXED_WIDTH_STATE_COUNT - state)
    )
}

/// Sparse Rust with an exact byte size and a valid fixed-width edit line.
pub(crate) fn gen_sparse_rust(bytes: usize) -> String {
    const PREFIX: &str = "/*";
    let suffix = format!("*/\n{}\n", fixed_width_marker_line(FIXED_WIDTH_STATE_COUNT));
    assert!(bytes >= PREFIX.len() + suffix.len());
    let mut source = String::with_capacity(bytes);
    source.push_str(PREFIX);
    source.extend(std::iter::repeat_n(
        'x',
        bytes - PREFIX.len() - suffix.len(),
    ));
    source.push_str(&suffix);
    assert_eq!(source.len(), bytes);
    source
}

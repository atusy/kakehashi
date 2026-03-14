/// Convert UTF-16 position to byte position within a line.
/// Returns None if the UTF-16 position is invalid.
#[allow(dead_code)]
pub fn convert_utf16_to_byte_in_line(line_text: &str, utf16_pos: usize) -> Option<usize> {
    let mut byte_offset = 0;
    let mut utf16_offset = 0;

    for ch in line_text.chars() {
        if utf16_offset >= utf16_pos {
            return Some(byte_offset);
        }
        utf16_offset += ch.len_utf16();
        byte_offset += ch.len_utf8();
    }

    if utf16_offset == utf16_pos {
        Some(byte_offset)
    } else {
        None
    }
}

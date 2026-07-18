//! Non-cryptographic hashes for content-based cache keys.

const FNV_OFFSET: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;

/// Incremental FNV-1a writer for callers that already serialize into an
/// `io::Write` sink and should not materialize the complete byte stream.
pub(crate) struct Fnv1aWriter(u64);

impl Fnv1aWriter {
    pub(crate) fn new() -> Self {
        Self(FNV_OFFSET)
    }

    pub(crate) fn finish(&self) -> u64 {
        self.0
    }
}

impl std::io::Write for Fnv1aWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        for &byte in buf {
            self.0 = (self.0 ^ u64::from(byte)).wrapping_mul(FNV_PRIME);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// FNV-1a 64-bit hash. Fast and well-distributed enough for cache keys, change
/// detection, and dedup; do **not** use for adversarial collision resistance
/// (use SipHash) or cryptographic purposes (use SHA-256).
#[inline]
pub fn fnv1a_hash(text: &str) -> u64 {
    let mut hash = Fnv1aWriter::new();
    for byte in text.bytes() {
        hash.0 ^= u64::from(byte);
        hash.0 = hash.0.wrapping_mul(FNV_PRIME);
    }
    hash.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn streaming_writer_matches_one_shot_hash() {
        let mut writer = Fnv1aWriter::new();
        writer.write_all(b"hello ").unwrap();
        writer.write_all(b"world").unwrap();

        assert_eq!(writer.finish(), fnv1a_hash("hello world"));
    }

    #[test]
    fn test_fnv1a_hash_deterministic() {
        let text = "hello world";
        assert_eq!(fnv1a_hash(text), fnv1a_hash(text));
    }

    #[test]
    fn test_fnv1a_hash_different_inputs() {
        assert_ne!(fnv1a_hash("hello"), fnv1a_hash("world"));
    }

    #[test]
    fn test_fnv1a_hash_empty_string() {
        // Empty string returns the offset basis
        assert_eq!(fnv1a_hash(""), 0xcbf29ce484222325);
    }

    #[test]
    fn test_fnv1a_hash_known_value() {
        // "hello" has a well-known FNV-1a 64-bit hash
        // Verified against reference implementation
        assert_eq!(fnv1a_hash("hello"), 0xa430d84680aabd0b);
    }

    #[test]
    fn test_fnv1a_hash_unicode() {
        // Unicode characters should hash their UTF-8 bytes
        let hash1 = fnv1a_hash("日本語");
        let hash2 = fnv1a_hash("日本語");
        assert_eq!(hash1, hash2);

        // Different unicode strings should have different hashes
        assert_ne!(fnv1a_hash("日本語"), fnv1a_hash("中文"));
    }
}

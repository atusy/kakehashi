//! Hash utilities for content-based caching.
//!
//! This module provides fast, non-cryptographic hash functions suitable for
//! caching and deduplication of document content.

/// Compute FNV-1a 64-bit hash of text content.
///
/// FNV-1a (Fowler-Noll-Vo) is a fast, non-cryptographic hash function with
/// good distribution properties. It's suitable for:
/// - Content-based cache keys
/// - Change detection
/// - Deduplication
///
/// **Not suitable for**:
/// - Adversarial collision resistance (use SipHash)
/// - Cryptographic purposes (use SHA-256, etc.)
///
/// # Example
///
/// ```ignore
/// let hash1 = fnv1a_hash("hello world");
/// let hash2 = fnv1a_hash("hello world");
/// let hash3 = fnv1a_hash("different");
///
/// assert_eq!(hash1, hash2);
/// assert_ne!(hash1, hash3);
/// ```
#[inline]
pub fn fnv1a_hash(text: &str) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;

    let mut hash = FNV_OFFSET;
    for byte in text.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

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

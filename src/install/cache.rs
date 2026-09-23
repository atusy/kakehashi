//! Cache management for parsers.lua metadata.
//!
//! This module provides caching functionality to avoid repeated HTTP requests
//! when fetching parser metadata from nvim-treesitter.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Default cache TTL: 1 hour
pub(super) const DEFAULT_CACHE_TTL: Duration = Duration::from_secs(3600);

/// Cache for parsers.lua content.
pub(super) struct MetadataCache {
    /// Directory where cache files are stored.
    cache_dir: PathBuf,
    /// Time-to-live for cached content.
    ttl: Duration,
}

impl MetadataCache {
    /// Create a new cache with the given data directory and TTL.
    pub fn new(data_dir: &Path, ttl: Duration) -> Self {
        Self {
            cache_dir: data_dir.join("cache"),
            ttl,
        }
    }

    /// Create a new cache with default TTL (1 hour).
    pub fn with_default_ttl(data_dir: &Path) -> Self {
        Self::new(data_dir, DEFAULT_CACHE_TTL)
    }

    /// Path to the cached parsers.lua file.
    fn cache_path(&self) -> PathBuf {
        self.cache_dir.join("parsers.lua")
    }

    /// Read cached content if it exists and is fresh.
    ///
    /// Returns `None` if cache doesn't exist or is stale.
    pub fn read(&self) -> Option<String> {
        let cache_path = self.cache_path();

        if !cache_path.exists() {
            return None;
        }

        // Check if cache is fresh based on file modification time
        let metadata = fs::metadata(&cache_path).ok()?;
        let modified = metadata.modified().ok()?;
        let age = SystemTime::now().duration_since(modified).ok()?;

        if age > self.ttl {
            // Cache is stale
            return None;
        }

        fs::read_to_string(&cache_path).ok()
    }

    /// Atomically replace the cache entry with complete content.
    pub fn write(&self, content: &str) -> io::Result<()> {
        // Ensure cache directory exists
        fs::create_dir_all(&self.cache_dir)?;

        // Keep partial writes away from readers and replace a leaf symlink
        // itself rather than opening its target. The sibling stays on the same
        // filesystem so publication can use an atomic replacement.
        let mut temporary = tempfile::NamedTempFile::new_in(&self.cache_dir)?;
        temporary.write_all(content.as_bytes())?;
        temporary.as_file().sync_all()?;
        temporary.persist(self.cache_path()).map_err(|e| e.error)?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn cache_replacement_preserves_an_open_reader() {
        use std::io::Read;

        let temp = tempdir().unwrap();
        let cache = MetadataCache::with_default_ttl(temp.path());
        cache.write("previous complete metadata").unwrap();
        let mut reader = fs::File::open(cache.cache_path()).unwrap();

        cache.write("replacement metadata").unwrap();

        let mut previous = String::new();
        reader.read_to_string(&mut previous).unwrap();
        assert_eq!(previous, "previous complete metadata");
        assert_eq!(cache.read().as_deref(), Some("replacement metadata"));
    }

    #[test]
    fn failed_publication_preserves_destination_and_cleans_temporary_file() {
        let temp = tempdir().unwrap();
        let cache = MetadataCache::with_default_ttl(temp.path());
        fs::create_dir_all(cache.cache_path()).unwrap();
        let previous = cache.cache_path().join("keep");
        fs::write(&previous, "unchanged").unwrap();

        assert!(cache.write("replacement").is_err());

        assert_eq!(fs::read_to_string(previous).unwrap(), "unchanged");
        let entries: Vec<_> = fs::read_dir(&cache.cache_dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from("parsers.lua")]);
    }

    #[cfg(unix)]
    #[test]
    fn cache_write_replaces_dangling_symlink_without_creating_target() {
        let temp = tempdir().unwrap();
        let cache = MetadataCache::with_default_ttl(temp.path());
        fs::create_dir_all(&cache.cache_dir).unwrap();
        let missing = temp.path().join("absent.lua");
        std::os::unix::fs::symlink(&missing, cache.cache_path()).unwrap();

        cache.write("new metadata").unwrap();

        assert!(!missing.exists());
        assert!(fs::symlink_metadata(cache.cache_path()).unwrap().is_file());
        assert_eq!(cache.read().as_deref(), Some("new metadata"));
    }

    #[cfg(unix)]
    #[test]
    fn cache_write_replaces_symlink_without_modifying_target() {
        let temp = tempdir().unwrap();
        let cache = MetadataCache::with_default_ttl(temp.path());
        fs::create_dir_all(&cache.cache_dir).unwrap();
        let victim = temp.path().join("unrelated.lua");
        fs::write(&victim, "keep this content").unwrap();
        std::os::unix::fs::symlink(&victim, cache.cache_path()).unwrap();

        cache.write("new metadata").unwrap();

        assert_eq!(fs::read_to_string(&victim).unwrap(), "keep this content");
        assert!(fs::symlink_metadata(cache.cache_path()).unwrap().is_file());
        assert_eq!(cache.read().as_deref(), Some("new metadata"));
    }

    #[test]
    fn test_cache_write_and_read() {
        let temp = tempdir().expect("Failed to create temp dir");
        let cache = MetadataCache::with_default_ttl(temp.path());

        let content = "test content for cache";

        // Write to cache
        cache.write(content).expect("Failed to write cache");

        // Read from cache
        let cached = cache.read().expect("Cache should be readable");
        assert_eq!(cached, content);
    }

    #[test]
    fn test_cache_returns_none_when_empty() {
        let temp = tempdir().expect("Failed to create temp dir");
        let cache = MetadataCache::with_default_ttl(temp.path());

        // Should return None when cache doesn't exist
        assert!(cache.read().is_none());
    }

    #[test]
    fn test_cache_respects_ttl() {
        let temp = tempdir().expect("Failed to create temp dir");
        // Use 0 TTL so cache is always stale
        let cache = MetadataCache::new(temp.path(), Duration::from_secs(0));

        cache.write("content").expect("Failed to write");

        // With 0 TTL, cache should be considered stale immediately
        // (though this depends on timing, we use a small sleep to ensure)
        std::thread::sleep(Duration::from_millis(10));
        assert!(cache.read().is_none(), "Cache should be stale with 0 TTL");
    }

    #[test]
    fn test_cache_fresh_with_long_ttl() {
        let temp = tempdir().expect("Failed to create temp dir");
        // Use very long TTL
        let cache = MetadataCache::new(temp.path(), Duration::from_secs(3600));

        let content = "fresh content";
        cache.write(content).expect("Failed to write");

        // Should be readable immediately
        let cached = cache.read().expect("Cache should be fresh");
        assert_eq!(cached, content);
    }
}

//! Cache management for parsers.lua metadata.
//!
//! This module provides caching functionality to avoid repeated HTTP requests
//! when fetching parser metadata from nvim-treesitter.

use std::fs;
use std::io;
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

    /// Write content to cache.
    pub fn write(&self, content: &str) -> io::Result<()> {
        self.write_with(content, |file, content| {
            use std::io::Write as _;
            file.write_all(content.as_bytes())
        })
    }

    fn write_with(
        &self,
        content: &str,
        write: impl FnOnce(&mut fs::File, &str) -> io::Result<()>,
    ) -> io::Result<()> {
        // Ensure cache directory exists
        fs::create_dir_all(&self.cache_dir)?;

        let cache_path = self.cache_path();
        let existing_permissions = fs::symlink_metadata(&cache_path)
            .ok()
            .filter(|metadata| metadata.file_type().is_file())
            .map(|metadata| metadata.permissions());
        let mut builder = tempfile::Builder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            builder.permissions(fs::Permissions::from_mode(0o666));
        }
        let mut temporary = builder.tempfile_in(&self.cache_dir)?;
        write(temporary.as_file_mut(), content)?;
        if let Some(permissions) = existing_permissions {
            temporary.as_file().set_permissions(permissions)?;
        }
        temporary.as_file().sync_all()?;
        temporary.persist(cache_path).map_err(|error| error.error)?;

        // The file data is durable above. Best-effort directory sync also
        // persists the replaced directory entry on filesystems that support it.
        if let Ok(directory) = fs::File::open(&self.cache_dir) {
            let _ = directory.sync_all();
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

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

    #[cfg(unix)]
    #[test]
    fn cache_write_replaces_symlink_without_touching_target() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().unwrap();
        let cache_dir = temp.path().join("cache");
        let cache_path = cache_dir.join("parsers.lua");
        let target = temp.path().join("unrelated.lua");
        fs::create_dir_all(&cache_dir).unwrap();
        fs::write(&target, "unrelated").unwrap();
        symlink(&target, &cache_path).unwrap();

        MetadataCache::with_default_ttl(temp.path())
            .write("metadata")
            .unwrap();

        assert!(
            !cache_path
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_to_string(cache_path).unwrap(), "metadata");
        assert_eq!(fs::read_to_string(target).unwrap(), "unrelated");
    }

    #[test]
    fn failed_cache_write_preserves_previous_content() {
        use std::io::Write as _;

        let temp = tempdir().unwrap();
        let cache = MetadataCache::with_default_ttl(temp.path());
        cache.write("previous").unwrap();

        let result = cache.write_with("replacement", |file, content| {
            file.write_all(&content.as_bytes()[..3])?;
            Err(io::Error::other("injected write failure"))
        });

        assert!(result.is_err());
        assert_eq!(fs::read_to_string(cache.cache_path()).unwrap(), "previous");
    }

    #[cfg(unix)]
    #[test]
    fn cache_write_preserves_creation_and_existing_permissions() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempdir().unwrap();
        let cache = MetadataCache::with_default_ttl(temp.path());
        let ordinary = temp.path().join("ordinary");
        fs::write(&ordinary, "ordinary").unwrap();

        cache.write("first").unwrap();
        assert_eq!(
            fs::metadata(cache.cache_path())
                .unwrap()
                .permissions()
                .mode(),
            fs::metadata(ordinary).unwrap().permissions().mode()
        );

        fs::set_permissions(cache.cache_path(), fs::Permissions::from_mode(0o640)).unwrap();
        cache.write("second").unwrap();
        assert_eq!(
            fs::metadata(cache.cache_path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o640
        );
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

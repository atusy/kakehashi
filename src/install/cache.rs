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
    #[cfg(test)]
    fn cache_path(&self) -> PathBuf {
        self.cache_dir.join("parsers.lua")
    }

    /// Read cached content if it exists and is fresh.
    ///
    /// Returns `None` if cache doesn't exist or is stale.
    pub fn read(&self) -> Option<String> {
        let cache_dir = self.open_cache_dir(false).ok()?;
        let mut file = open_cache_file(&cache_dir).ok()?;
        let metadata = file.metadata().ok()?;
        if !cache_metadata_is_regular(&metadata) {
            return None;
        }
        let modified = metadata.modified().ok()?;
        let age = SystemTime::now().duration_since(modified.into_std()).ok()?;

        if age > self.ttl {
            // Cache is stale
            return None;
        }

        let mut content = String::new();
        io::Read::read_to_string(&mut file, &mut content).ok()?;
        Some(content)
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
        write: impl FnOnce(&mut cap_std::fs::File, &str) -> io::Result<()>,
    ) -> io::Result<()> {
        let cache_dir = self.open_cache_dir(true)?;
        let existing_permissions = cache_dir
            .symlink_metadata("parsers.lua")
            .ok()
            .filter(|metadata| metadata.file_type().is_file())
            .map(|metadata| metadata.permissions());
        let temporary_name = format!(".parsers.lua.{}.tmp", ulid::Ulid::new());
        let mut temporary = create_cache_temp(&cache_dir, &temporary_name)?;
        let publish = (|| {
            write(&mut temporary, content)?;
            if let Some(permissions) = existing_permissions {
                temporary.set_permissions(permissions)?;
            }
            temporary.sync_all()?;
            publish_cache_temp(&cache_dir, temporary, &temporary_name)
        })();
        if let Err(error) = publish {
            let _ = cache_dir.remove_file(&temporary_name);
            return Err(error);
        }

        if let Ok(directory) = cache_dir.try_clone() {
            let _ = directory.into_std_file().sync_all();
        }

        Ok(())
    }

    fn open_cache_dir(&self, create: bool) -> io::Result<cap_std::fs::Dir> {
        use cap_std::ambient_authority;

        let data_dir = self.cache_dir.parent().unwrap_or_else(|| Path::new("."));
        if create {
            fs::create_dir_all(data_dir)?;
        }
        let root = cap_std::fs::Dir::open_ambient_dir(data_dir, ambient_authority())?;
        if create {
            match root.create_dir("cache") {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        let cache =
            cap_primitives::fs::open_dir_nofollow(&root.into_std_file(), Path::new("cache"))?;
        Ok(cap_std::fs::Dir::from_std_file(cache))
    }
}

#[cfg(not(windows))]
fn publish_cache_temp(
    cache_dir: &cap_std::fs::Dir,
    temporary: cap_std::fs::File,
    temporary_name: &str,
) -> io::Result<()> {
    drop(temporary);
    cache_dir.rename(temporary_name, cache_dir, "parsers.lua")
}

#[cfg(windows)]
fn publish_cache_temp(
    cache_dir: &cap_std::fs::Dir,
    temporary: cap_std::fs::File,
    _temporary_name: &str,
) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_RENAME_INFO, FileRenameInfo, SetFileInformationByHandle,
    };

    let directory = cache_dir.try_clone()?.into_std_file();
    let temporary = temporary.into_std();
    let filename: Vec<u16> = "parsers.lua".encode_utf16().collect();
    let header = std::mem::offset_of!(FILE_RENAME_INFO, FileName);
    let size = header + filename.len() * std::mem::size_of::<u16>();
    let units = size.div_ceil(std::mem::size_of::<FILE_RENAME_INFO>());
    let mut buffer = vec![FILE_RENAME_INFO::default(); units];
    let information = buffer.as_mut_ptr();
    // SAFETY: `buffer` is sized for the fixed header plus the UTF-16 filename;
    // both file handles remain alive for the call, and the relative filename is
    // resolved by Windows against the validated cache-directory handle.
    let succeeded = unsafe {
        (*information).Anonymous.ReplaceIfExists = true;
        (*information).RootDirectory = directory.as_raw_handle();
        (*information).FileNameLength = u32::try_from(filename.len() * 2).unwrap();
        std::ptr::copy_nonoverlapping(
            filename.as_ptr(),
            (*information).FileName.as_mut_ptr(),
            filename.len(),
        );
        SetFileInformationByHandle(
            temporary.as_raw_handle(),
            FileRenameInfo,
            information.cast(),
            u32::try_from(size).unwrap(),
        )
    };
    if succeeded == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn open_cache_file(cache_dir: &cap_std::fs::Dir) -> io::Result<cap_std::fs::File> {
    use cap_std::fs::OpenOptionsExt as _;

    let mut options = cap_std::fs::OpenOptions::new();
    options.read(true).custom_flags(nix::libc::O_NOFOLLOW);
    cache_dir.open_with("parsers.lua", &options)
}

#[cfg(windows)]
fn cache_metadata_is_regular(metadata: &cap_std::fs::Metadata) -> bool {
    use cap_std::fs::MetadataExt as _;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    metadata.file_type().is_file() && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0
}

#[cfg(not(windows))]
fn cache_metadata_is_regular(metadata: &cap_std::fs::Metadata) -> bool {
    metadata.file_type().is_file()
}

#[cfg(windows)]
fn open_cache_file(cache_dir: &cap_std::fs::Dir) -> io::Result<cap_std::fs::File> {
    use cap_std::fs::OpenOptionsExt as _;

    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    let mut options = cap_std::fs::OpenOptions::new();
    options
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    cache_dir.open_with("parsers.lua", &options)
}

#[cfg(unix)]
fn create_cache_temp(cache_dir: &cap_std::fs::Dir, name: &str) -> io::Result<cap_std::fs::File> {
    use cap_std::fs::OpenOptionsExt as _;

    let mut options = cap_std::fs::OpenOptions::new();
    options
        .write(true)
        .create_new(true)
        .mode(0o666)
        .custom_flags(nix::libc::O_NOFOLLOW);
    cache_dir.open_with(name, &options)
}

#[cfg(windows)]
fn create_cache_temp(cache_dir: &cap_std::fs::Dir, name: &str) -> io::Result<cap_std::fs::File> {
    use cap_std::fs::OpenOptionsExt as _;

    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    let mut options = cap_std::fs::OpenOptions::new();
    options
        .write(true)
        .create_new(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    cache_dir.open_with(name, &options)
}

#[cfg(not(any(unix, windows)))]
fn create_cache_temp(cache_dir: &cap_std::fs::Dir, name: &str) -> io::Result<cap_std::fs::File> {
    let mut options = cap_std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    cache_dir.open_with(name, &options)
}

#[cfg(not(any(unix, windows)))]
fn open_cache_file(cache_dir: &cap_std::fs::Dir) -> io::Result<cap_std::fs::File> {
    cache_dir.open("parsers.lua")
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

    #[cfg(windows)]
    #[test]
    fn windows_cache_write_atomically_replaces_existing_entry() {
        let temp = tempdir().unwrap();
        let cache = MetadataCache::with_default_ttl(temp.path());
        cache.write("first").unwrap();

        cache.write("second").unwrap();

        assert_eq!(cache.read().as_deref(), Some("second"));
        assert_eq!(fs::read_dir(&cache.cache_dir).unwrap().count(), 1);
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

    #[cfg(unix)]
    #[test]
    fn cache_read_ignores_symlink_without_reading_target() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().unwrap();
        let cache_dir = temp.path().join("cache");
        let cache_path = cache_dir.join("parsers.lua");
        let target = temp.path().join("outside.lua");
        fs::create_dir_all(&cache_dir).unwrap();
        fs::write(&target, "outside metadata").unwrap();
        symlink(&target, cache_path).unwrap();

        let cache = MetadataCache::with_default_ttl(temp.path());

        assert_eq!(cache.read(), None);
        assert_eq!(fs::read_to_string(target).unwrap(), "outside metadata");
    }

    #[cfg(unix)]
    #[test]
    fn cache_directory_symlink_redirects_neither_reads_nor_writes() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().unwrap();
        let outside = temp.path().join("outside");
        let cache_dir = temp.path().join("cache");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("parsers.lua"), "outside metadata").unwrap();
        symlink(&outside, &cache_dir).unwrap();
        let cache = MetadataCache::with_default_ttl(temp.path());

        assert_eq!(cache.read(), None);
        assert!(cache.write("replacement").is_err());
        assert_eq!(
            fs::read_to_string(outside.join("parsers.lua")).unwrap(),
            "outside metadata"
        );
        assert!(
            cache_dir
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_data_directory_remains_supported() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().unwrap();
        let actual = temp.path().join("actual-data");
        let linked = temp.path().join("linked-data");
        fs::create_dir_all(&actual).unwrap();
        symlink(&actual, &linked).unwrap();
        let cache = MetadataCache::with_default_ttl(&linked);

        cache.write("metadata").unwrap();

        assert_eq!(cache.read().as_deref(), Some("metadata"));
        assert_eq!(
            fs::read_to_string(actual.join("cache/parsers.lua")).unwrap(),
            "metadata"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_cache_directory_symlink_redirects_neither_reads_nor_writes() {
        use std::os::windows::fs::symlink_dir;

        let temp = tempdir().unwrap();
        let outside = temp.path().join("outside");
        let cache_dir = temp.path().join("cache");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("parsers.lua"), "outside metadata").unwrap();
        if let Err(error) = symlink_dir(&outside, &cache_dir) {
            if error.kind() == io::ErrorKind::PermissionDenied {
                return;
            }
            panic!("create cache directory symlink: {error}");
        }
        let cache = MetadataCache::with_default_ttl(temp.path());

        assert_eq!(cache.read(), None);
        assert!(cache.write("replacement").is_err());
        assert_eq!(
            fs::read_to_string(outside.join("parsers.lua")).unwrap(),
            "outside metadata"
        );
        assert!(
            cache_dir
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[cfg(windows)]
    #[test]
    fn cache_read_ignores_windows_symlink_without_reading_target() {
        use std::os::windows::fs::symlink_file;

        let temp = tempdir().unwrap();
        let cache_dir = temp.path().join("cache");
        let cache_path = cache_dir.join("parsers.lua");
        let target = temp.path().join("outside.lua");
        fs::create_dir_all(&cache_dir).unwrap();
        fs::write(&target, "outside metadata").unwrap();
        if let Err(error) = symlink_file(&target, &cache_path) {
            if error.kind() == io::ErrorKind::PermissionDenied {
                return;
            }
            panic!("create cache symlink: {error}");
        }

        let cache = MetadataCache::with_default_ttl(temp.path());

        assert_eq!(cache.read(), None);
        assert_eq!(fs::read_to_string(target).unwrap(), "outside metadata");
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
        assert_eq!(fs::read_dir(&cache.cache_dir).unwrap().count(), 1);
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

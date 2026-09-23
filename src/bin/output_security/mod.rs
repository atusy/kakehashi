//! Protection outside `std::fs::Permissions` for atomic output replacement.
//!
//! We do not copy ACLs: a replacement is permitted only when its inherited
//! protection already matches the destination, both before writing and after
//! restoring mode/ownership. Otherwise leave the original intact.

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::from_file;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::from_file;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::from_file;

pub fn from_path(path: &std::path::Path) -> std::io::Result<Vec<u8>> {
    // Preserve the old writer's kernel access checks without truncating or
    // writing. Mode bits alone do not capture owner-vs-group permission
    // selection, ACL write denials, or Linux fs-verity's writable-open refusal.
    let mut options = std::fs::OpenOptions::new();
    options.write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        // Refuse leaf links; do not block on a raced FIFO or write lease.
        options.custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "refusing to replace non-regular output",
        ));
    }
    from_file(&file)
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
pub fn from_file(_: &std::fs::File) -> std::io::Result<Vec<u8>> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "cannot verify output ACLs on this platform; use a new output path",
    ))
}

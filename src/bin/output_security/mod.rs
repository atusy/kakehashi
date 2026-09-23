//! Protection outside `std::fs::Permissions` for atomic output replacement.
//!
//! We do not copy ACLs: a replacement is permitted only when its inherited
//! protection already matches the destination, both before writing and after
//! restoring mode/ownership. Otherwise leave the original intact.

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::{from_file, from_path};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::{from_file, from_path};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{from_file, from_path};

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
pub fn from_path(_: &std::path::Path) -> std::io::Result<Vec<u8>> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "cannot verify output ACLs on this platform; use a new output path",
    ))
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
pub fn from_file(_: &std::fs::File) -> std::io::Result<Vec<u8>> {
    from_path(std::path::Path::new(""))
}

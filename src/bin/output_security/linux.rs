use std::io;
use xattr::FileExt as _;

const ACCESS_ACL: &str = "system.posix_acl_access";

fn normalize(result: io::Result<Option<Vec<u8>>>) -> io::Result<Vec<u8>> {
    match result {
        Ok(value) => Ok(value.unwrap_or_default()),
        // Filesystems without ACL support have no access ACL to preserve.
        Err(error) if error.raw_os_error() == Some(nix::libc::EOPNOTSUPP) => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

pub fn from_path(path: &std::path::Path) -> io::Result<Vec<u8>> {
    // xattr::get inspects the leaf itself, without following symlinks.
    normalize(xattr::get(path, ACCESS_ACL))
}

pub fn from_file(file: &std::fs::File) -> io::Result<Vec<u8>> {
    normalize(file.get_xattr(ACCESS_ACL))
}

use std::io;
use xattr::FileExt as _;

const ACCESS_ACL: &str = "system.posix_acl_access";

fn normalize(result: io::Result<Option<Vec<u8>>>) -> io::Result<Vec<u8>> {
    // Unsupported POSIX ACL inspection is not proof of absent protection:
    // NFSv4 exposes a different ACL model. Fail closed rather than discarding it.
    result.map(Option::unwrap_or_default)
}

pub fn from_path(path: &std::path::Path) -> io::Result<Vec<u8>> {
    // xattr::get inspects the leaf itself, without following symlinks.
    normalize(xattr::get(path, ACCESS_ACL))
}

pub fn from_file(file: &std::fs::File) -> io::Result<Vec<u8>> {
    normalize(file.get_xattr(ACCESS_ACL))
}

#[cfg(test)]
mod tests {
    #[test]
    fn unsupported_acl_inspection_is_not_an_empty_acl() {
        let error = super::normalize(Err(std::io::Error::from_raw_os_error(
            nix::libc::EOPNOTSUPP,
        )))
        .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(nix::libc::EOPNOTSUPP));
        assert!(super::normalize(Ok(None)).unwrap().is_empty());
    }
}

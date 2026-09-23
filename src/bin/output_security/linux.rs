use std::io;
use xattr::FileExt as _;

const ACCESS_ACL: &str = "system.posix_acl_access";

fn reject_native_acls(mut get: impl FnMut(&str) -> io::Result<Option<Vec<u8>>>) -> io::Result<()> {
    // Older NFSv4 kernels can report ENODATA for POSIX ACLs even though the
    // inode has a native ACL. SMB can also expose its own security descriptor.
    // These models are not represented by the POSIX snapshot below.
    for name in ["system.nfs4_acl", "system.cifs_acl", "system.smb3_acl"] {
        match get(name) {
            Ok(Some(_)) => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "cannot retain native filesystem ACLs in atomic replacement; use a new output path",
                ));
            }
            Ok(None) => {}
            // This particular native model is unavailable; still require the
            // POSIX ACL query itself to succeed before permitting replacement.
            Err(error) if error.raw_os_error() == Some(nix::libc::EOPNOTSUPP) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn normalize(result: io::Result<Option<Vec<u8>>>) -> io::Result<Vec<u8>> {
    // Unsupported POSIX ACL inspection is not proof of absent protection:
    // NFSv4 exposes a different ACL model. Fail closed rather than discarding it.
    result.map(Option::unwrap_or_default)
}

pub fn from_path(path: &std::path::Path) -> io::Result<Vec<u8>> {
    // xattr::get inspects the leaf itself, without following symlinks.
    reject_native_acls(|name| xattr::get(path, name))?;
    normalize(xattr::get(path, ACCESS_ACL))
}

pub fn from_file(file: &std::fs::File) -> io::Result<Vec<u8>> {
    reject_native_acls(|name| file.get_xattr(name))?;
    normalize(file.get_xattr(ACCESS_ACL))
}

#[cfg(test)]
mod tests {
    #[test]
    fn native_acls_are_not_mistaken_for_absent_posix_acls() {
        for native_name in ["system.nfs4_acl", "system.cifs_acl", "system.smb3_acl"] {
            let error = super::reject_native_acls(|name| {
                if name == native_name {
                    Ok(Some(vec![1]))
                } else {
                    Ok(None)
                }
            })
            .unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        }
        super::reject_native_acls(|_| {
            Err(std::io::Error::from_raw_os_error(nix::libc::EOPNOTSUPP))
        })
        .unwrap();
        let error = super::reject_native_acls(|_| {
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
        })
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    }

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

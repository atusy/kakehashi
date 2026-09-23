use nix::sys::statfs::{FsType, NFS_SUPER_MAGIC, SMB_SUPER_MAGIC, fstatfs};
use std::io;
use xattr::FileExt as _;

const ACCESS_ACL: &str = "system.posix_acl_access";
// Linux UAPI linux/magic.h; nix does not export the CIFS/SMB2 constants.
const CIFS_SUPER_MAGIC: FsType = FsType(0xff53_4d42u32 as _);
const SMB2_SUPER_MAGIC: FsType = FsType(0xfe53_4d42u32 as _);

fn check_filesystem(kind: FsType) -> io::Result<()> {
    // Server/mount/kernel options can hide native ACL queries on these models.
    // Refuse them even if every xattr probe reports absent or unsupported.
    if [
        NFS_SUPER_MAGIC,
        SMB_SUPER_MAGIC,
        CIFS_SUPER_MAGIC,
        SMB2_SUPER_MAGIC,
    ]
    .contains(&kind)
    {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "cannot verify network filesystem access rules for atomic replacement; use a new output path",
        ));
    }
    Ok(())
}

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
    use std::os::unix::fs::OpenOptionsExt as _;

    // Keep the old writer's kernel access checks without truncating or writing.
    // In particular fs-verity forbids writable opens but permits replacement;
    // matching mode/ACL/labels alone would silently bypass its integrity policy.
    // NOFOLLOW retains leaf refusal; NONBLOCK avoids blocking on a raced FIFO
    // or a write lease. Read metadata through this same non-truncating handle.
    let file = std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing to replace non-regular output",
        ));
    }
    from_file(&file)
}

pub fn from_file(file: &std::fs::File) -> io::Result<Vec<u8>> {
    check_filesystem(fstatfs(file)?.filesystem_type())?;
    snapshot(|name| file.get_xattr(name))
}

fn snapshot(mut get: impl FnMut(&str) -> io::Result<Option<Vec<u8>>>) -> io::Result<Vec<u8>> {
    reject_native_acls(&mut get)?;
    let mut protection = vec![normalize(get(ACCESS_ACL))?];
    // Linux mandatory-access labels are independent of POSIX ACLs and may
    // differ from a new inode's inherited defaults. Compare, never copy them.
    for name in [
        "security.selinux",
        "security.SMACK64",
        "security.SMACK64EXEC",
        "security.SMACK64MMAP",
        "security.SMACK64TRANSMUTE",
    ] {
        let value = match get(name) {
            Ok(value) => value.unwrap_or_default(),
            // An inactive LSM has no handler for its security attribute. This
            // does not relax the mandatory POSIX/native ACL checks above.
            Err(error) if error.raw_os_error() == Some(nix::libc::EOPNOTSUPP) => Vec::new(),
            Err(error) => return Err(error),
        };
        protection.push(value);
    }
    // Preserve field boundaries: concatenating labels could hide a change.
    serde_json::to_vec(&protection).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    #[test]
    fn network_acl_models_are_refused_even_without_visible_attributes() {
        for kind in [
            super::NFS_SUPER_MAGIC,
            super::SMB_SUPER_MAGIC,
            super::CIFS_SUPER_MAGIC,
            super::SMB2_SUPER_MAGIC,
        ] {
            assert_eq!(
                super::check_filesystem(kind).unwrap_err().kind(),
                std::io::ErrorKind::Unsupported
            );
        }
        super::check_filesystem(nix::sys::statfs::EXT4_SUPER_MAGIC).unwrap();
        super::check_filesystem(nix::sys::statfs::TMPFS_MAGIC).unwrap();
    }

    #[test]
    fn mandatory_labels_participate_in_protection_comparison() {
        for label in [
            "security.selinux",
            "security.SMACK64",
            "security.SMACK64EXEC",
            "security.SMACK64MMAP",
            "security.SMACK64TRANSMUTE",
        ] {
            let read = |value: &'static [u8]| {
                super::snapshot(|name| {
                    Ok(if name == label {
                        Some(value.to_vec())
                    } else {
                        None
                    })
                })
                .unwrap()
            };
            assert_ne!(read(b"restricted"), read(b"inherited"));
            assert_eq!(read(b"restricted"), read(b"restricted"));
        }
        assert!(
            super::snapshot(|name| {
                if name == "security.selinux" {
                    Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
                } else {
                    Ok(None)
                }
            })
            .is_err()
        );
    }

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

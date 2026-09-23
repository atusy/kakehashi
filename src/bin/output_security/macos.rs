use std::ffi::{c_char, c_int, c_void};
use std::io;
use std::os::unix::io::AsRawFd as _;

// Darwin sys/acl.h. The libc crate does not expose these APIs.
const ACL_TYPE_EXTENDED: c_int = 0x100;
unsafe extern "C" {
    fn acl_get_fd_np(fd: c_int, kind: c_int) -> *mut c_void;
    fn acl_to_text(acl: *mut c_void, length: *mut isize) -> *mut c_char;
    fn acl_free(object: *mut c_void) -> c_int;
}

struct AclObject(*mut c_void);

impl Drop for AclObject {
    fn drop(&mut self) {
        // SAFETY: this owns exactly one allocation returned by an ACL API.
        unsafe { acl_free(self.0) };
    }
}

fn text(acl: *mut c_void) -> io::Result<Vec<u8>> {
    if acl.is_null() {
        let error = io::Error::last_os_error();
        // Darwin reports ENOENT for an existing inode with no extended ACL.
        // Unsupported inspection is deliberately an error: unavailable ACL
        // information does not establish that replacement retains protection.
        return if error.raw_os_error() == Some(nix::libc::ENOENT) {
            Ok(Vec::new())
        } else {
            Err(error)
        };
    }
    let acl = AclObject(acl);
    let mut length = 0;
    // SAFETY: acl is valid and length points to writable storage.
    let value = unsafe { acl_to_text(acl.0, &mut length) };
    if value.is_null() {
        return Err(io::Error::last_os_error());
    }
    let value = AclObject(value.cast());
    let length = usize::try_from(length).map_err(io::Error::other)?;
    // SAFETY: acl_to_text returned an allocation containing length bytes.
    Ok(unsafe { std::slice::from_raw_parts(value.0.cast::<u8>(), length) }.to_vec())
}

pub fn from_file(file: &std::fs::File) -> io::Result<Vec<u8>> {
    // SAFETY: file owns a live descriptor for the duration of this call.
    text(unsafe { acl_get_fd_np(file.as_raw_fd(), ACL_TYPE_EXTENDED) })
}

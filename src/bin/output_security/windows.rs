use std::io;
use std::os::windows::{ffi::OsStrExt as _, io::AsRawHandle as _};
use windows_sys::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    DACL_SECURITY_INFORMATION, GROUP_SECURITY_INFORMATION, GetFileSecurityW,
    GetKernelObjectSecurity, LABEL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR,
};

// Include mandatory integrity labels, which can restrict access independently
// of the DACL. These fields require READ_CONTROL, not the audit-SACL privilege.
const INFORMATION: u32 = OWNER_SECURITY_INFORMATION
    | GROUP_SECURITY_INFORMATION
    | DACL_SECURITY_INFORMATION
    | LABEL_SECURITY_INFORMATION;

fn read(get: impl Fn(PSECURITY_DESCRIPTOR, u32, *mut u32) -> i32) -> io::Result<Vec<u8>> {
    let mut needed = 0;
    if get(std::ptr::null_mut(), 0, &mut needed) == 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32) {
            return Err(error);
        }
    }
    // usize storage supplies alignment for the native descriptor. A concurrent
    // descriptor-size change simply fails this attempt, leaving the output alone.
    let mut descriptor = vec![0usize; (needed as usize).div_ceil(size_of::<usize>())];
    let pointer = descriptor.as_mut_ptr().cast();
    if get(pointer, needed, &mut needed) == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut string = std::ptr::null_mut();
    let mut length = 0;
    // SAFETY: pointer contains a successfully retrieved security descriptor;
    // string and length are writable outputs. SDDL normalizes relative offsets.
    if unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            pointer,
            SDDL_REVISION_1,
            INFORMATION,
            &mut string,
            &mut length,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the successful conversion allocated length UTF-16 code units,
    // including the terminator, and requires LocalFree after copying.
    let result = unsafe { std::slice::from_raw_parts(string, length as usize) }
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect();
    unsafe { LocalFree(string.cast()) };
    Ok(result)
}

pub fn from_path(path: &std::path::Path) -> io::Result<Vec<u8>> {
    let mut path: Vec<u16> = path.as_os_str().encode_wide().collect();
    if path.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path contains NUL",
        ));
    }
    path.push(0);
    read(|buffer, length, needed| {
        // SAFETY: path is NUL-terminated and read supplies the output buffer.
        unsafe { GetFileSecurityW(path.as_ptr(), INFORMATION, buffer, length, needed) }
    })
}

pub fn from_file(file: &std::fs::File) -> io::Result<Vec<u8>> {
    read(|buffer, length, needed| {
        // SAFETY: file owns a live handle and read supplies the output buffer.
        unsafe {
            GetKernelObjectSecurity(file.as_raw_handle(), INFORMATION, buffer, length, needed)
        }
    })
}

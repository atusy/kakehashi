use std::io;
use std::os::windows::io::AsRawHandle as _;
use windows_sys::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    ATTRIBUTE_SECURITY_INFORMATION, DACL_SECURITY_INFORMATION, GROUP_SECURITY_INFORMATION,
    GetKernelObjectSecurity, LABEL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR, SCOPE_SECURITY_INFORMATION,
};

// Integrity labels, resource attributes and central policy IDs can restrict
// access independently of the DACL. All require only READ_CONTROL to inspect.
// Auditing policy is outside this access-control snapshot: reading audit ACEs
// requires ACCESS_SYSTEM_SECURITY and would require privileged forced output.
const INFORMATION: u32 = OWNER_SECURITY_INFORMATION
    | GROUP_SECURITY_INFORMATION
    | DACL_SECURITY_INFORMATION
    | LABEL_SECURITY_INFORMATION
    | ATTRIBUTE_SECURITY_INFORMATION
    | SCOPE_SECURITY_INFORMATION;

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

pub fn from_file(file: &std::fs::File) -> io::Result<Vec<u8>> {
    read(|buffer, length, needed| {
        // SAFETY: file owns a live handle and read supplies the output buffer.
        unsafe {
            GetKernelObjectSecurity(file.as_raw_handle(), INFORMATION, buffer, length, needed)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Foundation::SetLastError;
    use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
    use windows_sys::Win32::Security::GetSecurityDescriptorLength;

    fn snapshot(sddl: &str) -> Vec<u8> {
        let text: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
        let mut descriptor = std::ptr::null_mut();
        // SAFETY: text is terminated and descriptor is a writable output.
        assert_ne!(
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    text.as_ptr(),
                    SDDL_REVISION_1,
                    &mut descriptor,
                    std::ptr::null_mut(),
                )
            },
            0
        );
        // SAFETY: the conversion returned a valid native descriptor.
        let length = unsafe { GetSecurityDescriptorLength(descriptor) };
        let result = read(|buffer, capacity, needed| {
            // SAFETY: read supplies a writable length and a buffer of capacity
            // bytes, or null on its initial size query. The source remains live.
            unsafe {
                *needed = length;
                if capacity < length {
                    SetLastError(ERROR_INSUFFICIENT_BUFFER);
                    return 0;
                }
                std::ptr::copy_nonoverlapping(
                    descriptor.cast::<u8>(),
                    buffer.cast::<u8>(),
                    length as usize,
                );
            }
            1
        });
        unsafe { LocalFree(descriptor) };
        result.unwrap()
    }

    #[test]
    fn resource_attributes_and_central_policy_affect_snapshot() {
        let resource = r#"O:SYG:SYD:(A;;FA;;;SY)S:(RA;;;;;WD;("Department",TS,0x0,"Finance"))"#;
        let policy = "O:SYG:SYD:(A;;FA;;;SY)S:ARAI(SP;ID;;;;S-1-17-1442530252-1178042555-1247349694-2318402534)";
        assert_ne!(
            snapshot(resource),
            snapshot(&resource.replace("Finance", "Engineering"))
        );
        assert_ne!(
            snapshot(policy),
            snapshot(&policy.replace("2318402534", "2318402535"))
        );
    }
}

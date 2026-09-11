//! Windows implementation: protected DACLs expressed as SDDL, token elevation checks and a
//! UAC relaunch. This file and `service/windows.rs` contain every `unsafe` block in the crate.

use std::ffi::{c_void, OsStr, OsString};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::ptr;

use windows_sys::Win32::Foundation::ERROR_CANCELLED;
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, LocalFree, ERROR_ALREADY_EXISTS, GENERIC_WRITE, HANDLE,
    INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW,
    ConvertStringSecurityDescriptorToSecurityDescriptorW, GetNamedSecurityInfoW,
    SetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    GetSecurityDescriptorDacl, ACL, DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateDirectoryW, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_ALWAYS,
};
use windows_sys::Win32::System::Com::{
    CoInitializeEx, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetExitCodeProcess, OpenProcessToken, WaitForSingleObject, INFINITE,
};
use windows_sys::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

use super::winquote::quote_command_line;
use super::PlatformError;

/// File name of the child binary next to the agent executable.
pub const CHILD_BINARY_NAME: &str = "logger-child.exe";

/// Protected DACL: full access for BUILTIN\Administrators and SYSTEM, nothing else,
/// no inheritance from the parent directory (`P`).
pub const FILE_SDDL: &str = "D:P(A;;FA;;;BA)(A;;FA;;;SY)";

/// Same principals; `OI`/`CI` make the entries inherit to files and subdirectories.
pub const DIR_SDDL: &str = "D:P(A;OICI;FA;;;BA)(A;OICI;FA;;;SY)";

/// `%ProgramData%\FlamingoAgent`, falling back to `C:\ProgramData\FlamingoAgent`.
pub fn default_log_dir() -> PathBuf {
    std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
        .join("FlamingoAgent")
}

/// NUL-terminated UTF-16 for Win32 `W` APIs.
pub(crate) fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

pub(crate) fn last_error(call: &'static str) -> PlatformError {
    // SAFETY: GetLastError has no preconditions.
    let code = unsafe { GetLastError() };
    PlatformError::Os { call, code }
}

/// A self-relative security descriptor allocated by the system; released with `LocalFree`.
struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

impl SecurityDescriptor {
    fn from_sddl(sddl: &str) -> Result<Self, PlatformError> {
        let text = wide(OsStr::new(sddl));
        let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
        let mut size = 0u32;
        // SAFETY: `text` is NUL-terminated and outlives the call; out-pointers are valid.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                text.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                &mut size,
            )
        };
        if ok == 0 || descriptor.is_null() {
            return Err(last_error(
                "ConvertStringSecurityDescriptorToSecurityDescriptorW",
            ));
        }
        Ok(Self(descriptor))
    }

    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0,
            bInheritHandle: 0,
        }
    }

    fn dacl(&self) -> Result<*mut ACL, PlatformError> {
        let mut present = 0i32;
        let mut dacl: *mut ACL = ptr::null_mut();
        let mut defaulted = 0i32;
        // SAFETY: self.0 is a valid descriptor for the lifetime of self.
        let ok =
            unsafe { GetSecurityDescriptorDacl(self.0, &mut present, &mut dacl, &mut defaulted) };
        if ok == 0 {
            return Err(last_error("GetSecurityDescriptorDacl"));
        }
        if present == 0 || dacl.is_null() {
            return Err(PlatformError::Os {
                call: "GetSecurityDescriptorDacl (descriptor has no DACL)",
                code: 0,
            });
        }
        Ok(dacl)
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: the pointer came from a Win32 allocator whose documented release is LocalFree.
        unsafe { LocalFree(self.0) };
    }
}

/// Create the directory born-locked with `DIR_SDDL`; if it already exists, re-apply the DACL.
pub fn secure_dir(path: &Path) -> Result<(), PlatformError> {
    let descriptor = SecurityDescriptor::from_sddl(DIR_SDDL)?;
    let name = wide(path.as_os_str());
    let attributes = descriptor.attributes();
    // SAFETY: `name` is NUL-terminated; `attributes` and its descriptor outlive the call.
    let created = unsafe { CreateDirectoryW(name.as_ptr(), &attributes) };
    // SAFETY: GetLastError has no preconditions.
    if created == 0 && unsafe { GetLastError() } != ERROR_ALREADY_EXISTS {
        return Err(last_error("CreateDirectoryW"));
    }
    apply_dacl(path, &descriptor)
}

/// Create the file born-locked with `FILE_SDDL` (content untouched if it exists), then
/// re-apply the protected DACL so a pre-existing or hand-edited file is corrected too.
/// If the file already exists but its current DACL denies the caller `GENERIC_WRITE`
/// (so `CreateFileW` itself fails), the DACL is still re-applied — an elevated caller
/// typically holds `WRITE_DAC` even without data access — which is what actually
/// corrects it; `CreateFileW`'s error is only returned when the file does not exist.
pub fn secure_file(path: &Path) -> Result<(), PlatformError> {
    let descriptor = SecurityDescriptor::from_sddl(FILE_SDDL)?;
    let name = wide(path.as_os_str());
    let attributes = descriptor.attributes();
    // SAFETY: `name` is NUL-terminated; `attributes` outlives the call; the handle is closed below.
    let handle: HANDLE = unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            &attributes,
            OPEN_ALWAYS,
            FILE_ATTRIBUTE_NORMAL,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        let create_err = last_error("CreateFileW");
        return if path.exists() {
            apply_dacl(path, &descriptor)
        } else {
            Err(create_err)
        };
    }
    // SAFETY: `handle` is valid and owned here.
    unsafe { CloseHandle(handle) };
    apply_dacl(path, &descriptor)
}

fn apply_dacl(path: &Path, descriptor: &SecurityDescriptor) -> Result<(), PlatformError> {
    let dacl = descriptor.dacl()?;
    let mut name = wide(path.as_os_str());
    // SAFETY: `name` is NUL-terminated and mutable as the API requires; `dacl` points into
    // `descriptor`, which is alive for the call. Owner/group/SACL are not being set.
    let code = unsafe {
        SetNamedSecurityInfoW(
            name.as_mut_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            dacl,
            ptr::null(),
        )
    };
    if code != 0 {
        return Err(PlatformError::Os {
            call: "SetNamedSecurityInfoW",
            code,
        });
    }
    Ok(())
}

/// The effective DACL as an SDDL string, e.g. `D:P(A;;FA;;;BA)(A;;FA;;;SY)` (Windows may
/// render the flags as `PAI`).
pub fn describe_protection(path: &Path) -> Result<String, PlatformError> {
    let name = wide(path.as_os_str());
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    // SAFETY: `name` is NUL-terminated; only the descriptor out-pointer is requested.
    let code = unsafe {
        GetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    if code != 0 || descriptor.is_null() {
        return Err(PlatformError::Os {
            call: "GetNamedSecurityInfoW",
            code,
        });
    }
    let owned = SecurityDescriptor(descriptor);
    let mut text: *mut u16 = ptr::null_mut();
    let mut len = 0u32;
    // SAFETY: `owned.0` is valid; out-pointers are valid.
    let ok = unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            owned.0,
            SDDL_REVISION_1,
            DACL_SECURITY_INFORMATION,
            &mut text,
            &mut len,
        )
    };
    if ok == 0 || text.is_null() {
        return Err(last_error(
            "ConvertSecurityDescriptorToStringSecurityDescriptorW",
        ));
    }
    // SAFETY: `text` points to `len` UTF-16 units allocated by the system; freed once, here.
    let string = unsafe {
        let units = std::slice::from_raw_parts(text, len as usize);
        let s = OsString::from_wide(units).to_string_lossy().into_owned();
        LocalFree(text.cast::<c_void>());
        s
    };
    Ok(string.trim_end_matches('\0').to_string())
}

/// True when the current token is elevated (a full administrator token or SYSTEM).
pub fn is_privileged() -> Result<bool, PlatformError> {
    let mut token: HANDLE = ptr::null_mut();
    // SAFETY: GetCurrentProcess returns a pseudo-handle that needs no closing; `token` is a
    // valid out-pointer.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(last_error("OpenProcessToken"));
    }
    let mut info = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut returned = 0u32;
    // SAFETY: `token` is valid; the buffer is exactly sizeof(TOKEN_ELEVATION).
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            (&mut info as *mut TOKEN_ELEVATION).cast::<c_void>(),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        )
    };
    let failure = (ok == 0).then(|| last_error("GetTokenInformation"));
    // SAFETY: `token` was opened above and is not used after this.
    unsafe { CloseHandle(token) };
    match failure {
        Some(err) => Err(err),
        None => Ok(info.TokenIsElevated != 0),
    }
}

/// Relaunch this executable through the shell's `runas` verb (one UAC prompt), wait for the
/// elevated instance and return its exit code. Fails with `ElevationDeclined` if the user
/// cancels the prompt.
pub fn relaunch_privileged(args: &[OsString]) -> Result<i32, PlatformError> {
    let exe =
        std::env::current_exe().map_err(|e| PlatformError::io("locating own executable", e))?;
    let verb = wide(OsStr::new("runas"));
    let file = wide(exe.as_os_str());
    let parameters = wide(OsStr::new(&quote_command_line(args)));

    // SAFETY: COM initialisation is recommended before ShellExecuteEx; a failure (e.g. already
    // initialised with another model) is harmless for this call.
    unsafe {
        CoInitializeEx(
            ptr::null(),
            (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32,
        )
    };

    // SAFETY: an all-zero SHELLEXECUTEINFOW is the documented "unset" state.
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS;
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.lpParameters = parameters.as_ptr();
    info.nShow = SW_SHOWNORMAL;

    // SAFETY: every string pointer is NUL-terminated and outlives the call.
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        // SAFETY: GetLastError has no preconditions.
        return Err(match unsafe { GetLastError() } {
            ERROR_CANCELLED => PlatformError::ElevationDeclined,
            code => PlatformError::Os {
                call: "ShellExecuteExW",
                code,
            },
        });
    }
    if info.hProcess.is_null() {
        return Err(PlatformError::Os {
            call: "ShellExecuteExW (no process handle returned)",
            code: 0,
        });
    }
    let mut exit_code = 0u32;
    // SAFETY: hProcess is a real handle we own because of SEE_MASK_NOCLOSEPROCESS.
    unsafe {
        WaitForSingleObject(info.hProcess, INFINITE);
        GetExitCodeProcess(info.hProcess, &mut exit_code);
        CloseHandle(info.hProcess);
    }
    Ok(exit_code as i32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn assert_file_policy(sddl: &str) {
        assert!(sddl.starts_with("D:P"), "not protected: {sddl}");
        assert!(
            sddl.ends_with("(A;;FA;;;BA)(A;;FA;;;SY)"),
            "unexpected ACEs: {sddl}"
        );
    }

    #[test]
    fn secure_file_creates_born_locked() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("child.log");
        secure_file(&file).unwrap();
        assert!(file.is_file());
        assert_file_policy(&describe_protection(&file).unwrap());
    }

    #[test]
    fn secure_file_replaces_inherited_acl_and_keeps_content() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("child.log");
        fs::write(&file, "keep me\r\n").unwrap();
        let before = describe_protection(&file).unwrap();
        assert!(
            !before.starts_with("D:P"),
            "temp file unexpectedly protected: {before}"
        );
        secure_file(&file).unwrap();
        assert_file_policy(&describe_protection(&file).unwrap());
        assert_eq!(fs::read_to_string(&file).unwrap(), "keep me\r\n");
    }

    #[test]
    fn secure_dir_applies_inheritable_protected_dacl() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("FlamingoAgent");
        secure_dir(&dir).unwrap();
        secure_dir(&dir).unwrap(); // idempotent
        let sddl = describe_protection(&dir).unwrap();
        assert!(sddl.starts_with("D:P"), "{sddl}");
        assert!(sddl.ends_with("(A;OICI;FA;;;BA)(A;OICI;FA;;;SY)"), "{sddl}");
    }

    #[test]
    fn default_log_dir_is_under_program_data() {
        let dir = default_log_dir();
        assert!(dir.ends_with("FlamingoAgent"), "{}", dir.display());
    }

    #[test]
    fn is_privileged_answers() {
        let _ = is_privileged().unwrap();
    }
}

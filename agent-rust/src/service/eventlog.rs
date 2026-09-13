//! Windows Application event log reporting for the service lifecycle (design §4.3).
//!
//! The service has no console, and `agent.log` sits inside a directory only administrators
//! can open. The Application log is where an operator looks first, so the lifecycle facts
//! go there as well: started, stopped, failed to start, panicked, bad arguments. The agent
//! executable carries its own message table (embedded by `build.rs`), so the viewer renders
//! each event's text from the agent itself. Everything here is best-effort by design: an
//! event log that cannot be written must never stop the agent from doing its job, so
//! failures are reported to the caller as `false` and ignored.

use std::ffi::OsStr;
use std::path::Path;
use std::ptr;

use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, HANDLE};
use windows_sys::Win32::System::EventLog::{
    DeregisterEventSource, RegisterEventSourceW, ReportEventW, EVENTLOG_ERROR_TYPE,
    EVENTLOG_INFORMATION_TYPE,
};
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteKeyW, RegSetValueExW, HKEY, HKEY_LOCAL_MACHINE,
    KEY_WRITE, REG_DWORD, REG_EXPAND_SZ, REG_OPTION_NON_VOLATILE,
};

use super::ServiceError;
use crate::platform::wide;

/// The service reached Running.
pub const EVENT_STARTED: u32 = 1;
/// The service stopped on request.
pub const EVENT_STOPPED: u32 = 2;
/// Bootstrap failed before the loop started; the message carries the error.
pub const EVENT_BOOTSTRAP_FAILED: u32 = 3;
/// The agent panicked.
pub const EVENT_PANIC: u32 = 4;
/// The registered command line did not parse.
pub const EVENT_BAD_ARGUMENTS: u32 = 5;

/// `EVENTLOG_ERROR_TYPE | EVENTLOG_WARNING_TYPE | EVENTLOG_INFORMATION_TYPE`.
const TYPES_SUPPORTED: u32 = 7;

/// An open handle to the Application log for one source name.
#[derive(Debug)]
pub struct EventSource {
    handle: HANDLE,
}

impl EventSource {
    /// Open the source on the local machine. `None` when the event log service refuses,
    /// in which case lifecycle reporting is simply skipped.
    pub fn open(source: &str) -> Option<Self> {
        let name = wide(OsStr::new(source));
        // SAFETY: `name` is NUL-terminated; a null server name means the local machine.
        let handle = unsafe { RegisterEventSourceW(ptr::null(), name.as_ptr()) };
        (!handle.is_null()).then_some(Self { handle })
    }

    /// Report an informational event with one message string.
    pub fn info(&self, id: u32, message: &str) -> bool {
        self.report(EVENTLOG_INFORMATION_TYPE, id, message)
    }

    /// Report an error event with one message string.
    pub fn error(&self, id: u32, message: &str) -> bool {
        self.report(EVENTLOG_ERROR_TYPE, id, message)
    }

    fn report(&self, kind: u16, id: u32, message: &str) -> bool {
        let text = wide(OsStr::new(message));
        let strings = [text.as_ptr()];
        // SAFETY: `handle` is open; `strings` holds one NUL-terminated string and the count
        // says so; no SID and no binary data are attached.
        unsafe {
            ReportEventW(
                self.handle,
                kind,
                0,
                id,
                ptr::null_mut(),
                1,
                0,
                strings.as_ptr(),
                ptr::null(),
            ) != 0
        }
    }
}

impl Drop for EventSource {
    fn drop(&mut self) {
        // SAFETY: `handle` was returned by RegisterEventSourceW and is closed exactly once.
        unsafe { DeregisterEventSource(self.handle) };
    }
}

fn source_key(source: &str) -> Vec<u16> {
    wide(OsStr::new(&format!(
        r"SYSTEM\CurrentControlSet\Services\EventLog\Application\{source}"
    )))
}

/// Register the source under the Application log so Event Viewer renders the message text
/// instead of "the description for Event ID ... cannot be found". `message_file` is the
/// module carrying the message table: the agent's own executable, whose build embeds a
/// table mapping IDs 1–5 to the first insertion string (see `build.rs`). Needs
/// administrator rights, which `--install` already has. Idempotent.
pub fn register_source(source: &str, message_file: &Path) -> Result<(), ServiceError> {
    let key_path = source_key(source);
    let mut key: HKEY = ptr::null_mut();
    // SAFETY: all pointers are valid for the call; `key` receives the opened handle.
    let code = unsafe {
        RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            key_path.as_ptr(),
            0,
            ptr::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            ptr::null(),
            &mut key,
            ptr::null_mut(),
        )
    };
    if code != ERROR_SUCCESS {
        return Err(registry_error("RegCreateKeyExW", code));
    }
    let result = set_values(key, message_file);
    // SAFETY: `key` is open and closed exactly once.
    unsafe { RegCloseKey(key) };
    result
}

fn set_values(key: HKEY, message_file: &Path) -> Result<(), ServiceError> {
    let name = wide(OsStr::new("EventMessageFile"));
    let file = wide(message_file.as_os_str());
    let size = u32::try_from(file.len() * 2).map_err(|_| registry_error("RegSetValueExW", 0))?;
    // SAFETY: `file` is a NUL-terminated UTF-16 buffer of `size` bytes.
    let code = unsafe {
        RegSetValueExW(
            key,
            name.as_ptr(),
            0,
            REG_EXPAND_SZ,
            file.as_ptr().cast(),
            size,
        )
    };
    if code != ERROR_SUCCESS {
        return Err(registry_error("RegSetValueExW", code));
    }
    let name = wide(OsStr::new("TypesSupported"));
    let value = TYPES_SUPPORTED.to_ne_bytes();
    // SAFETY: `value` is a 4-byte buffer and the size says so.
    let code = unsafe {
        RegSetValueExW(
            key,
            name.as_ptr(),
            0,
            REG_DWORD,
            value.as_ptr(),
            value.len() as u32,
        )
    };
    if code != ERROR_SUCCESS {
        return Err(registry_error("RegSetValueExW", code));
    }
    Ok(())
}

/// Remove the source registration. A source that was never registered is not an error.
pub fn unregister_source(source: &str) -> Result<(), ServiceError> {
    let key_path = source_key(source);
    // SAFETY: `key_path` is NUL-terminated.
    let code = unsafe { RegDeleteKeyW(HKEY_LOCAL_MACHINE, key_path.as_ptr()) };
    if code == ERROR_SUCCESS || code == ERROR_FILE_NOT_FOUND {
        Ok(())
    } else {
        Err(registry_error("RegDeleteKeyW", code))
    }
}

fn registry_error(call: &'static str, code: u32) -> ServiceError {
    ServiceError::Registry { call, code }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reporting needs no registration: the event log service accepts any source name and
    /// files the event under the Application log. (This leaves one informational test event
    /// in the Application log of the machine running the tests.)
    #[test]
    fn unregistered_source_can_still_report() {
        let source = EventSource::open("FlamingoAgentTestSource").expect("open");
        assert!(source.info(EVENT_STARTED, "flamingo-agent unit test event"));
    }

    /// Registration writes and removes the message-file key under HKLM, which needs an
    /// elevated token; removing a key that does not exist is accepted as already done.
    #[test]
    fn register_and_unregister_round_trip() {
        if !crate::platform::is_privileged().unwrap() {
            eprintln!("skipped: not elevated");
            return;
        }
        let exe = std::env::current_exe().unwrap();
        unregister_source("FlamingoAgentTestSource").unwrap();
        register_source("FlamingoAgentTestSource", &exe).unwrap();
        register_source("FlamingoAgentTestSource", &exe).expect("idempotent");
        let (file, kind) = read_string_value("FlamingoAgentTestSource", "EventMessageFile");
        assert_eq!(file, exe.display().to_string());
        assert_eq!(kind, REG_EXPAND_SZ);
        assert_eq!(
            read_dword_value("FlamingoAgentTestSource", "TypesSupported"),
            TYPES_SUPPORTED
        );
        unregister_source("FlamingoAgentTestSource").unwrap();
        assert!(
            !key_exists("FlamingoAgentTestSource"),
            "the registration must be gone after unregister"
        );
        unregister_source("FlamingoAgentTestSource").expect("absent key is not an error");
    }

    /// `ReportEventW` refuses a string above its documented limit of 31,839 characters, so
    /// the boolean the source returns reflects the call, not a constant. (Leaves one error
    /// test event in the Application log of the machine running the tests.)
    #[test]
    fn report_returns_false_when_the_event_log_refuses() {
        let source = EventSource::open("FlamingoAgentTestSource").expect("open");
        assert!(source.error(
            EVENT_BOOTSTRAP_FAILED,
            "flamingo-agent unit test error event"
        ));
        let too_long = "x".repeat(40_000);
        assert!(!source.info(EVENT_STARTED, &too_long));
        assert!(!source.error(EVENT_BOOTSTRAP_FAILED, &too_long));
    }

    fn read_string_value(source: &str, value: &str) -> (String, u32) {
        use windows_sys::Win32::System::Registry::{
            RegGetValueW, RRF_NOEXPAND, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ,
        };
        let key_path = source_key(source);
        let value = wide(OsStr::new(value));
        let mut kind = 0u32;
        let mut buffer = [0u16; 1024];
        let mut size = (buffer.len() * 2) as u32;
        // SAFETY: every pointer is valid for the call and `size` is the buffer's byte size.
        let code = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                key_path.as_ptr(),
                value.as_ptr(),
                RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND,
                &mut kind,
                buffer.as_mut_ptr().cast(),
                &mut size,
            )
        };
        assert_eq!(code, ERROR_SUCCESS, "RegGetValueW failed with {code}");
        let units = (size as usize / 2).saturating_sub(1);
        (String::from_utf16_lossy(&buffer[..units]), kind)
    }

    fn read_dword_value(source: &str, value: &str) -> u32 {
        use windows_sys::Win32::System::Registry::{RegGetValueW, RRF_RT_REG_DWORD};
        let key_path = source_key(source);
        let value = wide(OsStr::new(value));
        let mut data = 0u32;
        let mut size = 4u32;
        // SAFETY: every pointer is valid for the call and `size` is the buffer's byte size.
        let code = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                key_path.as_ptr(),
                value.as_ptr(),
                RRF_RT_REG_DWORD,
                ptr::null_mut(),
                (&mut data as *mut u32).cast(),
                &mut size,
            )
        };
        assert_eq!(code, ERROR_SUCCESS, "RegGetValueW failed with {code}");
        data
    }

    fn key_exists(source: &str) -> bool {
        use windows_sys::Win32::System::Registry::{RegOpenKeyExW, KEY_READ};
        let key_path = source_key(source);
        let mut key: HKEY = ptr::null_mut();
        // SAFETY: `key_path` is NUL-terminated and `key` is a valid out-pointer.
        let code =
            unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, key_path.as_ptr(), 0, KEY_READ, &mut key) };
        if code == ERROR_SUCCESS {
            // SAFETY: `key` is open and closed exactly once.
            unsafe { RegCloseKey(key) };
            true
        } else {
            false
        }
    }

    /// The message table `build.rs` embeds must resolve every lifecycle ID to its first
    /// insertion string, through the same `FormatMessageW` path the event log viewer uses.
    /// This test binary carries the table like the agent binary does.
    #[test]
    fn embedded_message_table_renders_the_insertion_string() {
        use windows_sys::Win32::Foundation::GetLastError;
        use windows_sys::Win32::System::Diagnostics::Debug::{
            FormatMessageW, FORMAT_MESSAGE_ARGUMENT_ARRAY, FORMAT_MESSAGE_FROM_HMODULE,
        };
        use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;

        // SAFETY: a null name asks for the calling executable's own module handle.
        let module = unsafe { GetModuleHandleW(ptr::null()) };
        assert!(!module.is_null());
        let text = wide(OsStr::new("hello from the table"));
        let arguments: [*const u16; 1] = [text.as_ptr()];
        for id in [
            EVENT_STARTED,
            EVENT_STOPPED,
            EVENT_BOOTSTRAP_FAILED,
            EVENT_PANIC,
            EVENT_BAD_ARGUMENTS,
        ] {
            let mut buffer = [0u16; 256];
            // SAFETY: `module` is valid; `arguments` holds one NUL-terminated string as the
            // ARGUMENT_ARRAY flag requires; `buffer` is writable for `len` code units.
            let written = unsafe {
                FormatMessageW(
                    FORMAT_MESSAGE_FROM_HMODULE | FORMAT_MESSAGE_ARGUMENT_ARRAY,
                    module.cast(),
                    id,
                    0,
                    buffer.as_mut_ptr(),
                    buffer.len() as u32,
                    arguments.as_ptr().cast(),
                )
            };
            // SAFETY: GetLastError has no preconditions.
            let code = unsafe { GetLastError() };
            assert!(written > 0, "no message for id {id}: error {code}");
            let rendered = String::from_utf16_lossy(&buffer[..written as usize]);
            assert_eq!(rendered.trim_end(), "hello from the table", "id {id}");
        }
    }
}

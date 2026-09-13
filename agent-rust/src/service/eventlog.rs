//! Windows Application event log reporting for the service lifecycle (design §4.3).
//!
//! The service has no console, and `agent.log` sits inside a directory only administrators
//! can open. The Application log is where an operator looks first, so the lifecycle facts
//! go there as well: started, stopped, failed to start, panicked, bad arguments. Everything
//! here is best-effort by design: an event log that cannot be written must never stop the
//! agent from doing its job, so failures are reported to the caller as `false` and ignored.

use std::ffi::OsStr;
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

/// Message file whose message table maps every ID from 1 to 1000 to the first insertion
/// string, so an event's text is shown verbatim without shipping a message DLL. It is part
/// of every Windows installation (`eventcreate.exe` registers its own sources the same way).
const MESSAGE_FILE: &str = r"%SystemRoot%\System32\eventcreate.exe";
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
/// instead of "the description for Event ID ... cannot be found". Needs administrator
/// rights, which `--install` already has. Idempotent.
pub fn register_source(source: &str) -> Result<(), ServiceError> {
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
    let result = set_values(key);
    // SAFETY: `key` is open and closed exactly once.
    unsafe { RegCloseKey(key) };
    result
}

fn set_values(key: HKEY) -> Result<(), ServiceError> {
    let name = wide(OsStr::new("EventMessageFile"));
    let file = wide(OsStr::new(MESSAGE_FILE));
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
        unregister_source("FlamingoAgentTestSource").unwrap();
        register_source("FlamingoAgentTestSource").unwrap();
        register_source("FlamingoAgentTestSource").expect("idempotent");
        unregister_source("FlamingoAgentTestSource").unwrap();
        unregister_source("FlamingoAgentTestSource").expect("absent key is not an error");
    }
}

//! Service Control Manager integration. Windows gets the real implementation (Task 13);
//! every other platform reports the operation as unsupported and always runs interactively.

/// Registered service name.
pub const SERVICE_NAME: &str = "FlamingoAgent";
/// Name shown in the Services console.
pub const SERVICE_DISPLAY_NAME: &str = "Flamingo Agent";
/// Description shown in the Services console.
pub const SERVICE_DESCRIPTION: &str =
    "Collects UTC time and process RSS every 5 seconds and launches logger-child with a restricted log file.";

/// How a bare invocation was handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dispatch {
    /// The SCM started us; the service ran to completion.
    RanAsService,
    /// Not started by the SCM; the caller should run interactively.
    NotAService,
}

/// Service management failures.
#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    /// A Service Control Manager API call failed.
    #[cfg(windows)]
    #[error("service control manager: {0}")]
    Api(#[from] windows_service::Error),
    /// The service stopped during start-up instead of reaching Running.
    #[error("service stopped during start-up with exit code {exit_code}; see agent.log or the System event log")]
    FailedToStart {
        /// Debug rendering of the reported exit code.
        exit_code: String,
    },
    /// The service did not reach the wanted state in time.
    #[error("timed out waiting for the service to reach {wanted}")]
    Timeout {
        /// The state that was awaited.
        wanted: String,
    },
    /// No service manager on this platform.
    #[error("Windows service registration is not supported on this platform; see README for the systemd/launchd mapping")]
    Unsupported,
}

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{install, run_dispatcher, uninstall};

#[cfg(not(windows))]
mod unsupported;
#[cfg(not(windows))]
pub use unsupported::{install, run_dispatcher, uninstall};

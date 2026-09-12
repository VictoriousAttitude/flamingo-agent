//! Platform boundary. Every OS-specific call in the agent lives in one of the submodules,
//! which expose an identical set of function signatures selected with `cfg`.
//! Business logic imports `crate::platform::*` and never sees a `cfg`.

/// Errors from the platform layer. Variants carry enough to log the failing call.
#[derive(Debug, thiserror::Error)]
pub enum PlatformError {
    /// A standard-library I/O operation failed.
    #[error("{context}: {source}")]
    Io {
        /// What was being attempted.
        context: &'static str,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// A raw OS API call failed.
    #[error("{call} failed with OS error {code}")]
    Os {
        /// The API that failed.
        call: &'static str,
        /// The OS error code.
        code: u32,
    },
    /// The user declined the elevation prompt.
    #[error("elevation prompt was declined")]
    ElevationDeclined,
    /// Elevation is refused outright by policy for this account, so no prompt is offered.
    #[error("elevation is blocked by policy for this account")]
    ElevationBlockedByPolicy,
    /// The operation has no implementation on this platform.
    #[error("{0} is not supported on this platform")]
    Unsupported(&'static str),
}

impl PlatformError {
    pub(crate) fn io(context: &'static str, source: std::io::Error) -> Self {
        Self::Io { context, source }
    }
}

pub mod winquote;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

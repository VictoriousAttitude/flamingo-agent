//! Platforms without a Service Control Manager.

use std::path::Path;

use super::{Dispatch, ServiceError};

/// Always unsupported.
pub fn install(_exe: &Path) -> Result<(), ServiceError> {
    Err(ServiceError::Unsupported)
}

/// Always unsupported.
pub fn uninstall() -> Result<(), ServiceError> {
    Err(ServiceError::Unsupported)
}

/// There is no SCM, so a bare invocation is always interactive.
pub fn run_dispatcher() -> Result<Dispatch, ServiceError> {
    Ok(Dispatch::NotAService)
}

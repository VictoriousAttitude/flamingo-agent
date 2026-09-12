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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_is_unsupported() {
        let err = install(Path::new("/opt/flamingo/flamingo-agent")).unwrap_err();
        assert!(matches!(err, ServiceError::Unsupported));
        // The CLI prints this and the message has to point at the documented alternative.
        assert!(err.to_string().contains("not supported"), "{err}");
        assert!(err.to_string().contains("systemd"), "{err}");
    }

    #[test]
    fn uninstall_is_unsupported() {
        assert!(matches!(uninstall(), Err(ServiceError::Unsupported)));
    }

    #[test]
    fn dispatcher_reports_not_a_service() {
        assert_eq!(run_dispatcher().unwrap(), Dispatch::NotAService);
    }
}

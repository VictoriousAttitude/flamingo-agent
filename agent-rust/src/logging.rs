//! tracing setup: a non-blocking, size-rotated file writer always, plus stderr when
//! interactive.

use std::path::Path;

use crate::rotation::{RotatingWriter, RotationPolicy};

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{fmt, EnvFilter};

/// Keeps the background writer alive; dropping it flushes the file.
#[derive(Debug)]
pub struct LogGuard {
    _file: WorkerGuard,
}

/// Logging setup failures.
#[derive(Debug, thiserror::Error)]
pub enum LoggingError {
    /// The level filter is not a valid tracing directive.
    #[error("invalid log level {0:?}")]
    Filter(String),
    /// The log path has no parent directory or file name.
    #[error("invalid agent log path {0:?}")]
    Path(String),
    /// A global subscriber was already installed.
    #[error("logging was already initialised")]
    AlreadyInitialised,
    /// The agent log could not be created or opened.
    #[error("opening the agent log: {0}")]
    Open(#[source] std::io::Error),
}

/// Install the global subscriber. Call after the log directory has been secured so the file
/// is created inside a locked directory.
pub fn init(
    agent_log: &Path,
    level: &str,
    also_stderr: bool,
    rotation: RotationPolicy,
) -> Result<LogGuard, LoggingError> {
    let filter = EnvFilter::try_new(level).map_err(|_| LoggingError::Filter(level.to_string()))?;
    if agent_log.parent().is_none() || agent_log.file_name().is_none() {
        return Err(LoggingError::Path(agent_log.display().to_string()));
    }
    let appender = RotatingWriter::open(agent_log, rotation).map_err(LoggingError::Open)?;
    let (writer, guard) = tracing_appender::non_blocking(appender);

    let file_layer = fmt::layer()
        .with_writer(writer)
        .with_ansi(false)
        .with_target(false);
    let stderr_layer =
        also_stderr.then(|| fmt::layer().with_writer(std::io::stderr).with_target(false));

    tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .with(stderr_layer)
        .try_init()
        .map_err(|_| LoggingError::AlreadyInitialised)?;

    Ok(LogGuard { _file: guard })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_ROTATION: RotationPolicy = RotationPolicy {
        max_bytes: 1 << 20,
        keep: 1,
    };

    #[test]
    fn invalid_level_is_rejected_before_installing() {
        let tmp = tempfile::tempdir().unwrap();
        let err = init(
            &tmp.path().join("agent.log"),
            "[invalid",
            false,
            TEST_ROTATION,
        )
        .unwrap_err();
        assert!(matches!(err, LoggingError::Filter(_)));
    }

    /// A root directory has no file name, so there is nowhere to write. The check runs
    /// before `try_init`, so this test neither installs a global subscriber nor depends on
    /// running before the one that does.
    #[test]
    fn path_without_file_name_is_rejected() {
        #[cfg(unix)]
        let root = Path::new("/");
        #[cfg(windows)]
        let root = Path::new(r"C:\");
        let err = init(root, "info", false, TEST_ROTATION).unwrap_err();
        assert!(matches!(err, LoggingError::Path(_)), "{err:?}");
    }

    /// A path ending in `..` has a parent but no file name; it must be rejected as a path,
    /// not attempted as a file.
    #[test]
    fn path_with_parent_but_no_file_name_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let err = init(&tmp.path().join(".."), "info", false, TEST_ROTATION).unwrap_err();
        assert!(matches!(err, LoggingError::Path(_)), "{err:?}");
    }

    #[test]
    fn writes_events_to_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("agent.log");
        let guard = init(&log, "info", false, TEST_ROTATION).unwrap();
        tracing::info!(answer = 42, "hello from the test");
        drop(guard);
        let content = std::fs::read_to_string(&log).unwrap();
        assert!(content.contains("hello from the test"), "{content}");
        assert!(content.contains("answer=42"), "{content}");
    }
}

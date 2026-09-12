//! tracing setup: a non-blocking file appender always, plus stderr when interactive.

use std::path::Path;

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
}

/// Install the global subscriber. Call after the log directory has been secured so the file
/// is created inside a locked directory.
pub fn init(agent_log: &Path, level: &str, also_stderr: bool) -> Result<LogGuard, LoggingError> {
    let filter = EnvFilter::try_new(level).map_err(|_| LoggingError::Filter(level.to_string()))?;
    let (dir, name) = match (agent_log.parent(), agent_log.file_name()) {
        (Some(dir), Some(name)) => (dir, name),
        _ => return Err(LoggingError::Path(agent_log.display().to_string())),
    };
    let appender = tracing_appender::rolling::never(dir, name);
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

    #[test]
    fn invalid_level_is_rejected_before_installing() {
        let tmp = tempfile::tempdir().unwrap();
        let err = init(&tmp.path().join("agent.log"), "[invalid", false).unwrap_err();
        assert!(matches!(err, LoggingError::Filter(_)));
    }

    #[test]
    fn writes_events_to_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("agent.log");
        let guard = init(&log, "info", false).unwrap();
        tracing::info!(answer = 42, "hello from the test");
        drop(guard);
        let content = std::fs::read_to_string(&log).unwrap();
        assert!(content.contains("hello from the test"), "{content}");
        assert!(content.contains("answer=42"), "{content}");
    }
}

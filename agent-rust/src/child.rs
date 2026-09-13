//! Spawning `logger-child`: argument construction, bounded wait, outcome classification.
//! No shell is involved anywhere, so arguments are never interpreted.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::metrics::Metrics;
use crate::platform::{self, PlatformError};

/// Where the child is and how long it may run.
#[derive(Debug, Clone)]
pub struct ChildSpec {
    /// Absolute path of the child binary.
    pub program: PathBuf,
    /// Kill the child if it has not exited within this time.
    pub timeout: Duration,
}

/// What happened to one child invocation. None of these is fatal to the agent.
#[derive(Debug)]
pub enum ChildOutcome {
    /// The child exited on its own.
    Completed {
        /// Exit code, `None` if killed by a signal.
        code: Option<i32>,
        /// Captured, trimmed stdout.
        stdout: String,
        /// Captured, trimmed stderr.
        stderr: String,
    },
    /// The child exceeded `ChildSpec::timeout` and was killed.
    TimedOut,
    /// Shutdown was requested while waiting; the child was killed.
    Cancelled,
    /// The process could not be started (missing binary, permissions, ...).
    SpawnFailed(std::io::Error),
    /// The process started but waiting for it failed.
    WaitFailed(std::io::Error),
    /// The process started but could not be bound to the agent's lifetime, so it was killed
    /// rather than left able to outlive the agent.
    BindFailed(PlatformError),
}

/// `--utc <ts> --rss-bytes <n> --log-file <path>`.
pub fn build_args(metrics: &Metrics, log_file: &Path) -> Vec<OsString> {
    vec![
        OsString::from("--utc"),
        OsString::from(metrics.utc_string()),
        OsString::from("--rss-bytes"),
        OsString::from(metrics.rss_bytes.to_string()),
        OsString::from("--log-file"),
        log_file.as_os_str().to_owned(),
    ]
}

/// Spawn the child with captured stdio and wait at most `spec.timeout`, or until `cancel`
/// fires. Dropping the wait future kills the child (`kill_on_drop`), so neither a timeout nor
/// a cancellation can leave an orphan. The child is also bound to the agent's own lifetime
/// through the platform layer (a job object on Windows, the parent-death signal on Linux), so
/// a hard kill or a crash of the agent, where nothing is dropped, cannot leave one either.
pub async fn run_child(
    spec: &ChildSpec,
    args: &[OsString],
    cancel: &CancellationToken,
) -> ChildOutcome {
    let mut command = Command::new(&spec.program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    platform::prepare_child(&mut command);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => return ChildOutcome::SpawnFailed(err),
    };
    if let Err(err) = platform::bind_child(&child) {
        // Unbound, the child could outlive a hard-killed agent; kill it now instead.
        let _ = child.start_kill();
        return ChildOutcome::BindFailed(err);
    }

    tokio::select! {
        () = cancel.cancelled() => ChildOutcome::Cancelled,
        waited = tokio::time::timeout(spec.timeout, child.wait_with_output()) => match waited {
            Err(_elapsed) => ChildOutcome::TimedOut,
            Ok(Err(err)) => ChildOutcome::WaitFailed(err),
            Ok(Ok(output)) => ChildOutcome::Completed {
                code: output.status.code(),
                stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            },
        },
    }
}

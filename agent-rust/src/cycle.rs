//! One collection cycle (design §5): collect → log → secure the child log → spawn → log the
//! outcome. Only metric collection and securing the log return errors; every child outcome
//! is logged and treated as success for the loop.

use std::sync::Arc;

use anyhow::Context;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::child::{self, ChildOutcome, ChildSpec};
use crate::config::Config;
use crate::metrics::{Metrics, MetricsError};
use crate::{metrics, platform, rotation};

/// Run one cycle, sampling with [`metrics::collect`].
pub async fn run_cycle(cfg: Arc<Config>, cancel: CancellationToken) -> anyhow::Result<()> {
    run_cycle_with(cfg, cancel, metrics::collect).await
}

/// Run one cycle with an injected collector, so a collection failure can be exercised
/// without the OS cooperating.
///
/// Exists for tests (see `tests/cycle_run.rs`); production code should call [`run_cycle`].
#[doc(hidden)]
pub async fn run_cycle_with<F>(
    cfg: Arc<Config>,
    cancel: CancellationToken,
    collect: F,
) -> anyhow::Result<()>
where
    F: FnOnce() -> Result<Metrics, MetricsError>,
{
    let sample = collect().context("collecting metrics")?;
    info!(utc = %sample.utc_string(), rss_bytes = sample.rss_bytes, "metrics");

    platform::secure_file(&cfg.child_log)
        .with_context(|| format!("securing child log {}", cfg.child_log.display()))?;
    // Rotation happens between children, so no writer holds the file; the rotated
    // generation keeps its DACL because a rename preserves the security descriptor, and the
    // fresh live file is born locked by the second secure_file. A rotation that fails (a
    // viewer holding the file open without delete sharing, say) is logged and retried next
    // cycle; it never stops the cycle.
    match rotation::rotate_if_large(&cfg.child_log, &cfg.log_rotation) {
        Ok(false) => {}
        Ok(true) => {
            info!(path = %cfg.child_log.display(), "child log rotated");
            platform::secure_file(&cfg.child_log)
                .with_context(|| format!("securing child log {}", cfg.child_log.display()))?;
        }
        Err(err) => warn!(error = %err, "child log could not be rotated; keeping the current file"),
    }

    let spec = ChildSpec {
        program: cfg.child_path.clone(),
        timeout: cfg.child_timeout,
    };
    let args = child::build_args(&sample, &cfg.child_log);
    match child::run_child(&spec, &args, &cancel).await {
        ChildOutcome::Completed {
            code: Some(0),
            stdout,
            ..
        } => {
            info!(child_stdout = %stdout, "child completed");
        }
        ChildOutcome::Completed {
            code,
            stdout,
            stderr,
        } => {
            warn!(?code, %stdout, %stderr, "child exited with failure");
        }
        ChildOutcome::TimedOut => {
            error!(
                timeout_secs = spec.timeout.as_secs(),
                "child timed out and was killed"
            );
        }
        ChildOutcome::Cancelled => info!("child wait cancelled by shutdown"),
        ChildOutcome::SpawnFailed(err) => {
            error!(path = %spec.program.display(), error = %err, "child could not be spawned");
        }
        ChildOutcome::WaitFailed(err) => error!(error = %err, "waiting for child failed"),
        ChildOutcome::BindFailed(err) => {
            error!(error = %err, "child could not be bound to the agent's lifetime and was killed");
        }
    }
    Ok(())
}

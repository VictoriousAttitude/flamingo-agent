use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use flamingo_agent::config::Config;
use flamingo_agent::cycle::{run_cycle, run_cycle_with};
use flamingo_agent::metrics::MetricsError;
use tokio_util::sync::CancellationToken;

fn fixture(name: &str) -> PathBuf {
    let ext = if cfg!(windows) { "cmd" } else { "sh" };
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(format!("{name}.{ext}"))
}

fn config(child: PathBuf, log_dir: &std::path::Path) -> Config {
    Config {
        period: Duration::from_secs(5),
        child_timeout: Duration::from_secs(4),
        child_path: child,
        log_dir: log_dir.to_path_buf(),
        agent_log: log_dir.join("agent.log"),
        child_log: log_dir.join("child.log"),
        log_level: "info".into(),
    }
}

#[tokio::test]
async fn cycle_secures_the_child_log_and_runs_the_child() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = Arc::new(config(fixture("echo_args"), tmp.path()));
    run_cycle(cfg.clone(), CancellationToken::new())
        .await
        .unwrap();
    assert!(cfg.child_log.is_file(), "child log must be pre-created");
    assert_protected(&cfg.child_log);
}

#[tokio::test]
async fn missing_child_is_not_an_error() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = Arc::new(config(PathBuf::from("/definitely/not/here"), tmp.path()));
    run_cycle(cfg, CancellationToken::new()).await.unwrap();
}

/// The protection every platform must report for the child log.
fn assert_protected(child_log: &std::path::Path) {
    let description = flamingo_agent::platform::describe_protection(child_log).unwrap();
    #[cfg(unix)]
    assert!(description.starts_with("mode=0600"), "{description}");
    #[cfg(windows)]
    assert!(description.starts_with("D:P"), "{description}");
}

#[tokio::test]
async fn metrics_failure_skips_child_and_returns_err() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = Arc::new(config(fixture("touch_marker"), tmp.path()));
    let err = run_cycle_with(cfg.clone(), CancellationToken::new(), || {
        Err(MetricsError::RssUnavailable)
    })
    .await
    .unwrap_err();
    assert!(format!("{err:#}").contains("collecting metrics"), "{err:#}");
    // Collection is the first step, so the cycle returns before the log is secured and
    // long before the child could run: the child log is never created at all.
    assert!(
        !cfg.child_log.exists(),
        "child log must not exist: {}",
        cfg.child_log.display()
    );
}

/// Unix only: a directory can be made unreachable by simply not creating it, while on
/// Windows `secure_file` on a missing directory is exercised by the same code path.
#[cfg(unix)]
#[tokio::test]
async fn secure_file_failure_returns_err_before_spawn() {
    let tmp = tempfile::tempdir().unwrap();
    let mut cfg = config(fixture("touch_marker"), tmp.path());
    // The marker fixture creates its parent directories, so the file appearing would mean
    // the child ran; securing the log must fail before the spawn and leave nothing behind.
    cfg.child_log = tmp.path().join("does/not/exist/child.log");
    let cfg = Arc::new(cfg);
    let err = run_cycle(cfg.clone(), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("securing child log"), "{err:#}");
    assert!(
        !cfg.child_log.exists(),
        "the child must not have run: {} exists",
        cfg.child_log.display()
    );
}

#[tokio::test]
async fn nonzero_child_is_ok() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = Arc::new(config(fixture("exit_3"), tmp.path()));
    // A failing child is logged, not propagated: the loop must keep running.
    run_cycle(cfg, CancellationToken::new()).await.unwrap();
}

#[tokio::test]
async fn timed_out_child_is_ok() {
    let tmp = tempfile::tempdir().unwrap();
    let mut cfg = config(fixture("sleep_10"), tmp.path());
    cfg.child_timeout = Duration::from_millis(300);
    let started = std::time::Instant::now();
    run_cycle(Arc::new(cfg), CancellationToken::new())
        .await
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the cycle waited {:?}, so the timeout did not fire",
        started.elapsed()
    );
}

#[tokio::test]
async fn dacl_is_reapplied_after_the_log_is_deleted() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = Arc::new(config(fixture("echo_args"), tmp.path()));
    run_cycle(cfg.clone(), CancellationToken::new())
        .await
        .unwrap();
    assert_protected(&cfg.child_log);

    std::fs::remove_file(&cfg.child_log).unwrap();
    run_cycle(cfg.clone(), CancellationToken::new())
        .await
        .unwrap();
    assert!(cfg.child_log.is_file(), "the log must be recreated");
    assert_protected(&cfg.child_log);
}

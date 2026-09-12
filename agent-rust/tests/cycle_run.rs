use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use flamingo_agent::config::Config;
use flamingo_agent::cycle::run_cycle;
use tokio_util::sync::CancellationToken;

fn fixture(name: &str) -> PathBuf {
    let ext = if cfg!(windows) { "cmd" } else { "sh" };
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(format!("{name}.{ext}"))
}

fn config(child: PathBuf, log_dir: &std::path::Path) -> Arc<Config> {
    Arc::new(Config {
        period: Duration::from_secs(5),
        child_timeout: Duration::from_secs(4),
        child_path: child,
        log_dir: log_dir.to_path_buf(),
        agent_log: log_dir.join("agent.log"),
        child_log: log_dir.join("child.log"),
        log_level: "info".into(),
    })
}

#[tokio::test]
async fn cycle_secures_the_child_log_and_runs_the_child() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = config(fixture("echo_args"), tmp.path());
    run_cycle(cfg.clone(), CancellationToken::new())
        .await
        .unwrap();
    assert!(cfg.child_log.is_file(), "child log must be pre-created");
    let description = flamingo_agent::platform::describe_protection(&cfg.child_log).unwrap();
    #[cfg(unix)]
    assert!(description.starts_with("mode=0600"), "{description}");
    #[cfg(windows)]
    assert!(description.starts_with("D:P"), "{description}");
}

#[tokio::test]
async fn missing_child_is_not_an_error() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = config(PathBuf::from("/definitely/not/here"), tmp.path());
    run_cycle(cfg, CancellationToken::new()).await.unwrap();
}

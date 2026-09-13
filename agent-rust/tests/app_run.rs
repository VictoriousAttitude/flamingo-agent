//! The whole bootstrap-and-loop path in one process, without privilege: this is the path the
//! service and the interactive command share, exercised here against a temporary directory.
//! It lives in its own test binary because logging installs a process-wide subscriber.

use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use flamingo_agent::app::run_agent_blocking;
use flamingo_agent::cli::Cli;
use tokio_util::sync::CancellationToken;

fn fixture(name: &str) -> PathBuf {
    let ext = if cfg!(windows) { "cmd" } else { "sh" };
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(format!("{name}.{ext}"))
}

/// A run that is cancelled after its first cycles must have secured and described the
/// child log, logged every cycle, and logged its own clean stop.
#[test]
fn bootstrap_runs_cycles_and_stops_on_cancel() {
    let tmp = tempfile::tempdir().unwrap();
    let log_dir = tmp.path().join("logs");
    let cli = Cli::try_parse_from([
        "flamingo-agent",
        "--log-dir",
        log_dir.to_str().unwrap(),
        "--child-path",
        fixture("echo_args").to_str().unwrap(),
        "--period-secs",
        "2",
        "--child-timeout-secs",
        "1",
    ])
    .unwrap();

    let cancel = CancellationToken::new();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let stopper = cancel.clone();
    std::thread::spawn(move || {
        ready_rx.recv().unwrap();
        std::thread::sleep(Duration::from_millis(4500));
        stopper.cancel();
    });

    run_agent_blocking(&cli, cancel, false, move || ready_tx.send(()).unwrap()).unwrap();

    let agent_log = std::fs::read_to_string(log_dir.join("agent.log")).unwrap();
    assert!(agent_log.contains("flamingo-agent starting"), "{agent_log}");
    #[cfg(unix)]
    assert!(agent_log.contains("protection=mode=0600"), "{agent_log}");
    #[cfg(windows)]
    assert!(agent_log.contains("protection=D:P"), "{agent_log}");
    let cycles = agent_log.matches(" metrics utc=").count();
    assert!(cycles >= 2, "only {cycles} metrics lines in:\n{agent_log}");
    assert!(agent_log.contains("flamingo-agent stopped"), "{agent_log}");
    assert!(log_dir.join("child.log").is_file());
}

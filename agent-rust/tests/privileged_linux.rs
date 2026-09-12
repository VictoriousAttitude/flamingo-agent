//! Tests that only mean anything when the process is root: the chown-to-root branch of the
//! Unix platform layer, and a full interactive run of the real binary under root.
//!
//! Every test here is `#[ignore]`d so an ordinary `cargo test` stays green for an
//! unprivileged developer; CI runs them separately with
//! `sudo cargo test --test privileged_linux -- --ignored`. Each one also re-checks the
//! effective uid and skips with a printed reason, so an accidental unprivileged run reports
//! "skipped" instead of a false pass.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use flamingo_agent::platform::{secure_dir, secure_file};

/// How long to wait for the agent to exit after SIGINT before declaring the test failed.
const SHUTDOWN_GUARD: Duration = Duration::from_secs(10);

/// True when the test can exercise the privileged paths; prints why it cannot otherwise.
fn root_or_skip(test: &str) -> bool {
    // SAFETY: geteuid has no preconditions and cannot fail.
    if unsafe { libc::geteuid() } == 0 {
        return true;
    }
    eprintln!("{test}: skipped: not root");
    false
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// `chown_root_if_privileged` is dead code for an unprivileged run, so this is the only place
/// where "root-owned" is actually observed rather than assumed.
#[test]
#[ignore = "needs root; run with sudo cargo test --test privileged_linux -- --ignored"]
fn secure_paths_are_root_owned_0600() {
    if !root_or_skip("secure_paths_are_root_owned_0600") {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("flamingo-agent");
    let file = dir.join("child.log");

    secure_dir(&dir).unwrap();
    secure_file(&file).unwrap();

    let dir_meta = fs::metadata(&dir).unwrap();
    assert_eq!(dir_meta.mode() & 0o7777, 0o700, "log directory mode");
    assert_eq!(dir_meta.uid(), 0, "log directory uid");
    assert_eq!(dir_meta.gid(), 0, "log directory gid");

    let file_meta = fs::metadata(&file).unwrap();
    assert_eq!(file_meta.mode() & 0o7777, 0o600, "child log mode");
    assert_eq!(file_meta.uid(), 0, "child log uid");
    assert_eq!(file_meta.gid(), 0, "child log gid");
}

/// The whole interactive path end to end as the deployed agent runs it: privilege check,
/// secured log directory and child log, at least two cycles, Ctrl-C, clean exit 0.
///
/// The period is 2 s (the timeout must stay strictly below it, and 1 s is the smallest value
/// the CLI accepts), so a 4.5 s run covers cycles at 0 s, 2 s and 4 s and still asserts only
/// the two the name promises.
#[test]
#[ignore = "needs root; run with sudo cargo test --test privileged_linux -- --ignored"]
fn interactive_run_under_root_completes_two_cycles() {
    if !root_or_skip("interactive_run_under_root_completes_two_cycles") {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let log_dir = tmp.path().join("logs");

    let mut child = Command::new(env!("CARGO_BIN_EXE_flamingo-agent"))
        .arg("--log-dir")
        .arg(&log_dir)
        .arg("--child-path")
        .arg(fixture("echo_args.sh"))
        .args(["--period-secs", "2", "--child-timeout-secs", "1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    std::thread::sleep(Duration::from_millis(4500));
    let pid = child.id();
    assert!(
        child.try_wait().unwrap().is_none(),
        "agent exited before it was asked to stop"
    );
    // SAFETY: `pid` belongs to the child spawned just above, which has not been reaped
    // (checked immediately above), so the pid is still ours and cannot have been reused.
    let sent = unsafe { libc::kill(pid as libc::pid_t, libc::SIGINT) };
    assert_eq!(sent, 0, "kill failed: {}", std::io::Error::last_os_error());

    let status = wait_with_guard(&mut child);
    let out = child.wait_with_output().unwrap();
    assert_eq!(
        status.code(),
        Some(0),
        "stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let agent_log = fs::read_to_string(log_dir.join("agent.log")).unwrap();
    let cycles = agent_log
        .lines()
        .filter(|l| l.contains(" metrics utc="))
        .count();
    assert!(cycles >= 2, "only {cycles} metrics lines in:\n{agent_log}");
    assert!(
        agent_log.contains("flamingo-agent stopped"),
        "no clean-shutdown line in:\n{agent_log}"
    );

    let child_log = log_dir.join("child.log");
    let meta = fs::metadata(&child_log).unwrap();
    assert_eq!(meta.mode() & 0o7777, 0o600, "child log mode");
    assert_eq!(meta.uid(), 0, "child log uid");
    assert_protected_dir(&log_dir);
}

fn assert_protected_dir(dir: &Path) {
    let meta = fs::metadata(dir).unwrap();
    assert_eq!(meta.mode() & 0o7777, 0o700, "log directory mode");
    assert_eq!(meta.uid(), 0, "log directory uid");
}

/// Wait for the agent to exit, failing the test (after killing it) if it hangs. A plain
/// `wait()` would turn "shutdown never completes" into a test run that never ends.
fn wait_with_guard(child: &mut Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + SHUTDOWN_GUARD;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("agent did not exit within {SHUTDOWN_GUARD:?} of SIGINT");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

//! Tests that only mean anything when the process is root: the chown-to-root branch of the
//! Unix platform layer, and a full interactive run of the real binary under root.
//!
//! Every test here is `#[ignore]`d so an ordinary `cargo test` stays green for an
//! unprivileged developer; CI runs them separately with
//! `sudo cargo test --test privileged_linux -- --ignored`. Each one also re-checks the
//! effective uid: locally it skips with a printed reason, so an accidental unprivileged run
//! reports "skipped" instead of a false pass; on CI (`GITHUB_ACTIONS` set) it panics instead,
//! because a non-root CI run means the privileged setup is broken, not something to skip past.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use flamingo_agent::platform::{secure_dir, secure_file, PlatformError};

/// How long to wait for the agent to exit after SIGINT before declaring the test failed.
const SHUTDOWN_GUARD: Duration = Duration::from_secs(10);

/// True when the test can exercise the privileged paths; prints why it cannot otherwise.
///
/// On CI (`GITHUB_ACTIONS` set) running as non-root is a configuration error, not a reason to
/// skip silently, so this panics instead: a green CI run must mean the privileged tests
/// actually ran as root, not that they were quietly skipped.
fn require_root_on_ci_or_skip(test: &str) -> bool {
    // SAFETY: geteuid has no preconditions and cannot fail.
    let euid = unsafe { libc::geteuid() };
    if euid == 0 {
        return true;
    }
    if std::env::var_os("GITHUB_ACTIONS").is_some() {
        panic!("privileged tests must run as root on CI, but euid is {euid}");
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
    if !require_root_on_ci_or_skip("secure_paths_are_root_owned_0600") {
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
    if !require_root_on_ci_or_skip("interactive_run_under_root_completes_two_cycles") {
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

/// A second agent pointed at a log directory that a running one owns must exit with code 5
/// and say so, without writing a single line into the first agent's logs.
#[test]
#[ignore = "needs root; run with sudo cargo test --test privileged_linux -- --ignored"]
fn second_agent_on_the_same_log_directory_exits_5() {
    if !require_root_on_ci_or_skip("second_agent_on_the_same_log_directory_exits_5") {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let log_dir = tmp.path().join("logs");
    let spawn = || {
        Command::new(env!("CARGO_BIN_EXE_flamingo-agent"))
            .arg("--log-dir")
            .arg(&log_dir)
            .arg("--child-path")
            .arg(fixture("echo_args.sh"))
            .args(["--period-secs", "2", "--child-timeout-secs", "1"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    let mut first = spawn();
    std::thread::sleep(Duration::from_millis(1500));
    assert!(
        first.try_wait().unwrap().is_none(),
        "first agent exited early"
    );

    let second = spawn().wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert_eq!(second.status.code(), Some(5), "stderr:\n{stderr}");
    assert!(
        stderr.contains("already holds the lock"),
        "stderr:\n{stderr}"
    );

    // SAFETY: `first` is our own unreaped child (checked above); SIGINT asks it to stop.
    unsafe { libc::kill(first.id() as libc::pid_t, libc::SIGINT) };
    let status = wait_with_guard(&mut first);
    assert_eq!(status.code(), Some(0));
    let agent_log = fs::read_to_string(log_dir.join("agent.log")).unwrap();
    assert_eq!(
        agent_log.matches("flamingo-agent starting").count(),
        1,
        "the second agent must not have written to the first one's log:\n{agent_log}"
    );
}

/// A hard kill of the agent (`SIGKILL`, which no handler can intercept) must still take the
/// running child with it. `kill_on_drop` cannot run in that case; only the parent-death signal
/// armed at spawn time can. The child is the 10 s sleep fixture and the period is long, so
/// exactly one child is running when the agent is killed.
#[test]
#[ignore = "needs root; run with sudo cargo test --test privileged_linux -- --ignored"]
fn hard_killed_agent_takes_its_child_with_it() {
    if !require_root_on_ci_or_skip("hard_killed_agent_takes_its_child_with_it") {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let mut agent = Command::new(env!("CARGO_BIN_EXE_flamingo-agent"))
        .arg("--log-dir")
        .arg(tmp.path().join("logs"))
        .arg("--child-path")
        .arg(fixture("sleep_10.sh"))
        .args(["--period-secs", "30", "--child-timeout-secs", "20"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    std::thread::sleep(Duration::from_millis(1500));
    assert!(
        agent.try_wait().unwrap().is_none(),
        "agent exited before it was killed"
    );
    let child_pid = only_child_of(agent.id());

    // SAFETY: `agent.id()` is our own unreaped child (checked just above), so the pid cannot
    // have been reused; SIGKILL needs no cooperation from the target.
    let sent = unsafe { libc::kill(agent.id() as libc::pid_t, libc::SIGKILL) };
    assert_eq!(sent, 0, "kill failed: {}", std::io::Error::last_os_error());
    let _ = agent.wait();

    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        // SAFETY: signal 0 performs no action; it only checks whether the pid exists.
        let alive = unsafe { libc::kill(child_pid, 0) } == 0;
        if !alive {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "child {child_pid} outlived the hard-killed agent"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A log directory that another user created before the agent's first start must be refused,
/// never adopted: the owner would keep the right to change its permissions back. Only root
/// can hand a directory to another user, so this is the one place the case can be produced.
#[test]
#[ignore = "needs root; run with sudo cargo test --test privileged_linux -- --ignored"]
fn foreign_owned_log_directory_is_refused() {
    if !require_root_on_ci_or_skip("foreign_owned_log_directory_is_refused") {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let planted = tmp.path().join("planted");
    fs::create_dir(&planted).unwrap();
    // `nobody` on every mainstream distribution; any uid other than 0 would do.
    std::os::unix::fs::chown(&planted, Some(65534), Some(65534)).unwrap();

    let err = secure_dir(&planted).unwrap_err();
    assert!(
        matches!(
            err,
            PlatformError::Untrusted {
                reason: "owned by another user",
                ..
            }
        ),
        "{err}"
    );
    assert_eq!(
        fs::metadata(&planted).unwrap().uid(),
        65534,
        "ownership must not be taken"
    );
}

/// The single child process of `pid`, according to `pgrep -P`.
fn only_child_of(pid: u32) -> libc::pid_t {
    let out = Command::new("pgrep")
        .args(["-P", &pid.to_string()])
        .output()
        .unwrap();
    let pids: Vec<libc::pid_t> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect();
    assert_eq!(
        pids.len(),
        1,
        "expected exactly one child of {pid}, got {pids:?}"
    );
    pids[0]
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

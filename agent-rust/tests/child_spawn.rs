use std::ffi::OsString;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use chrono::{TimeZone, Utc};
use flamingo_agent::child::{build_args, run_child, ChildOutcome, ChildSpec};
use flamingo_agent::metrics::Metrics;
use tokio_util::sync::CancellationToken;

fn fixture(name: &str) -> PathBuf {
    let ext = if cfg!(windows) { "cmd" } else { "sh" };
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(format!("{name}.{ext}"))
}

fn spec(name: &str, timeout: Duration) -> ChildSpec {
    ChildSpec {
        program: fixture(name),
        timeout,
    }
}

fn args(items: &[&str]) -> Vec<OsString> {
    items.iter().map(OsString::from).collect()
}

#[test]
fn build_args_layout() {
    let m = Metrics {
        utc: Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap(),
        rss_bytes: 42,
    };
    let got = build_args(&m, std::path::Path::new("/logs/child.log"));
    assert_eq!(
        got,
        args(&[
            "--utc",
            "2026-01-02T03:04:05.000Z",
            "--rss-bytes",
            "42",
            "--log-file",
            "/logs/child.log"
        ])
    );
}

#[tokio::test]
async fn completed_child_reports_stdout() {
    let outcome = run_child(
        &spec("echo_args", Duration::from_secs(5)),
        &args(&["--utc", "T", "--rss-bytes", "1"]),
        &CancellationToken::new(),
    )
    .await;
    match outcome {
        ChildOutcome::Completed { code, stdout, .. } => {
            assert_eq!(code, Some(0));
            assert!(stdout.contains("--utc T --rss-bytes 1"), "{stdout}");
        }
        other => panic!("unexpected outcome {other:?}"),
    }
}

#[tokio::test]
async fn nonzero_exit_reports_code_and_stderr() {
    let outcome = run_child(
        &spec("exit_3", Duration::from_secs(5)),
        &[],
        &CancellationToken::new(),
    )
    .await;
    match outcome {
        ChildOutcome::Completed { code, stderr, .. } => {
            assert_eq!(code, Some(3));
            assert!(stderr.contains("boom"), "{stderr}");
        }
        other => panic!("unexpected outcome {other:?}"),
    }
}

#[tokio::test]
async fn slow_child_times_out_and_is_killed() {
    let started = Instant::now();
    let outcome = run_child(
        &spec("sleep_10", Duration::from_millis(300)),
        &[],
        &CancellationToken::new(),
    )
    .await;
    assert!(matches!(outcome, ChildOutcome::TimedOut), "{outcome:?}");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "took {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn missing_binary_is_spawn_failed() {
    let outcome = run_child(
        &ChildSpec {
            program: PathBuf::from("/definitely/not/here"),
            timeout: Duration::from_secs(1),
        },
        &[],
        &CancellationToken::new(),
    )
    .await;
    assert!(
        matches!(outcome, ChildOutcome::SpawnFailed(_)),
        "{outcome:?}"
    );
}

#[tokio::test]
async fn cancellation_interrupts_the_wait() {
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        trigger.cancel();
    });
    let started = Instant::now();
    let outcome = run_child(&spec("sleep_10", Duration::from_secs(8)), &[], &cancel).await;
    assert!(matches!(outcome, ChildOutcome::Cancelled), "{outcome:?}");
    assert!(started.elapsed() < Duration::from_secs(3));
}

/// The fixture writes its PID as soon as it starts; wait for it so a slow start is not
/// mistaken for a child that never ran.
async fn pid_written(path: &std::path::Path) -> Option<u32> {
    for _ in 0..200 {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(pid) = text.trim().parse::<u32>() {
                return Some(pid);
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    None
}

/// Poll for up to a second for the process to disappear. `kill(pid, 0)` still succeeds
/// while the killed child is an unreaped zombie, so this also proves tokio reaped it; the
/// wait must yield to the runtime for the reaper to make progress.
#[cfg(unix)]
async fn reaped(pid: u32) -> bool {
    for _ in 0..100 {
        // SAFETY: signal 0 only performs the existence and permission check.
        if unsafe { libc::kill(pid as libc::pid_t, 0) } == -1
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

#[tokio::test]
async fn timed_out_child_process_is_gone() {
    let tmp = tempfile::tempdir().unwrap();
    let pid_file = tmp.path().join("child.pid");
    let outcome = run_child(
        &spec("pidfile_sleep", Duration::from_millis(500)),
        &[pid_file.clone().into_os_string()],
        &CancellationToken::new(),
    )
    .await;
    assert!(matches!(outcome, ChildOutcome::TimedOut), "{outcome:?}");

    let pid = pid_written(&pid_file)
        .await
        .unwrap_or_else(|| panic!("no pid in {}", pid_file.display()));

    #[cfg(unix)]
    assert!(
        reaped(pid).await,
        "pid {pid} is still alive after the timeout"
    );

    // On Windows the PID in the file belongs to the PowerShell process cmd.exe started,
    // and killing cmd.exe does not kill its children: the "process is gone" assertion
    // would be about the wrong process, so only the timeout itself is asserted here.
    #[cfg(windows)]
    assert!(pid > 0, "pid must be a real process id");
}

#[tokio::test]
async fn large_stdout_does_not_deadlock() {
    // 300 kB is far past the 64 kB pipe buffer: a child whose output is only drained
    // after it exits would block forever here.
    let outcome = run_child(
        &spec("spam", Duration::from_secs(10)),
        &[],
        &CancellationToken::new(),
    )
    .await;
    match outcome {
        ChildOutcome::Completed { code, stdout, .. } => {
            assert_eq!(code, Some(0));
            assert!(stdout.len() >= 300_000, "only {} bytes", stdout.len());
        }
        other => panic!("unexpected outcome {other:?}"),
    }
}

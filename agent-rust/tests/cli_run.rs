use std::process::Command;

fn agent() -> Command {
    Command::new(env!("CARGO_BIN_EXE_flamingo-agent"))
}

#[test]
fn help_exits_zero() {
    let out = agent().arg("--help").output().unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("--install"));
}

#[test]
fn conflicting_flags_are_a_usage_error() {
    let out = agent().args(["--install", "--uninstall"]).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
}

#[cfg(not(windows))]
#[test]
fn install_is_unsupported_off_windows() {
    let out = agent().arg("--install").output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(4),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("not supported"));
}

#[cfg(unix)]
#[test]
fn unprivileged_run_explains_sudo_and_exits_3() {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        return; // running as root (CI containers sometimes do); the check cannot be exercised
    }
    let tmp = tempfile::tempdir().unwrap();
    let out = agent()
        .args(["--log-dir"])
        .arg(tmp.path())
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("sudo"));
}

#[test]
fn version_flag_works() {
    let out = agent().arg("--version").output().unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains(env!("CARGO_PKG_VERSION")), "{stdout}");
}

#[test]
fn install_with_overrides_is_a_usage_error() {
    // The registered service is started without arguments, so an override passed here
    // would silently do nothing: clap must reject it.
    let out = agent()
        .args(["--install", "--period-secs", "7"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

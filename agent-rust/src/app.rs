//! Process-level orchestration: bootstrap in the order design §4.6 requires, the interactive
//! entry point, and privilege enforcement shared by all commands.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::cli::Cli;
use crate::config::Config;
use crate::platform::{self, PlatformError};
use crate::{agent, cycle, logging};

/// Success.
pub const EXIT_OK: u8 = 0;
/// Runtime failure.
pub const EXIT_FAILURE: u8 = 1;
/// Command-line usage error (clap uses the same value).
pub const EXIT_USAGE: u8 = 2;
/// Not running with the required privilege, or elevation was declined.
pub const EXIT_PRIVILEGE: u8 = 3;
/// The requested operation has no implementation on this platform.
pub const EXIT_UNSUPPORTED: u8 = 4;
/// Another agent instance already owns the log directory.
pub const EXIT_ALREADY_RUNNING: u8 = 5;

/// CLI plus platform defaults.
pub fn resolve_config(cli: &Cli) -> anyhow::Result<Config> {
    let exe = std::env::current_exe().context("locating own executable")?;
    Config::resolve(
        cli,
        &exe,
        platform::default_log_dir(),
        platform::CHILD_BINARY_NAME,
    )
    .map_err(anyhow::Error::from)
}

/// Bootstrap, then run the loop on a private runtime until `cancel` fires.
/// `on_ready` is invoked once the loop is about to start (the service reports Running there).
pub fn run_agent_blocking(
    cli: &Cli,
    cancel: CancellationToken,
    interactive: bool,
    on_ready: impl FnOnce(),
) -> anyhow::Result<()> {
    let cfg = resolve_config(cli)?;

    platform::secure_dir(&cfg.log_dir)
        .with_context(|| format!("securing log directory {}", cfg.log_dir.display()))?;
    // Taken before the file logger opens, so a second instance never writes a line into the
    // first one's logs. Held until the end of this function, that is for the whole run.
    let _instance =
        platform::acquire_instance_lock(&cfg.log_dir).context("acquiring the instance lock")?;
    let _log_guard = logging::init(&cfg.agent_log, &cfg.log_level, interactive)
        .context("initialising logging")?;
    install_panic_hook();

    // Logging exists from here on, so a bootstrap failure is logged before it propagates:
    // in service mode there is no console and the caller only sees an SCM exit code.
    let protection = secure_and_describe_child_log(&cfg)
        .inspect_err(|err| tracing::error!(error = format!("{err:#}"), "bootstrap failed"))?;

    info!(
        mode = if interactive { "interactive" } else { "service" },
        child = %cfg.child_path.display(),
        child_log = %cfg.child_log.display(),
        protection = %protection,
        period_secs = cfg.period.as_secs(),
        child_timeout_secs = cfg.child_timeout.as_secs(),
        "flamingo-agent starting"
    );

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .context("building async runtime")?;

    on_ready();
    let cfg = Arc::new(cfg);
    let cycle_cancel = cancel.clone();
    runtime.block_on(agent::run_loop(cfg.period, cancel, move || {
        cycle::run_cycle(Arc::clone(&cfg), cycle_cancel.clone())
    }));
    runtime.shutdown_timeout(Duration::from_secs(5));
    info!("flamingo-agent stopped");
    Ok(())
}

/// Lock down the child log and report how it is protected.
fn secure_and_describe_child_log(cfg: &Config) -> anyhow::Result<String> {
    platform::secure_file(&cfg.child_log)
        .with_context(|| format!("securing child log {}", cfg.child_log.display()))?;
    platform::describe_protection(&cfg.child_log).context("reading child log protection")
}

/// Route panic messages into the log before the default hook prints them.
fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!(panic = %info, "panic");
        default_hook(info);
    }));
}

/// Ensure we run privileged. On Windows an unprivileged process relaunches itself elevated
/// and returns `Err(exit code of the elevated run)`; elsewhere the user is told to use sudo.
pub fn require_privilege() -> Result<(), u8> {
    match platform::is_privileged() {
        Ok(true) => Ok(()),
        Ok(false) => Err(relaunch_or_explain()),
        Err(err) => {
            eprintln!("flamingo-agent: cannot determine privilege level: {err}");
            Err(EXIT_FAILURE)
        }
    }
}

/// Explanation printed when the relaunch failed for a reason we cannot interpret; the
/// error's own detail is appended to it.
const RELAUNCH_FAILED: &str = "could not relaunch with elevation";

/// Exit code for a relaunch attempt, plus the line to print on stderr (if any).
///
/// Pure: the whole policy of the privileged relaunch is decided here and nothing else.
pub(crate) fn exit_code_for_relaunch(
    result: Result<i32, PlatformError>,
) -> (u8, Option<&'static str>) {
    match result {
        // An exit code the elevated run produced that we cannot reproduce in our own
        // status byte (negative, or above 255) is reported as a plain failure.
        Ok(code) => (u8::try_from(code).unwrap_or(EXIT_FAILURE), None),
        Err(PlatformError::ElevationDeclined) => {
            (EXIT_PRIVILEGE, Some("administrator approval was declined"))
        }
        Err(PlatformError::ElevationBlockedByPolicy) => (
            EXIT_PRIVILEGE,
            Some("elevation is blocked by policy for this account; run from an administrator account"),
        ),
        Err(PlatformError::Unsupported(_)) => (
            EXIT_PRIVILEGE,
            Some("this command needs root privileges; run it with sudo"),
        ),
        Err(_) => (EXIT_FAILURE, Some(RELAUNCH_FAILED)),
    }
}

fn relaunch_or_explain() -> u8 {
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let result = platform::relaunch_privileged(&args);
    // The mapping consumes the result, so the error's own detail (the failing API call and
    // its OS code) is captured first and appended to the uninterpreted-failure line.
    let detail = result.as_ref().err().map(PlatformError::to_string);
    let (code, message) = exit_code_for_relaunch(result);
    if let Some(line) = relaunch_report(message, detail) {
        eprintln!("flamingo-agent: {line}");
    }
    code
}

/// The stderr line for a relaunch outcome: the interpreted message alone, or the
/// uninterpreted-failure message with the error's own detail appended.
fn relaunch_report(message: Option<&'static str>, detail: Option<String>) -> Option<String> {
    match (message, detail) {
        (Some(message), Some(detail)) if message == RELAUNCH_FAILED => {
            Some(format!("{message}: {detail}"))
        }
        (Some(message), _) => Some(message.to_string()),
        (None, _) => None,
    }
}

/// Run from a shell: enforce privilege, stop on Ctrl-C, report errors on stderr.
pub fn run_interactive(cli: &Cli) -> u8 {
    if let Err(code) = require_privilege() {
        return code;
    }
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    if let Err(err) = ctrlc::set_handler(move || trigger.cancel()) {
        eprintln!("flamingo-agent: cannot install Ctrl-C handler: {err}");
        return EXIT_FAILURE;
    }
    match run_agent_blocking(cli, cancel, true, || {
        eprintln!("flamingo-agent: running; press Ctrl-C to stop");
    }) {
        Ok(()) => EXIT_OK,
        Err(err) => {
            eprintln!("flamingo-agent: {err:#}");
            if matches!(
                err.downcast_ref::<PlatformError>(),
                Some(PlatformError::AlreadyRunning { .. })
            ) {
                EXIT_ALREADY_RUNNING
            } else {
                EXIT_FAILURE
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn relaunch_exit_code_mapping() {
        // The elevated run's own exit code is passed through when it fits in a u8.
        assert_eq!(exit_code_for_relaunch(Ok(0)), (EXIT_OK, None));
        assert_eq!(exit_code_for_relaunch(Ok(7)), (7, None));
        assert_eq!(exit_code_for_relaunch(Ok(255)), (255, None));
        // Codes we cannot report through our own exit status become a plain failure.
        assert_eq!(exit_code_for_relaunch(Ok(256)), (EXIT_FAILURE, None));
        assert_eq!(exit_code_for_relaunch(Ok(-1)), (EXIT_FAILURE, None));

        let (code, message) = exit_code_for_relaunch(Err(PlatformError::ElevationDeclined));
        assert_eq!(code, EXIT_PRIVILEGE);
        assert!(
            message.unwrap_or_default().contains("declined"),
            "{message:?}"
        );

        let (code, message) = exit_code_for_relaunch(Err(PlatformError::ElevationBlockedByPolicy));
        assert_eq!(code, EXIT_PRIVILEGE);
        assert!(
            message.unwrap_or_default().contains("blocked by policy"),
            "{message:?}"
        );

        let (code, message) =
            exit_code_for_relaunch(Err(PlatformError::Unsupported("self-elevation")));
        assert_eq!(code, EXIT_PRIVILEGE);
        assert!(message.unwrap_or_default().contains("sudo"), "{message:?}");

        let (code, message) = exit_code_for_relaunch(Err(PlatformError::Os {
            call: "ShellExecuteExW",
            code: 5,
        }));
        assert_eq!(code, EXIT_FAILURE);
        assert_eq!(message, Some(RELAUNCH_FAILED));
    }

    #[test]
    fn relaunch_report_appends_detail_only_to_the_uninterpreted_failure() {
        assert_eq!(
            relaunch_report(Some(RELAUNCH_FAILED), Some("ShellExecuteExW failed".into())),
            Some(format!("{RELAUNCH_FAILED}: ShellExecuteExW failed"))
        );
        assert_eq!(
            relaunch_report(
                Some("administrator approval was declined"),
                Some("x".into())
            ),
            Some("administrator approval was declined".to_string())
        );
        assert_eq!(
            relaunch_report(Some(RELAUNCH_FAILED), None),
            Some(RELAUNCH_FAILED.to_string())
        );
        assert_eq!(relaunch_report(None, Some("x".into())), None);
    }

    /// The hook must put the panic into the log (the service has no console), then let the
    /// default hook run so an interactive user still sees it on stderr.
    #[test]
    fn panic_hook_logs_the_panic_message() {
        use std::io::Write;
        use std::sync::{Arc, Mutex};
        use tracing_subscriber::layer::SubscriberExt;

        #[derive(Clone)]
        struct Sink(Arc<Mutex<Vec<u8>>>);
        impl Write for Sink {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let sink = Sink(Arc::new(Mutex::new(Vec::new())));
        let writer = sink.clone();
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .with_writer(move || writer.clone())
                .with_ansi(false),
        );
        tracing::subscriber::with_default(subscriber, || {
            install_panic_hook();
            let outcome = std::panic::catch_unwind(|| panic!("boom from the test"));
            assert!(outcome.is_err());
        });
        // Put the default hook back so later panics in this process are reported normally.
        drop(std::panic::take_hook());

        let logged = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
        assert!(logged.contains("boom from the test"), "{logged}");
        assert!(logged.contains("ERROR"), "{logged}");
    }

    #[test]
    fn resolve_config_rejects_timeout_not_below_period() {
        let cli = Cli::try_parse_from([
            "flamingo-agent",
            "--period-secs",
            "4",
            "--child-timeout-secs",
            "4",
        ])
        .unwrap();
        let err = resolve_config(&cli).unwrap_err();
        assert!(format!("{err:#}").contains("must be shorter"), "{err:#}");
    }
}

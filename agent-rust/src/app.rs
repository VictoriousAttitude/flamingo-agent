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
    let _log_guard = logging::init(&cfg.agent_log, &cfg.log_level, interactive)
        .context("initialising logging")?;
    install_panic_hook();
    platform::secure_file(&cfg.child_log)
        .with_context(|| format!("securing child log {}", cfg.child_log.display()))?;
    let protection =
        platform::describe_protection(&cfg.child_log).context("reading child log protection")?;

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

fn relaunch_or_explain() -> u8 {
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    match platform::relaunch_privileged(&args) {
        Ok(code) => u8::try_from(code).unwrap_or(EXIT_FAILURE),
        Err(PlatformError::ElevationDeclined) => {
            eprintln!("flamingo-agent: administrator approval was declined");
            EXIT_PRIVILEGE
        }
        Err(PlatformError::Unsupported(_)) => {
            eprintln!("flamingo-agent: this command needs root privileges; run it with sudo");
            EXIT_PRIVILEGE
        }
        Err(err) => {
            eprintln!("flamingo-agent: could not relaunch with elevation: {err}");
            EXIT_FAILURE
        }
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
            EXIT_FAILURE
        }
    }
}

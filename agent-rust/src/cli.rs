//! Command-line interface.

use std::path::PathBuf;

use clap::Parser;

/// Flags that configure a run of the agent itself; `--install` and `--uninstall` reject them
/// because the registered service is started without arguments. Clap only counts explicitly
/// passed arguments as conflicts, so the defaults below are unaffected.
const RUNTIME_OVERRIDES: [&str; 5] = [
    "period_secs",
    "child_timeout_secs",
    "child_path",
    "log_dir",
    "log_level",
];

/// Flamingo background agent.
#[derive(Debug, Parser)]
#[command(
    name = "flamingo-agent",
    version,
    about = "Samples UTC time and process RSS every 5 seconds and launches a privileged logger child"
)]
pub struct Cli {
    /// Register FlamingoAgent as a Windows service with automatic start, then start it.
    /// The runtime overrides below do not apply: the service is registered without
    /// arguments, so passing them here would silently have no effect.
    #[arg(long, conflicts_with = "uninstall", conflicts_with_all = RUNTIME_OVERRIDES)]
    pub install: bool,

    /// Stop and remove the FlamingoAgent Windows service.
    #[arg(long, conflicts_with_all = RUNTIME_OVERRIDES)]
    pub uninstall: bool,

    /// Seconds between collection cycles.
    #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u64).range(1..))]
    pub period_secs: u64,

    /// Seconds to wait for the child before killing it (must be below the period).
    #[arg(long, default_value_t = 4, value_parser = clap::value_parser!(u64).range(1..))]
    pub child_timeout_secs: u64,

    /// Path to the logger-child binary. Defaults to a sibling of this executable.
    #[arg(long)]
    pub child_path: Option<PathBuf>,

    /// Directory for agent.log and child.log. Defaults to the platform data directory.
    #[arg(long)]
    pub log_dir: Option<PathBuf>,

    /// Log level filter: error, warn, info, debug or trace.
    #[arg(long, default_value = "info")]
    pub log_level: String,
}

/// What the invocation asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Register and start the Windows service.
    Install,
    /// Stop and remove the Windows service.
    Uninstall,
    /// Run the agent (as a service when started by the SCM, otherwise interactively).
    Run,
}

impl Cli {
    /// The requested command. `--install` and `--uninstall` are mutually exclusive (clap
    /// rejects both together before this is reached).
    pub fn command(&self) -> Command {
        if self.install {
            Command::Install
        } else if self.uninstall {
            Command::Uninstall
        } else {
            Command::Run
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("flamingo-agent").chain(args.iter().copied()))
    }

    #[test]
    fn bare_invocation_runs_with_defaults() {
        let cli = parse(&[]).unwrap();
        assert_eq!(cli.command(), Command::Run);
        assert_eq!(cli.period_secs, 5);
        assert_eq!(cli.child_timeout_secs, 4);
        assert_eq!(cli.log_level, "info");
        assert!(cli.child_path.is_none());
        assert!(cli.log_dir.is_none());
    }

    #[test]
    fn install_and_uninstall_flags() {
        assert_eq!(parse(&["--install"]).unwrap().command(), Command::Install);
        assert_eq!(
            parse(&["--uninstall"]).unwrap().command(),
            Command::Uninstall
        );
    }

    #[test]
    fn install_and_uninstall_together_is_an_error() {
        assert!(parse(&["--install", "--uninstall"]).is_err());
    }

    #[test]
    fn install_rejects_runtime_overrides() {
        assert!(parse(&["--install", "--period-secs", "7"]).is_err());
        assert!(parse(&["--uninstall", "--log-level", "debug"]).is_err());
    }

    #[test]
    fn zero_period_is_rejected() {
        assert!(parse(&["--period-secs", "0"]).is_err());
    }

    #[test]
    fn overrides_parse() {
        let cli = parse(&[
            "--period-secs",
            "7",
            "--child-timeout-secs",
            "3",
            "--child-path",
            "/opt/child",
            "--log-dir",
            "/tmp/logs",
            "--log-level",
            "debug",
        ])
        .unwrap();
        assert_eq!(cli.period_secs, 7);
        assert_eq!(cli.child_timeout_secs, 3);
        assert_eq!(
            cli.child_path.as_deref(),
            Some(std::path::Path::new("/opt/child"))
        );
        assert_eq!(
            cli.log_dir.as_deref(),
            Some(std::path::Path::new("/tmp/logs"))
        );
        assert_eq!(cli.log_level, "debug");
    }
}

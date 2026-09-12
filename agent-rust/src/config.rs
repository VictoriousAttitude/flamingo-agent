//! Resolved runtime configuration: CLI overrides on top of platform defaults.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::cli::Cli;

/// Everything the agent needs at run time, with all paths absolute.
#[derive(Debug, Clone)]
pub struct Config {
    /// Time between collection cycles.
    pub period: Duration,
    /// Maximum time to wait for the child each cycle.
    pub child_timeout: Duration,
    /// Absolute path of the child binary.
    pub child_path: PathBuf,
    /// Directory holding both logs.
    pub log_dir: PathBuf,
    /// The agent's own log.
    pub agent_log: PathBuf,
    /// The child's log (the file whose ACL is enforced).
    pub child_log: PathBuf,
    /// tracing filter directive.
    pub log_level: String,
}

/// Configuration validation failures.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The child could overrun the next cycle.
    #[error("child timeout ({timeout}s) must be shorter than the period ({period}s)")]
    TimeoutNotBelowPeriod {
        /// Requested timeout in seconds.
        timeout: u64,
        /// Requested period in seconds.
        period: u64,
    },
}

impl Config {
    /// Combine CLI values with platform defaults. `exe` is the agent's own executable path;
    /// the child defaults to a sibling named `child_binary_name`.
    pub fn resolve(
        cli: &Cli,
        exe: &Path,
        default_log_dir: PathBuf,
        child_binary_name: &str,
    ) -> Result<Config, ConfigError> {
        if cli.child_timeout_secs >= cli.period_secs {
            return Err(ConfigError::TimeoutNotBelowPeriod {
                timeout: cli.child_timeout_secs,
                period: cli.period_secs,
            });
        }
        let exe_dir = exe.parent().map(Path::to_path_buf).unwrap_or_default();
        let log_dir = cli.log_dir.clone().unwrap_or(default_log_dir);
        Ok(Config {
            period: Duration::from_secs(cli.period_secs),
            child_timeout: Duration::from_secs(cli.child_timeout_secs),
            child_path: cli
                .child_path
                .clone()
                .unwrap_or_else(|| exe_dir.join(child_binary_name)),
            agent_log: log_dir.join("agent.log"),
            child_log: log_dir.join("child.log"),
            log_dir,
            log_level: cli.log_level.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn cli(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("flamingo-agent").chain(args.iter().copied())).unwrap()
    }

    #[test]
    fn defaults_derive_from_exe_and_platform_dir() {
        let cfg = Config::resolve(
            &cli(&[]),
            Path::new("/opt/flamingo/flamingo-agent"),
            PathBuf::from("/var/log/flamingo-agent"),
            "logger-child",
        )
        .unwrap();
        assert_eq!(cfg.period, Duration::from_secs(5));
        assert_eq!(cfg.child_timeout, Duration::from_secs(4));
        assert_eq!(cfg.child_path, Path::new("/opt/flamingo/logger-child"));
        assert_eq!(cfg.log_dir, Path::new("/var/log/flamingo-agent"));
        assert_eq!(
            cfg.agent_log,
            Path::new("/var/log/flamingo-agent/agent.log")
        );
        assert_eq!(
            cfg.child_log,
            Path::new("/var/log/flamingo-agent/child.log")
        );
    }

    #[test]
    fn overrides_win() {
        let cfg = Config::resolve(
            &cli(&[
                "--child-path",
                "/x/child",
                "--log-dir",
                "/y",
                "--period-secs",
                "9",
                "--child-timeout-secs",
                "2",
            ]),
            Path::new("/opt/flamingo/flamingo-agent"),
            PathBuf::from("/var/log/flamingo-agent"),
            "logger-child",
        )
        .unwrap();
        assert_eq!(cfg.child_path, Path::new("/x/child"));
        assert_eq!(cfg.child_log, Path::new("/y/child.log"));
        assert_eq!(cfg.period, Duration::from_secs(9));
        assert_eq!(cfg.child_timeout, Duration::from_secs(2));
    }

    #[test]
    fn timeout_must_be_below_period() {
        let err = Config::resolve(
            &cli(&["--period-secs", "4", "--child-timeout-secs", "4"]),
            Path::new("/a/b"),
            PathBuf::from("/l"),
            "c",
        )
        .unwrap_err();
        assert!(matches!(
            err,
            ConfigError::TimeoutNotBelowPeriod {
                timeout: 4,
                period: 4
            }
        ));
    }
}

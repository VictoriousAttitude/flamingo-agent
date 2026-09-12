//! Binary entry point: parse the command line and dispatch.

use std::process::ExitCode;

use clap::Parser;
use flamingo_agent::cli::{Cli, Command};
use flamingo_agent::{app, platform, service};

fn main() -> ExitCode {
    let cli = Cli::parse();
    let code = match cli.command() {
        Command::Install => install(),
        Command::Uninstall => uninstall(),
        Command::Run => run(&cli),
    };
    ExitCode::from(code)
}

fn install() -> u8 {
    if let Err(code) = privilege_for_service_management() {
        return code;
    }
    let result = std::env::current_exe()
        .map_err(anyhow::Error::from)
        .and_then(|exe| service::install(&exe).map_err(anyhow::Error::from));
    match result {
        Ok(()) => {
            println!("{} installed and running", service::SERVICE_NAME);
            app::EXIT_OK
        }
        Err(err) => report(&err),
    }
}

fn uninstall() -> u8 {
    if let Err(code) = privilege_for_service_management() {
        return code;
    }
    match service::uninstall() {
        Ok(()) => {
            println!("{} removed", service::SERVICE_NAME);
            app::EXIT_OK
        }
        Err(err) => report(&anyhow::Error::from(err)),
    }
}

fn run(cli: &Cli) -> u8 {
    match service::run_dispatcher() {
        Ok(service::Dispatch::RanAsService) => app::EXIT_OK,
        Ok(service::Dispatch::NotAService) => app::run_interactive(cli),
        Err(err) => {
            eprintln!("flamingo-agent: service dispatcher failed: {err}");
            app::EXIT_FAILURE
        }
    }
}

/// Talking to the SCM needs an elevated token; on platforms without an SCM there is nothing
/// to elevate for, and `service::install` reports Unsupported with the right exit code.
fn privilege_for_service_management() -> Result<(), u8> {
    if platform::SERVICE_MANAGEMENT_NEEDS_PRIVILEGE {
        app::require_privilege()
    } else {
        Ok(())
    }
}

fn report(err: &anyhow::Error) -> u8 {
    eprintln!("flamingo-agent: {err:#}");
    match err.downcast_ref::<service::ServiceError>() {
        Some(service::ServiceError::Unsupported) => app::EXIT_UNSUPPORTED,
        _ => app::EXIT_FAILURE,
    }
}

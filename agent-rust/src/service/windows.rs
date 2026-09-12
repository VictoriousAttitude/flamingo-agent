//! Windows Service Control Manager glue (design §4.2, §4.3, §7).

use std::ffi::OsString;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use clap::Parser;
use tokio_util::sync::CancellationToken;
use windows_service::service::{
    Service, ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl,
    ServiceExitCode, ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{
    self, ServiceControlHandlerResult, ServiceStatusHandle,
};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_service::{define_windows_service, service_dispatcher};

use super::{Dispatch, ServiceError, SERVICE_DESCRIPTION, SERVICE_DISPLAY_NAME, SERVICE_NAME};
use crate::app;
use crate::cli::Cli;

/// `StartServiceCtrlDispatcher` fails with this when the process was not started by the SCM.
const ERROR_FAILED_SERVICE_CONTROLLER_CONNECT: i32 = 1063;
/// `CreateService` fails with this when the name is already registered.
const ERROR_SERVICE_EXISTS: i32 = 1073;

const WAIT_HINT: Duration = Duration::from_secs(10);
const START_TIMEOUT: Duration = Duration::from_secs(10);
const STOP_TIMEOUT: Duration = Duration::from_secs(15);
const POLL: Duration = Duration::from_millis(250);

/// Service-specific exit codes reported to the SCM (visible in `sc query`).
const EXIT_BOOTSTRAP_FAILED: u32 = 1;
const EXIT_PANIC: u32 = 2;
const EXIT_BAD_ARGUMENTS: u32 = 3;

define_windows_service!(ffi_service_main, service_main);

/// Hand the process to the SCM. Returns `NotAService` when launched from a shell.
pub fn run_dispatcher() -> Result<Dispatch, ServiceError> {
    match service_dispatcher::start(SERVICE_NAME, ffi_service_main) {
        Ok(()) => Ok(Dispatch::RanAsService),
        Err(windows_service::Error::Winapi(err))
            if err.raw_os_error() == Some(ERROR_FAILED_SERVICE_CONTROLLER_CONNECT) =>
        {
            Ok(Dispatch::NotAService)
        }
        Err(err) => Err(ServiceError::Api(err)),
    }
}

fn service_main(_arguments: Vec<OsString>) {
    // Errors here have nowhere to go: logging may not exist yet and there is no console.
    // The exit code reported to the SCM is the diagnostic channel.
    let _ = run_service();
}

fn run_service() -> windows_service::Result<()> {
    let cancel = CancellationToken::new();
    let handler_cancel = cancel.clone();
    // The handler needs the handle that `register` returns, so it captures a shared slot that
    // is filled in immediately afterwards. `SetServiceStatus` is a cheap, non-blocking call to
    // the SCM, so reporting StopPending from the handler keeps the "do no work here" rule.
    let handler_status: Arc<OnceLock<ServiceStatusHandle>> = Arc::new(OnceLock::new());
    let handler_slot = Arc::clone(&handler_status);
    let status = service_control_handler::register(SERVICE_NAME, move |control| match control {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            if let Some(handle) = handler_slot.get() {
                let _ = handle.set_service_status(pending(ServiceState::StopPending));
            }
            handler_cancel.cancel();
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    })?;
    let _ = handler_status.set(status);

    status.set_service_status(pending(ServiceState::StartPending))?;

    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(_) => {
            return status.set_service_status(stopped(ServiceExitCode::ServiceSpecific(
                EXIT_BAD_ARGUMENTS,
            )))
        }
    };

    let ready_status = status;
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        app::run_agent_blocking(&cli, cancel, false, move || {
            let _ = ready_status.set_service_status(running());
        })
    }));

    // Best-effort: the terminal `Stopped` report below must never be skipped, so a
    // failed intermediate report here is deliberately ignored.
    let _ = status.set_service_status(pending(ServiceState::StopPending));
    let exit_code = match outcome {
        Ok(Ok(())) => ServiceExitCode::Win32(0),
        Ok(Err(_)) => ServiceExitCode::ServiceSpecific(EXIT_BOOTSTRAP_FAILED),
        Err(_) => ServiceExitCode::ServiceSpecific(EXIT_PANIC),
    };
    status.set_service_status(stopped(exit_code))
}

fn pending(state: ServiceState) -> ServiceStatus {
    ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: WAIT_HINT,
        process_id: None,
    }
}

fn running() -> ServiceStatus {
    ServiceStatus {
        current_state: ServiceState::Running,
        controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
        wait_hint: Duration::default(),
        ..pending(ServiceState::Running)
    }
}

fn stopped(exit_code: ServiceExitCode) -> ServiceStatus {
    ServiceStatus {
        current_state: ServiceState::Stopped,
        exit_code,
        wait_hint: Duration::default(),
        ..pending(ServiceState::Stopped)
    }
}

fn service_info(exe: &Path) -> ServiceInfo {
    ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from(SERVICE_DISPLAY_NAME),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe.to_path_buf(),
        launch_arguments: Vec::new(),
        dependencies: Vec::new(),
        account_name: None, // LocalSystem
        account_password: None,
    }
}

/// Register (or update) the service, start it, and wait until it reports Running.
pub fn install(exe: &Path) -> Result<(), ServiceError> {
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )?;
    let info = service_info(exe);
    let access = ServiceAccess::CHANGE_CONFIG | ServiceAccess::START | ServiceAccess::QUERY_STATUS;
    let service = match manager.create_service(&info, access) {
        Ok(service) => service,
        Err(windows_service::Error::Winapi(err))
            if err.raw_os_error() == Some(ERROR_SERVICE_EXISTS) =>
        {
            let service = manager.open_service(SERVICE_NAME, access)?;
            service.change_config(&info)?;
            service
        }
        Err(err) => return Err(ServiceError::Api(err)),
    };
    service.set_description(SERVICE_DESCRIPTION)?;
    // Only a fully stopped service is started: a StartPending one is already on its way and
    // starting it again fails with ERROR_SERVICE_ALREADY_RUNNING (1056).
    if service.query_status()?.current_state == ServiceState::Stopped {
        service.start::<OsString>(&[])?;
    }
    wait_for_state(&service, ServiceState::Running, START_TIMEOUT)
}

/// Stop the service if running, then delete it.
pub fn uninstall() -> Result<(), ServiceError> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = manager.open_service(
        SERVICE_NAME,
        ServiceAccess::STOP | ServiceAccess::QUERY_STATUS | ServiceAccess::DELETE,
    )?;
    if service.query_status()?.current_state != ServiceState::Stopped {
        // A failed `stop` is not fatal on its own (the service may be stopping already), but
        // if it then never reaches Stopped the stop error is the useful diagnostic.
        let stop_result = service.stop();
        if let Err(wait_err) = wait_for_state(&service, ServiceState::Stopped, STOP_TIMEOUT) {
            return match stop_result {
                Err(source) => Err(ServiceError::StopFailed { source }),
                Ok(_) => Err(wait_err),
            };
        }
    }
    service.delete()?;
    Ok(())
}

fn wait_for_state(
    service: &Service,
    wanted: ServiceState,
    timeout: Duration,
) -> Result<(), ServiceError> {
    let deadline = Instant::now() + timeout;
    loop {
        let status = service.query_status()?;
        if status.current_state == wanted {
            return Ok(());
        }
        if wanted == ServiceState::Running && status.current_state == ServiceState::Stopped {
            return Err(ServiceError::FailedToStart {
                exit_code: format!("{:?}", status.exit_code),
            });
        }
        if Instant::now() >= deadline {
            return Err(ServiceError::Timeout {
                wanted: format!("{wanted:?}"),
            });
        }
        std::thread::sleep(POLL);
    }
}

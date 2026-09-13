//! Windows Service Control Manager glue (design §4.2, §4.3, §7).

use std::ffi::OsString;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use clap::Parser;
use tokio_util::sync::CancellationToken;
use windows_service::service::{
    Service, ServiceAccess, ServiceAction, ServiceActionType, ServiceControl, ServiceControlAccept,
    ServiceErrorControl, ServiceExitCode, ServiceFailureActions, ServiceFailureResetPeriod,
    ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{
    self, ServiceControlHandlerResult, ServiceStatusHandle,
};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_service::{define_windows_service, service_dispatcher};

use super::eventlog::{
    self, EventSource, EVENT_BAD_ARGUMENTS, EVENT_BOOTSTRAP_FAILED, EVENT_PANIC, EVENT_STARTED,
    EVENT_STOPPED,
};
use super::{Dispatch, ServiceError, SERVICE_DESCRIPTION, SERVICE_DISPLAY_NAME, SERVICE_NAME};
use crate::app;
use crate::cli::Cli;

/// `StartServiceCtrlDispatcher` fails with this when the process was not started by the SCM.
const ERROR_FAILED_SERVICE_CONTROLLER_CONNECT: i32 = 1063;
/// `CreateService` fails with this when the name is already registered.
const ERROR_SERVICE_EXISTS: i32 = 1073;
/// `CreateService`/`OpenService` fail with this while a deleted registration still has open
/// handles (typically the Services console); the name becomes free once they are closed.
const ERROR_SERVICE_MARKED_FOR_DELETE: i32 = 1072;
/// `OpenService` fails with this when the name was never registered.
const ERROR_SERVICE_DOES_NOT_EXIST: i32 = 1060;

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

    // Lifecycle facts also go to the Application event log (design §4.3). Every call is
    // best-effort: a log that cannot be written never changes what the service does.
    let events = EventSource::open(SERVICE_NAME);
    let report = |kind: EventKind, id: u32, message: String| {
        if let Some(events) = &events {
            match kind {
                EventKind::Info => events.info(id, &message),
                EventKind::Error => events.error(id, &message),
            };
        }
    };

    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            report(
                EventKind::Error,
                EVENT_BAD_ARGUMENTS,
                format!("Flamingo Agent could not parse its registered command line: {err}"),
            );
            return status.set_service_status(stopped(ServiceExitCode::ServiceSpecific(
                EXIT_BAD_ARGUMENTS,
            )));
        }
    };
    let agent_log = app::resolve_config(&cli)
        .map(|cfg| cfg.agent_log.display().to_string())
        .unwrap_or_else(|_| "agent.log".to_string());

    let ready_status = status;
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        app::run_agent_blocking(&cli, cancel, false, || {
            let _ = ready_status.set_service_status(running());
            report(
                EventKind::Info,
                EVENT_STARTED,
                format!("Flamingo Agent started; details are logged to {agent_log}"),
            );
        })
    }));

    // Best-effort: the terminal `Stopped` report below must never be skipped, so a
    // failed intermediate report here is deliberately ignored.
    let _ = status.set_service_status(pending(ServiceState::StopPending));
    let exit_code = match outcome {
        Ok(Ok(())) => {
            report(
                EventKind::Info,
                EVENT_STOPPED,
                "Flamingo Agent stopped on request".to_string(),
            );
            ServiceExitCode::Win32(0)
        }
        Ok(Err(err)) => {
            report(
                EventKind::Error,
                EVENT_BOOTSTRAP_FAILED,
                format!("Flamingo Agent failed to start: {err:#}"),
            );
            ServiceExitCode::ServiceSpecific(EXIT_BOOTSTRAP_FAILED)
        }
        Err(payload) => {
            report(
                EventKind::Error,
                EVENT_PANIC,
                format!(
                    "Flamingo Agent stopped after a panic: {}; see {agent_log}",
                    panic_message(payload.as_ref())
                ),
            );
            ServiceExitCode::ServiceSpecific(EXIT_PANIC)
        }
    };
    status.set_service_status(stopped(exit_code))
}

#[derive(Clone, Copy)]
enum EventKind {
    Info,
    Error,
}

/// The text of a panic payload, when it is one of the two forms `panic!` produces.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("(non-string panic payload)")
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
        controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
        wait_hint: Duration::default(),
        ..pending(ServiceState::Running)
    }
}

fn stopped(exit_code: ServiceExitCode) -> ServiceStatus {
    ServiceStatus {
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

/// Delay before the SCM restarts the service after a failure.
const RESTART_DELAY: Duration = Duration::from_secs(5);
/// A failure-free stretch this long resets the attempt counter, so a service that has run
/// well for a day gets a fresh set of restarts the next time it fails.
const FAILURE_RESET_PERIOD: Duration = Duration::from_secs(24 * 60 * 60);

/// What the SCM does when the service fails: restart it twice, five seconds apart, then
/// leave it stopped. A bounded count keeps a persistently broken deployment from restarting
/// forever, while the reset period makes the bound apply per incident rather than per
/// lifetime.
fn recovery_policy() -> ServiceFailureActions {
    let restart = ServiceAction {
        action_type: ServiceActionType::Restart,
        delay: RESTART_DELAY,
    };
    ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(FAILURE_RESET_PERIOD),
        reboot_msg: None,
        command: None,
        actions: Some(vec![
            restart.clone(),
            restart,
            ServiceAction {
                action_type: ServiceActionType::None,
                delay: Duration::ZERO,
            },
        ]),
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
        Err(windows_service::Error::Winapi(err))
            if err.raw_os_error() == Some(ERROR_SERVICE_MARKED_FOR_DELETE) =>
        {
            return Err(ServiceError::MarkedForDelete)
        }
        Err(err) => return Err(ServiceError::Api(err)),
    };
    service.set_description(SERVICE_DESCRIPTION)?;
    service.update_failure_actions(recovery_policy())?;
    // Apply the policy to a non-zero exit as well as to a crash: a bootstrap failure that
    // was transient (a volume not mounted yet, a directory briefly unavailable) then recovers
    // on its own, and a persistent one stops after the bounded number of attempts.
    service.set_failure_actions_on_non_crash_failures(true)?;
    // Registered before the first start so even the first lifecycle event renders.
    eventlog::register_source(SERVICE_NAME, exe)?;
    // Only a fully stopped service is started: a StartPending one is already on its way and
    // starting it again fails with ERROR_SERVICE_ALREADY_RUNNING (1056).
    if service.query_status()?.current_state == ServiceState::Stopped {
        service.start::<OsString>(&[])?;
    }
    wait_for_state(&service, ServiceState::Running, START_TIMEOUT)
}

/// Stop the service if running, then delete it. Removing something that was never
/// registered is reported as `NotInstalled`, because the raw OS error 1060 behind it reads
/// as a failure of the tool rather than as the answer to the question that was asked.
pub fn uninstall() -> Result<(), ServiceError> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = match manager.open_service(
        SERVICE_NAME,
        ServiceAccess::STOP | ServiceAccess::QUERY_STATUS | ServiceAccess::DELETE,
    ) {
        Ok(service) => service,
        Err(windows_service::Error::Winapi(err))
            if err.raw_os_error() == Some(ERROR_SERVICE_DOES_NOT_EXIST) =>
        {
            return Err(ServiceError::NotInstalled)
        }
        Err(err) => return Err(ServiceError::Api(err)),
    };
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
    eventlog::unregister_source(SERVICE_NAME)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panic_message_reads_both_payload_forms() {
        let s: Box<dyn std::any::Any + Send> = Box::new("static text");
        assert_eq!(panic_message(s.as_ref()), "static text");
        let s: Box<dyn std::any::Any + Send> = Box::new(String::from("owned text"));
        assert_eq!(panic_message(s.as_ref()), "owned text");
        let s: Box<dyn std::any::Any + Send> = Box::new(7u8);
        assert_eq!(panic_message(s.as_ref()), "(non-string panic payload)");
    }

    /// The recovery policy is what the SCM applies when the service dies; its shape is
    /// asserted here and its registration is verified live in CI with `sc qfailure`.
    #[test]
    fn recovery_policy_restarts_twice_then_gives_up() {
        let policy = recovery_policy();
        assert_eq!(
            policy.reset_period,
            ServiceFailureResetPeriod::After(Duration::from_secs(86_400))
        );
        assert!(policy.reboot_msg.is_none() && policy.command.is_none());
        let actions = policy.actions.expect("actions are set");
        let kinds: Vec<_> = actions.iter().map(|a| a.action_type).collect();
        assert_eq!(
            kinds,
            [
                ServiceActionType::Restart,
                ServiceActionType::Restart,
                ServiceActionType::None
            ]
        );
        assert!(actions[..2]
            .iter()
            .all(|a| a.delay == Duration::from_secs(5)));
    }

    /// The three status reports the SCM sees. `wait_hint` is the contract that keeps the SCM
    /// from declaring the service hung during start-up and stop, and a `Stopped` report must
    /// carry the diagnostic exit code through unchanged; both are easy to lose in a refactor
    /// and invisible until a service actually misbehaves on a machine.
    #[test]
    fn status_builders_have_the_specified_shape() {
        let starting = pending(ServiceState::StartPending);
        assert_eq!(starting.service_type, ServiceType::OWN_PROCESS);
        assert_eq!(starting.current_state, ServiceState::StartPending);
        assert!(
            starting.controls_accepted.is_empty(),
            "a pending service must not advertise controls: {:?}",
            starting.controls_accepted
        );
        assert_eq!(starting.wait_hint, Duration::from_secs(10));
        assert_eq!(starting.checkpoint, 0);
        assert_eq!(starting.process_id, None);

        let running = running();
        assert_eq!(running.current_state, ServiceState::Running);
        assert_eq!(
            running.controls_accepted,
            ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN
        );
        assert_eq!(running.wait_hint, Duration::default());

        let stopped = stopped(ServiceExitCode::ServiceSpecific(EXIT_PANIC));
        assert_eq!(stopped.current_state, ServiceState::Stopped);
        assert_eq!(
            stopped.exit_code,
            ServiceExitCode::ServiceSpecific(EXIT_PANIC)
        );
        assert_eq!(stopped.wait_hint, Duration::default());
        assert_eq!(stopped.service_type, ServiceType::OWN_PROCESS);
    }

    /// The registration the Services console shows (design §4.2): automatic start, its own
    /// process, LocalSystem (`account_name == None`) and no launch arguments — the CLI
    /// rejects runtime overrides next to `--install` precisely because none would be stored.
    #[test]
    fn service_info_matches_the_design() {
        let exe = Path::new(r"C:\Program Files\FlamingoAgent\flamingo-agent.exe");
        let info = service_info(exe);
        assert_eq!(info.name, OsString::from(SERVICE_NAME));
        assert_eq!(info.display_name, OsString::from(SERVICE_DISPLAY_NAME));
        assert_eq!(info.service_type, ServiceType::OWN_PROCESS);
        assert_eq!(info.start_type, ServiceStartType::AutoStart);
        assert_eq!(info.error_control, ServiceErrorControl::Normal);
        assert_eq!(info.executable_path, exe);
        assert!(
            info.launch_arguments.is_empty(),
            "{:?}",
            info.launch_arguments
        );
        assert!(info.dependencies.is_empty(), "{:?}", info.dependencies);
        assert_eq!(info.account_name, None, "must run as LocalSystem");
        assert_eq!(info.account_password, None);
    }
}

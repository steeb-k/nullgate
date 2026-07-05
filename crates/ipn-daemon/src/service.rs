//! Windows service integration (compiled only on Windows). Mirrors seed-sync.
//!
//! The service runs as LocalSystem (so it can create the wintun TUN); the GUI
//! runs as the logged-in user. They meet over the named pipe, whose DACL
//! (`ipn_ipc::transport`) lets the user open a pipe the service created.

use std::ffi::{OsStr, OsString};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use windows_service::{
    define_windows_service,
    service::{
        Service, ServiceAccess, ServiceAction, ServiceActionType, ServiceControl,
        ServiceControlAccept, ServiceErrorControl, ServiceExitCode, ServiceFailureActions,
        ServiceFailureResetPeriod, ServiceInfo, ServiceStartType, ServiceState, ServiceStatus,
        ServiceType,
    },
    service_control_handler::{self, ServiceControlHandlerResult},
    service_dispatcher,
    service_manager::{ServiceManager, ServiceManagerAccess},
    Result as WsResult,
};

const SERVICE_NAME: &str = "NullgateDaemon";
const SERVICE_DISPLAY: &str = "Nullgate";
const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;

/// SCM entry point (invoked when started via `ipn-daemon service`).
pub fn run_as_service() -> WsResult<()> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)
}

define_windows_service!(ffi_service_main, service_main);

fn service_main(_args: Vec<OsString>) {
    if let Err(e) = run_service() {
        tracing::error!("service exited with error: {e}");
    }
}

fn run_service() -> WsResult<()> {
    let shutdown = Arc::new(Notify::new());

    let handler_shutdown = shutdown.clone();
    let event_handler = move |control: ServiceControl| -> ServiceControlHandlerResult {
        match control {
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            ServiceControl::Stop | ServiceControl::Shutdown => {
                handler_shutdown.notify_one();
                ServiceControlHandlerResult::NoError
            }
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };
    let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)?;

    let set_state = |state: ServiceState, accepts: ServiceControlAccept| ServiceStatus {
        service_type: SERVICE_TYPE,
        current_state: state,
        controls_accepted: accepts,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    };

    status_handle.set_service_status(set_state(
        ServiceState::Running,
        ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
    ))?;

    let data_dir = crate::default_data_dir();
    let socket = ipn_ipc::default_socket();
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let result = rt.block_on(async move {
        crate::serve(data_dir, socket, async move {
            shutdown.notified().await;
        })
        .await
    });
    if let Err(e) = result {
        tracing::error!("daemon serve error: {e:#}");
    }

    status_handle.set_service_status(set_state(
        ServiceState::Stopped,
        ServiceControlAccept::empty(),
    ))?;
    Ok(())
}

/// Configure SCM auto-recovery so a daemon crash restarts itself. Windows sets
/// no failure actions by default, so without this a panic (the observed
/// `0xc0000409` fastfail) leaves the service dead until the next boot. Restart
/// with an escalating back-off and reset the failure counter after a day of
/// health. Requires the handle to carry `CHANGE_CONFIG`.
fn set_recovery(service: &Service) -> WsResult<()> {
    let actions = ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(24 * 60 * 60)),
        reboot_msg: None,
        command: None,
        actions: Some(vec![
            ServiceAction {
                action_type: ServiceActionType::Restart,
                delay: Duration::from_secs(5),
            },
            ServiceAction {
                action_type: ServiceActionType::Restart,
                delay: Duration::from_secs(15),
            },
            ServiceAction {
                action_type: ServiceActionType::Restart,
                delay: Duration::from_secs(60),
            },
        ]),
    };
    service.update_failure_actions(actions)?;
    // Also restart on a non-zero clean exit. Our graceful stop reports
    // Stopped/0, so an intentional `stop` still won't bounce.
    let _ = service.set_failure_actions_on_non_crash_failures(true);
    Ok(())
}

/// install / uninstall / start / stop / recover via the service control manager.
pub fn manage(cmd: &str) -> WsResult<()> {
    let access = ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE;
    let manager = ServiceManager::local_computer(None::<&str>, access)?;

    match cmd {
        "install" => {
            let exe = std::env::current_exe().expect("current exe path");
            let info = ServiceInfo {
                name: OsString::from(SERVICE_NAME),
                display_name: OsString::from(SERVICE_DISPLAY),
                service_type: SERVICE_TYPE,
                start_type: ServiceStartType::AutoStart,
                error_control: ServiceErrorControl::Normal,
                executable_path: exe,
                launch_arguments: vec![OsString::from("service")],
                dependencies: vec![],
                account_name: None, // LocalSystem
                account_password: None,
            };
            let service = manager
                .create_service(&info, ServiceAccess::CHANGE_CONFIG | ServiceAccess::START)?;
            service.set_description("Nullgate P2P virtual LAN daemon")?;
            if let Err(e) = set_recovery(&service) {
                tracing::warn!("could not set service recovery actions: {e}");
            }
            let _ = service.start::<&OsStr>(&[]);
            println!("installed and started service '{SERVICE_NAME}'");
        }
        "uninstall" => {
            let access = ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE;
            let service = manager.open_service(SERVICE_NAME, access)?;
            if service.query_status()?.current_state != ServiceState::Stopped {
                let _ = service.stop();
            }
            service.delete()?;
            println!("uninstalled service '{SERVICE_NAME}'");
        }
        "start" => {
            let service = manager.open_service(SERVICE_NAME, ServiceAccess::START)?;
            service.start::<&OsStr>(&[])?;
            println!("started service '{SERVICE_NAME}'");
        }
        "stop" => {
            let service = manager.open_service(SERVICE_NAME, ServiceAccess::STOP)?;
            service.stop()?;
            println!("stopped service '{SERVICE_NAME}'");
        }
        "restart" => {
            // Stop (if running), wait for it to actually reach Stopped, then start.
            // `stop()` only *requests* the stop, so starting immediately would race
            // the SCM; poll up to ~10s. Used by the app's elevated restart action.
            let access =
                ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::START;
            let service = manager.open_service(SERVICE_NAME, access)?;
            if service.query_status()?.current_state != ServiceState::Stopped {
                let _ = service.stop();
                for _ in 0..50 {
                    std::thread::sleep(Duration::from_millis(200));
                    if service.query_status()?.current_state == ServiceState::Stopped {
                        break;
                    }
                }
            }
            service.start::<&OsStr>(&[])?;
            println!("restarted service '{SERVICE_NAME}'");
        }
        "recover" => {
            // Apply/repair recovery actions on an already-installed service (e.g.
            // one installed by an older MSI that predates this). Needs elevation.
            let service = manager.open_service(SERVICE_NAME, ServiceAccess::CHANGE_CONFIG)?;
            set_recovery(&service)?;
            println!("configured auto-restart recovery for service '{SERVICE_NAME}'");
        }
        other => tracing::warn!("unknown service command: {other}"),
    }
    Ok(())
}

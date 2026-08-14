//! Windows service (SCM) integration for rathole-x.
//!
//! Whole-module cfg: compiled only on Windows. Provides:
//!
//! - the UAC elevation relay (`is_elevated`, `relaunch_elevated_wait`,
//!   `redirect_stdio_to_file`),
//! - SCM registration (`install`/`uninstall`) plus the CLI flow wrappers
//!   (`install_service`/`uninstall_service`/`elevate_for_config_if_needed`),
//! - the service entry point (`run_service`) dispatched via the hidden
//!   `service run` subcommand when the Service Control Manager starts us.
#![cfg(windows)]

use crate::cli::{InstallArgs, UninstallArgs};
use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use tokio::sync::broadcast;
use tracing::{error, info, warn};
use windows_service::service::{
    ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode,
    ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_service::{define_windows_service, service_dispatcher, Error as ServiceError};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_CANCELLED, ERROR_SERVICE_DOES_NOT_EXIST, ERROR_SERVICE_EXISTS,
    ERROR_SERVICE_MARKED_FOR_DELETE, HANDLE,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
};

use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetExitCodeProcess, OpenProcessToken, WaitForSingleObject, INFINITE,
};
use windows_sys::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

/// Default service name, also used as the name registered with the SCM
/// dispatcher. For `SERVICE_WIN32_OWN_PROCESS` services the name passed to
/// `RegisterServiceCtrlHandlerW` is ignored by the SCM, so a custom
/// `--name` at install time does not need to be threaded through here.
const SERVICE_NAME: &str = "rathole-x";

/// How long `uninstall` waits for the service to report `Stopped` before
/// attempting deletion anyway.
const STOP_WAIT_TIMEOUT: Duration = Duration::from_secs(15);

/// Poll interval while waiting for the service to stop.
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Options for [`install`].
pub struct ServiceInstallOptions {
    /// Short service name (SCM name becomes rathole-x-<role>-<name>).
    pub name: String,
    /// The role the service runs as.
    pub role: crate::config_edit::ServiceRole,
    /// Resolved absolute config path passed to `service run --config`.
    pub config_path: PathBuf,
    /// Allow non-admin users to modify the config (ACL) and have the CLI
    /// write it without elevation. Stored in `version.toml` next to the config;
    /// changing it requires reinstalling the service.
    pub allow_user_config: bool,
}

/// The SCM service name for an installed rathole-x service.
pub fn scm_name(role: crate::config_edit::ServiceRole, name: &str) -> String {
    format!("rathole-x-{}-{}", role.key(), name)
}

/// Path of the authorization policy file next to `config_path`.
fn version_path(config_path: &Path) -> PathBuf {
    config_path.with_file_name("version.toml")
}

/// Returns `true` if the current process token is elevated (running as
/// administrator). Failures are treated as "not elevated" so callers fall
/// back to the UAC relaunch path.
pub fn is_elevated() -> bool {
    // SAFETY: `GetCurrentProcess` returns a pseudo-handle that is always
    // valid for the current process and must NOT be closed.
    let process = unsafe { GetCurrentProcess() };

    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: `process` is a valid process (pseudo-)handle; `token` points to
    // writable storage for a HANDLE that OpenProcessToken initializes on
    // success.
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
        return false;
    }

    let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut return_length: u32 = 0;
    // SAFETY: `token` is a valid token handle opened above. `elevation` is a
    // properly aligned, writable TOKEN_ELEVATION buffer whose exact size is
    // passed; `return_length` is writable. TokenElevation has no
    // variable-length tail, so the fixed-size query cannot truncate.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            &mut elevation as *mut TOKEN_ELEVATION as *mut core::ffi::c_void,
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut return_length,
        )
    };
    // SAFETY: `token` is a valid handle we own; it is closed exactly once.
    unsafe { CloseHandle(token) };

    ok != 0 && elevation.TokenIsElevated != 0
}

/// Encode an `OsStr` as a nul-terminated wide (UTF-16) buffer for Win32 APIs.
fn to_wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

/// Quote a single command-line argument following the Windows C runtime
/// argument parsing rules (the convention `CommandLineToArgvW` uses).
fn quote_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.chars().any(|c| c == ' ' || c == '\t' || c == '"') {
        return arg.to_owned();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                // Backslashes before a quote are doubled, then the quote is escaped.
                out.push_str(&"\\".repeat(backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.push_str(&"\\".repeat(backslashes));
                backslashes = 0;
                out.push(c);
            }
        }
    }
    // Trailing backslashes before the closing quote are doubled.
    out.push_str(&"\\".repeat(backslashes * 2));
    out.push('"');
    out
}

/// Relaunch the current executable with `args` under UAC elevation, hidden,
/// wait for it to finish, and replay its output in this process.
///
/// The elevated child writes all output to a temp log (via the hidden
/// `--elevated-log` flag handled in main.rs) so nothing shows up in a
/// separate console window. Returns the child's output on success; errors on
/// a cancelled UAC prompt or a failing child.
pub fn relaunch_elevated_wait(args: &[String]) -> Result<String> {
    let exe = std::env::current_exe().context("failed to resolve current executable path")?;

    let log_path = std::env::temp_dir().join(format!(
        "rathole-x-elev-{}-{}.log",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));

    let mut params: Vec<String> = args.to_vec();
    params.push("--elevated-log".to_owned());
    params.push(log_path.to_string_lossy().into_owned());
    let params = params
        .iter()
        .map(|a| quote_arg(a))
        .collect::<Vec<_>>()
        .join(" ");

    let verb_w = to_wide(OsStr::new("runas"));
    let exe_w = to_wide(exe.as_os_str());
    let params_w = to_wide(OsStr::new(&params));

    // SAFETY: `verb_w`, `exe_w` and `params_w` are valid nul-terminated
    // UTF-16 buffers that outlive the call. The struct is zero-initialized
    // with the correct cbSize. ShellExecuteExW does not retain the pointers.
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS;
    info.lpVerb = verb_w.as_ptr();
    info.lpFile = exe_w.as_ptr();
    info.lpParameters = params_w.as_ptr();
    info.nShow = SW_HIDE;

    let ok = unsafe { ShellExecuteExW(&mut info) };
    if ok == 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(ERROR_CANCELLED as i32) {
            return Err(anyhow!("UAC elevation cancelled by user"));
        }
        return Err(anyhow!("failed to relaunch elevated: {}", err));
    }

    // SAFETY: `hProcess` is a valid process handle from ShellExecuteExW when
    // SEE_MASK_NOCLOSEPROCESS is set.
    let exit_code = unsafe {
        WaitForSingleObject(info.hProcess, INFINITE);
        let mut code: u32 = 0;
        GetExitCodeProcess(info.hProcess, &mut code);
        CloseHandle(info.hProcess);
        code
    };

    let output = std::fs::read_to_string(&log_path).unwrap_or_default();
    let _ = std::fs::remove_file(&log_path);

    if exit_code != 0 {
        bail!(
            "elevated command failed (exit code {}):\n{}",
            exit_code,
            output.trim()
        );
    }
    Ok(output)
}

/// The SCM launch arguments: hidden `service run` entry + config flag.
/// The --config flag is load-bearing: without it the service exits at
/// startup and the SCM reports error 1053.
fn launch_arguments(config_path: &Path) -> Vec<OsString> {
    vec![
        OsString::from("service"),
        OsString::from("run"),
        OsString::from("--config"),
        config_path.as_os_str().to_os_string(),
    ]
}

/// Create and start a Windows service that runs
/// `"<current_exe>" service run --config "<config_path>"` at boot.
/// Also writes the `version.toml` policy and, when allowed, opens the config
/// ACL to BUILTIN\Users.
pub fn install(opts: &ServiceInstallOptions) -> Result<()> {
    if let Some(parent) = opts.config_path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("failed to create config directory {}", parent.display())
            })?;
        }
    }

    // Write the version stamp. It deliberately has no CLI editor: changing it
    // requires reinstalling the service. No permission policy is stored —
    // the CLI probes the config's actual writability instead.
    std::fs::write(
        version_path(&opts.config_path),
        format!(
            "# Written by `rathole-x service install`. Reinstall the service to change this.\n\
             version = {}\n",
            crate::cli::major_version()
        ),
    )
    .context("failed to write version.toml")?;

    if opts.allow_user_config {
        // Grant BUILTIN\Users the right to create files in the config
        // directory (needed for the CLI's atomic write + rename) and modify
        // rights on the config file itself. Deliberately NOT modify on the
        // directory: the service binary lives there and must not be
        // replaceable by non-admins (that would be LocalSystem code exec).
        let dir = opts.config_path.parent().unwrap_or(Path::new("."));
        let status = std::process::Command::new("icacls")
            .arg(dir)
            .arg("/grant")
            .arg("*S-1-5-32-545:WD")
            .status()
            .context("failed to run icacls on the config directory")?;
        if !status.success() {
            bail!("icacls failed granting Users create rights on {}", dir.display());
        }
        let status = std::process::Command::new("icacls")
            .arg(&opts.config_path)
            .arg("/grant")
            .arg("*S-1-5-32-545:M")
            .status()
            .context("failed to run icacls on the config file")?;
        if !status.success() {
            bail!(
                "icacls failed granting Users modify on {}",
                opts.config_path.display()
            );
        }
    }

    // Self-install the binary next to the config so the service keeps a
    // stable path even when the user moves or deletes the original file.
    let exe = std::env::current_exe().context("failed to resolve current executable path")?;
    let exe_name = exe
        .file_name()
        .ok_or_else(|| anyhow!("current executable has no file name"))?;
    let installed_exe = opts
        .config_path
        .parent()
        .unwrap_or(Path::new("."))
        .join(exe_name);
    ensure_binary_copy(&exe, &installed_exe)?;

    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .context("failed to open the service control manager (elevated rights required)")?;

    let service_name = scm_name(opts.role, &opts.name);
    let service_info = ServiceInfo {
        name: OsString::from(&service_name),
        display_name: OsString::from(format!("rathole-x ({})", service_name)),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: installed_exe.clone(),
        launch_arguments: launch_arguments(&opts.config_path),
        dependencies: vec![],
        account_name: None, // LocalSystem
        account_password: None,
    };

    let service = manager
        .create_service(
            &service_info,
            ServiceAccess::QUERY_STATUS | ServiceAccess::START,
        )
        .map_err(|e| match e {
            ServiceError::Winapi(io_err)
                if io_err.raw_os_error() == Some(ERROR_SERVICE_EXISTS as i32) =>
            {
                anyhow!(
                    "service '{}' already exists; run `rathole-x service uninstall --yes --name {}` first",
                    service_name, opts.name
                )
            }
            other => anyhow!(other)
                .context(format!("failed to create service '{}'", service_name)),
        })?;
    service
        .start::<&OsStr>(&[])
        .with_context(|| format!("failed to start service '{}'", service_name))?;

    write_uninstall_bat(&installed_exe, &opts.config_path, &opts.name, opts.role);

    info!(
        "service '{}' installed and started (binary: {}, config: {})",
        service_name,
        installed_exe.display(),
        opts.config_path.display()
    );
    Ok(())
}

/// Copy `source` to `dest` unless they are the same file (reinstalling from
/// the installed location). Overwrites a stale copy.
fn ensure_binary_copy(source: &Path, dest: &Path) -> Result<()> {
    if source == dest {
        return Ok(());
    }
    std::fs::copy(source, dest).with_context(|| {
        format!(
            "failed to copy {} to {} (is the service running from {}? run `rathole-x service uninstall --yes` first)",
            source.display(),
            dest.display(),
            dest.display()
        )
    })?;
    Ok(())
}

/// Write an `uninstall-<name>.bat` next to the installed binary so the
/// service can be removed without the original binary. Purge is opt-in,
/// matching the CLI.
fn write_uninstall_bat(
    installed_exe: &Path,
    config_path: &Path,
    name: &str,
    role: crate::config_edit::ServiceRole,
) {
    let exe_name = installed_exe
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or(std::borrow::Cow::Borrowed("rathole-x.exe"));
    let config_name = config_path
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or(std::borrow::Cow::Borrowed("default.toml"));
    let bat = installed_exe.with_file_name(format!("uninstall-{}.bat", name));
    let service = scm_name(role, name);
    let content = format!(
        "@echo off\r\n\
         setlocal\r\n\
         echo Uninstalling {service} ...\r\n\
         \"%~dp0{exe}\" service uninstall --yes --name \"{name}\" --config \"%~dp0{config}\"\r\n\
         if errorlevel 1 goto :failed\r\n\
         echo.\r\n\
         echo Done. To also delete the configuration, run:\r\n\
         echo   \"%~dp0{exe}\" service uninstall --yes --purge --name \"{name}\" --config \"%~dp0{config}\"\r\n\
         del \"%~f0\"\r\n\
         goto :eof\r\n\
         :failed\r\n\
         echo.\r\n\
         echo Uninstall failed. Right-click this file and choose \"Run as administrator\".\r\n\
         pause\r\n",
        service = service,
        name = name,
        exe = exe_name,
        config = config_name,
    );
    if let Err(e) = std::fs::write(&bat, content) {
        warn!("failed to write {}: {}", bat.display(), e);
    }
}

/// Stop (best-effort, waiting up to [`STOP_WAIT_TIMEOUT`]) and delete the
/// service that owns `config_path`. The SCM name is derived from the config
/// role and file stem; when the config is unreadable, both role variants are
/// tried. File cleanup: the config file is kept unless `purge`; version.toml is
/// removed when no other service config remains in the directory.
pub fn uninstall(config_path: &Path, purge: bool) -> Result<()> {
    let name = config_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("default")
        .to_string();

    let role = std::fs::read_to_string(config_path)
        .ok()
        .and_then(|c| toml::from_str::<crate::config::Config>(&c).ok())
        .map(|c| {
            if c.client.is_some() {
                crate::config_edit::ServiceRole::Client
            } else {
                crate::config_edit::ServiceRole::Server
            }
        });

    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .context("failed to open the service control manager (elevated rights required)")?;

    let open = |service_name: &str| {
        manager.open_service(
            service_name,
            ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
        )
    };

    let service = match &role {
        Some(role) => {
            let primary = scm_name(*role, &name);
            match open(&primary) {
                Ok(s) => s,
                Err(_) => {
                    let alt_role = match role {
                        crate::config_edit::ServiceRole::Client => {
                            crate::config_edit::ServiceRole::Server
                        }
                        crate::config_edit::ServiceRole::Server => {
                            crate::config_edit::ServiceRole::Client
                        }
                    };
                    open(&scm_name(alt_role, &name)).map_err(|e| match e {
                        ServiceError::Winapi(io_err)
                            if io_err.raw_os_error()
                                == Some(ERROR_SERVICE_DOES_NOT_EXIST as i32) =>
                        {
                            anyhow!("service '{}' does not exist", primary)
                        }
                        other => anyhow!(other)
                            .context(format!("failed to open service '{}'", primary)),
                    })?
                }
            }
        }
        None => {
            // Config unreadable: try both role variants.
            let a = scm_name(crate::config_edit::ServiceRole::Client, &name);
            let b = scm_name(crate::config_edit::ServiceRole::Server, &name);
            match open(&a).or_else(|_| open(&b)) {
                Ok(s) => s,
                Err(e) => {
                    return Err(anyhow!(e).context(format!(
                        "failed to open service '{}' or '{}'",
                        a, b
                    )))
                }
            }
        }
    };

    let status = service
        .query_status()
        .context("failed to query service status")?;

    if status.current_state != ServiceState::Stopped {
        // Best-effort stop: even if sending STOP fails we still try to
        // delete below, which marks the service for deletion once it exits.
        match service.stop() {
            Ok(_) => {
                let deadline = Instant::now() + STOP_WAIT_TIMEOUT;
                loop {
                    let current = service
                        .query_status()
                        .context("failed to poll service status while stopping")?;
                    if current.current_state == ServiceState::Stopped {
                        break;
                    }
                    if Instant::now() >= deadline {
                        warn!(
                            "service did not stop within {:?}; deleting anyway",
                            STOP_WAIT_TIMEOUT
                        );
                        break;
                    }
                    std::thread::sleep(STOP_POLL_INTERVAL);
                }
            }
            Err(e) => {
                warn!("failed to send STOP to the service: {}; deleting anyway", e);
            }
        }
    }

    match service.delete() {
        Ok(()) => {}
        Err(ServiceError::Winapi(io_err))
            if io_err.raw_os_error() == Some(ERROR_SERVICE_MARKED_FOR_DELETE as i32) => {}
        Err(e) => {
            return Err(anyhow!(e)).context("failed to delete service")
        }
    }

    remove_service_files(config_path, purge);

    info!("service for '{}' uninstalled", name);
    Ok(())
}

/// Remove the service-managed files. The config file and stale atomic-write
/// temp files only when `purge` is set; version.toml when no other service
/// config remains in the directory. Errors are logged, never fatal: the
fn remove_service_files(config_path: &Path, purge: bool) {
    if purge {
        match std::fs::remove_file(config_path) {
            Ok(()) => info!("removed config {}", config_path.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => warn!("failed to remove {}: {}", config_path.display(), e),
        }

        // Stale temp files from interrupted atomic writes (config.tmp<pid>)
        if let Some(parent) = config_path.parent() {
            let stem = config_path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            if let Ok(entries) = std::fs::read_dir(parent) {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if name.starts_with(&format!("{}.tmp", stem)) {
                        let _ = std::fs::remove_file(entry.path());
                    }
                }
            }
        }
    }

    // version.toml goes with the last service: remove it when no OTHER
    // service config remains in the directory.
    let dir = config_path.parent().unwrap_or(Path::new("."));
    let others_remain = std::fs::read_dir(dir)
        .map(|entries| {
            entries.flatten().any(|e| {
                let p = e.path();
                p != config_path
                    && p.file_stem().and_then(|s| s.to_str()) != Some("version")
                    && p.extension().and_then(|x| x.to_str()) == Some("toml")
            })
        })
        .unwrap_or(false);
    if !others_remain {
        let vp = version_path(config_path);
        match std::fs::remove_file(&vp) {
            Ok(()) => info!("removed version stamp {}", vp.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => warn!("failed to remove {}: {}", vp.display(), e),
        }
    }
}

// ---------------------------------------------------------------------------
// Service entry point (invoked by the SCM via `service run`)
// ---------------------------------------------------------------------------

define_windows_service!(ffi_service_main, service_main);

/// Config path handed from `run_service` to `service_main`, which the SCM
/// invokes on a different thread with no way to pass user data.
static SERVICE_CONFIG_PATH: OnceLock<PathBuf> = OnceLock::new();

/// Entry point of the hidden `service run` subcommand. Connects this process
/// to the SCM dispatcher and blocks until the service stops.
pub fn run_service(config_path: PathBuf) -> Result<()> {
    SERVICE_CONFIG_PATH
        .set(config_path)
        .map_err(|_| anyhow!("service config path already initialized"))?;
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)
        .context("failed to connect to the service control manager; `service run` must be started by the SCM")?;
    Ok(())
}

/// High-level service entry point, invoked on a background thread by the
/// dispatcher once the SCM starts the service.
fn service_main(_arguments: Vec<OsString>) {
    let config_path = SERVICE_CONFIG_PATH
        .get()
        .cloned()
        .unwrap_or_else(|| PathBuf::from("rathole-x.toml"));
    if let Err(e) = service_main_inner(config_path) {
        // Tracing may not be initialized yet if we failed early; this is
        // best-effort before the process exits.
        error!("rathole-x service failed: {:#}", e);
    }
}

/// Report a service status transition to the SCM.
fn report_status(
    status_handle: &windows_service::service_control_handler::ServiceStatusHandle,
    current_state: ServiceState,
    controls_accepted: ServiceControlAccept,
    exit_code: ServiceExitCode,
    checkpoint: u32,
    wait_hint: Duration,
) -> std::result::Result<(), ServiceError> {
    status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state,
        controls_accepted,
        exit_code,
        checkpoint,
        wait_hint,
        process_id: None,
    })
}

fn service_main_inner(config_path: PathBuf) -> Result<()> {
    // Channel the control handler uses to ask the main thread to shut down.
    // Status transitions are reported from the main thread only; the handler
    // stays cheap and never blocks on rathole's shutdown.
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    // Shutdown broadcast consumed by `rathole::run`.
    let (shutdown_tx, shutdown_rx) = broadcast::channel::<bool>(1);

    let status_handle = service_control_handler::register(SERVICE_NAME, move |control_event| {
        match control_event {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                // If the main thread is gone there is nothing to notify.
                let _ = stop_tx.send(());
                ServiceControlHandlerResult::NoError
            }
            // Interrogate must always be answered, even as a no-op.
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    })
    .context("failed to register service control handler")?;

    report_status(
        &status_handle,
        ServiceState::StartPending,
        ServiceControlAccept::empty(),
        ServiceExitCode::Win32(0),
        1,
        Duration::from_secs(10),
    )
    .context("failed to report StartPending")?;

    // File logging: <config_dir>\logs\rathole-x.log (daily rotation).
    let log_dir = config_path
        .parent()
        .map(|dir| dir.join("logs"))
        .unwrap_or_else(|| PathBuf::from("logs"));
    std::fs::create_dir_all(&log_dir)
        .with_context(|| format!("failed to create log directory {}", log_dir.display()))?;
    let file_appender = tracing_appender::rolling::daily(&log_dir, "rathole-x.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
    // Keep the guard alive for the whole function scope so buffered log
    // records are flushed on shutdown.
    let _guard = guard;
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    // try_init: never panic if a subscriber is somehow already installed.
    let _ = tracing_subscriber::fmt()
        .with_writer(non_blocking)
        .with_env_filter(env_filter)
        .with_ansi(false)
        .try_init();

    info!("rathole-x service starting; config: {}", config_path.display());

    report_status(
        &status_handle,
        ServiceState::Running,
        ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
        ServiceExitCode::Win32(0),
        0,
        Duration::ZERO,
    )
    .context("failed to report Running")?;

    // Run rathole on a dedicated thread with its own multi-threaded tokio
    // runtime so this thread stays free to react to SCM stop requests.
    let cli = crate::Cli {
        command: Some(crate::Commands::Run(crate::RunArgs {
            config: Some(config_path),
            ..Default::default()
        })),
        ..Default::default()
    };
    let worker = std::thread::spawn(move || -> Result<()> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("failed to build tokio runtime")?;
        runtime.block_on(crate::run(cli, shutdown_rx))
    });

    // Wait for either an SCM stop request or rathole exiting on its own.
    let run_result = loop {
        match stop_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                info!("stop requested by the service control manager");
                if let Err(e) = report_status(
                    &status_handle,
                    ServiceState::StopPending,
                    ServiceControlAccept::empty(),
                    ServiceExitCode::Win32(0),
                    2,
                    Duration::from_secs(30),
                ) {
                    warn!("failed to report StopPending: {}", e);
                }
                // A broadcast send only fails when there are no receivers,
                // i.e. rathole already exited; joining below handles that.
                let _ = shutdown_tx.send(true);
                break worker
                    .join()
                    .unwrap_or_else(|_| Err(anyhow!("rathole worker thread panicked")));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if worker.is_finished() {
                    break worker
                        .join()
                        .unwrap_or_else(|_| Err(anyhow!("rathole worker thread panicked")));
                }
            }
        }
    };

    let exit_code = match &run_result {
        Ok(()) => {
            info!("rathole-x service stopped cleanly");
            ServiceExitCode::Win32(0)
        }
        Err(e) => {
            error!("rathole-x service exiting with error: {:#}", e);
            ServiceExitCode::ServiceSpecific(1)
        }
    };
    report_status(
        &status_handle,
        ServiceState::Stopped,
        ServiceControlAccept::empty(),
        exit_code,
        0,
        Duration::ZERO,
    )
    .context("failed to report Stopped")?;

    run_result
}

/// CLI flow for `rathole-x service install`: print the plan in the caller's window,
/// elevate if needed (the elevated phase runs hidden), and run the SCM
/// registration. Every user-visible line is printed from this process.
pub fn install_service(
    args: &InstallArgs,
    role: crate::config_edit::ServiceRole,
    name: &str,
    config_path: &Path,
) -> Result<()> {
    let service_name = scm_name(role, name);
    println!("Installing {} service '{}':", role.key(), name);
    println!("  Service:     {}", service_name);
    println!("  Config:      {}", config_path.display());
    if args.allow_user_config {
        println!("  Access:      normal users may edit the config (no UAC)");
    } else {
        println!("  Access:      admin-only config; edits elevate via UAC");
    }
    println!("  Auto start:  enabled (boot)");

    if !is_elevated() {
        println!("Requesting administrator rights (UAC)...");
        let _ = relaunch_elevated_wait(&std::env::args().skip(1).collect::<Vec<_>>())?;
        println!("Service '{}' installed and started", service_name);
        println!(
            "Run `rathole-x status --name {}` to inspect, `rathole-x config add --name {} ...` to configure (hot-reloaded).",
            name, name
        );
        return Ok(());
    }

    install(&ServiceInstallOptions {
        name: name.to_owned(),
        role,
        config_path: config_path.to_path_buf(),
        allow_user_config: args.allow_user_config,
    })?;
    println!("Service '{}' installed and started", service_name);
    println!(
        "Run `rathole-x status --name {}` to inspect, `rathole-x config add --name {} ...` to configure (hot-reloaded).",
        name, name
    );
    Ok(())
}

/// CLI flow for `rathole-x service uninstall`: print the plan in the caller's
/// window. When the service is already gone the leftover files are removed
/// WITHOUT elevation; otherwise the elevated phase removes the service.
/// After a normal uninstall the kept config is left user-deletable.
pub fn uninstall_service(args: &UninstallArgs, config_path: &Path) -> Result<()> {
    let name = config_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("default")
        .to_string();

    println!("Uninstalling service '{}':", name);
    if args.purge {
        println!("  Config:      will be removed ({})", config_path.display());
    } else {
        println!("  Config:      will be kept ({})", config_path.display());
    }

    let role = std::fs::read_to_string(config_path)
        .ok()
        .and_then(|c| toml::from_str::<crate::config::Config>(&c).ok())
        .map(|c| {
            if c.client.is_some() {
                crate::config_edit::ServiceRole::Client
            } else {
                crate::config_edit::ServiceRole::Server
            }
        });

    let exists = match &role {
        Some(r) => service_exists(&scm_name(*r, &name)),
        None => {
            service_exists(&scm_name(crate::config_edit::ServiceRole::Client, &name))
                || service_exists(&scm_name(crate::config_edit::ServiceRole::Server, &name))
        }
    };

    if !exists {
        // Service already gone: clean the files directly, no UAC needed.
        println!("Service '{}' is not installed; removing leftover files without elevation.", name);
        remove_service_files(config_path, true);
        println!("Done. Leftover files removed.");
        return Ok(());
    }

    if !is_elevated() {
        println!("Requesting administrator rights (UAC)...");
        let _ = relaunch_elevated_wait(&std::env::args().skip(1).collect::<Vec<_>>())?;
        println!("Service '{}' stopped and removed", name);
        if args.purge {
            println!("  Config:      removed ({})", config_path.display());
        } else {
            println!("  Config:      kept (user-deletable) ({})", config_path.display());
        }
        return Ok(());
    }

    uninstall(config_path, args.purge)?;
    // Leave the kept config user-deletable after a normal uninstall.
    if !args.purge {
        grant_users_modify(config_path);
        grant_users_modify(&config_path.with_file_name(format!("uninstall-{}.bat", name)));
    }
    println!("Service '{}' stopped and removed", name);
    if args.purge {
        println!("  Config:      removed ({})", config_path.display());
    } else {
        println!("  Config:      kept (user-deletable) ({})", config_path.display());
    }
    Ok(())
}

/// `rathole-x service uninstall --all`: remove every installed service, every
/// config, the leftover uninstall bats and the shared binary.
pub fn uninstall_all(_args: &UninstallArgs) -> Result<()> {
    let services = crate::config_edit::list_installed_services()?;
    if services.is_empty() {
        println!("No installed services to remove.");
        return Ok(());
    }

    if !is_elevated() {
        println!("Requesting administrator rights (UAC) to remove all services...");
        let _ = relaunch_elevated_wait(&std::env::args().skip(1).collect::<Vec<_>>())?;
        println!("All rathole-x services removed.");
        return Ok(());
    }

    let dir = crate::config_edit::config_dir();
    let mut removed = 0usize;
    for (name, role) in &services {
        let path = dir.join(format!("{}.toml", name));
        if service_exists(&scm_name(*role, name)) {
            uninstall(&path, true)?;
        } else {
            remove_service_files(&path, true);
        }
        removed += 1;
    }

    // Leftover per-service bats and the shared binary (best effort).
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let fname = entry.file_name().to_string_lossy().to_string();
            if (fname.starts_with("uninstall-") && fname.ends_with(".bat"))
                || fname == "uninstall.bat"
                || fname == "rathole-x.exe"
            {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    println!("Removed {} service(s) and the shared files in {}.", removed, dir.display());
    Ok(())
}

/// `rathole-x service start|stop|restart` — drive the SCM state of installed
/// services. Requires elevation (SCM Start/Stop are admin-only).
pub fn control_service(cmd: crate::cli::ServiceCmd) -> Result<()> {
    use crate::cli::ServiceCmd::*;
    let (action, args) = match cmd {
        Start(a) => ("start", a),
        Stop(a) => ("stop", a),
        Restart(a) => ("restart", a),
        // Install/Uninstall/Run are handled by the dispatch layer.
        _ => unreachable!("only start/stop/restart reach control_service"),
    };

    let targets: Vec<(String, crate::config_edit::ServiceRole)> = if args.all {
        crate::config_edit::list_installed_services()?
    } else {
        let path =
            crate::config_edit::resolve_service_config(None, args.name.as_deref())?;
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("default")
            .to_string();
        let role = std::fs::read_to_string(&path)
            .ok()
            .and_then(|c| toml::from_str::<crate::config::Config>(&c).ok())
            .map(|c| {
                if c.client.is_some() {
                    crate::config_edit::ServiceRole::Client
                } else {
                    crate::config_edit::ServiceRole::Server
                }
            })
            .ok_or_else(|| anyhow!("cannot determine the role of {}", path.display()))?;
        vec![(name, role)]
    };
    if targets.is_empty() {
        bail!("no installed services");
    }

    let names = targets
        .iter()
        .map(|(n, _)| n.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    println!("Will {}: {}", action, names);

    if !is_elevated() {
        println!("Requesting administrator rights (UAC)...");
        let _ = relaunch_elevated_wait(&std::env::args().skip(1).collect::<Vec<_>>())?;
        println!("Done: {} {}", action, names);
        return Ok(());
    }

    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .context("failed to open the service control manager (elevated rights required)")?;
    for (name, role) in &targets {
        let scm = scm_name(*role, name);
        let service = manager
            .open_service(
                &scm,
                ServiceAccess::QUERY_STATUS | ServiceAccess::START | ServiceAccess::STOP,
            )
            .with_context(|| format!("failed to open service '{}'", scm))?;
        match action {
            "start" => {
                service
                    .start::<&OsStr>(&[])
                    .with_context(|| format!("failed to start service '{}'", scm))?;
            }
            "stop" => stop_and_wait(&service, &scm)?,
            "restart" => {
                stop_and_wait(&service, &scm)?;
                service
                    .start::<&OsStr>(&[])
                    .with_context(|| format!("failed to restart service '{}'", scm))?;
            }
            _ => unreachable!(),
        }
        println!("{}ed '{}'", action, scm);
    }
    Ok(())
}

/// `rathole-x upgrade`: stop every service, replace the shared binary with
/// the running one, start every service again.
pub fn upgrade_binary() -> Result<()> {
    if !is_elevated() {
        println!("Requesting administrator rights (UAC) to update the installed binary...");
        let _ = relaunch_elevated_wait(&std::env::args().skip(1).collect::<Vec<_>>())?;
        println!("Binary updated and services restarted.");
        return Ok(());
    }

    let services = crate::config_edit::list_installed_services()?;
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .context("failed to open the service control manager (elevated rights required)")?;

    // Stop everything so the shared binary can be replaced.
    for (name, role) in &services {
        let scm = scm_name(*role, name);
        if let Ok(service) = manager.open_service(
            &scm,
            ServiceAccess::QUERY_STATUS | ServiceAccess::START | ServiceAccess::STOP,
        ) {
            if let Err(e) = stop_and_wait(&service, &scm) {
                warn!("failed to stop '{}': {:#}", scm, e);
            }
        }
    }

    // Replace the shared binary.
    let exe = std::env::current_exe().context("failed to resolve current executable path")?;
    let exe_name = exe
        .file_name()
        .ok_or_else(|| anyhow!("current executable has no file name"))?;
    let installed = crate::config_edit::config_dir().join(exe_name);
    ensure_binary_copy(&exe, &installed)?;

    // Start everything again.
    let mut started = 0usize;
    for (name, role) in &services {
        let scm = scm_name(*role, name);
        if let Ok(service) = manager.open_service(
            &scm,
            ServiceAccess::QUERY_STATUS | ServiceAccess::START | ServiceAccess::STOP,
        ) {
            match service.start::<&OsStr>(&[]) {
                Ok(()) => started += 1,
                Err(e) => warn!("failed to start '{}': {}", scm, e),
            }
        }
    }

    println!(
        "Updated {} and restarted {} service(s).",
        installed.display(),
        started
    );
    Ok(())
}

/// Whether a named SCM service exists (read-only query, no admin needed).
fn service_exists(service_name: &str) -> bool {
    let Ok(manager) =
        ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
    else {
        return false;
    };
    manager
        .open_service(service_name, ServiceAccess::QUERY_STATUS)
        .is_ok()
}

/// Stop a service and wait until it reports Stopped (bounded).
fn stop_and_wait(
    service: &windows_service::service::Service,
    name: &str,
) -> Result<()> {
    let status = service
        .query_status()
        .with_context(|| format!("failed to query status of '{}'", name))?;
    if status.current_state == ServiceState::Stopped {
        return Ok(());
    }
    service
        .stop()
        .with_context(|| format!("failed to send STOP to '{}'", name))?;
    let deadline = Instant::now() + STOP_WAIT_TIMEOUT;
    loop {
        let current = service
            .query_status()
            .with_context(|| format!("failed to poll status of '{}'", name))?;
        if current.current_state == ServiceState::Stopped {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("service '{}' did not stop within {:?}", name, STOP_WAIT_TIMEOUT);
        }
        std::thread::sleep(STOP_POLL_INTERVAL);
    }
}

/// Grant BUILTIN\Users modify rights on a file (used to leave post-uninstall
/// leftovers user-deletable). Best effort; logged, never fatal.
fn grant_users_modify(path: &Path) {
    if !path.exists() {
        return;
    }
    let status = std::process::Command::new("icacls")
        .arg(path)
        .arg("/grant")
        .arg("*S-1-5-32-545:M")
        .status();
    match status {
        Ok(s) if s.success() => info!("granted Users modify on {}", path.display()),
        Ok(s) => warn!("icacls failed granting Users modify on {} (exit {})", path.display(), s),
        Err(e) => warn!("failed to run icacls on {}: {}", path.display(), e),
    }
}

/// Elevate (UAC) before the caller writes `path`, when its auth policy
/// forbids user writes. Returns `true` when the elevated child already did
/// the work (its output is replayed here) and the caller must exit without
/// doing it again.
pub fn elevate_for_config_if_needed(path: &Path) -> Result<bool> {
    if !crate::config_edit::writable_by_current_user(path) && !is_elevated() {
        println!("Requesting administrator rights (UAC) to modify the service config...");
        let output =
            relaunch_elevated_wait(&std::env::args().skip(1).collect::<Vec<_>>())?;
        print!("{}", output);
        return Ok(true);
    }
    Ok(false)
}

/// Redirect stdout and stderr of this process to `path`. Must run before any
/// output happens: Rust's stdio fetches the OS handle lazily on first use.
pub fn redirect_stdio_to_file(path: &Path) {
    use std::io::Write;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Console::{SetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE};

    let Ok(file) = std::fs::File::create(path) else {
        return;
    };
    let handle = file.as_raw_handle() as HANDLE;
    // SAFETY: `handle` is a valid file handle owned by `file`, which is
    // intentionally leaked so the handle stays valid for the process
    // lifetime. SetStdHandle only fails on invalid handles.
    unsafe {
        SetStdHandle(STD_OUTPUT_HANDLE, handle);
        SetStdHandle(STD_ERROR_HANDLE, handle);
    }
    let mut file = file;
    let _ = file.write_all(b"");
    std::mem::forget(file);
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn binary_copy_skips_same_path_and_copies_elsewhere() {
        let dir = std::env::temp_dir().join("rathole-x-bin-copy");
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src.exe");
        let dst = dir.join("dst.exe");
        let _ = std::fs::remove_file(&dst);
        std::fs::write(&src, b"binary-content").unwrap();

        // Same path: no-op, must not error even when the file exists
        ensure_binary_copy(&src, &src).unwrap();

        ensure_binary_copy(&src, &dst).unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), b"binary-content");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn launch_arguments_carry_config_flag() {
        // The --config flag is load-bearing: missing it makes the service
        // exit at startup and the SCM reports error 1053.
        let args = launch_arguments(Path::new(r"C:\dir\svc.toml"));
        assert_eq!(args.len(), 4);
        assert_eq!(args[0].to_str(), Some("service"));
        assert_eq!(args[1].to_str(), Some("run"));
        assert_eq!(args[2].to_str(), Some("--config"));
        assert_eq!(args[3].to_str(), Some(r"C:\dir\svc.toml"));
    }

    #[test]
    fn binary_copy_helper_same_file_noop() {
        let dir = std::env::temp_dir().join("rathole-x-bin-same");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("x.exe");
        std::fs::write(&f, b"x").unwrap();
        ensure_binary_copy(&f, &f).unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn uninstall_bat_written_next_to_binary() {
        let dir = std::env::temp_dir().join("rathole-x-bat");
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("rathole-x.exe");
        let config = dir.join("cfg.toml");
        std::fs::write(&exe, b"x").unwrap();
        std::fs::write(&config, b"y").unwrap();

        write_uninstall_bat(&exe, &config, "relay", crate::config_edit::ServiceRole::Server);

        let bat = std::fs::read_to_string(dir.join("uninstall-relay.bat")).unwrap();
        assert!(bat.contains("service uninstall --yes --name \"relay\""));
        assert!(bat.contains("--config \"%~dp0cfg.toml\""));
        assert!(bat.contains("--purge"), "purge hint documented in the bat");
        assert!(bat.contains("rathole-x.exe"));
        assert!(bat.contains("rathole-x-server-relay"), "SCM name in the bat");

        std::fs::remove_dir_all(&dir).ok();
    }

    fn setup(dir: &Path) -> PathBuf {


        std::fs::create_dir_all(dir).unwrap();
        let config = dir.join("config.toml");
        let auth = dir.join("version.toml");
        let tmp = dir.join("config.toml.tmp4242");
        std::fs::write(&config, "x").unwrap();
        std::fs::write(&auth, "y").unwrap();
        std::fs::write(&tmp, "z").unwrap();
        config
    }

    #[test]
    fn uninstall_keeps_config_removes_auth() {
        let dir = std::env::temp_dir().join("rathole-x-uninstall-keep");
        let _ = std::fs::remove_dir_all(&dir);
        let config = setup(&dir);

        remove_service_files(&config, false);

        assert!(config.exists(), "config kept without --purge");
        assert!(!dir.join("version.toml").exists(), "version.toml always removed");
        assert!(
            dir.join("config.toml.tmp4242").exists(),
            "tmp files kept without --purge"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn uninstall_purge_removes_everything() {
        let dir = std::env::temp_dir().join("rathole-x-uninstall-purge");
        let _ = std::fs::remove_dir_all(&dir);
        let config = setup(&dir);

        remove_service_files(&config, true);

        assert!(!config.exists(), "config removed with --purge");
        assert!(!dir.join("version.toml").exists());
        assert!(
            !dir.join("config.toml.tmp4242").exists(),
            "stale tmp files removed with --purge"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn uninstall_missing_config_still_cleans_auth() {
        let dir = std::env::temp_dir().join("rathole-x-uninstall-missing");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let auth = dir.join("version.toml");
        std::fs::write(&auth, "y").unwrap();
        // Config file gone (purged earlier); only version.toml remains
        remove_service_files(&dir.join("config.toml"), true);
        assert!(!auth.exists(), "version.toml removed with the last service");
        std::fs::remove_dir_all(&dir).ok();
    }
}

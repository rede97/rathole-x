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
use std::io::{Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::RawHandle;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::runtime_status::{
    endpoint_for_config, snapshot, RuntimeRegistry, RuntimeSnapshot, MAX_STATUS_RESPONSE,
    STATUS_REQUEST,
};
use anyhow::{anyhow, bail, Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::NamedPipeServer;
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
    CloseHandle, LocalFree, ERROR_CANCELLED, ERROR_SERVICE_DOES_NOT_EXIST, ERROR_SERVICE_EXISTS,
    ERROR_SERVICE_MARKED_FOR_DELETE, HANDLE,
};
use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use windows_sys::Win32::Security::{
    GetTokenInformation, TokenElevation, SECURITY_ATTRIBUTES, TOKEN_ELEVATION, TOKEN_QUERY,
};

use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, PIPE_ACCESS_DUPLEX,
};
use windows_sys::Win32::System::Pipes::{
    CreateNamedPipeW, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES,
    PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetExitCodeProcess, OpenProcessToken, WaitForSingleObject, INFINITE,
};
use windows_sys::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

// Platform actions report progress through this module. In JSON mode stdout
// belongs exclusively to the top-level envelope, including across UAC relay.
macro_rules! println {
    ($($arg:tt)*) => {{
        if crate::is_json_mode() {
            eprintln!($($arg)*);
        } else {
            ::std::println!($($arg)*);
        }
    }};
}

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

/// Remove any `--elevated-log <path>` pair and any `--confirmed` flag from
/// `args` so a relay never forwards stale relay flags from the original argv
/// before appending its own.
fn strip_relay_flags(args: &[OsString]) -> Vec<OsString> {
    let mut out = Vec::with_capacity(args.len());
    let mut iter = args.iter();
    while let Some(a) = iter.next() {
        if a.as_os_str() == OsStr::new("--elevated-log") {
            // Drop the flag and its value.
            let _ = iter.next();
            continue;
        }
        if a.as_os_str() == OsStr::new("--confirmed") {
            // Boolean flag, no value to drop.
            continue;
        }
        out.push(a.clone());
    }
    out
}

/// Pick a free, unpredictable temp log path for the elevation relay.
/// `create_new` never follows a pre-planted file or reparse point; a
/// collision retries with a fresh random name. The reserved file is removed
/// again so the elevated child can re-create it with `create_new` too (see
/// [`redirect_stdio_to_file`]).
fn pick_elevated_log_path() -> Result<PathBuf> {
    for attempt in 0..8u32 {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let candidate = std::env::temp_dir().join(format!(
            "rathole-x-elev-{}-{}-{}.log",
            std::process::id(),
            nanos,
            attempt
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(_) => {
                let _ = std::fs::remove_file(&candidate);
                return Ok(candidate);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(anyhow!(e)).context("failed to create the elevation log file"),
        }
    }
    bail!("failed to pick a free elevation log path after 8 attempts");
}

/// Relaunch the current executable with `args` under UAC elevation, hidden,
/// wait for it to finish, and replay its output in this process.
///
/// The elevated child writes all output to a temp log (via the hidden
/// `--elevated-log` flag handled in main.rs) so nothing shows up in a
/// separate console window. Returns the child's output on success; errors on
/// a cancelled UAC prompt or a failing child.
pub fn relaunch_elevated_wait(args: &[OsString]) -> Result<String> {
    let exe = std::env::current_exe().context("failed to resolve current executable path")?;

    let log_path = pick_elevated_log_path()?;

    let mut params: Vec<OsString> = strip_relay_flags(args);
    params.push(OsString::from("--elevated-log"));
    params.push(log_path.as_os_str().to_os_string());
    // The parent already confirmed interactively; the hidden TTY-less child
    // must not fall back to printing the usage (environment inheritance is
    // not guaranteed across the UAC boundary).
    params.push(OsString::from("--confirmed"));
    let params = params
        .iter()
        .map(|a| quote_arg(&a.to_string_lossy()))
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
        // WAIT_OBJECT_0 (0) is the only success state for a process wait;
        // WAIT_FAILED (0xFFFFFFFF) and abandoned states must not be treated
        // as a successful elevated run.
        let waited = WaitForSingleObject(info.hProcess, INFINITE);
        if waited != 0 {
            let err = std::io::Error::last_os_error();
            CloseHandle(info.hProcess);
            return Err(anyhow!(
                "failed waiting on the elevated process (WaitForSingleObject returned {}: {})",
                waited,
                err
            ));
        }
        let mut code: u32 = 0;
        if GetExitCodeProcess(info.hProcess, &mut code) == 0 {
            let err = std::io::Error::last_os_error();
            CloseHandle(info.hProcess);
            return Err(anyhow!(
                "failed to read the elevated process exit code: {}",
                err
            ));
        }
        CloseHandle(info.hProcess);
        code
    };

    let output = std::fs::read_to_string(&log_path)
        .unwrap_or_else(|e| format!("<(elevation log {} unreadable: {})", log_path.display(), e));
    if exit_code != 0 {
        // Keep the log file for diagnosis: it holds everything the hidden
        // child printed before failing.
        bail!(
            "elevated command failed (exit code {}); full output kept at {}:\n{}",
            exit_code,
            log_path.display(),
            output.trim()
        );
    }
    let _ = std::fs::remove_file(&log_path);
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

/// Build the icacls invocations that lock down the service config directory,
/// the installed binary and the config file to Administrators + SYSTEM,
/// regardless of who pre-created the directory (the pre-elevation CLI runs
/// as a normal user, leaving CREATOR OWNER full control otherwise).
///
/// Layout: one `/setowner` per path (a user-precreated directory/exe/config
/// keeps its creator as owner, and an NTFS owner implicitly holds WRITE_DAC —
/// enough to silently undo every grant below), then one atomic
/// `/inheritance:r` + explicit grants per path (see `path_lockdown_args` for
/// why there is no `/reset`). SIDs are locale-independent:
/// S-1-5-32-544 = Administrators, S-1-5-18 = SYSTEM, S-1-5-32-545 = Users.
///
/// The config defaults to Administrators full control (elevated edits),
/// SYSTEM read (the service only reads it), and Users read so read-only
/// `status --name` can render the configured ports without UAC. With
/// `allow_user_config` the directory grants Users create-files (atomic write
/// + rename) and the config grants Users modify.
fn lockdown_icacls_args(
    dir: &Path,
    exe: &Path,
    config: &Path,
    allow_user_config: bool,
) -> Vec<Vec<OsString>> {
    let admins_full = OsString::from("*S-1-5-32-544:F");
    let mut cmds: Vec<Vec<OsString>> = Vec::new();

    let system_read = OsString::from("*S-1-5-18:R");
    let users_read = OsString::from("*S-1-5-32-545:R");
    let users_create = OsString::from("*S-1-5-32-545:WD");
    let users_modify = OsString::from("*S-1-5-32-545:M");

    // Directory grants MUST carry (OI)(CI): rewriting the directory DACL
    // (SetNamedSecurityInfo) auto-propagates to the children, and the
    // binary/config are created BEFORE the lockdown with purely inherited
    // ACEs. Non-inheritable directory grants propagate as "remove every
    // inherited ACE" — the children end up with an EMPTY DACL (not even
    // an elevated process can open them; observed on a real machine).
    // Inheritable grants propagate into valid child ACEs instead, and each
    // child is then locked individually below.
    let admins_dir_full = OsString::from("*S-1-5-32-544:(OI)(CI)F");
    let system_dir_full = OsString::from("*S-1-5-18:(OI)(CI)F");
    let users_dir_read = OsString::from("*S-1-5-32-545:(OI)(CI)R");
    let mut dir_grants: Vec<&OsStr> = vec![&admins_dir_full, &system_dir_full, &users_dir_read];
    if allow_user_config {
        // Directory-local only: Users may create files here; the files
        // themselves are owned (and thus writable) by their creator.
        dir_grants.push(&users_create);
    }
    cmds.extend(path_lockdown_args(dir, &dir_grants));

    cmds.extend(exe_lockdown_args(exe));

    let mut config_grants: Vec<&OsStr> = vec![&admins_full, &system_read, &users_read];
    if allow_user_config {
        config_grants.push(&users_modify);
    }
    cmds.extend(path_lockdown_args(config, &config_grants));

    cmds
}

/// The two icacls invocations that lock one path down: take ownership
/// (the pre-creation owner implicitly holds WRITE_DAC and could otherwise
/// silently undo the grants), then — in ONE atomic command — strip the
/// inherited ACEs and grant exactly `grants`.
///
/// Deliberately no `/reset`: resetting the DACL to "inherit from the
/// parent" while the parent directory is already locked (its grants carry
/// no OI/CI flags, nothing is inheritable) leaves an EMPTY DACL as an
/// intermediate state — an empty DACL denies everyone but the owner's
/// implicit READ_CONTROL/WRITE_DAC, so not even an elevated process can
/// read the file. If the grant command then fails, that state is permanent.
/// A single `/inheritance:r + grants` invocation is applied atomically, so
/// a failure leaves the previous (readable) DACL untouched.
fn path_lockdown_args(path: &Path, grants: &[&OsStr]) -> Vec<Vec<OsString>> {
    let mut cmds = vec![vec![
        path.as_os_str().to_os_string(),
        OsString::from("/setowner"),
        OsString::from("*S-1-5-32-544"),
    ]];
    let mut cmd = vec![
        path.as_os_str().to_os_string(),
        OsString::from("/inheritance:r"),
    ];
    for grant in grants {
        cmd.push(OsString::from("/grant"));
        cmd.push(grant.to_os_string());
    }
    cmds.push(cmd);
    cmds
}

/// The binary lockdown, shared by `install` (via `lockdown_icacls_args`)
/// and `upgrade` (re-applied after the binary is replaced: the replacement
/// inherits the temp file's security descriptor, which would leave a
/// user-replaceable LocalSystem service binary behind).
fn exe_lockdown_args(exe: &Path) -> Vec<Vec<OsString>> {
    let admins = OsString::from("*S-1-5-32-544:F");
    let system = OsString::from("*S-1-5-18:F");
    path_lockdown_args(exe, &[&admins, &system])
}

/// Atomically replace `replaced` with `replacement` while KEEPING the
/// security descriptor, owner and attributes of `replaced`.
///
/// `std::fs::rename` (MoveFileEx REPLACE_EXISTING) makes the new file
/// inherit the *replacement's* security descriptor: a config written by an
/// un-elevated CLI would lose the install-time ACL lockdown and hand
/// ownership to the writing user (an NTFS owner implicitly holds
/// WRITE_DAC). `ReplaceFileW` is the Windows API built for exactly this
/// swap — only the content changes.
///
/// When `replaced` does not exist yet (first write) there is nothing to
/// preserve and a plain rename is used. On any other failure the
/// replacement file is removed (it may contain secrets) and the error is
/// returned.
pub fn replace_file_preserving_security(replacement: &Path, replaced: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{
        GetLastError, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND,
    };
    use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;

    fn wide(p: &Path) -> Vec<u16> {
        p.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }
    let replaced_w = wide(replaced);
    let replacement_w = wide(replacement);
    // No library calls between ReplaceFileW and GetLastError — they would
    // clobber the thread's last error.
    let ok = unsafe {
        ReplaceFileW(
            replaced_w.as_ptr(),
            replacement_w.as_ptr(),
            std::ptr::null(),
            0,
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    let err = unsafe { GetLastError() };
    if ok != 0 {
        return Ok(());
    }
    if err == ERROR_FILE_NOT_FOUND || err == ERROR_PATH_NOT_FOUND {
        // First write: no security descriptor to preserve.
        if let Err(e) = std::fs::rename(replacement, replaced) {
            let _ = std::fs::remove_file(replacement);
            return Err(anyhow!(e)).with_context(|| {
                format!(
                    "failed to create {} from {}",
                    replaced.display(),
                    replacement.display()
                )
            });
        }
        return Ok(());
    }
    // On failure the replacement file is in an undefined state (it may
    // already have been consumed); best-effort cleanup — never leave a
    // secret-bearing temp file behind.
    let _ = std::fs::remove_file(replacement);
    bail!(
        "failed to replace {} preserving its security descriptor (winerror {})",
        replaced.display(),
        err
    );
}

/// Run one icacls invocation; failures are fatal: an install that cannot
/// lock down its files must not proceed (LocalSystem escalation risk).
fn run_icacls(args: &[OsString]) -> Result<()> {
    let status = std::process::Command::new("icacls")
        .args(args)
        .status()
        .context("failed to run icacls")?;
    if !status.success() {
        bail!(
            "icacls failed (exit {}) on `{}`",
            status,
            Path::new(&args[0]).display()
        );
    }
    Ok(())
}

/// Create and start a Windows service that runs
/// `"<current_exe>" service run --config "<config_path>"` at boot.
/// Also writes the `version.toml` policy and locks the config directory,
/// binary and config down to Administrators + SYSTEM (plus Users write on
/// the config only when `allow_user_config`).
pub fn install(opts: &ServiceInstallOptions) -> Result<()> {
    let dir = opts
        .config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .to_path_buf();
    if !dir.exists() {
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create config directory {}", dir.display()))?;
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

    // Self-install the binary next to the config so the service keeps a
    // stable path even when the user moves or deletes the original file.
    let exe = std::env::current_exe().context("failed to resolve current executable path")?;
    let exe_name = exe
        .file_name()
        .ok_or_else(|| anyhow!("current executable has no file name"))?;
    let installed_exe = dir.join(exe_name);
    ensure_binary_copy(&exe, &installed_exe)?;

    // Lock down the directory, the service binary and the config BEFORE the
    // service is registered: the directory may have been pre-created by the
    // un-elevated CLI (CREATOR OWNER = the installing user) or `--config`
    // may point at any user-writable directory; either way a LocalSystem
    // service must never run a binary or read secrets from a location a
    // normal user can replace or read.
    for args in lockdown_icacls_args(
        &dir,
        &installed_exe,
        &opts.config_path,
        opts.allow_user_config,
    ) {
        run_icacls(&args)?;
    }

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
            // DELETE is needed for the rollback path when start() fails.
            ServiceAccess::QUERY_STATUS | ServiceAccess::START | ServiceAccess::DELETE,
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
    if let Err(e) = service.start::<&OsStr>(&[]) {
        // Best-effort rollback: leaving the registration behind would make a
        // retry fail with ERROR_SERVICE_EXISTS (half-installed state).
        return match service.delete() {
            Ok(()) => Err(anyhow!(e)).context(format!(
                "failed to start service '{}'; rolled back its SCM registration",
                service_name
            )),
            Err(de) => Err(anyhow!(e)).context(format!(
                "failed to start service '{}' (and failed to roll back the registration: {}); run `rathole-x service uninstall --yes --name {}` before retrying",
                service_name, de, opts.name
            )),
        };
    }

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
/// the installed location). Overwrites a stale copy. The copy lands in a
/// sibling temp file first and is then renamed over `dest`, so a failed copy
/// never leaves a truncated binary behind.
fn ensure_binary_copy(source: &Path, dest: &Path) -> Result<()> {
    // Compare canonical paths so aliases (`dir/./x.exe`, 8.3 names, symlinks
    // to the same file) are also recognized as "same file".
    let same = match (source.canonicalize(), dest.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => source == dest,
    };
    if same {
        return Ok(());
    }
    let file_name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "rathole-x.exe".to_owned());
    let tmp = dest.with_file_name(format!("{}.tmp{}", file_name, std::process::id()));
    let copy_err = match std::fs::copy(source, &tmp) {
        // ReplaceFileW keeps the destination's security descriptor and
        // owner (a plain rename would let the temp file's descriptor win);
        // `upgrade` re-applies the exe lockdown afterwards.
        Ok(_) => match replace_file_preserving_security(&tmp, dest) {
            Ok(()) => return Ok(()),
            Err(e) => e,
        },
        Err(e) => anyhow!(e),
    };
    let _ = std::fs::remove_file(&tmp);
    Err(anyhow!(copy_err)).with_context(|| {
        format!(
            "failed to copy {} to {} (is the service running from {}? run `rathole-x service uninstall --yes` first)",
            source.display(),
            dest.display(),
            dest.display()
        )
    })
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

    let service =
        match &role {
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
                        return Err(anyhow!(e)
                            .context(format!("failed to open service '{}' or '{}'", a, b)))
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
        Err(e) => return Err(anyhow!(e)).context("failed to delete service"),
    }

    remove_service_files(config_path, purge);

    info!("service for '{}' uninstalled", name);
    Ok(())
}

/// Remove the service-managed files. The config file and stale atomic-write
/// temp files only when `purge` is set; version.toml when no other service
/// config remains in the directory; the per-service uninstall helper bat
/// always (the service is gone). Errors are logged, never fatal: the SCM
/// registration is already deleted when this runs.
fn remove_service_files(config_path: &Path, purge: bool) {
    // Bare relative paths (`-c x.toml`) have an empty parent: fall back to
    // the current directory so temp-file cleanup and the version.toml
    // last-service check still run.
    let dir = config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));

    // The per-service uninstall helper bat is useless once the service is
    // gone; delete it rather than leaving a user-modifiable script behind.
    let stem = config_path
        .file_stem()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let bat = dir.join(format!("uninstall-{}.bat", stem));
    match std::fs::remove_file(&bat) {
        Ok(()) => info!("removed helper script {}", bat.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => warn!("failed to remove {}: {}", bat.display(), e),
    }

    if purge {
        match std::fs::remove_file(config_path) {
            Ok(()) => info!("removed config {}", config_path.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => warn!("failed to remove {}: {}", config_path.display(), e),
        }

        // Stale temp files from interrupted atomic writes. write_atomic
        // names them `<stem>.tmp<pid>` (with_extension replaces the `.toml`
        // extension), so match on the file stem, not the full file name.
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with(&format!("{}.tmp", stem)) {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }

    // version.toml goes with the last service: remove it when no OTHER
    // service config remains in the directory.
    let others_remain = std::fs::read_dir(dir)
        .map(|entries| {
            entries.flatten().any(|e| {
                let p = e.path();
                p.file_name() != config_path.file_name()
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

    info!(
        "rathole-x service starting; config: {}",
        config_path.display()
    );

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

/// Relay helper for the un-elevated CLI flows: elevate, wait for the
/// child, then REPLAY its output verbatim. The elevated child prints every
/// user-visible line (including the final success or failure message), so
/// the parent must not invent its own summary on top — a failed child
/// surfaces through the returned error, never as a success line.
fn replay_elevated_output(output: &str) -> Result<()> {
    if !crate::is_json_mode() {
        print!("{}", output);
        return Ok(());
    }
    let mut envelope = None;
    for line in output.lines() {
        match serde_json::from_str::<serde_json::Value>(line) {
            Ok(value) if value.get("ok").is_some() => envelope = Some(line),
            _ => eprintln!("{}", line),
        }
    }
    let envelope = envelope
        .ok_or_else(|| anyhow!("elevated command completed without a JSON result envelope"))?;
    std::env::set_var("RATHOLE_X_JSON_RELAYED", "1");
    ::std::println!("{}", envelope);
    Ok(())
}

fn elevate_and_replay() -> Result<()> {
    let output = relaunch_elevated_wait(&std::env::args_os().skip(1).collect::<Vec<_>>())?;
    replay_elevated_output(&output)
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
        elevate_and_replay()?;
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
        println!(
            "Service '{}' is not installed; removing leftover files without elevation.",
            name
        );
        remove_service_files(config_path, args.purge);
        // Best effort, matching the elevated path: a kept config should stay
        // readable/deletable by the user (fails silently without rights).
        if !args.purge {
            grant_users_modify(config_path);
        }
        if args.purge {
            println!("Done. Leftover files and the config removed.");
        } else {
            println!("Done. Leftover files removed; config kept.");
        }
        return Ok(());
    }

    if !is_elevated() {
        println!("Requesting administrator rights (UAC)...");
        elevate_and_replay()?;
        return Ok(());
    }

    uninstall(config_path, args.purge)?;
    // Leave the kept config user-deletable after a normal uninstall. The
    // helper bat was already deleted by remove_service_files: granting
    // Users modify on it would leave a "run as administrator" script a
    // standard user could rewrite.
    if !args.purge {
        grant_users_modify(config_path);
    }
    println!("Service '{}' stopped and removed", name);
    if args.purge {
        println!("  Config:      removed ({})", config_path.display());
    } else {
        println!(
            "  Config:      kept (user-deletable) ({})",
            config_path.display()
        );
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
        elevate_and_replay()?;
        return Ok(());
    }

    let dir = crate::config_edit::config_dir();
    let mut removed = 0usize;
    for (name, _role) in &services {
        let path = dir.join(format!("{}.toml", name));
        // The role parsed from the config may be stale (role section edited
        // by hand after install): check BOTH role variants, otherwise the
        // old-role service would be orphaned while its config is deleted.
        let exists = service_exists(&scm_name(crate::config_edit::ServiceRole::Client, name))
            || service_exists(&scm_name(crate::config_edit::ServiceRole::Server, name));
        if exists {
            // uninstall() itself also falls back to the other role when the
            // primary SCM name does not open.
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
    println!(
        "Removed {} service(s) and the shared files in {}.",
        removed,
        dir.display()
    );
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
        let path = crate::config_edit::resolve_service_config(None, args.name.as_deref())?;
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
        elevate_and_replay()?;
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
        elevate_and_replay()?;
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

    // Replace the shared binary. On failure, best-effort restart the
    // services we just stopped before returning the error, so a failed
    // upgrade never leaves every service down.
    let exe = std::env::current_exe().context("failed to resolve current executable path")?;
    let exe_name = exe
        .file_name()
        .ok_or_else(|| anyhow!("current executable has no file name"))?;
    let installed = crate::config_edit::config_dir().join(exe_name);
    let update_result = (|| -> Result<()> {
        ensure_binary_copy(&exe, &installed)?;
        // The replacement inherits the temp file's security descriptor;
        // re-apply the binary lockdown so an upgrade can never leave a
        // user-replaceable LocalSystem service binary behind.
        for args in exe_lockdown_args(&installed) {
            run_icacls(&args)?;
        }
        Ok(())
    })();
    if let Err(e) = update_result {
        warn!(
            "binary update failed: {:#}; restarting services best-effort",
            e
        );
        let restarted = start_services_best_effort(&manager, &services);
        warn!("restarted {} service(s) after the failed update", restarted);
        return Err(e);
    }

    // Start everything again.
    let started = start_services_best_effort(&manager, &services);

    println!(
        "Updated {} and restarted {} service(s).",
        installed.display(),
        started
    );
    Ok(())
}

/// Start every listed service, logging failures without stopping. Returns
/// the number of services successfully started.
fn start_services_best_effort(
    manager: &ServiceManager,
    services: &[(String, crate::config_edit::ServiceRole)],
) -> usize {
    let mut started = 0usize;
    for (name, role) in services {
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
    started
}

/// Whether a named SCM service exists (read-only query, no admin needed).
fn service_exists(service_name: &str) -> bool {
    let Ok(manager) = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
    else {
        return false;
    };
    manager
        .open_service(service_name, ServiceAccess::QUERY_STATUS)
        .is_ok()
}

/// Stop a service and wait until it reports Stopped (bounded).
fn stop_and_wait(service: &windows_service::service::Service, name: &str) -> Result<()> {
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
            bail!(
                "service '{}' did not stop within {:?}",
                name,
                STOP_WAIT_TIMEOUT
            );
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
        Ok(s) => warn!(
            "icacls failed granting Users modify on {} (exit {})",
            path.display(),
            s
        ),
        Err(e) => warn!("failed to run icacls on {}: {}", path.display(), e),
    }
}

/// Elevate (UAC) before the caller writes `path`, when its auth policy
/// forbids user writes. Returns `true` when the elevated child already did
/// the work (its output is replayed here) and the caller must exit without
/// doing it again.
pub fn elevate_for_config_if_needed(path: &Path) -> Result<bool> {
    if !crate::config_edit::writable_by_current_user(path) && !is_elevated() {
        if crate::is_json_mode() {
            eprintln!("Requesting administrator rights (UAC) to modify the service config...");
        } else {
            println!("Requesting administrator rights (UAC) to modify the service config...");
        }
        let output = relaunch_elevated_wait(&std::env::args_os().skip(1).collect::<Vec<_>>())?;
        replay_elevated_output(&output)?;
        return Ok(true);
    }
    Ok(false)
}

/// Redirect stdout and stderr of this process to `path`. Must run before any
/// output happens: Rust's stdio fetches the OS handle lazily on first use.
///
/// The file is opened with `create_new` semantics so a pre-planted file or
/// reparse point at the (predictable-ish) relay path is never followed; on a
/// collision a fresh random suffix is tried. When every attempt fails the
/// process keeps its original stdio (the parent then just replays nothing).
pub fn redirect_stdio_to_file(path: &Path) {
    use std::io::Write;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Console::{SetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE};

    let mut candidate = path.to_path_buf();
    let file = 'attempts: {
        for attempt in 0..8u32 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&candidate)
            {
                Ok(f) => break 'attempts f,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let nanos = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_nanos())
                        .unwrap_or(0);
                    let stem = candidate
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "rathole-x-elev".to_owned());
                    candidate =
                        candidate.with_file_name(format!("{}-{}-{}.log", stem, nanos, attempt));
                }
                Err(_) => return,
            }
        }
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
/// The local status pipe has no command surface: every authenticated caller
/// can request exactly one bounded snapshot. `AU` covers local authenticated
/// logons; the pipe also grants Administrators and LocalSystem. Remote
/// clients are rejected separately by the pipe creation flag.
const STATUS_PIPE_SDDL: &str = "D:P(A;;GRGW;;;AU)(A;;GA;;;BA)(A;;GA;;;SY)";

fn create_status_pipe(endpoint: &str, first_instance: bool) -> Result<NamedPipeServer> {
    let pipe_name: Vec<u16> = OsStr::new(endpoint).encode_wide().chain(Some(0)).collect();
    let mut descriptor = std::ptr::null_mut();
    // SAFETY: both the pipe name and SDDL are nul-terminated. Windows copies
    // the descriptor while creating the pipe, so it is released immediately
    // after CreateNamedPipeW returns.
    let sddl: Vec<u16> = STATUS_PIPE_SDDL.encode_utf16().chain(Some(0)).collect();
    let descriptor_ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    };
    if descriptor_ok == 0 {
        return Err(std::io::Error::last_os_error()).context("failed to build status pipe ACL");
    }
    let security = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    let access = PIPE_ACCESS_DUPLEX
        | FILE_FLAG_OVERLAPPED
        | if first_instance {
            FILE_FLAG_FIRST_PIPE_INSTANCE
        } else {
            0
        };
    let mode = PIPE_TYPE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS;
    let handle = unsafe {
        CreateNamedPipeW(
            pipe_name.as_ptr(),
            access,
            mode,
            PIPE_UNLIMITED_INSTANCES,
            MAX_STATUS_RESPONSE as u32 + 4,
            STATUS_REQUEST.len() as u32,
            0,
            &security,
        )
    };
    // SAFETY: descriptor was allocated by ConvertStringSecurityDescriptor...
    unsafe { LocalFree(descriptor) };
    if handle == -1isize as HANDLE {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("failed to create runtime status endpoint {}", endpoint));
    }
    // SAFETY: CreateNamedPipeW returned an owned, overlapped named-pipe
    // handle; Tokio takes ownership and closes it on drop.
    unsafe { NamedPipeServer::from_raw_handle(handle as RawHandle) }
        .with_context(|| format!("failed to adopt runtime status endpoint {}", endpoint))
}

async fn answer_status_request(
    mut pipe: tokio::net::windows::named_pipe::NamedPipeServer,
    registry: RuntimeRegistry,
) {
    let mut request = [0u8; STATUS_REQUEST.len()];
    if pipe.read_exact(&mut request).await.is_err() || request != STATUS_REQUEST {
        return;
    }
    let response = match serde_json::to_vec(&snapshot(&registry)) {
        Ok(response) if response.len() <= MAX_STATUS_RESPONSE => response,
        Ok(_) => {
            error!("runtime status snapshot exceeds its bounded response size");
            return;
        }
        Err(error) => {
            error!("failed to serialize runtime status snapshot: {}", error);
            return;
        }
    };
    let len = (response.len() as u32).to_le_bytes();
    if pipe.write_all(&len).await.is_ok() {
        let _ = pipe.write_all(&response).await;
    }
}

/// Start the ACL-protected, local-only status endpoint for this runtime
/// generation. Failure to expose status never interrupts proxy traffic.
pub fn spawn_runtime_status_server(
    config_path: PathBuf,
    registry: RuntimeRegistry,
    mut shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()> {
    let endpoint = endpoint_for_config(&config_path);
    let initial = create_status_pipe(&endpoint, true)?;
    tokio::spawn(async move {
        let mut listener = initial;
        loop {
            tokio::select! {
                result = listener.connect() => {
                    if let Err(error) = result {
                        warn!("runtime status pipe connection failed: {}", error);
                        break;
                    }
                    let connected = listener;
                    match create_status_pipe(&endpoint, false) {
                        Ok(next) => listener = next,
                        Err(error) => {
                            error!("runtime status pipe cannot accept another request: {:#}", error);
                            break;
                        }
                    }
                    tokio::spawn(answer_status_request(connected, registry.clone()));
                }
                _ = shutdown_rx.recv() => break,
            }
        }
    });
    Ok(())
}

/// Query the fixed local pipe verb. A missing/stopped endpoint is returned as
/// an error so `status` can render `runtime: null` without failing the static
/// SCM/config query.
pub fn query_runtime_status(config_path: &Path) -> Result<RuntimeSnapshot> {
    let endpoint = endpoint_for_config(config_path);
    let mut pipe = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&endpoint)
        .with_context(|| format!("runtime status unavailable at {}", endpoint))?;
    pipe.write_all(STATUS_REQUEST)
        .context("failed to request runtime status")?;
    pipe.flush()
        .context("failed to flush runtime status request")?;
    let mut length = [0u8; 4];
    pipe.read_exact(&mut length)
        .context("failed to read runtime status response length")?;
    let length = u32::from_le_bytes(length) as usize;
    if length > MAX_STATUS_RESPONSE {
        bail!("runtime status response exceeds the maximum size");
    }
    let mut response = vec![0; length];
    pipe.read_exact(&mut response)
        .context("failed to read runtime status response")?;
    serde_json::from_slice(&response).context("invalid runtime status response")
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

        write_uninstall_bat(
            &exe,
            &config,
            "relay",
            crate::config_edit::ServiceRole::Server,
        );

        let bat = std::fs::read_to_string(dir.join("uninstall-relay.bat")).unwrap();
        assert!(bat.contains("service uninstall --yes --name \"relay\""));
        assert!(bat.contains("--config \"%~dp0cfg.toml\""));
        assert!(bat.contains("--purge"), "purge hint documented in the bat");
        assert!(bat.contains("rathole-x.exe"));
        assert!(
            bat.contains("rathole-x-server-relay"),
            "SCM name in the bat"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    fn setup(dir: &Path) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let config = dir.join("config.toml");
        let auth = dir.join("version.toml");
        // write_atomic names its temp file `<stem>.tmp<pid>` (with_extension
        // replaces the `.toml` extension); anything else is a fake name the
        // cleanup must not rely on.
        let tmp = dir.join("config.tmp4242");
        // A name write_atomic never produces: must NOT be matched.
        let decoy = dir.join("config.toml.tmp4242");
        std::fs::write(&config, "x").unwrap();
        std::fs::write(&auth, "y").unwrap();
        std::fs::write(&tmp, "z").unwrap();
        std::fs::write(&decoy, "z").unwrap();
        config
    }

    #[test]
    fn uninstall_keeps_config_removes_auth() {
        let dir = std::env::temp_dir().join("rathole-x-uninstall-keep");
        let _ = std::fs::remove_dir_all(&dir);
        let config = setup(&dir);

        remove_service_files(&config, false);

        assert!(config.exists(), "config kept without --purge");
        assert!(
            !dir.join("version.toml").exists(),
            "version.toml always removed"
        );
        assert!(
            dir.join("config.tmp4242").exists(),
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
            !dir.join("config.tmp4242").exists(),
            "stale write_atomic tmp files removed with --purge"
        );
        assert!(
            dir.join("config.toml.tmp4242").exists(),
            "names write_atomic never produces are left alone"
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

    #[test]
    fn uninstall_removes_helper_bat_even_when_keeping_config() {
        let dir = std::env::temp_dir().join("rathole-x-uninstall-bat");
        let _ = std::fs::remove_dir_all(&dir);
        let config = setup(&dir);
        let bat = dir.join("uninstall-config.bat");
        std::fs::write(&bat, "@echo off").unwrap();

        remove_service_files(&config, false);

        assert!(!bat.exists(), "helper bat removed with the service");
        assert!(config.exists(), "config kept without --purge");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn binary_copy_same_file_via_alias_is_noop() {
        let dir = std::env::temp_dir().join("rathole-x-bin-alias");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("x.exe");
        std::fs::write(&f, b"x").unwrap();

        // Same file spelled with a `.` component: must be recognized as
        // identical (canonicalized comparison) and left untouched.
        let alias = dir.join(".").join("x.exe");
        ensure_binary_copy(&alias, &f).unwrap();
        assert_eq!(std::fs::read(&f).unwrap(), b"x");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn strip_relay_flags_removes_stale_values() {
        let args: Vec<OsString> = [
            "service",
            "uninstall",
            "--elevated-log",
            r"C:\t\old.log",
            "--confirmed",
            "--yes",
        ]
        .iter()
        .map(OsString::from)
        .collect();
        let stripped = strip_relay_flags(&args);
        let plain: Vec<String> = stripped
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(plain, ["service", "uninstall", "--yes"]);

        // A trailing flag without a value is dropped too.
        let args: Vec<OsString> = ["status", "--elevated-log"]
            .iter()
            .map(OsString::from)
            .collect();
        assert_eq!(strip_relay_flags(&args).len(), 1);

        // No relay flag: unchanged.
        let args: Vec<OsString> = ["status", "--json"].iter().map(OsString::from).collect();
        assert_eq!(strip_relay_flags(&args).len(), 2);
    }

    /// Concatenate one icacls invocation's arguments (after the target
    /// path) into a single string for exact-match assertions.
    fn cmd_string(cmd: &[OsString]) -> String {
        cmd.iter()
            .skip(1)
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn replace_file_first_write_falls_back_to_rename() {
        let dir = std::env::temp_dir().join("rathole-x-replace-first");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("cfg.toml");
        let tmp = dir.join("cfg.toml.tmp1");
        std::fs::write(&tmp, "v1").unwrap();

        replace_file_preserving_security(&tmp, &target).unwrap();

        assert_eq!(std::fs::read_to_string(&target).unwrap(), "v1");
        assert!(!tmp.exists(), "first-write fallback consumes the temp file");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn replace_file_preserves_security_descriptor() {
        // Audit lesson: string-level unit tests cannot validate external
        // semantics. This test performs a REAL icacls lockdown, a REAL
        // replacement and compares the ACL text before and after — exactly
        // the semantics `write_atomic` depends on.
        let dir = std::env::temp_dir().join("rathole-x-replace-acl");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("cfg.toml");
        std::fs::write(&target, "v1").unwrap();

        // Install-time lockdown shape for the config DACL under
        // --allow-user-config: inheritance off, Administrators full,
        // SYSTEM read, Users read+modify — the exact scenario the preserved
        // descriptor must survive (an un-elevated user CLI writes the
        // config; the watcher service keeps reading it as SYSTEM).
        // (The /setowner step of the real lockdown is skipped: setting the
        // owner to Administrators needs an elevated token; the preserved
        // DACL is what makes this test's comparison meaningful.)
        for args in [vec![
            "/inheritance:r",
            "/grant",
            "*S-1-5-32-544:F",
            "/grant",
            "*S-1-5-18:R",
            "/grant",
            "*S-1-5-32-545:R",
            "/grant",
            "*S-1-5-32-545:M",
        ]] {
            let mut cmd = std::process::Command::new("icacls");
            cmd.arg(&target);
            for a in args {
                cmd.arg(a);
            }
            let st = cmd.status().unwrap();
            assert!(
                st.success(),
                "icacls {:?} failed with {} — this test requires a writable DACL on its own file",
                target,
                st
            );
        }

        let acl_text = |p: &Path| -> String {
            let out = std::process::Command::new("icacls")
                .arg(p)
                .output()
                .expect("failed to run icacls");
            assert!(out.status.success(), "icacls {:?} listing failed", p);
            String::from_utf8_lossy(&out.stdout).into_owned()
        };
        let before = acl_text(&target);
        assert!(
            !before.contains("(I)"),
            "lockdown must drop inherited ACEs so the comparison is meaningful"
        );

        // The swap a config write performs.
        let tmp = dir.join("cfg.toml.tmp1");
        std::fs::write(&tmp, "v2").unwrap();
        replace_file_preserving_security(&tmp, &target).unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "v2");
        assert!(!tmp.exists());

        // The security descriptor (DACL and owner) must be byte-identical
        // to what the lockdown installed.
        let after = acl_text(&target);
        assert_eq!(
            before, after,
            "replacement must keep the target's security descriptor"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn lockdown_args_restrict_dir_exe_and_config() {
        let dir = Path::new(r"C:\cfg");
        let exe = Path::new(r"C:\cfg\rathole-x.exe");
        let config = Path::new(r"C:\cfg\config.toml");
        let cmds = lockdown_icacls_args(dir, exe, config, false);

        // Two invocations per path: /setowner, then one ATOMIC
        // /inheritance:r + grants (no /reset — see path_lockdown_args).
        assert_eq!(cmds.len(), 6);
        // Ownership is taken first so the pre-creation owner cannot
        // rewrite the new DACL.
        assert_eq!(cmds[0][0].as_os_str(), dir.as_os_str());
        assert_eq!(cmd_string(&cmds[0]), "/setowner *S-1-5-32-544");
        assert_eq!(cmds[1][0].as_os_str(), dir.as_os_str());
        assert_eq!(
            cmd_string(&cmds[1]),
            "/inheritance:r /grant *S-1-5-32-544:(OI)(CI)F /grant *S-1-5-18:(OI)(CI)F /grant *S-1-5-32-545:(OI)(CI)R"
        );
        assert_eq!(cmds[2][0].as_os_str(), exe.as_os_str());
        assert_eq!(cmd_string(&cmds[2]), "/setowner *S-1-5-32-544");
        assert_eq!(cmds[3][0].as_os_str(), exe.as_os_str());
        assert_eq!(
            cmd_string(&cmds[3]),
            "/inheritance:r /grant *S-1-5-32-544:F /grant *S-1-5-18:F"
        );
        assert_eq!(cmds[4][0].as_os_str(), config.as_os_str());
        assert_eq!(cmd_string(&cmds[4]), "/setowner *S-1-5-32-544");
        // Config: read-only `status` works without UAC, but only
        // Administrators may edit by default.
        assert_eq!(
            cmd_string(&cmds[5]),
            "/inheritance:r /grant *S-1-5-32-544:F /grant *S-1-5-18:R /grant *S-1-5-32-545:R"
        );
        for cmd in [&cmds[2], &cmds[3], &cmds[4], &cmds[5]] {
            let s = cmd_string(cmd);
            assert!(
                !s.contains("S-1-5-32-545:W") && !s.contains("S-1-5-32-545:M"),
                "no Users write grants without --allow-user-config: {}",
                s
            );
        }
        assert!(
            !cmd_string(&cmds[3]).contains("S-1-5-32-545"),
            "the installed binary is never user-readable/writable"
        );
    }

    #[test]
    fn lockdown_args_allow_user_config_grants_write_only() {
        let dir = Path::new(r"C:\cfg");
        let exe = Path::new(r"C:\cfg\rathole-x.exe");
        let config = Path::new(r"C:\cfg\config.toml");
        let cmds = lockdown_icacls_args(dir, exe, config, true);

        assert_eq!(cmds.len(), 6);
        // Directory: grants are inheritable so the DACL rewrite propagates
        // as valid child ACEs instead of stripping the children to an
        // empty DACL; Users may read and create files (atomic write + rename)...
        assert_eq!(
            cmd_string(&cmds[1]),
            "/inheritance:r /grant *S-1-5-32-544:(OI)(CI)F /grant *S-1-5-18:(OI)(CI)F /grant *S-1-5-32-545:(OI)(CI)R /grant *S-1-5-32-545:WD"
        );
        // ...the binary is never user-writable (LocalSystem escalation)...
        assert_eq!(
            cmd_string(&cmds[3]),
            "/inheritance:r /grant *S-1-5-32-544:F /grant *S-1-5-18:F"
        );
        // ...and the config gets Users read+modify.
        assert_eq!(
            cmd_string(&cmds[5]),
            "/inheritance:r /grant *S-1-5-32-544:F /grant *S-1-5-18:R /grant *S-1-5-32-545:R /grant *S-1-5-32-545:M"
        );
    }
    #[tokio::test]
    async fn runtime_status_pipe_round_trip_and_missing_endpoint() {
        let dir = std::env::temp_dir().join(format!(
            "rathole-x-status-pipe-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let config = dir.join("service.toml");
        std::fs::write(&config, "[client]\nremote_addr = \"127.0.0.1:1\"\n").unwrap();
        assert!(query_runtime_status(&config).is_err());

        let registry =
            crate::runtime_status::client_registry("127.0.0.1:2333", ["demo".to_owned()]);
        let (shutdown_tx, shutdown_rx) = broadcast::channel(1);
        spawn_runtime_status_server(config.clone(), registry, shutdown_rx).unwrap();
        let queried_config = config.clone();
        let snapshot = tokio::task::spawn_blocking(move || query_runtime_status(&queried_config))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.services.len(), 1);
        let _ = shutdown_tx.send(true);
        std::fs::remove_dir_all(&dir).ok();
    }
}

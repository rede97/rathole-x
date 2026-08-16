//! OpenRC backend for the Linux service integration (Alpine, Gentoo and
//! other OpenRC distributions).
//!
//! Compiled only on Linux and reachable through the `linux` facade, which
//! detects the init system. An installed service is an openrc-run script in
//! /etc/init.d managed with `rc-update`/`rc-service`; supervise-daemon
//! provides the crash respawn (the counterpart of systemd's
//! `Restart=on-failure`/`RestartSec=3`). Classic SysV initd stays
//! unsupported: OpenRC is the only non-systemd backend.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};
use tracing::warn;

use crate::cli::{InstallArgs, UninstallArgs};
use crate::config_edit::ServiceRole;

/// Directory holding the generated openrc-run scripts.
const INITD_DIR: &str = "/etc/init.d";

/// Where supervise-daemon pidfiles live.
const RUN_DIR: &str = "/run";

/// Where supervise-daemon writes the relay's stdout/stderr.
const LOG_DIR: &str = "/var/log";

// ---------------------------------------------------------------------------
// Service helpers
// ---------------------------------------------------------------------------

/// The OpenRC service name (init.d script name) of an installed service.
pub(crate) fn service_name(role: ServiceRole, name: &str) -> String {
    format!("rathole-x-{}-{}", role.key(), name)
}

fn initd_path(svc: &str) -> PathBuf {
    Path::new(INITD_DIR).join(svc)
}

fn pidfile_path(svc: &str) -> PathBuf {
    Path::new(RUN_DIR).join(format!("{}.pid", svc))
}

/// The supervise-daemon output/error log of an installed service. Embedded
/// in the init.d script and pre-created at install time.
fn log_path(svc: &str) -> PathBuf {
    Path::new(LOG_DIR).join(format!("{}.log", svc))
}

/// Run a tool (`rc-update`, `rc-service`, `chown`, …), passing stderr
/// through verbatim on failure.
fn run_rc(tool: &str, args: &[&str]) -> Result<()> {
    let output = Command::new(tool)
        .args(args)
        .output()
        .with_context(|| format!("failed to run `{} {}`", tool, args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "`{} {}` failed:\n{}",
            tool,
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Best-effort rc tool: reports success instead of failing.
fn try_rc(tool: &str, args: &[&str]) -> bool {
    Command::new(tool)
        .args(args)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Pre-create the service log for the unprivileged service account.
///
/// OpenRC redirects the child's stdout/stderr to output_log/error_log, but
/// the redirect happens as the service user: /var/log is root:root 0755, so
/// the rathole-x account cannot create the file and the freshly spawned
/// relay dies immediately (supervise-daemon then respawns it forever).
/// Creating the log here, owned by the service account, keeps the relay
/// alive. Idempotent: an existing log (reinstall, rotation) is re-owned and
/// re-permissioned, never truncated. Only called as root during install,
/// after `ensure_service_account`.
fn ensure_log_file(svc: &str) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::fs::PermissionsExt;
    let path = log_path(svc);
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o640)
        .open(&path)
        .with_context(|| format!("failed to create log file {}", path.display()))?;
    // An existing file keeps its mode; enforce 0640 either way.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640))
        .with_context(|| format!("failed to chmod {}", path.display()))?;
    let owner = format!("{}:{}", super::SERVICE_ACCOUNT, super::SERVICE_GROUP);
    let value = path.display().to_string();
    run_rc("chown", &[owner.as_str(), value.as_str()])
}

/// OpenRC refuses to manage services when it did not boot the system
/// (containers, rescue environments). Initializing the default runlevel
/// creates the runtime state that `rc-service` needs; on a real OpenRC host
/// `softlevel` is already present and this is a no-op. Only called as root
/// during install.
fn ensure_openrc_runtime() {
    let run = Path::new("/run/openrc");
    if run.join("softlevel").exists() {
        return;
    }
    if !run.exists() {
        let _ = std::fs::create_dir(run);
    }
    let _ = std::fs::write(run.join("softlevel"), b"default");
    // In a non-booted container, softlevel alone is insufficient on modern
    // OpenRC: `openrc default` also initializes service-state directories.
    // It may report unrelated container cgroup warnings, so the subsequent
    // rc-service invocation remains the authoritative install result.
    let _ = Command::new("openrc").arg("default").status();
}

/// Quote a path for embedding in shell-assignments/args of an init.d
/// script. Values that cannot be quoted safely are rejected instead of
/// writing a broken script.
fn shell_quote(path: &Path) -> Result<String> {
    let s = path
        .to_str()
        .ok_or_else(|| anyhow!("path {} is not valid UTF-8", path.display()))?;
    if s.contains('\'') || s.contains('\\') || s.contains('"') {
        bail!(
            "cannot embed path `{}` in an init.d script; move the file to a \
             path without quotes or backslashes",
            s
        );
    }
    if s.chars().any(char::is_whitespace) {
        Ok(format!("'{}'", s))
    } else {
        Ok(s.to_owned())
    }
}

/// libcap IAB tuple passed to supervise-daemon's `--capabilities`.
/// `^cap_net_bind_service` keeps CAP_NET_BIND_SERVICE inheritable+ambient;
/// every `!cap_*` entry drops that capability from the bounding set.
/// libcap's IAB parser (verified 2.78) has no "all" aggregate — `!all`,
/// `all=`, `-all`, `none` are all rejected — so the drops are enumerated
/// explicitly. Verified on Alpine (libcap 2.78, openrc 0.63.2): the
/// daemon ends up with exactly 0x400 (CAP_NET_BIND_SERVICE) in
/// CapInh/CapPrm/CapBnd/CapAmb.
const RESTRICTED_IAB: &str = "!cap_chown,!cap_dac_override,!cap_dac_read_search,\
!cap_fowner,!cap_fsetid,!cap_kill,!cap_setgid,!cap_setuid,!cap_setpcap,\
!cap_linux_immutable,!cap_net_broadcast,!cap_net_admin,!cap_net_raw,!cap_ipc_lock,\
!cap_ipc_owner,!cap_sys_module,!cap_sys_rawio,!cap_sys_chroot,!cap_sys_ptrace,\
!cap_sys_pacct,!cap_sys_admin,!cap_sys_boot,!cap_sys_nice,!cap_sys_resource,\
!cap_sys_time,!cap_sys_tty_config,!cap_mknod,!cap_lease,!cap_audit_write,\
!cap_audit_control,!cap_setfcap,!cap_mac_override,!cap_mac_admin,!cap_syslog,\
!cap_wake_alarm,!cap_block_suspend,!cap_audit_read,!cap_perfmon,!cap_bpf,\
!cap_checkpoint_restore,^cap_net_bind_service";

/// Render the openrc-run script contents. Pure function for testability.
///
/// OpenRC forwards `command_args_foreground` to the daemon when
/// `supervisor=supervise-daemon`; `command_args` is for start-stop-daemon
/// and would silently omit the daemon's `run --config` arguments.
fn render_initd(exe: &Path, config: &Path, svc: &str) -> Result<String> {
    Ok(format!(
        r#"#!/sbin/openrc-run

description="rathole-x reverse proxy service (single role)"
command={exe}
command_args_foreground="run --config {config}"
# Bare user name only: openrc-run expands `$command_user` unquoted and
# supervise-daemon takes --user as ONE argument — a "user:group" value is
# split and the group token becomes the daemon's command. `-u` already
# applies the account's primary group (rathole-x).
command_user="{user}"
# libcap IAB text for supervise-daemon (see RESTRICTED_IAB): every
# capability except net_bind_service is dropped from the bounding set.
capabilities="{iab}"
supervisor=supervise-daemon
pidfile="/run/{svc}.pid"
respawn_delay=3
output_log="{log}"
error_log="{log}"
rc_ulimit="-n 1048576"

depend() {{
	# rathole retries connections itself, so net is not a hard need;
	# this also lets the service start in containers without the net
	# service running.
	after net firewall
}}
"#,
        exe = shell_quote(exe)?,
        config = shell_quote(config)?,
        svc = svc,
        log = log_path(svc).display(),
        user = super::SERVICE_ACCOUNT,
        iab = RESTRICTED_IAB,
    ))
}

/// Find an existing init.d script for `name` under either role. The role
/// stored in the config may be stale (hand-edited after install).
fn existing_service(name: &str) -> Option<String> {
    for role in [ServiceRole::Client, ServiceRole::Server] {
        let svc = service_name(role, name);
        if initd_path(&svc).exists() {
            return Some(svc);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// CLI flows
// ---------------------------------------------------------------------------

/// CLI flow for `service install`: write a restricted init.d script and start it.
pub(crate) fn install_service(
    args: &InstallArgs,
    role: ServiceRole,
    name: &str,
    config_path: &Path,
) -> Result<()> {
    if args.allow_user_config {
        bail!("--allow-user-config is unsafe for Linux managed services and is not supported");
    }
    let svc = service_name(role, name);
    println!("Installing {} service '{}':", role.key(), name);
    println!("  Service:     {}", svc);
    println!("  Config:      {}", config_path.display());
    println!("  Access:      root-managed config; edits require sudo");
    println!(
        "  Runtime:     {} (CAP_NET_BIND_SERVICE only)",
        super::SERVICE_ACCOUNT
    );
    println!("  Auto start:  enabled (default runlevel)");

    super::ensure_root()?;
    if initd_path(&svc).exists() {
        bail!(
            "service '{}' already exists; run `rathole-x service uninstall --yes --name {}` first",
            svc,
            name
        );
    }

    super::ensure_service_account()?;
    super::write_version_stamp(config_path)?;
    super::secure_managed_config(&crate::config_edit::version_path(config_path))?;
    super::secure_managed_config(config_path)?;
    ensure_openrc_runtime();

    let exe = super::deploy_binary()?;
    let content = render_initd(&exe, config_path, &svc)?;
    let spath = initd_path(&svc);
    std::fs::write(&spath, content)
        .with_context(|| format!("failed to write init.d script {}", spath.display()))?;
    set_executable(&spath)?;
    ensure_log_file(&svc)?;

    run_rc("rc-update", &["add", svc.as_str(), "default"])?;
    run_rc("rc-service", &[svc.as_str(), "start"])?;

    println!("Service '{}' installed and started", svc);
    println!(
        "Run `rathole-x status --name {}` to inspect, `rathole-x config add --name {} ...` to configure (hot-reloaded).",
        name, name
    );
    Ok(())
}

fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("failed to chmod {}", path.display()))
}

/// CLI flow for `service uninstall`: stop the service, remove it from the
/// runlevels, and delete the init.d script. The config remains protected unless
/// `--purge` removes it. When the service is already gone the leftover files
/// are removed without root, mirroring Windows.
pub(crate) fn uninstall_service(args: &UninstallArgs, config_path: &Path) -> Result<()> {
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

    match existing_service(&name) {
        Some(svc) => {
            super::ensure_root()?;
            // Best-effort: a stopped/crashed service must not block removal.
            let _ = try_rc("rc-service", &[svc.as_str(), "stop"]);
            let _ = try_rc("rc-update", &["delete", svc.as_str()]);
            let spath = initd_path(&svc);
            std::fs::remove_file(&spath)
                .with_context(|| format!("failed to remove init.d script {}", spath.display()))?;
            let _ = std::fs::remove_file(pidfile_path(&svc));
            let _ = std::fs::remove_file(log_path(&svc));
        }
        None => {
            println!(
                "Service '{}' is not installed; removing leftover files without root.",
                name
            );
        }
    }

    super::remove_service_files(config_path, args.purge);

    println!("Service '{}' stopped and removed", name);
    println!(
        "  Config:      {} ({})",
        if args.purge {
            "removed"
        } else {
            "kept protected"
        },
        config_path.display()
    );
    Ok(())
}

/// Drive one service's OpenRC state. Resolves the service with a both-roles
/// fallback (the config's role may be stale) and returns its name for the
/// caller's per-service output.
pub(crate) fn control_one(action: &str, name: &str, role: ServiceRole) -> Result<String> {
    let svc = existing_service(name).unwrap_or_else(|| service_name(role, name));
    run_rc("rc-service", &[svc.as_str(), action])
        .with_context(|| format!("failed to {} service '{}'", action, svc))?;
    Ok(svc)
}

/// `rathole-x upgrade`: stop every service, replace the protected deployed
/// binary, rewrite each init.d script to that path, then start everything.
pub(crate) fn upgrade_binary() -> Result<()> {
    let services = crate::config_edit::list_installed_services()?;
    if services.is_empty() {
        println!("No installed services; nothing to upgrade.");
        return Ok(());
    }
    super::ensure_root()?;
    ensure_openrc_runtime();

    for (name, role) in &services {
        let svc = existing_service(name).unwrap_or_else(|| service_name(*role, name));
        if !try_rc("rc-service", &[svc.as_str(), "stop"]) {
            warn!("failed to stop '{}'", svc);
        }
    }

    let installed = super::deploy_binary()?;
    for (name, role) in &services {
        let svc = existing_service(name).unwrap_or_else(|| service_name(*role, name));
        let script = initd_path(&svc);
        if !script.exists() {
            warn!("skipping missing init.d script '{}'", script.display());
            continue;
        }
        let config = crate::config_edit::config_dir().join(format!("{}.toml", name));
        let content = render_initd(&installed, &config, &svc)?;
        std::fs::write(&script, content)
            .with_context(|| format!("failed to rewrite init.d script {}", script.display()))?;
        set_executable(&script)?;
    }

    let started = start_services_best_effort(&services);
    println!(
        "Updated {} and restarted {} service(s).",
        installed.display(),
        started
    );
    Ok(())
}

/// Start every listed service, logging failures without stopping. Returns
/// the number of services successfully started.
fn start_services_best_effort(services: &[(String, ServiceRole)]) -> usize {
    let mut started = 0usize;
    for (name, role) in services {
        let svc = existing_service(name).unwrap_or_else(|| service_name(*role, name));
        if try_rc("rc-service", &[svc.as_str(), "start"]) {
            started += 1;
        } else {
            warn!("failed to start '{}'", svc);
        }
    }
    started
}

// ---------------------------------------------------------------------------
// Status queries
// ---------------------------------------------------------------------------

/// Query one installed service's OpenRC state. Only services installed by
/// `service install` (script under /etc/init.d) are reported; anything else
/// renders as "not installed". rc-service status needs no root.
pub(crate) fn query_named_service(role: &str, name: &str) -> Option<(String, Option<u32>)> {
    let svc = format!("rathole-x-{}-{}", role, name);
    if !initd_path(&svc).exists() {
        return None;
    }

    let out = Command::new("rc-service")
        .args([&svc, "status"])
        .output()
        .ok()?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // rc-service prints e.g. " * status: started"; check the more specific
    // words first ("stopped" does not contain "started" but be explicit).
    let state = if text.contains("crashed") {
        "Failed"
    } else if text.contains("started") {
        "Running"
    } else if text.contains("stopped") {
        "Stopped"
    } else {
        return None;
    };

    let pid = std::fs::read_to_string(pidfile_path(&svc))
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .filter(|p| *p != 0);
    Some((state.to_owned(), pid))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_name_matches_layout() {
        assert_eq!(
            service_name(ServiceRole::Server, "default"),
            "rathole-x-server-default"
        );
        assert_eq!(
            service_name(ServiceRole::Client, "vm_ros_test"),
            "rathole-x-client-vm_ros_test"
        );
        assert_eq!(
            initd_path("rathole-x-server-default"),
            Path::new("/etc/init.d/rathole-x-server-default")
        );
        assert_eq!(
            pidfile_path("rathole-x-server-default"),
            Path::new("/run/rathole-x-server-default.pid")
        );
        assert_eq!(
            log_path("rathole-x-server-default"),
            Path::new("/var/log/rathole-x-server-default.log")
        );
    }

    #[test]
    fn generated_script_runs_the_deployed_binary_with_foreground_args() {
        let deployed = super::super::installed_binary_path();
        let content = render_initd(
            &deployed,
            Path::new("/etc/rathole-x/default.toml"),
            "rathole-x-server-default",
        )
        .unwrap();

        assert!(content.starts_with("#!/sbin/openrc-run\n"));
        assert!(content.contains(&format!("command={}\n", deployed.display())));
        assert!(content
            .contains("command_args_foreground=\"run --config /etc/rathole-x/default.toml\"\n"));
        assert!(!content.contains("\ncommand_args="));
        assert!(content.contains("command_user=\"rathole-x\"\n"));
        assert!(content.contains(&format!("capabilities=\"{RESTRICTED_IAB}\"\n")));
        assert!(!content.contains("cap_net_bind_service+ep"));
        // Every other capability must be dropped from the bounding set;
        // libcap's IAB parser has no "all" aggregate to rely on.
        for cap in [
            "chown",
            "dac_override",
            "setpcap",
            "net_admin",
            "sys_admin",
            "setfcap",
            "bpf",
            "checkpoint_restore",
        ] {
            assert!(
                content.contains(&format!("!cap_{cap}")),
                "missing !cap_{cap}"
            );
        }
        assert!(!content.contains("!cap_net_bind_service"));
        assert!(content.contains("supervisor=supervise-daemon\n"));
        assert!(content.contains("pidfile=\"/run/rathole-x-server-default.pid\"\n"));
        // output_log/error_log must point at the file install pre-creates.
        let log = log_path("rathole-x-server-default");
        assert!(content.contains(&format!("output_log=\"{}\"\n", log.display())));
        assert!(content.contains(&format!("error_log=\"{}\"\n", log.display())));
        assert!(content.contains("respawn_delay=3\n"));
        assert!(content.contains("rc_ulimit=\"-n 1048576\"\n"));
        assert!(content.contains("after net firewall\n"));
    }

    #[test]
    fn render_initd_rejects_unquotable_paths() {
        assert!(render_initd(
            Path::new("/opt/rathole'x"),
            Path::new("/etc/rathole-x/default.toml"),
            "rathole-x-server-default",
        )
        .is_err());
    }
}

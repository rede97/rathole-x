//! systemd backend for the Linux service integration.
//!
//! Compiled only on Linux and reachable through the `linux` facade, which
//! detects the init system. Mirrors the Windows SCM flows: units live in
//! /etc/systemd/system, enable/start go through systemctl. systemctl is
//! driven via `std::process::Command`; no D-Bus dependency.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};
use tracing::warn;

use crate::cli::{InstallArgs, UninstallArgs};
use crate::config_edit::ServiceRole;

/// Directory holding the generated unit files.
const UNIT_DIR: &str = "/etc/systemd/system";

// ---------------------------------------------------------------------------
// Unit helpers
// ---------------------------------------------------------------------------

/// The systemd unit name of an installed rathole-x service.
pub(crate) fn unit_name(role: ServiceRole, name: &str) -> String {
    format!("rathole-x-{}-{}.service", role.key(), name)
}

fn unit_path(unit: &str) -> PathBuf {
    Path::new(UNIT_DIR).join(unit)
}

/// Run `systemctl <args>`, passing stderr through verbatim on failure.
fn run_systemctl(args: &[&str]) -> Result<()> {
    let output = Command::new("systemctl")
        .args(args)
        .output()
        .with_context(|| format!("failed to run `systemctl {}`", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "`systemctl {}` failed:\n{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Best-effort systemctl: reports success instead of failing.
fn try_systemctl(args: &[&str]) -> bool {
    Command::new("systemctl")
        .args(args)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Embed a path in an ExecStart line, quoting it when needed. Paths that
/// cannot be quoted safely are rejected instead of writing a broken unit.
fn exec_path_arg(path: &Path) -> Result<String> {
    let s = path
        .to_str()
        .ok_or_else(|| anyhow!("path {} is not valid UTF-8", path.display()))?;
    if s.contains('"') || s.contains('\\') {
        bail!(
            "cannot embed path `{}` in a systemd unit; move the file to a \
             path without quotes or backslashes",
            s
        );
    }
    if s.chars().any(char::is_whitespace) {
        Ok(format!("\"{}\"", s))
    } else {
        Ok(s.to_owned())
    }
}

/// Render the unit file contents. Pure function for testability.
/// Render the unit file contents. The daemon gets only the identity and
/// capability it needs: configuration reads and optional binding below 1024.
fn render_unit(exe: &Path, config: &Path) -> Result<String> {
    Ok(format!(
        "[Unit]\n\
         Description=rathole-x reverse proxy service (single role)\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         User={user}\n\
         Group={group}\n\
         ExecStart={exe} run --config {config}\n\
         Restart=on-failure\n\
         RestartSec=3\n\
         LimitNOFILE=1048576\n\
         NoNewPrivileges=true\n\
         CapabilityBoundingSet=CAP_NET_BIND_SERVICE\n\
         AmbientCapabilities=CAP_NET_BIND_SERVICE\n\
         PrivateTmp=true\n\
         ProtectHome=true\n\
         ProtectSystem=strict\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n",
        user = super::SERVICE_ACCOUNT,
        group = super::SERVICE_GROUP,
        exe = exec_path_arg(exe)?,
        config = exec_path_arg(config)?,
    ))
}

/// Find an existing unit file for `name` under either role. The role stored
/// in the config may be stale (hand-edited after install).
fn existing_unit(name: &str) -> Option<String> {
    for role in [ServiceRole::Client, ServiceRole::Server] {
        let unit = unit_name(role, name);
        if unit_path(&unit).exists() {
            return Some(unit);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// CLI flows
// ---------------------------------------------------------------------------

/// CLI flow for `service install`: write a restricted unit and start it.
pub(crate) fn install_service(
    args: &InstallArgs,
    role: ServiceRole,
    name: &str,
    config_path: &Path,
) -> Result<()> {
    if args.allow_user_config {
        bail!("--allow-user-config is unsafe for Linux managed services and is not supported");
    }
    let unit = unit_name(role, name);
    println!("Installing {} service '{}':", role.key(), name);
    println!("  Service:     {}", unit);
    println!("  Config:      {}", config_path.display());
    println!("  Access:      root-managed config; edits require sudo");
    println!(
        "  Runtime:     {} (CAP_NET_BIND_SERVICE only)",
        super::SERVICE_ACCOUNT
    );
    println!("  Auto start:  enabled (boot)");

    super::ensure_root()?;
    if unit_path(&unit).exists() {
        bail!(
            "service '{}' already exists; run `rathole-x service uninstall --yes --name {}` first",
            unit,
            name
        );
    }

    super::ensure_service_account()?;
    super::write_version_stamp(config_path)?;
    super::secure_managed_config(&crate::config_edit::version_path(config_path))?;
    super::secure_managed_config(config_path)?;
    let exe = super::deploy_binary()?;
    let content = render_unit(&exe, config_path)?;
    let upath = unit_path(&unit);
    std::fs::write(&upath, content)
        .with_context(|| format!("failed to write unit file {}", upath.display()))?;

    run_systemctl(&["daemon-reload"])?;
    run_systemctl(&["enable", "--now", unit.as_str()])?;

    println!("Service '{}' installed and started", unit);
    println!(
        "Run `rathole-x status --name {}` to inspect, `rathole-x config add --name {} ...` to configure (hot-reloaded).",
        name, name
    );
    Ok(())
}

/// CLI flow for `service uninstall`: stop and disable the unit, remove it
/// and reload systemd. The config is kept unless `--purge` (then left owned
/// by the invoking sudo user). When the service is already gone the leftover
/// files are removed without root, mirroring Windows.
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

    match existing_unit(&name) {
        Some(unit) => {
            super::ensure_root()?;
            // Best-effort: a failed/inactive unit must not block removal.
            let _ = try_systemctl(&["disable", "--now", unit.as_str()]);
            let upath = unit_path(&unit);
            std::fs::remove_file(&upath)
                .with_context(|| format!("failed to remove unit file {}", upath.display()))?;
            run_systemctl(&["daemon-reload"])?;
        }
        None => {
            println!(
                "Service '{}' is not installed; cleaning up leftover files.",
                name
            );
        }
    }

    super::remove_service_files(config_path, args.purge)?;

    println!("Service '{}' stopped and removed", name);
    if args.purge {
        println!("  Config:      removed ({})", config_path.display());
    } else {
        println!(
            "  Config:      kept (delete with sudo or rerun with --purge) ({})",
            config_path.display()
        );
    }
    Ok(())
}

/// Drive one service's systemd state. Resolves the unit with a both-roles
/// fallback (the config's role may be stale) and returns its name for the
/// caller's per-service output.
pub(crate) fn control_one(action: &str, name: &str, role: ServiceRole) -> Result<String> {
    let unit = existing_unit(name).unwrap_or_else(|| unit_name(role, name));
    run_systemctl(&[action, unit.as_str()])
        .with_context(|| format!("failed to {} service '{}'", action, unit))?;
    Ok(unit)
}

/// `rathole-x upgrade`: stop every unit, replace the protected deployed
/// binary, rewrite each unit to that path, then start everything again.
pub(crate) fn upgrade_binary() -> Result<()> {
    let services = crate::config_edit::list_installed_services()?;
    if services.is_empty() {
        println!("No installed services; nothing to upgrade.");
        return Ok(());
    }
    super::ensure_root()?;

    for (name, role) in &services {
        let unit = existing_unit(name).unwrap_or_else(|| unit_name(*role, name));
        if !try_systemctl(&["stop", unit.as_str()]) {
            warn!("failed to stop '{}'", unit);
        }
    }

    let installed = super::deploy_binary()?;
    for (name, role) in &services {
        let unit = existing_unit(name).unwrap_or_else(|| unit_name(*role, name));
        let upath = unit_path(&unit);
        if !upath.exists() {
            warn!("skipping missing unit '{}'", upath.display());
            continue;
        }
        let config = crate::config_edit::config_dir().join(format!("{}.toml", name));
        let content = render_unit(&installed, &config)?;
        std::fs::write(&upath, content)
            .with_context(|| format!("failed to rewrite unit file {}", upath.display()))?;
    }
    run_systemctl(&["daemon-reload"])?;

    let started = start_units_best_effort(&services);
    println!(
        "Updated {} and restarted {} service(s).",
        installed.display(),
        started
    );
    Ok(())
}

/// Start every listed unit, logging failures without stopping. Returns the
/// number of units successfully started.
fn start_units_best_effort(services: &[(String, ServiceRole)]) -> usize {
    let mut started = 0usize;
    for (name, role) in services {
        let unit = existing_unit(name).unwrap_or_else(|| unit_name(*role, name));
        if try_systemctl(&["start", unit.as_str()]) {
            started += 1;
        } else {
            warn!("failed to start '{}'", unit);
        }
    }
    started
}

// ---------------------------------------------------------------------------
// Status queries
// ---------------------------------------------------------------------------

/// Query one installed service's systemd state. Only units installed by
/// `service install` (unit file under /etc/systemd/system) are reported;
/// anything else renders as "not installed".
pub(crate) fn query_named_service(role: &str, name: &str) -> Option<(String, Option<u32>)> {
    let unit = format!("rathole-x-{}-{}.service", role, name);
    if !unit_path(&unit).exists() {
        return None;
    }
    query_systemd_unit(&unit)
}

/// Query the systemd state of an installed unit via systemctl. `is-active`
/// needs no root, so plain `status` works for normal users.
fn query_systemd_unit(unit: &str) -> Option<(String, Option<u32>)> {
    let out = Command::new("systemctl")
        .arg("is-active")
        .arg(unit)
        .output()
        .ok()?;
    let state = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if state.is_empty() {
        return None;
    }
    let mapped = match state.as_str() {
        "active" => "Running",
        "inactive" => "Stopped",
        "failed" => "Failed",
        other => other,
    };
    let pid = Command::new("systemctl")
        .args(["show", "-p", "MainPID", "--value", unit])
        .output()
        .ok()
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse::<u32>()
                .ok()
        })
        .filter(|p| *p != 0);
    Some((mapped.to_owned(), pid))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_name_matches_plan() {
        assert_eq!(
            unit_name(ServiceRole::Server, "default"),
            "rathole-x-server-default.service"
        );
        assert_eq!(
            unit_name(ServiceRole::Client, "vm_ros_test"),
            "rathole-x-client-vm_ros_test.service"
        );
        assert_eq!(
            unit_path("rathole-x-server-default.service"),
            Path::new("/etc/systemd/system/rathole-x-server-default.service")
        );
    }

    #[test]
    fn generated_unit_executes_only_the_deployed_binary() {
        let deployed = super::super::installed_binary_path();
        let content = render_unit(&deployed, Path::new("/etc/rathole-x/default.toml")).unwrap();

        assert!(content.contains(&format!(
            "ExecStart={} run --config /etc/rathole-x/default.toml\n",
            deployed.display()
        )));
        assert!(content.contains("Type=simple\n"));
        assert!(content.contains("User=rathole-x\n"));
        assert!(content.contains("Group=rathole-x\n"));
        assert!(content.contains("NoNewPrivileges=true\n"));
        assert!(content.contains("CapabilityBoundingSet=CAP_NET_BIND_SERVICE\n"));
        assert!(content.contains("AmbientCapabilities=CAP_NET_BIND_SERVICE\n"));
        assert!(content.contains("Restart=on-failure\n"));
        assert!(content.contains("RestartSec=3\n"));
        assert!(content.contains("LimitNOFILE=1048576\n"));
        assert!(content.contains("After=network-online.target\n"));
        assert!(content.contains("Wants=network-online.target\n"));
        assert!(content.contains("WantedBy=multi-user.target\n"));
    }

    #[test]
    fn render_unit_rejects_unquotable_paths() {
        assert!(render_unit(
            Path::new("/opt/rathole\"x"),
            Path::new("/etc/rathole-x/default.toml"),
        )
        .is_err());
    }
}

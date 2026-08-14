//! Portable stubs for platforms without service support yet.
//! Linux systemd is planned: see docs/plan-linux-service.md.

use std::path::Path;

use anyhow::{bail, Result};

use crate::cli::{InstallArgs, UninstallArgs};
use crate::config_edit::ServiceRole;

pub fn redirect_stdio_to_file(_path: &Path) {
    // Nothing to redirect outside the UAC relay.
}

pub fn elevate_for_config_if_needed(_path: &Path) -> Result<bool> {
    // On Linux the service config lives under /etc and the CLI is expected to
    // run under sudo when needed; see docs/plan-linux-service.md.
    Ok(false)
}

pub fn install_service(
    _args: &InstallArgs,
    role: ServiceRole,
    name: &str,
    config_path: &Path,
) -> Result<()> {
    println!("Linux systemd support is planned; see docs/plan-linux-service.md");
    println!("Would install {} service '{}' (config: {})", role.key(), name, config_path.display());
    Ok(())
}

pub fn uninstall_service(_args: &UninstallArgs, _config_path: &Path) -> Result<()> {
    println!("Linux systemd support is planned; see docs/plan-linux-service.md");
    Ok(())
}

pub fn uninstall_all(_args: &UninstallArgs) -> Result<()> {
    println!("Linux systemd support is planned; see docs/plan-linux-service.md");
    Ok(())
}

pub fn control_service(_cmd: crate::cli::ServiceCmd) -> Result<()> {
    println!("Linux systemd support is planned; see docs/plan-linux-service.md");
    Ok(())
}

pub fn upgrade_binary() -> Result<()> {
    println!("Linux systemd support is planned; see docs/plan-linux-service.md");
    Ok(())
}

pub fn run_service(_config: std::path::PathBuf) -> Result<()> {
    bail!("`service run` is the Windows SCM entry point and is not available on this platform")
}

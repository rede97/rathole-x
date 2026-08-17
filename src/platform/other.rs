//! Portable stubs for platforms without service support (anything that is
//! not Windows or Linux).

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use tokio::sync::broadcast;

use crate::cli::{InstallArgs, UninstallArgs};
use crate::config_edit::ServiceRole;
use crate::runtime_status::{RuntimeRegistry, RuntimeSnapshot};

pub fn redirect_stdio_to_file(_path: &Path) {
    // Nothing to redirect outside the UAC relay.
}

pub fn elevate_for_config_if_needed(_path: &Path) -> Result<bool> {
    // No elevation mechanism on this platform.
    Ok(false)
}

pub fn install_service(
    _args: &InstallArgs,
    role: ServiceRole,
    name: &str,
    config_path: &Path,
) -> Result<()> {
    if !crate::is_json_mode() {
        println!("service management is not supported on this platform");
        println!(
            "Would install {} service '{}' (config: {})",
            role.key(),
            name,
            config_path.display()
        );
    }
    Ok(())
}

pub fn uninstall_service(_args: &UninstallArgs, _config_path: &Path) -> Result<()> {
    if !crate::is_json_mode() {
        println!("service management is not supported on this platform");
    }
    Ok(())
}

pub fn uninstall_all(_args: &UninstallArgs) -> Result<()> {
    if !crate::is_json_mode() {
        println!("service management is not supported on this platform");
    }
    Ok(())
}

pub fn control_service(_cmd: crate::cli::ServiceCmd) -> Result<()> {
    if !crate::is_json_mode() {
        println!("service management is not supported on this platform");
    }
    Ok(())
}

pub fn upgrade_binary() -> Result<()> {
    if !crate::is_json_mode() {
        println!("service management is not supported on this platform");
    }
    Ok(())
}

pub fn run_service(_config: std::path::PathBuf) -> Result<()> {
    bail!("`service run` is the Windows SCM entry point and is not available on this platform")
}

/// This platform has no runtime-status endpoint yet (Windows and Linux do).
/// Keep the façade so callers can represent this as unavailable rather than
/// failing `status`.
pub fn spawn_runtime_status_server(
    _config_path: PathBuf,
    _registry: RuntimeRegistry,
    _shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()> {
    Ok(())
}

pub fn query_runtime_status(_config_path: &Path) -> Result<RuntimeSnapshot> {
    bail!("runtime status endpoint is not available on this platform")
}

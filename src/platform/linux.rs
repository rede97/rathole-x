//! Linux service integration with systemd and OpenRC backends.

use std::io::{Read, Write};
use std::os::linux::net::SocketAddrExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::broadcast;
use tracing::{error, warn};

use crate::cli::{InstallArgs, ServiceCmd, UninstallArgs};
use crate::config_edit::{self, ServiceRole};
use crate::runtime_status::{
    snapshot, status_name_for_config, RuntimeRegistry, RuntimeSnapshot, MAX_STATUS_RESPONSE,
    STATUS_REQUEST,
};

pub(crate) const SERVICE_ACCOUNT: &str = "rathole-x";
pub(crate) const SERVICE_GROUP: &str = "rathole-x";
const INSTALLED_BINARY: &str = "/usr/local/lib/rathole-x/rathole-x";

/// The protected, root-owned deployed binary the service definitions
/// execute — never the user-supplied `current_exe()` path.
pub(crate) fn installed_binary_path() -> PathBuf {
    PathBuf::from(INSTALLED_BINARY)
}

#[derive(Clone, Copy)]
enum Init {
    Systemd,
    OpenRc,
}
fn init() -> Result<Init> {
    if Path::new("/run/systemd/system").is_dir() {
        return Ok(Init::Systemd);
    }
    if Command::new("rc-service").arg("--version").output().is_ok() {
        return Ok(Init::OpenRc);
    }
    bail!("no supported Linux init system found; rathole-x supports systemd and OpenRC")
}
unsafe extern "C" {
    fn geteuid() -> u32;
}
pub(crate) fn ensure_root() -> Result<()> {
    if unsafe { geteuid() } != 0 {
        bail!("please run with sudo");
    }
    Ok(())
}

fn command_ok(program: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("failed to run {program}"))?;
    if !status.success() {
        bail!("{program} {} failed", args.join(" "));
    }
    Ok(())
}
fn exists(database: &str, key: &str) -> bool {
    Command::new("getent")
        .args([database, key])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
pub(crate) fn ensure_service_account() -> Result<()> {
    ensure_root()?;
    if !exists("group", SERVICE_GROUP)
        && !Command::new("groupadd")
            .args(["--system", SERVICE_GROUP])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    {
        command_ok("addgroup", &["-S", SERVICE_GROUP])?;
    }
    if !exists("passwd", SERVICE_ACCOUNT)
        && !Command::new("useradd")
            .args([
                "--system",
                "--no-create-home",
                "--shell",
                "/usr/sbin/nologin",
                "--gid",
                SERVICE_GROUP,
                SERVICE_ACCOUNT,
            ])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    {
        command_ok(
            "adduser",
            &[
                "-S",
                "-D",
                "-H",
                "-s",
                "/sbin/nologin",
                "-G",
                SERVICE_GROUP,
                SERVICE_ACCOUNT,
            ],
        )?;
    }
    Ok(())
}
fn owner(path: &Path, mode: &str) -> Result<()> {
    let value = path.display().to_string();
    command_ok("chown", &["root:rathole-x", &value])?;
    command_ok("chmod", &[mode, &value])
}
pub(crate) fn secure_managed_config(path: &Path) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("config path has no parent"))?;
    std::fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    // World-listable directory and world-readable configs (0755/0644),
    // matching the Windows `status` readability contract and normal /etc
    // conventions: configs carry no secrets worth hiding from local users
    // (tokens are per-service and visible in `status` output anyway).
    owner(dir, "0755")?;
    if path.exists() {
        owner(path, "0644")?;
    }
    Ok(())
}
pub(crate) fn write_version_stamp(config: &Path) -> Result<()> {
    let version = config_edit::version_path(config);
    std::fs::write(
        &version,
        format!(
            "# Written by rathole-x service install.\nversion = {}\n",
            crate::cli::major_version()
        ),
    )
    .with_context(|| format!("failed to write {}", version.display()))?;
    secure_managed_config(&version)
}
pub(crate) fn deploy_binary() -> Result<PathBuf> {
    ensure_root()?;
    let dest = installed_binary_path();
    std::fs::create_dir_all(dest.parent().expect("installed path has parent"))?;
    std::fs::copy(
        std::env::current_exe().context("failed to resolve current executable")?,
        &dest,
    )
    .with_context(|| format!("failed to install binary to {}", dest.display()))?;
    command_ok("chown", &["root:root", INSTALLED_BINARY])?;
    command_ok("chmod", &["0755", INSTALLED_BINARY])?;
    Ok(dest)
}
pub(crate) fn remove_service_files(config: &Path, purge: bool) -> Result<()> {
    if purge && config.exists() {
        // Deleting a root-owned config requires root; fail loudly instead
        // of claiming the config was removed.
        ensure_root()?;
        std::fs::remove_file(config)
            .with_context(|| format!("failed to remove config {}", config.display()))?;
    }
    let version = config_edit::version_path(config);
    let others = config
        .parent()
        .and_then(|dir| std::fs::read_dir(dir).ok())
        .map(|entries| {
            entries
                .flatten()
                .any(|e| e.path().extension().is_some_and(|x| x == "toml") && e.path() != version)
        })
        .unwrap_or(false);
    if !others && version.exists() {
        std::fs::remove_file(&version)
            .with_context(|| format!("failed to remove version stamp {}", version.display()))?;
    }
    Ok(())
}

pub fn install_service(
    args: &InstallArgs,
    role: ServiceRole,
    name: &str,
    config: &Path,
) -> Result<()> {
    if args.allow_user_config {
        bail!("--allow-user-config is unsafe for Linux managed services and is not supported");
    }
    ensure_root()?;
    ensure_service_account()?;
    secure_managed_config(config)?;
    match init()? {
        Init::Systemd => super::systemd::install_service(args, role, name, config),
        Init::OpenRc => super::openrc::install_service(args, role, name, config),
    }
}

pub fn uninstall_service(args: &UninstallArgs, config: &Path) -> Result<()> {
    match init()? {
        Init::Systemd => super::systemd::uninstall_service(args, config),
        Init::OpenRc => super::openrc::uninstall_service(args, config),
    }
}

pub fn uninstall_all(args: &UninstallArgs) -> Result<()> {
    for (name, _) in config_edit::list_installed_services()? {
        uninstall_service(
            args,
            &config_edit::config_dir().join(format!("{name}.toml")),
        )?;
    }
    let _ = std::fs::remove_file(INSTALLED_BINARY);
    Ok(())
}

pub fn control_service(cmd: ServiceCmd) -> Result<()> {
    let (action, args) = match &cmd {
        ServiceCmd::Start(args) => ("start", args),
        ServiceCmd::Stop(args) => ("stop", args),
        ServiceCmd::Restart(args) => ("restart", args),
        _ => unreachable!(),
    };
    let services = config_edit::list_installed_services()?;
    let names = if args.all {
        services.iter().map(|(name, _)| name.clone()).collect()
    } else {
        vec![args.name.clone().unwrap_or_else(|| "default".into())]
    };
    for name in names {
        let role = services
            .iter()
            .find(|(installed_name, _)| installed_name == &name)
            .map(|(_, role)| *role)
            .unwrap_or(ServiceRole::Server);
        let _ = match init()? {
            Init::Systemd => super::systemd::control_one(action, &name, role)?,
            Init::OpenRc => super::openrc::control_one(action, &name, role)?,
        };
    }
    Ok(())
}

pub fn upgrade_binary() -> Result<()> {
    match init()? {
        Init::Systemd => super::systemd::upgrade_binary(),
        Init::OpenRc => super::openrc::upgrade_binary(),
    }
}
pub fn run_service(_config: PathBuf) -> Result<()> {
    bail!("service run is a Windows SCM entry point and is not available on Linux")
}
pub fn redirect_stdio_to_file(_path: &Path) {}
pub fn elevate_for_config_if_needed(_path: &Path) -> Result<bool> {
    Ok(false)
}
/// Abstract-namespace socket address for the config's status endpoint. An
/// abstract name needs no filesystem directory, has no stale-file lifecycle,
/// and any local user may connect — the Linux analogue of the Windows pipe
/// ACL that grants every authenticated user read/write access.
fn status_socket_addr(config_path: &Path) -> Result<std::os::unix::net::SocketAddr> {
    let name = status_name_for_config(config_path);
    <std::os::unix::net::SocketAddr as SocketAddrExt>::from_abstract_name(name.as_bytes())
        .context("failed to build runtime status socket address")
}

async fn answer_status_request(mut stream: tokio::net::UnixStream, registry: RuntimeRegistry) {
    let mut request = [0u8; STATUS_REQUEST.len()];
    if stream.read_exact(&mut request).await.is_err() || request != STATUS_REQUEST {
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
    if stream.write_all(&len).await.is_ok() {
        let _ = stream.write_all(&response).await;
    }
}

/// Start the local-only status endpoint for this runtime generation. Failure
/// to expose status never interrupts proxy traffic.
pub fn spawn_runtime_status_server(
    config_path: PathBuf,
    registry: RuntimeRegistry,
    mut shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()> {
    let addr = status_socket_addr(&config_path)?;
    let listener = std::os::unix::net::UnixListener::bind_addr(&addr)
        .context("failed to bind runtime status endpoint")?;
    listener
        .set_nonblocking(true)
        .context("failed to configure runtime status endpoint")?;
    let listener =
        tokio::net::UnixListener::from_std(listener).context("failed to adopt status endpoint")?;
    tokio::spawn(async move {
        loop {
            tokio::select! {
                result = listener.accept() => {
                    match result {
                        Ok((stream, _)) => {
                            tokio::spawn(answer_status_request(stream, registry.clone()));
                        }
                        Err(error) => {
                            warn!("runtime status socket accept failed: {}", error);
                            break;
                        }
                    }
                }
                _ = shutdown_rx.recv() => break,
            }
        }
    });
    Ok(())
}

/// Query the fixed local socket verb. A missing/stopped endpoint is returned
/// as an error so `status` can render `runtime: null` without failing the
/// static service/config query.
pub fn query_runtime_status(config_path: &Path) -> Result<RuntimeSnapshot> {
    let addr = status_socket_addr(config_path)?;
    let mut stream = std::os::unix::net::UnixStream::connect_addr(&addr)
        .with_context(|| format!("runtime status unavailable for {}", config_path.display()))?;
    stream
        .write_all(STATUS_REQUEST)
        .context("failed to request runtime status")?;
    stream
        .flush()
        .context("failed to flush runtime status request")?;
    let mut length = [0u8; 4];
    stream
        .read_exact(&mut length)
        .context("failed to read runtime status response length")?;
    let length = u32::from_le_bytes(length) as usize;
    if length > MAX_STATUS_RESPONSE {
        bail!("runtime status response exceeds the maximum size");
    }
    let mut response = vec![0; length];
    stream
        .read_exact(&mut response)
        .context("failed to read runtime status response")?;
    serde_json::from_slice(&response).context("invalid runtime status response")
}
pub fn query_service_state(role: &str, name: &str) -> Option<(String, Option<u32>)> {
    match init().ok()? {
        Init::Systemd => super::systemd::query_named_service(role, name),
        Init::OpenRc => super::openrc::query_named_service(role, name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn runtime_status_socket_round_trip_and_missing_endpoint() {
        let dir = std::env::temp_dir().join(format!(
            "rathole-x-status-sock-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let config = dir.join("service.toml");
        std::fs::write(&config, "[client]\nremote_addr = \"127.0.0.1:1\"\n").unwrap();
        // No listener yet: the query must fail instead of hanging.
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

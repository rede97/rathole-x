//! Linux service integration with systemd and OpenRC backends.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use tokio::sync::broadcast;

use crate::cli::{InstallArgs, ServiceCmd, UninstallArgs};
use crate::config_edit::{self, ServiceRole};
use crate::runtime_status::{RuntimeRegistry, RuntimeSnapshot};

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
    owner(dir, "0750")?;
    if path.exists() {
        owner(path, "0640")?;
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
pub(crate) fn remove_service_files(config: &Path, purge: bool) {
    if purge {
        let _ = std::fs::remove_file(config);
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
    if !others {
        let _ = std::fs::remove_file(version);
    }
}
pub(crate) fn chown_to_sudo_user(_path: &Path) {}

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
pub fn spawn_runtime_status_server(
    _config: PathBuf,
    _registry: RuntimeRegistry,
    _shutdown: broadcast::Receiver<bool>,
) -> Result<()> {
    Ok(())
}
pub fn query_runtime_status(_config: &Path) -> Result<RuntimeSnapshot> {
    bail!("runtime status endpoint is not available on Linux")
}
pub fn query_service_state(role: &str, name: &str) -> Option<(String, Option<u32>)> {
    match init().ok()? {
        Init::Systemd => super::systemd::query_named_service(role, name),
        Init::OpenRc => super::openrc::query_named_service(role, name),
    }
}

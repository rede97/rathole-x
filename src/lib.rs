mod cli;
mod config;
mod config_edit;
mod config_watcher;
mod constants;
mod helper;
mod multi_map;
mod protocol;
mod transport;
mod status;
pub mod platform;


pub use cli::{Cli, Commands, RunArgs};
pub use config::Config;
pub use constants::UDP_BUFFER_SIZE;

use cli::KeypairType;
use anyhow::{bail, Context, Result};

use tokio::sync::{broadcast, mpsc};
use tracing::{debug, error, info, warn};

#[cfg(feature = "client")]
mod client;
#[cfg(feature = "client")]
use client::run_client;

#[cfg(feature = "server")]
mod server;
#[cfg(feature = "server")]
use server::run_server;

use crate::config_watcher::{ConfigChange, ConfigWatcherHandle};

const DEFAULT_CURVE: KeypairType = KeypairType::X25519;

fn get_str_from_keypair_type(curve: KeypairType) -> &'static str {
    match curve {
        KeypairType::X25519 => "25519",
        KeypairType::X448 => "448",
    }
}

/// Generate a noise keypair for the given DH function.
///
/// Returns `(private_key, public_key)`, base64 encoded.
#[cfg(feature = "noise")]
pub fn generate_keypair(curve: KeypairType) -> Result<(String, String)> {
    let builder = snowstorm::Builder::new(
        format!(
            "Noise_KK_{}_ChaChaPoly_BLAKE2s",
            get_str_from_keypair_type(curve)
        )
        .parse()?,
    );
    let keypair = builder.generate_keypair()?;
    Ok((
        base64::encode(keypair.private),
        base64::encode(keypair.public),
    ))
}

#[cfg(not(feature = "noise"))]
pub fn generate_keypair(_curve: KeypairType) -> Result<(String, String)> {
    crate::helper::feature_not_compile("noise")
}

fn genkey(curve: Option<KeypairType>) -> Result<()> {
    let (private_key, public_key) = generate_keypair(curve.unwrap_or(DEFAULT_CURVE))?;

    println!("Private Key:\n{}\n", private_key);
    println!("Public Key:\n{}", public_key);
    Ok(())
}

/// Default configuration path used by `run`/`install` without `--config`.
pub(crate) fn os_default_config_path() -> std::path::PathBuf {
    #[cfg(windows)]
    {
        let base = std::env::var_os("ProgramData")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(r"C:\ProgramData"));
        base.join("rathole-x").join("rathole-x.toml")
    }
    #[cfg(not(windows))]
    {
        std::path::PathBuf::from("/etc/rathole-x.toml")
    }
}

pub async fn run(args: Cli, shutdown_rx: broadcast::Receiver<bool>) -> Result<()> {
    match args.command {
        Some(cmd) => dispatch_command(cmd, shutdown_rx).await,
        // clap's ArgRequiredElseHelp prints help and exits before we get
        // here; keep a graceful fallback for direct library callers.
        None => bail!("No subcommand given; run `rathole-x --help`"),
    }
}

async fn dispatch_command(cmd: Commands, shutdown_rx: broadcast::Receiver<bool>) -> Result<()> {
    use Commands::*;

    match cmd {
        Run(run_args) => {
            let config_path = match &run_args.config {
                Some(p) => p.clone(),
                None => os_default_config_path(),
            };
            run_with_config(config_path, run_args, shutdown_rx).await
        }
        Genkey { curve } => genkey(curve),
        Config { cmd } => dispatch_config_command(cmd).await,
        Status(s) => status::run_status(&s),

        Service { cmd } => Ok(dispatch_service_command(cmd)?),
        Upgrade(u) => {
            if !confirm_action::<cli::UpgradeArgs>(
                u.yes,
                "upgrade",
                "Replace the installed rathole-x binary in place and restart every \
                 installed service. Proceed?",
            )? {
                return Ok(());
            }
            run_action_json(u.json, "upgrade", &[], "binary upgraded", || {
                platform::upgrade_binary()
            })
        }
    }
}

async fn dispatch_config_command(cmd: cli::ConfigCmd) -> Result<()> {
    use cli::ConfigCmd::*;
    match cmd {
        Add(a) => {
            let path =
                config_edit::resolve_service_config(a.config.as_ref(), a.name.as_deref())?;
            config_edit::check_version_compat(&path)?;
            if platform::elevate_for_config_if_needed(&path)? {
                return Ok(());
            }
            config_edit::run_add(&a, &path)
        }
        Remove(r) => {
            let path =
                config_edit::resolve_service_config(r.config.as_ref(), r.name.as_deref())?;
            config_edit::check_version_compat(&path)?;
            if platform::elevate_for_config_if_needed(&path)? {
                return Ok(());
            }
            config_edit::run_remove(&r, &path)
        }
        List(l) => {
            let path = config_edit::resolve_service_config(l.config.as_ref(), l.name.as_deref())?;
            config_edit::run_list(&l, &path)
        }
        Set(s) => {
            let path =
                config_edit::resolve_service_config(s.config.as_ref(), s.name.as_deref())?;
            config_edit::check_version_compat(&path)?;
            if platform::elevate_for_config_if_needed(&path)? {
                return Ok(());
            }
            config_edit::run_set(&s, &path)
        }
    }
}

/// Dispatches `rathole-x service <action>`: install, uninstall, control
/// and the hidden SCM run entry.
fn dispatch_service_command(cmd: cli::ServiceCmd) -> Result<()> {
    use cli::ServiceCmd::*;
    match cmd {
        Install(i) => {
            let role = match i.role {
                Some(r) => config_edit::ServiceRole::from_cli(r),
                None => bail!("A role is required: `service install server` or `service install client`"),
            };
            // `-c` pins the service to an explicit config file; without it
            // the config lives in the OS default directory.
            let (name, config_path) = match &i.config {
                Some(p) => {
                    let path = std::env::current_dir()
                        .context("failed to resolve the current directory for `--config`")?
                        .join(p);
                    let name = match &i.name {
                        Some(n) => n.clone(),
                        None => match path.file_stem().and_then(|s| s.to_str()) {
                            Some(stem) => stem.to_owned(),
                            None => bail!(
                                "cannot derive a service name from `{}`; pass `--name`",
                                path.display()
                            ),
                        },
                    };
                    config_edit::validate_service_name(&name)?;
                    (name, path)
                }
                None => config_edit::service_config_path(i.name.as_deref())?,
            };
            if !confirm_action::<cli::InstallArgs>(
                i.yes,
                "install",
                &format!(
                    "Install a {} service '{}' (SCM name rathole-x-{}-{}) with config {}. Proceed?",
                    role.key(),
                    name,
                    role.key(),
                    name,
                    config_path.display()
                ),
            )? {
                return Ok(());
            }
            // The role check runs before any elevation: a config created for
            // the wrong role is rejected in the caller's unprivileged context.
            let created = config_edit::ensure_role_config(&config_path, role)?;
            if created {
                println!(
                    "Created {} config at {} (no services configured yet).",
                    role.key(),
                    config_path.display()
                );
            }
            let names = [name.clone()];
            run_action_json(i.json, "install", &names, "service installed", || {
                platform::install_service(&i, role, &name, &config_path)
            })
        }
        Uninstall(u) => {
            if u.all {
                let names: Vec<String> = config_edit::list_installed_services()?
                    .into_iter()
                    .map(|(n, _)| n)
                    .collect();
                if !confirm_action::<cli::UninstallArgs>(
                    u.yes,
                    "uninstall",
                    &format!(
                        "Remove EVERY installed rathole-x service ({}), their configs and \
                         the shared binary. Proceed?",
                        if names.is_empty() {
                            "none".to_owned()
                        } else {
                            names.join(", ")
                        }
                    ),
                )? {
                    return Ok(());
                }
                return run_action_json(
                    u.json,
                    "uninstall",
                    &names,
                    "all services uninstalled",
                    || platform::uninstall_all(&u),
                );
            }
            let config_path =
                config_edit::resolve_service_config(u.config.as_ref(), u.name.as_deref())?;
            let name = config_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("?")
                .to_owned();
            if !confirm_action::<cli::UninstallArgs>(
                u.yes,
                "uninstall",
                &format!(
                    "Uninstall service '{}' (config {}){}. Proceed?",
                    name,
                    config_path.display(),
                    if u.purge {
                        " and delete its config file"
                    } else {
                        ""
                    }
                ),
            )? {
                return Ok(());
            }
            let names = [name];
            run_action_json(u.json, "uninstall", &names, "service uninstalled", || {
                platform::uninstall_service(&u, &config_path)
            })
        }
        Run { config } => platform::run_service(config),
        other => {
            let (action, json, names) = match &other {
                Start(a) => ("start", a.json, control_target_names(a.name.as_deref())),
                Stop(a) => ("stop", a.json, control_target_names(a.name.as_deref())),
                Restart(a) => ("restart", a.json, control_target_names(a.name.as_deref())),
                _ => unreachable!("install/uninstall/run are handled above"),
            };
            run_action_json(json, action, &names, "ok", || {
                platform::control_service(other)
            })
        }
    }
}

/// Print the usage of a subcommand built from its `Args` type. Used when a
/// dangerous subcommand runs without its confirmation flag.
fn print_subcommand_usage<T: clap::Args>(name: &str) -> Result<()> {
    let mut cmd = T::augment_args(clap::Command::new(name));
    cmd.print_help()?;
    println!();
    Ok(())
}

/// Environment variable set after a successful interactive confirmation.
/// The UAC elevation relay re-runs the same command line in a child process
/// that has no TTY; the inherited variable marks that child as confirmed so
/// it does not fall back to printing usage. The relay forwards it explicitly
/// via the hidden `--confirmed` flag (`main.rs`) because environment
/// inheritance across the UAC boundary is not guaranteed.
pub const CONFIRMED_ENV: &str = "RATHOLE_X_CONFIRMED";

/// Gate a privileged subcommand behind user confirmation.
///
/// `--yes` (or an already-confirmed parent process, see `CONFIRMED_ENV`)
/// proceeds directly. On an interactive terminal (both stdin and stdout are
/// TTYs) the user is shown an action summary and asked to confirm. On a
/// non-interactive shell the subcommand usage is printed and nothing is
/// executed (legacy behavior for scripts).
///
/// Returns `Ok(true)` when the operation may proceed.
fn confirm_action<T: clap::Args>(yes: bool, subcommand: &str, summary: &str) -> Result<bool> {
    if yes || std::env::var_os(CONFIRMED_ENV).is_some() {
        return Ok(true);
    }
    if !(atty::is(atty::Stream::Stdin) && atty::is(atty::Stream::Stdout)) {
        print_subcommand_usage::<T>(subcommand)?;
        return Ok(false);
    }
    let confirmed = dialoguer::Confirm::new()
        .with_prompt(summary.to_string())
        .default(false)
        .interact()
        .context("failed to read the confirmation prompt")?;
    if !confirmed {
        println!("Aborted.");
        return Ok(false);
    }
    std::env::set_var(CONFIRMED_ENV, "1");
    Ok(true)
}

/// Print the machine-readable (`--json`) result line of a service action.
fn emit_action_json(action: &str, names: &[String], success: bool, message: &str) {
    println!(
        "{}",
        serde_json::json!({
            "action": action,
            "names": names,
            "success": success,
            "message": message,
        })
    );
}

/// Run a service action, emitting its `--json` result line when requested.
/// The platform layer reports human-readable progress itself; this only adds
/// the final machine-readable summary.
fn run_action_json(
    json: bool,
    action: &str,
    names: &[String],
    ok_message: &str,
    f: impl FnOnce() -> Result<()>,
) -> Result<()> {
    match f() {
        Ok(()) => {
            if json {
                emit_action_json(action, names, true, ok_message);
            }
            Ok(())
        }
        Err(e) => {
            if json {
                emit_action_json(action, names, false, &format!("{:#}", e));
            }
            Err(e)
        }
    }
}

/// The services a control action (start/stop/restart) applies to: the
/// explicit `--name`, or every installed service for `--all` and for the
/// implicit single-service targeting (the platform layer errors out when the
/// implicit target is ambiguous).
fn control_target_names(name: Option<&str>) -> Vec<String> {
    match name {
        Some(n) => vec![n.to_owned()],
        None => config_edit::list_installed_services()
            .map(|v| v.into_iter().map(|(n, _)| n).collect())
            .unwrap_or_default(),
    }
}

async fn run_with_config(
    config_path: std::path::PathBuf,
    run_args: cli::RunArgs,
    mut shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()> {
    // Raise `nofile` limit on linux and mac
    fdlimit::raise_fd_limit();

    // shutdown_tx owns the watcher and the instance(s).
    let (shutdown_tx, _) = broadcast::channel(1);
    // Forward the external shutdown signal into the local channel so the
    // retry loop below and every subscribe() share one source.
    {
        let tx = shutdown_tx.clone();
        tokio::spawn(async move {
            let _ = shutdown_rx.recv().await;
            let _ = tx.send(true);
        });
    }

    // Spawn a config watcher. When the config is missing or invalid at
    // startup, do not die: hibernate in a degraded state and keep waiting
    // for the config to appear or become valid (hot reload picks it up).
    let mut retry_rx = shutdown_tx.subscribe();
    let mut cfg_watcher = loop {
        match ConfigWatcherHandle::new(&config_path, shutdown_tx.subscribe()).await {
            Ok(w) => break w,
            Err(e) => {
                warn!(
                    "Config {} is not usable yet: {:#}. Degraded: waiting for a config update...",
                    config_path.display(),
                    e
                );
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {}
                    _ = retry_rx.recv() => return Ok(()),
                }
            }
        }
    };

    // (The join handle of the last instance, the service update channels.)
    // A single process can host both a client and a server instance
    // (RunMode::Both), each with its own update channel; service events are
    // fanned out to every half.
    type Instance = (
        tokio::task::JoinHandle<Result<()>>,
        Vec<mpsc::Sender<ConfigChange>>,
    );
    let mut last_instance: Option<Instance> = None;

    // Each instance generation gets its own shutdown channel. A General
    // restart signals only the old generation; the process-level
    // `shutdown_tx` stays reserved for the watcher, the retry loop and the
    // external signal, so a hot reload can never stop the whole process.
    let mut instance_shutdown_tx: Option<broadcast::Sender<bool>> = None;
    let mut process_shutdown_rx = shutdown_tx.subscribe();

    loop {
        let e = tokio::select! {
            e = cfg_watcher.event_rx.recv() => match e {
                Some(e) => e,
                None => break,
            },
            _ = process_shutdown_rx.recv() => break,
        };
        match e {
            ConfigChange::General(config) => {
                if let Some((i, _)) = last_instance.take() {
                    info!("General configuration change detected. Restarting...");
                    if let Some(tx) = instance_shutdown_tx.take() {
                        let _ = tx.send(true);
                    }
                    // The instance already logged its own error; only a
                    // join failure (panic) is worth reporting here. Never
                    // propagate: a dead instance must not kill the process.
                    if let Err(e) = i.await {
                        error!("The last instance task failed to join: {}", e);
                    }
                }

                debug!("{:?}", config);

                let halves = if determine_run_mode(&config, run_args.client, run_args.server)
                    == RunMode::Both
                {
                    2
                } else {
                    1
                };

                let (itx, _) = broadcast::channel(1);
                let mut service_update_txs = Vec::with_capacity(halves);
                let mut service_update_rxs = Vec::with_capacity(halves);
                let mut shutdown_rxs = Vec::with_capacity(halves);
                for _ in 0..halves {
                    let (service_update_tx, service_update_rx) = mpsc::channel(1024);
                    service_update_txs.push(service_update_tx);
                    service_update_rxs.push(service_update_rx);
                    shutdown_rxs.push(itx.subscribe());
                }
                instance_shutdown_tx = Some(itx);

                let client = run_args.client;
                let server = run_args.server;
                last_instance = Some((
                    tokio::spawn(async move {
                        let r = run_instance(
                            *config,
                            client,
                            server,
                            shutdown_rxs,
                            service_update_rxs,
                        )
                        .await;
                        if let Err(e) = &r {
                            error!("The instance exited with an error: {:#}", e);
                        }
                        r
                    }),
                    service_update_txs,
                ));
            }
            ev => {
                info!("Service change detected. {:?}", ev);
                if let Some((_, service_update_txs)) = &last_instance {
                    for service_update_tx in service_update_txs {
                        let _ = service_update_tx.send(ev.clone()).await;
                    }
                }
            }
        }
    }

    // Final teardown: stop the current generation and actually wait for it,
    // so control channels and connection pools shut down cleanly.
    if let Some(tx) = instance_shutdown_tx.take() {
        let _ = tx.send(true);
    }
    if let Some((i, _)) = last_instance.take() {
        if let Err(e) = i.await {
            error!("The last instance task failed to join: {}", e);
        }
    }

    Ok(())
}

async fn run_instance(
    config: Config,
    client: bool,
    server: bool,
    mut shutdown_rxs: Vec<broadcast::Receiver<bool>>,
    mut service_updates: Vec<mpsc::Receiver<ConfigChange>>,
) -> Result<()> {
    match determine_run_mode(&config, client, server) {
        RunMode::Undetermine => bail!(
            "cannot determine the running mode: pass `--server` or `--client`, or add a [server]/[client] section to the config"
        ),
        RunMode::Client => {
            #[cfg(not(feature = "client"))]
            crate::helper::feature_not_compile("client");
            #[cfg(feature = "client")]
            run_client(
                config,
                shutdown_rxs.pop().expect("no shutdown receiver"),
                service_updates.pop().expect("no update receiver"),
            )
            .await
        }
        RunMode::Server => {
            #[cfg(not(feature = "server"))]
            crate::helper::feature_not_compile("server");
            #[cfg(feature = "server")]
            run_server(
                config,
                shutdown_rxs.pop().expect("no shutdown receiver"),
                service_updates.pop().expect("no update receiver"),
            )
            .await
        }
        RunMode::Both => {
            #[cfg(not(all(feature = "client", feature = "server")))]
            {
                let _ = (config, shutdown_rxs, service_updates);
                crate::helper::feature_not_compile("client and server")
            }
            #[cfg(all(feature = "client", feature = "server"))]
            {
                let client_task = run_client(
                    config.clone(),
                    shutdown_rxs.pop().expect("no client shutdown receiver"),
                    service_updates.pop().expect("no client update receiver"),
                );
                let server_task = run_server(
                    config,
                    shutdown_rxs.pop().expect("no server shutdown receiver"),
                    service_updates.pop().expect("no server update receiver"),
                );
                tokio::try_join!(client_task, server_task)?;
                Ok(())
            }
        }
    }
}

#[derive(PartialEq, Eq, Debug)]
enum RunMode {
    Server,
    Client,
    Both,
    Undetermine,
}

fn determine_run_mode(config: &Config, client: bool, server: bool) -> RunMode {
    use RunMode::*;
    if client && server {
        Undetermine
    } else if client {
        Client
    } else if server {
        Server
    } else if config.client.is_some() && config.server.is_some() {
        Both
    } else if config.client.is_some() && config.server.is_none() {
        Client
    } else if config.server.is_some() && config.client.is_none() {
        Server
    } else {
        Undetermine
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_determine_run_mode() {
        use config::*;
        use RunMode::*;

        struct T {
            cfg_s: bool,
            cfg_c: bool,
            arg_s: bool,
            arg_c: bool,
            run_mode: RunMode,
        }

        let tests = [
            T {
                cfg_s: false,
                cfg_c: false,
                arg_s: false,
                arg_c: false,
                run_mode: Undetermine,
            },
            T {
                cfg_s: true,
                cfg_c: false,
                arg_s: false,
                arg_c: false,
                run_mode: Server,
            },
            T {
                cfg_s: false,
                cfg_c: true,
                arg_s: false,
                arg_c: false,
                run_mode: Client,
            },
            T {
                cfg_s: true,
                cfg_c: true,
                arg_s: false,
                arg_c: false,
                run_mode: Both,
            },
            T {
                cfg_s: true,
                cfg_c: true,
                arg_s: true,
                arg_c: false,
                run_mode: Server,
            },
            T {
                cfg_s: true,
                cfg_c: true,
                arg_s: false,
                arg_c: true,
                run_mode: Client,
            },
            T {
                cfg_s: true,
                cfg_c: true,
                arg_s: true,
                arg_c: true,
                run_mode: Undetermine,
            },
        ];

        for t in tests {
            let config = Config {
                server: match t.cfg_s {
                    true => Some(ServerConfig::default()),
                    false => None,
                },
                client: match t.cfg_c {
                    true => Some(ClientConfig::default()),
                    false => None,
                },
            };

            assert_eq!(
                determine_run_mode(&config, t.arg_c, t.arg_s),
                t.run_mode
            );
        }
    }

    #[test]
    fn confirm_action_yes_short_circuits_without_tty() {
        // `--yes` proceeds without touching the terminal, so this is safe to
        // run in a non-interactive test harness.
        assert!(confirm_action::<cli::UpgradeArgs>(true, "upgrade", "summary").unwrap());
    }

    #[test]
    fn confirm_action_honors_confirmed_env() {
        std::env::set_var(CONFIRMED_ENV, "1");
        assert!(confirm_action::<cli::InstallArgs>(false, "install", "summary").unwrap());
        std::env::remove_var(CONFIRMED_ENV);
    }

    #[test]
    fn control_target_names_prefers_explicit_name() {
        assert_eq!(
            control_target_names(Some("svc")),
            vec!["svc".to_string()]
        );
    }
}

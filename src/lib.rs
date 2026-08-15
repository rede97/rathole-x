mod cli;
mod config;
mod config_edit;
mod config_watcher;
mod constants;
mod helper;
mod multi_map;
pub mod platform;
mod protocol;
mod runtime_status;
mod status;
mod transport;

pub use cli::{Cli, Commands, RunArgs};
pub use config::Config;
pub use constants::UDP_BUFFER_SIZE;

use anyhow::{bail, Context, Result};
use cli::KeypairType;
use serde_json::{json, Value};

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

/// Whether the active parsed command requested machine-readable output.
pub fn is_json_mode() -> bool {
    std::env::var_os("RATHOLE_X_JSON_MODE").is_some()
}

pub async fn run(args: Cli, shutdown_rx: broadcast::Receiver<bool>) -> Result<()> {
    let json_mode = args
        .command
        .as_ref()
        .map(command_requests_json)
        .unwrap_or(false);
    if json_mode {
        std::env::set_var("RATHOLE_X_JSON_MODE", "1");
        std::env::remove_var("RATHOLE_X_JSON_RELAYED");
    }
    let result = match args.command {
        Some(cmd) => dispatch_command(cmd, shutdown_rx).await,
        None => Err(anyhow::anyhow!(
            "No subcommand given; run `rathole-x --help`"
        )),
    };
    let relayed = json_mode && std::env::var_os("RATHOLE_X_JSON_RELAYED").is_some();
    if json_mode {
        std::env::remove_var("RATHOLE_X_JSON_MODE");
        std::env::remove_var("RATHOLE_X_JSON_RELAYED");
        if !relayed {
            match &result {
                Ok(result) => println!("{}", json_success_envelope(result)),
                Err(error) => println!("{}", json_failure_envelope(error)),
            }
        }
    }
    result.map(|_| ())
}

fn json_success_envelope(result: &Value) -> Value {
    json!({"ok": true, "result": result})
}

fn json_failure_envelope(error: &anyhow::Error) -> Value {
    json!({"ok": false, "error": {"message": format!("{:#}", error)}})
}

fn command_requests_json(cmd: &Commands) -> bool {
    match cmd {
        Commands::Config { cmd } => match cmd {
            cli::ConfigCmd::Add(a) => a.json,
            cli::ConfigCmd::Remove(a) => a.json,
            cli::ConfigCmd::List(a) => a.json,
            cli::ConfigCmd::Set(a) => a.json,
        },
        Commands::Status(a) => a.json,
        Commands::Service { cmd } => match cmd {
            cli::ServiceCmd::Install(a) => a.json,
            cli::ServiceCmd::Uninstall(a) => a.json,
            cli::ServiceCmd::Start(a) | cli::ServiceCmd::Stop(a) | cli::ServiceCmd::Restart(a) => {
                a.json
            }
            cli::ServiceCmd::Run { .. } => false,
        },
        Commands::Upgrade(a) => a.json,
        Commands::Run(_) | Commands::Genkey { .. } => false,
    }
}

async fn dispatch_command(cmd: Commands, shutdown_rx: broadcast::Receiver<bool>) -> Result<Value> {
    use Commands::*;

    match cmd {
        Run(run_args) => {
            let config_path = run_args
                .config
                .clone()
                .unwrap_or_else(os_default_config_path);
            run_with_config(config_path, run_args, shutdown_rx).await?;
            Ok(Value::Null)
        }
        Genkey { curve } => {
            genkey(curve)?;
            Ok(Value::Null)
        }
        Config { cmd } => dispatch_config_command(cmd).await,
        Status(s) => status::run_status(&s),
        Service { cmd } => dispatch_service_command(cmd),
        Upgrade(u) => {
            confirm_action(u.yes, u.json, "upgrade", "Replace the installed rathole-x binary in place and restart every installed service. Proceed?")?;
            platform::upgrade_binary()?;
            Ok(json!({"action": "upgrade", "message": "binary upgraded"}))
        }
    }
}

async fn dispatch_config_command(cmd: cli::ConfigCmd) -> Result<Value> {
    use cli::ConfigCmd::*;
    match cmd {
        Add(a) => {
            let path = config_edit::resolve_service_config(a.config.as_ref(), a.name.as_deref())?;
            config_edit::check_version_compat(&path)?;
            if platform::elevate_for_config_if_needed(&path)? {
                return Ok(json!({"relayed": true}));
            }
            config_edit::run_add(&a, &path)
        }
        Remove(r) => {
            let path = config_edit::resolve_service_config(r.config.as_ref(), r.name.as_deref())?;
            config_edit::check_version_compat(&path)?;
            confirm_action(
                r.yes,
                r.json,
                "config remove",
                &format!(
                    "Remove service '{}' from {}. Proceed?",
                    r.entry,
                    path.display()
                ),
            )?;
            if platform::elevate_for_config_if_needed(&path)? {
                return Ok(json!({"relayed": true}));
            }
            config_edit::run_remove(&r, &path)
        }
        List(l) => {
            let path = config_edit::resolve_service_config(l.config.as_ref(), l.name.as_deref())?;
            config_edit::run_list(&l, &path)
        }
        Set(s) => {
            let path = config_edit::resolve_service_config(s.config.as_ref(), s.name.as_deref())?;
            config_edit::check_version_compat(&path)?;
            if platform::elevate_for_config_if_needed(&path)? {
                return Ok(json!({"relayed": true}));
            }
            config_edit::run_set(&s, &path)
        }
    }
}

/// Dispatches `rathole-x service <action>`: install, uninstall, control
/// and the hidden SCM run entry.
fn dispatch_service_command(cmd: cli::ServiceCmd) -> Result<Value> {
    use cli::ServiceCmd::*;
    match cmd {
        Install(i) => {
            let role = match i.role {
                Some(r) => config_edit::ServiceRole::from_cli(r),
                None => bail!(
                    "A role is required: `service install server` or `service install client`"
                ),
            };
            let (name, config_path) = match &i.config {
                Some(p) => {
                    let path = std::env::current_dir()
                        .context("failed to resolve the current directory for `--config`")?
                        .join(p);
                    let name = match &i.name {
                        Some(n) => n.clone(),
                        None => path
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .map(str::to_owned)
                            .ok_or_else(|| {
                                anyhow::anyhow!(
                                    "cannot derive a service name from `{}`; pass `--name`",
                                    path.display()
                                )
                            })?,
                    };
                    config_edit::validate_service_name(&name)?;
                    (name, path)
                }
                None => config_edit::service_config_path(i.name.as_deref())?,
            };
            confirm_action(
                i.yes,
                i.json,
                "install",
                &format!(
                    "Install a {} service '{}' (SCM name rathole-x-{}-{}) with config {}. Proceed?",
                    role.key(),
                    name,
                    role.key(),
                    name,
                    config_path.display()
                ),
            )?;
            let created = config_edit::ensure_role_config(&config_path, role)?;
            if created && !i.json {
                println!(
                    "Created {} config at {} (no services configured yet).",
                    role.key(),
                    config_path.display()
                );
            }
            platform::install_service(&i, role, &name, &config_path)?;
            Ok(json!({
                "action": "install",
                "names": [name],
                "role": role.key(),
                "config": config_path,
                "created": created,
                "message": "service installed",
            }))
        }
        Uninstall(u) => {
            if u.all {
                let names: Vec<String> = config_edit::list_installed_services()?
                    .into_iter()
                    .map(|(name, _)| name)
                    .collect();
                confirm_action(
                    u.yes,
                    u.json,
                    "uninstall",
                    &format!(
                        "Remove EVERY installed rathole-x service ({}), their configs and the shared binary. Proceed?",
                        if names.is_empty() { "none".to_owned() } else { names.join(", ") }
                    ),
                )?;
                platform::uninstall_all(&u)?;
                return Ok(json!({
                    "action": "uninstall",
                    "names": names,
                    "all": true,
                    "message": "all services uninstalled",
                }));
            }
            let config_path =
                config_edit::resolve_service_config(u.config.as_ref(), u.name.as_deref())?;
            let name = config_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("?")
                .to_owned();
            confirm_action(
                u.yes,
                u.json,
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
            )?;
            platform::uninstall_service(&u, &config_path)?;
            Ok(json!({
                "action": "uninstall",
                "names": [name],
                "purge": u.purge,
                "message": "service uninstalled",
            }))
        }
        Run { config } => {
            platform::run_service(config)?;
            Ok(Value::Null)
        }
        other => {
            let (action, names, all) = match &other {
                Start(args) => ("start", service_action_names(args)?, args.all),
                Stop(args) => ("stop", service_action_names(args)?, args.all),
                Restart(args) => ("restart", service_action_names(args)?, args.all),
                _ => unreachable!("install, uninstall, and run are handled above"),
            };
            platform::control_service(other)?;
            Ok(json!({
                "action": action,
                "names": names,
                "all": all,
                "message": "ok",
            }))
        }
    }
}

fn service_action_names(args: &cli::ServiceArgs) -> Result<Vec<String>> {
    match &args.name {
        Some(name) => Ok(vec![name.clone()]),
        None => Ok(config_edit::list_installed_services()?
            .into_iter()
            .map(|(name, _)| name)
            .collect()),
    }
}

/// Environment variable set after a successful interactive confirmation.
/// The UAC elevation relay forwards this state through the hidden
/// `--confirmed` flag because environment inheritance across UAC is not
/// guaranteed.
pub const CONFIRMED_ENV: &str = "RATHOLE_X_CONFIRMED";

/// Gate destructive actions. JSON and unattended invocations cannot prompt:
/// they fail with an actionable error instead of producing usage/no-op output.
fn confirm_action(yes: bool, json: bool, subcommand: &str, summary: &str) -> Result<()> {
    if yes || std::env::var_os(CONFIRMED_ENV).is_some() {
        return Ok(());
    }
    if json || !(atty::is(atty::Stream::Stdin) && atty::is(atty::Stream::Stdout)) {
        bail!(
            "`{}` requires confirmation; re-run with --yes in an unattended or --json invocation",
            subcommand
        );
    }
    let confirmed = dialoguer::Confirm::new()
        .with_prompt(summary.to_string())
        .default(false)
        .interact()
        .context("failed to read the confirmation prompt")?;
    if !confirmed {
        bail!("{} cancelled by user", subcommand);
    }
    std::env::set_var(CONFIRMED_ENV, "1");
    Ok(())
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

    // The last single-role instance and its one service-update channel.
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
                if let Some((instance, _)) = last_instance.take() {
                    info!("General configuration change detected. Restarting...");
                    if let Some(tx) = instance_shutdown_tx.take() {
                        let _ = tx.send(true);
                    }
                    if let Err(error) = instance.await {
                        error!("The last instance task failed to join: {}", error);
                    }
                }
                debug!("{:?}", config);
                let (itx, _) = broadcast::channel(1);
                let (service_update_tx, service_update_rx) = mpsc::channel(1024);
                instance_shutdown_tx = Some(itx.clone());
                let client = run_args.client;
                let server = run_args.server;
                let instance_config_path = config_path.clone();
                last_instance = Some((
                    tokio::spawn(async move {
                        let result = run_instance(
                            instance_config_path,
                            *config,
                            client,
                            server,
                            vec![itx.subscribe()],
                            vec![service_update_rx],
                        )
                        .await;
                        if let Err(error) = &result {
                            error!("The instance exited with an error: {:#}", error);
                        }
                        result
                    }),
                    vec![service_update_tx],
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
    config_path: std::path::PathBuf,
    config: Config,
    client: bool,
    server: bool,
    mut shutdown_rxs: Vec<broadcast::Receiver<bool>>,
    mut service_updates: Vec<mpsc::Receiver<ConfigChange>>,
) -> Result<()> {
    match determine_run_mode(&config, client, server) {
        RunMode::Ambiguous => bail!(
            "configuration declares both [client] and [server]; select exactly one role with `run --client` or `run --server`, or split it into separate configs"
        ),
        RunMode::Undetermine => bail!(
            "cannot determine the running mode: pass `--server` or `--client`, or add exactly one [server]/[client] section to the config"
        ),
        RunMode::Client => {
            #[cfg(not(feature = "client"))]
            crate::helper::feature_not_compile("client");
            #[cfg(feature = "client")]
            {
                let client_config = config.client.as_ref().expect("client mode has client config");
                let registry = runtime_status::client_registry(
                    &client_config.remote_addr,
                    client_config.services.keys().cloned(),
                );
                if let Err(error) = platform::spawn_runtime_status_server(
                    config_path.clone(),
                    registry.clone(),
                    shutdown_rxs.last().expect("no shutdown receiver").resubscribe(),
                ) {
                    warn!("runtime status endpoint unavailable: {:#}", error);
                }
                run_client(
                    config,
                    shutdown_rxs.pop().expect("no shutdown receiver"),
                    service_updates.pop().expect("no update receiver"),
                    registry,
                )
                .await
            }
        }
        RunMode::Server => {
            #[cfg(not(feature = "server"))]
            crate::helper::feature_not_compile("server");
            #[cfg(feature = "server")]
            {
                let server_config = config.server.as_ref().expect("server mode has server config");
                let registry =
                    runtime_status::server_registry(server_config.services.keys().cloned());
                if let Err(error) = platform::spawn_runtime_status_server(
                    config_path,
                    registry.clone(),
                    shutdown_rxs.last().expect("no shutdown receiver").resubscribe(),
                ) {
                    warn!("runtime status endpoint unavailable: {:#}", error);
                }
                run_server(
                    config,
                    shutdown_rxs.pop().expect("no shutdown receiver"),
                    service_updates.pop().expect("no update receiver"),
                    registry,
                )
                .await
            }
        }
    }
}

#[derive(PartialEq, Eq, Debug)]
enum RunMode {
    Server,
    Client,
    Ambiguous,
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
        Ambiguous
    } else if config.client.is_some() {
        Client
    } else if config.server.is_some() {
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
                run_mode: Ambiguous,
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

            assert_eq!(determine_run_mode(&config, t.arg_c, t.arg_s), t.run_mode);
        }
    }

    #[test]
    fn confirm_action_yes_short_circuits_without_tty() {
        assert!(confirm_action(true, true, "upgrade", "summary").is_ok());
    }

    #[test]
    fn confirm_action_honors_confirmed_env() {
        std::env::set_var(CONFIRMED_ENV, "1");
        assert!(confirm_action(false, true, "install", "summary").is_ok());
        std::env::remove_var(CONFIRMED_ENV);
    }

    #[test]
    fn json_envelopes_have_one_stable_shape() {
        assert_eq!(
            json_success_envelope(&json!({"services": []})),
            json!({"ok": true, "result": {"services": []}})
        );
        let error = anyhow::anyhow!("bad input");
        assert_eq!(
            json_failure_envelope(&error),
            json!({"ok": false, "error": {"message": "bad input"}})
        );
    }
}

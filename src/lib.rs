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
use anyhow::{bail, Result};

use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info, warn};

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

        Install(i) => {
            if !i.yes {
                return print_subcommand_usage::<cli::InstallArgs>("install");
            }
            let role = match i.role {
                Some(r) => config_edit::ServiceRole::from_cli(r),
                None => bail!("A role is required: `install server` or `install client`"),
            };
            let (name, config_path) = config_edit::service_config_path(i.name.as_deref())?;
            let created = config_edit::ensure_role_config(&config_path, role)?;
            if created {
                println!(
                    "Created {} config at {} (no services configured yet).",
                    role.key(),
                    config_path.display()
                );
            }
            platform::install_service(&i, role, &name, &config_path)?;

            Ok(())
        }
        Uninstall(u) => {
            if !u.yes {
                return print_subcommand_usage::<cli::UninstallArgs>("uninstall");
            }
            if u.all {
                platform::uninstall_all(&u)?;
                return Ok(());
            }
            let config_path = config_edit::resolve_service_config(
                u.config.as_ref(),
                u.name.as_deref(),
            )?;
            platform::uninstall_service(&u, &config_path)?;

            Ok(())
        }
        Service { cmd } => Ok(platform::control_service(cmd)?),
        Upgrade(u) => {
            if !u.yes {
                return print_subcommand_usage::<cli::UpgradeArgs>("upgrade");
            }
            platform::upgrade_binary()?;
            Ok(())
        }
        ServiceRun { config } => platform::run_service(config),
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

/// Print the usage of a subcommand built from its `Args` type. Used when a
/// dangerous subcommand runs without its confirmation flag.
fn print_subcommand_usage<T: clap::Args>(name: &str) -> Result<()> {
    let mut cmd = T::augment_args(clap::Command::new(name));
    cmd.print_help()?;
    println!();
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

    // (The join handle of the last instance, the service update channels.)
    // A single process can host both a client and a server instance
    // (RunMode::Both), each with its own update channel; service events are
    // fanned out to every half.
    let mut last_instance: Option<(
        tokio::task::JoinHandle<Result<()>>,
        Vec<mpsc::Sender<ConfigChange>>,
    )> = None;

    while let Some(e) = cfg_watcher.event_rx.recv().await {
        match e {
            ConfigChange::General(config) => {
                if let Some((i, _)) = last_instance.take() {
                    info!("General configuration change detected. Restarting...");
                    let _ = shutdown_tx.send(true);
                    i.await??;
                }

                debug!("{:?}", config);

                let halves = if determine_run_mode(&config, run_args.client, run_args.server)
                    == RunMode::Both
                {
                    2
                } else {
                    1
                };

                let mut service_update_txs = Vec::with_capacity(halves);
                let mut service_update_rxs = Vec::with_capacity(halves);
                let mut shutdown_rxs = Vec::with_capacity(halves);
                for _ in 0..halves {
                    let (service_update_tx, service_update_rx) = mpsc::channel(1024);
                    service_update_txs.push(service_update_tx);
                    service_update_rxs.push(service_update_rx);
                    shutdown_rxs.push(shutdown_tx.subscribe());
                }

                last_instance = Some((
                    tokio::spawn(run_instance(
                        *config,
                        run_args.client,
                        run_args.server,
                        shutdown_rxs,
                        service_update_rxs,
                    )),
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

    let _ = shutdown_tx.send(true);

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
        RunMode::Undetermine => panic!("Cannot determine running as a server or a client"),
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
}

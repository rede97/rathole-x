//! H1 regression: a General configuration change (here: a server
//! `heartbeat_interval` change) must restart that single-role instance
//! **in-process** via the config watcher. The client and server intentionally
//! run as separate processes, matching the one-process-one-role contract.
//!
//! Contract asserted here:
//!   1. both `rathole::run` tasks remain alive after the server General change;
//!   2. a newly configured service takes effect end-to-end;
//!   3. the previously running service survives the restart.
#![cfg(all(feature = "client", feature = "server", feature = "hot-reload"))]

use std::time::Duration;

use anyhow::{bail, Result};
use common::tcp::echo_server;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::broadcast,
    time,
};

mod common;

const CONTROL_ADDR: &str = "127.0.0.1:12340";
const ECHO_EXPOSED: &str = "127.0.0.1:12341";
const ECHO_LOCAL: &str = "127.0.0.1:12342";
const ECHO2_EXPOSED: &str = "127.0.0.1:12343";
const ECHO2_LOCAL: &str = "127.0.0.1:12344";

const READY_TIMEOUT: Duration = Duration::from_secs(15);
const PROBE_TIMEOUT: Duration = Duration::from_millis(500);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

const TOKEN: &str = "hot_reload_test_token";

fn initial_server_config() -> String {
    format!(
        "\
[server]
bind_addr = \"{CONTROL_ADDR}\"
default_token = \"{TOKEN}\"
heartbeat_interval = 30

[server.transport]
type = \"tcp\"

[server.services.echo]
bind_addr = \"{ECHO_EXPOSED}\"
"
    )
}

fn initial_client_config() -> String {
    format!(
        "\
[client]
remote_addr = \"{CONTROL_ADDR}\"
default_token = \"{TOKEN}\"

[client.transport]
type = \"tcp\"

[client.services.echo]
local_addr = \"{ECHO_LOCAL}\"
"
    )
}

/// The server heartbeat 30 -> 15 is a General event. The second forwarding
/// service is added to both independently configured processes.
fn reloaded_server_config() -> String {
    format!(
        "\
[server]
bind_addr = \"{CONTROL_ADDR}\"
default_token = \"{TOKEN}\"
heartbeat_interval = 15

[server.transport]
type = \"tcp\"

[server.services.echo]
bind_addr = \"{ECHO_EXPOSED}\"

[server.services.echo2]
bind_addr = \"{ECHO2_EXPOSED}\"
"
    )
}

fn reloaded_client_config() -> String {
    format!(
        "\
[client]
remote_addr = \"{CONTROL_ADDR}\"
default_token = \"{TOKEN}\"

[client.transport]
type = \"tcp\"

[client.services.echo]
local_addr = \"{ECHO_LOCAL}\"

[client.services.echo2]
local_addr = \"{ECHO2_LOCAL}\"
"
    )
}

#[tokio::test]
async fn general_change_restarts_instance_in_process() -> Result<()> {
    let dir =
        std::env::temp_dir().join(format!("rathole-x-hot-reload-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let server_config_path = dir.join("server.toml");
    let client_config_path = dir.join("client.toml");
    std::fs::write(&server_config_path, initial_server_config())?;
    std::fs::write(&client_config_path, initial_client_config())?;

    for addr in [ECHO_LOCAL, ECHO2_LOCAL] {
        tokio::spawn(async move {
            if let Err(e) = echo_server(addr).await {
                panic!("Failed to run the echo server for testing: {:?}", e);
            }
        });
    }

    let (shutdown_tx, _) = broadcast::channel(1);
    let server_cli = rathole::Cli {
        command: Some(rathole::Commands::Run(rathole::RunArgs {
            config: Some(server_config_path.clone()),
            server: true,
            ..Default::default()
        })),
        ..Default::default()
    };
    let client_cli = rathole::Cli {
        command: Some(rathole::Commands::Run(rathole::RunArgs {
            config: Some(client_config_path.clone()),
            client: true,
            ..Default::default()
        })),
        ..Default::default()
    };
    let server_handle = tokio::spawn(rathole::run(server_cli, shutdown_tx.subscribe()));
    let client_handle = tokio::spawn(rathole::run(client_cli, shutdown_tx.subscribe()));

    wait_forwarding(ECHO_EXPOSED).await?;
    assert!(
        !server_handle.is_finished(),
        "server run exited while initial config was active"
    );
    assert!(
        !client_handle.is_finished(),
        "client run exited while initial config was active"
    );

    // The server General change restarts only the server-role instance;
    // client echo2 is added through its own independently watched config.
    std::fs::write(&server_config_path, reloaded_server_config())?;
    std::fs::write(&client_config_path, reloaded_client_config())?;

    wait_forwarding(ECHO2_EXPOSED).await?;
    assert!(
        !server_handle.is_finished(),
        "server run exited after a General config change (H1 regression)"
    );
    assert!(
        !client_handle.is_finished(),
        "client run exited while its separate config was updated"
    );
    wait_forwarding(ECHO_EXPOSED).await?;

    shutdown_tx.send(true)?;
    for (role, handle) in [("server", server_handle), ("client", client_handle)] {
        match time::timeout(SHUTDOWN_TIMEOUT, handle).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(e))) => {
                let _ = std::fs::remove_dir_all(&dir);
                bail!("{} run returned an error on shutdown: {:#}", role, e);
            }
            Ok(Err(e)) => {
                let _ = std::fs::remove_dir_all(&dir);
                bail!("{} run task panicked: {}", role, e);
            }
            Err(_) => {
                let _ = std::fs::remove_dir_all(&dir);
                bail!(
                    "{} run did not stop within {:?} after shutdown",
                    role,
                    SHUTDOWN_TIMEOUT
                );
            }
        }
    }

    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// Poll until a full echo roundtrip through the exposed service succeeds,
/// i.e. the client control channel for it is authenticated and forwarding.
async fn wait_forwarding(exposed: &str) -> Result<()> {
    let deadline = time::Instant::now() + READY_TIMEOUT;
    loop {
        if matches!(
            time::timeout(PROBE_TIMEOUT, echo_roundtrip(exposed)).await,
            Ok(Ok(()))
        ) {
            return Ok(());
        }
        if time::Instant::now() >= deadline {
            bail!(
                "service at {} not forwarding within {:?}",
                exposed,
                READY_TIMEOUT
            );
        }
        time::sleep(Duration::from_millis(100)).await;
    }
}

async fn echo_roundtrip(addr: &str) -> Result<()> {
    const PROBE: &[u8] = b"rathole";
    let mut conn = TcpStream::connect(addr).await?;
    conn.write_all(PROBE).await?;
    let mut rd = [0u8; PROBE.len()];
    conn.read_exact(&mut rd).await?;
    assert_eq!(&rd, PROBE);
    Ok(())
}

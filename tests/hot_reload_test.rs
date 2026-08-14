//! H1 regression: a General configuration change (here: `heartbeat_interval`
//! plus a service addition) must restart the instance **in-process** via the
//! config watcher. Before the fix, the watcher and the instance shared one
//! shutdown channel, so any General event made the whole `run` task exit
//! cleanly (taking the SCM service down with exit code 0).
//!
//! Contract asserted here:
//!   1. the `rathole::run` task is still alive after a General change;
//!   2. the new configuration takes effect (a newly added service port
//!      accepts and forwards traffic);
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

fn initial_config() -> String {
    format!(
        "\
[client]
remote_addr = \"{CONTROL_ADDR}\"
default_token = \"{TOKEN}\"

[client.transport]
type = \"tcp\"

[client.services.echo]
local_addr = \"{ECHO_LOCAL}\"

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

/// `heartbeat_interval` 30 -> 15 is a non-service change, i.e. a General
/// event; the added `echo2` service verifies the new config is live.
fn reloaded_config() -> String {
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

#[tokio::test]
async fn general_change_restarts_instance_in_process() -> Result<()> {
    let dir = std::env::temp_dir().join(format!(
        "rathole-x-hot-reload-test-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir)?;
    let config_path = dir.join("rathole.toml");
    std::fs::write(&config_path, initial_config())?;

    for addr in [ECHO_LOCAL, ECHO2_LOCAL] {
        tokio::spawn(async move {
            if let Err(e) = echo_server(addr).await {
                panic!("Failed to run the echo server for testing: {:?}", e);
            }
        });
    }

    let (shutdown_tx, shutdown_rx) = broadcast::channel(1);
    let cli = rathole::Cli {
        command: Some(rathole::Commands::Run(rathole::RunArgs {
            config: Some(config_path.clone()),
            // No --client/--server flag: with both sections present the
            // process hosts both halves (RunMode::Both).
            ..Default::default()
        })),
        ..Default::default()
    };
    let handle = tokio::spawn(rathole::run(cli, shutdown_rx));

    // Initial instance must come up end-to-end.
    wait_forwarding(ECHO_EXPOSED).await?;
    assert!(
        !handle.is_finished(),
        "run exited while the initial config was active"
    );

    // Trigger a General change (+ one added service).
    std::fs::write(&config_path, reloaded_config())?;

    // The new config must take effect: the added service forwards traffic.
    wait_forwarding(ECHO2_EXPOSED).await?;

    // H1: the process must still be running after the General change.
    assert!(
        !handle.is_finished(),
        "run exited after a General config change (H1 regression)"
    );

    // The pre-existing service must survive the in-process restart.
    wait_forwarding(ECHO_EXPOSED).await?;

    shutdown_tx.send(true)?;
    match time::timeout(SHUTDOWN_TIMEOUT, handle).await {
        Ok(Ok(Ok(()))) => {}
        Ok(Ok(Err(e))) => {
            let _ = std::fs::remove_dir_all(&dir);
            bail!("run returned an error on shutdown: {:#}", e);
        }
        Ok(Err(e)) => {
            let _ = std::fs::remove_dir_all(&dir);
            bail!("run task panicked: {}", e);
        }
        Err(_) => {
            let _ = std::fs::remove_dir_all(&dir);
            bail!("run did not stop within {:?} after shutdown", SHUTDOWN_TIMEOUT);
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
        if matches!(time::timeout(PROBE_TIMEOUT, echo_roundtrip(exposed)).await, Ok(Ok(()))) {
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

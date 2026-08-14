//! Wrong-token rejection: a client whose token does not match the server's
//! must never get a working tunnel, and must back off and retry instead of
//! crashing (the server only binds a service port after the control channel
//! for that service is authenticated, so a rejected client leaves the
//! exposed port dead).
#![cfg(all(feature = "client", feature = "server"))]

use std::time::Duration;

use anyhow::{bail, Result};
use common::{run_rathole_client, run_rathole_server};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::broadcast,
    time,
};

mod common;

const SERVER_CONFIG: &str = "tests/for_auth/server.toml";
const CLIENT_CONFIG: &str = "tests/for_auth/client.toml";
const SERVER_CONTROL_ADDR: &str = "127.0.0.1:12333";
const ECHO_SERVER_ADDR: &str = "127.0.0.1:12081";
const ECHO_EXPOSED_ADDR: &str = "127.0.0.1:12080";

const IO_TIMEOUT: Duration = Duration::from_secs(5);
const READY_TIMEOUT: Duration = Duration::from_secs(10);
/// With the default 1s client retry interval this window covers several
/// failed authentication attempts.
const OBSERVE_WINDOW: Duration = Duration::from_secs(5);

#[tokio::test]
async fn wrong_token_is_rejected() -> Result<()> {
    // The local echo server the client would forward to if it were let in.
    tokio::spawn(async move {
        if let Err(e) = common::tcp::echo_server(ECHO_SERVER_ADDR).await {
            panic!("Failed to run the echo server for testing: {:?}", e);
        }
    });

    let (server_shutdown_tx, server_shutdown_rx) = broadcast::channel(1);
    let (client_shutdown_tx, client_shutdown_rx) = broadcast::channel(1);

    let server = tokio::spawn(async move {
        run_rathole_server(SERVER_CONFIG, server_shutdown_rx).await
    });
    wait_control_listener().await?;

    let client = tokio::spawn(async move {
        run_rathole_client(CLIENT_CONFIG, client_shutdown_rx).await
    });

    // Let the client burn through several retry rounds with its bad token.
    time::sleep(OBSERVE_WINDOW).await;

    // The forwarding path must stay dead.
    assert!(
        !forwarding_alive().await,
        "echo roundtrip succeeded through a tunnel authenticated with the wrong token"
    );

    // Neither side may have died: the client backs off and retries, the
    // server keeps listening for (other) clients.
    assert!(
        !client.is_finished(),
        "client exited instead of retrying after auth failure"
    );
    assert!(
        !server.is_finished(),
        "server exited after rejecting a client"
    );

    server_shutdown_tx.send(true)?;
    client_shutdown_tx.send(true)?;
    let _ = time::timeout(IO_TIMEOUT, async {
        let _ = tokio::join!(server, client);
    })
    .await;

    Ok(())
}

/// Wait until the server accepts connections on its control channel
/// address, so the client's auth failures are really answered by the
/// server (not by a refused connection).
async fn wait_control_listener() -> Result<()> {
    let deadline = time::Instant::now() + READY_TIMEOUT;
    loop {
        if let Ok(Ok(_)) = time::timeout(IO_TIMEOUT, TcpStream::connect(SERVER_CONTROL_ADDR)).await
        {
            return Ok(());
        }
        if time::Instant::now() >= deadline {
            bail!(
                "server control listener at {} not up within {:?}",
                SERVER_CONTROL_ADDR,
                READY_TIMEOUT
            );
        }
        time::sleep(Duration::from_millis(100)).await;
    }
}

/// Probe the exposed echo service. Alive means a full echo roundtrip;
/// anything else (refused connect, accept without data, timeout) counts as
/// dead, because the server must not bind the port for a rejected service.
async fn forwarding_alive() -> bool {
    let probe = async {
        let mut conn = TcpStream::connect(ECHO_EXPOSED_ADDR).await?;
        const PROBE: &[u8] = b"rathole";
        conn.write_all(PROBE).await?;
        let mut rd = [0u8; PROBE.len()];
        conn.read_exact(&mut rd).await?;
        anyhow::Ok(rd == PROBE)
    };
    matches!(time::timeout(IO_TIMEOUT, probe).await, Ok(Ok(true)))
}

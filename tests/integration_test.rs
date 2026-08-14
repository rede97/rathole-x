#![cfg(all(feature = "client", feature = "server"))]
// ^ These tests drive a rathole client and server in the same process to
// exercise the full forwarding path. When only one side is compiled in
// there is nothing meaningful to run, so gate the whole file at compile
// time. (The previous runtime `return Ok(())` skip reported a fake pass.)

use anyhow::{bail, Result};
use common::{run_rathole_client, PING, PONG};
use rand::Rng;
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpStream, UdpSocket},
    sync::broadcast,
    time,
};
use tracing::{debug, info, instrument};
use tracing_subscriber::EnvFilter;

use crate::common::run_rathole_server;

mod common;

const ECHO_SERVER_ADDR: &str = "127.0.0.1:8080";
const PINGPONG_SERVER_ADDR: &str = "127.0.0.1:8081";
const ECHO_SERVER_ADDR_EXPOSED: &str = "127.0.0.1:2334";
const PINGPONG_SERVER_ADDR_EXPOSED: &str = "127.0.0.1:2335";
const HITTER_NUM: usize = 4;

/// Upper bound for any single I/O step in the hitters below. A broken
/// transport must fail fast instead of hanging the CI job for hours.
const IO_TIMEOUT: Duration = Duration::from_secs(10);

/// Overall budget for the tunnel to come up after a (re)start: the client
/// may need several retry rounds to (re)establish its control channels.
const READY_TIMEOUT: Duration = Duration::from_secs(20);

/// Timeout of a single readiness probe attempt.
const PROBE_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug)]
enum Type {
    Tcp,
    Udp,
}

fn init() {
    let level = "info";
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::from(level)),
        )
        .try_init();
}

#[tokio::test]
async fn tcp() -> Result<()> {
    init();

    // Spawn a echo server
    tokio::spawn(async move {
        if let Err(e) = common::tcp::echo_server(ECHO_SERVER_ADDR).await {
            panic!("Failed to run the echo server for testing: {:?}", e);
        }
    });

    // Spawn a pingpong server
    tokio::spawn(async move {
        if let Err(e) = common::tcp::pingpong_server(PINGPONG_SERVER_ADDR).await {
            panic!("Failed to run the pingpong server for testing: {:?}", e);
        }
    });

    test("tests/for_tcp/tcp_transport.toml", Type::Tcp).await?;

    #[cfg(any(
         // FIXME: Self-signed certificate on macOS nativetls requires manual interference.
         all(target_os = "macos", feature = "rustls"),
         // On other OS accept run with either
         all(not(target_os = "macos"), any(feature = "native-tls", feature = "rustls")),
     ))]
    test("tests/for_tcp/tls_transport.toml", Type::Tcp).await?;

    #[cfg(feature = "noise")]
    test("tests/for_tcp/noise_transport.toml", Type::Tcp).await?;

    #[cfg(any(feature = "websocket-native-tls", feature = "websocket-rustls"))]
    test("tests/for_tcp/websocket_transport.toml", Type::Tcp).await?;

    #[cfg(not(target_os = "macos"))]
    #[cfg(any(feature = "websocket-native-tls", feature = "websocket-rustls"))]
    test("tests/for_tcp/websocket_tls_transport.toml", Type::Tcp).await?;

    Ok(())
}

#[tokio::test]
async fn udp() -> Result<()> {
    init();

    // Spawn a echo server
    tokio::spawn(async move {
        if let Err(e) = common::udp::echo_server(ECHO_SERVER_ADDR).await {
            panic!("Failed to run the echo server for testing: {:?}", e);
        }
    });

    // Spawn a pingpong server
    tokio::spawn(async move {
        if let Err(e) = common::udp::pingpong_server(PINGPONG_SERVER_ADDR).await {
            panic!("Failed to run the pingpong server for testing: {:?}", e);
        }
    });

    test("tests/for_udp/tcp_transport.toml", Type::Udp).await?;

    #[cfg(any(
         // FIXME: Self-signed certificate on macOS nativetls requires manual interference.
         all(target_os = "macos", feature = "rustls"),
         // On other OS accept run with either
         all(not(target_os = "macos"), any(feature = "native-tls", feature = "rustls")),
     ))]
    test("tests/for_udp/tls_transport.toml", Type::Udp).await?;

    #[cfg(feature = "noise")]
    test("tests/for_udp/noise_transport.toml", Type::Udp).await?;

    #[cfg(any(feature = "websocket-native-tls", feature = "websocket-rustls"))]
    test("tests/for_udp/websocket_transport.toml", Type::Udp).await?;

    #[cfg(not(target_os = "macos"))]
    #[cfg(any(feature = "websocket-native-tls", feature = "websocket-rustls"))]
    test("tests/for_udp/websocket_tls_transport.toml", Type::Udp).await?;

    Ok(())
}

#[instrument]
async fn test(config_path: &'static str, t: Type) -> Result<()> {
    let (client_shutdown_tx, client_shutdown_rx) = broadcast::channel(1);
    let (server_shutdown_tx, server_shutdown_rx) = broadcast::channel(1);

    // Start the client
    info!("start the client");
    let client = tokio::spawn(async move {
        run_rathole_client(config_path, client_shutdown_rx)
            .await
            .unwrap();
    });

    // Deliberate stagger (not a readiness wait): the client must attempt to
    // reach the server while it is still down, exercising the retry path.
    time::sleep(Duration::from_secs(1)).await;

    // Start the server
    info!("start the server");
    let server = tokio::spawn(async move {
        run_rathole_server(config_path, server_shutdown_rx)
            .await
            .unwrap();
    });
    // Wait until the retried client has established its control channels
    wait_ready(t).await?;

    info!("echo");
    echo_hitter(ECHO_SERVER_ADDR_EXPOSED, t).await.unwrap();
    info!("pingpong");
    pingpong_hitter(PINGPONG_SERVER_ADDR_EXPOSED, t)
        .await
        .unwrap();

    // Simulate the client crash and restart
    info!("shutdown the client");
    client_shutdown_tx.send(true)?;
    let _ = tokio::join!(client);

    info!("restart the client");
    let client_shutdown_rx = client_shutdown_tx.subscribe();
    let client = tokio::spawn(async move {
        run_rathole_client(config_path, client_shutdown_rx)
            .await
            .unwrap();
    });
    // Wait until the restarted client is forwarding again
    wait_ready(t).await?;

    info!("echo");
    echo_hitter(ECHO_SERVER_ADDR_EXPOSED, t).await.unwrap();
    info!("pingpong");
    pingpong_hitter(PINGPONG_SERVER_ADDR_EXPOSED, t)
        .await
        .unwrap();

    // Simulate the server crash and restart
    info!("shutdown the server");
    server_shutdown_tx.send(true)?;
    let _ = tokio::join!(server);

    info!("restart the server");
    let server_shutdown_rx = server_shutdown_tx.subscribe();
    let server = tokio::spawn(async move {
        run_rathole_server(config_path, server_shutdown_rx)
            .await
            .unwrap();
    });
    // Wait until the client has retried and re-established the tunnel
    wait_ready(t).await?;

    // Simulate heavy load
    info!("lots of echo and pingpong");

    let mut v = Vec::new();

    for _ in 0..HITTER_NUM / 2 {
        v.push(tokio::spawn(async move {
            echo_hitter(ECHO_SERVER_ADDR_EXPOSED, t).await.unwrap();
        }));

        v.push(tokio::spawn(async move {
            pingpong_hitter(PINGPONG_SERVER_ADDR_EXPOSED, t)
                .await
                .unwrap();
        }));
    }

    for h in v {
        assert!(tokio::join!(h).0.is_ok());
    }

    // Shutdown
    info!("shutdown the server and the client");
    server_shutdown_tx.send(true)?;
    client_shutdown_tx.send(true)?;

    let _ = tokio::join!(server, client);

    Ok(())
}

/// Poll both exposed services until a real echo/pingpong roundtrip
/// succeeds, i.e. the client has (re)connected and its per-service control
/// channels are up. Replaces fixed sleeps, which were flaky on slow
/// machines and wasteful on fast ones.
async fn wait_ready(t: Type) -> Result<()> {
    let deadline = time::Instant::now() + READY_TIMEOUT;
    // Control-channel replacement tears the old UDP pool down while the new
    // one is still binding, so a single successful probe can fall into a
    // transition gap. Require several consecutive successful rounds before
    // declaring the tunnel ready.
    const STABLE_ROUNDS: u32 = 3;
    let mut stable = 0;
    loop {
        let echo = time::timeout(PROBE_TIMEOUT, probe_echo(ECHO_SERVER_ADDR_EXPOSED, t)).await;
        let pingpong = time::timeout(
            PROBE_TIMEOUT,
            probe_pingpong(PINGPONG_SERVER_ADDR_EXPOSED, t),
        )
        .await;
        if matches!(echo, Ok(Ok(()))) && matches!(pingpong, Ok(Ok(()))) {
            stable += 1;
            if stable >= STABLE_ROUNDS {
                return Ok(());
            }
            // Space out the confirmation rounds
            time::sleep(Duration::from_millis(250)).await;
            continue;
        }
        stable = 0;
        if time::Instant::now() >= deadline {
            bail!(
                "exposed services did not become ready within {:?}",
                READY_TIMEOUT
            );
        }
        time::sleep(Duration::from_millis(100)).await;
    }
}

/// One cheap echo roundtrip against the exposed echo service.
async fn probe_echo(addr: &str, t: Type) -> Result<()> {
    const PROBE: &[u8] = b"rathole";
    match t {
        Type::Tcp => {
            let mut conn = TcpStream::connect(addr).await?;
            conn.write_all(PROBE).await?;
            let mut rd = [0u8; PROBE.len()];
            conn.read_exact(&mut rd).await?;
            assert_eq!(&rd, PROBE);
        }
        Type::Udp => {
            let sock = UdpSocket::bind("127.0.0.1:0").await?;
            sock.connect(addr).await?;
            sock.send(PROBE).await?;
            let mut rd = [0u8; PROBE.len()];
            sock.recv(&mut rd).await?;
            assert_eq!(&rd, PROBE);
        }
    }
    Ok(())
}

/// One cheap ping/pong exchange against the exposed pingpong service.
async fn probe_pingpong(addr: &str, t: Type) -> Result<()> {
    match t {
        Type::Tcp => {
            let mut conn = TcpStream::connect(addr).await?;
            conn.write_all(PING.as_bytes()).await?;
            let mut rd = [0u8; PONG.len()];
            conn.read_exact(&mut rd).await?;
            assert_eq!(&rd, PONG.as_bytes());
        }
        Type::Udp => {
            let sock = UdpSocket::bind("127.0.0.1:0").await?;
            sock.connect(addr).await?;
            sock.send(PING.as_bytes()).await?;
            let mut rd = [0u8; PONG.len()];
            sock.recv(&mut rd).await?;
            assert_eq!(&rd, PONG.as_bytes());
        }
    }
    Ok(())
}

async fn echo_hitter(addr: &'static str, t: Type) -> Result<()> {
    match t {
        Type::Tcp => tcp_echo_hitter(addr).await,
        Type::Udp => udp_echo_hitter(addr).await,
    }
}

async fn pingpong_hitter(addr: &'static str, t: Type) -> Result<()> {
    match t {
        Type::Tcp => tcp_pingpong_hitter(addr).await,
        Type::Udp => udp_pingpong_hitter(addr).await,
    }
}

async fn tcp_echo_hitter(addr: &'static str) -> Result<()> {
    let mut conn = time::timeout(IO_TIMEOUT, TcpStream::connect(addr)).await??;

    let mut wr = [0u8; 1024];
    let mut rd = [0u8; 1024];
    for _ in 0..100 {
        rand::thread_rng().fill(&mut wr);
        time::timeout(IO_TIMEOUT, conn.write_all(&wr)).await??;
        time::timeout(IO_TIMEOUT, conn.read_exact(&mut rd)).await??;
        assert_eq!(wr, rd);
    }

    Ok(())
}

async fn udp_echo_hitter(addr: &'static str) -> Result<()> {
    let conn = time::timeout(IO_TIMEOUT, UdpSocket::bind("127.0.0.1:0")).await??;
    time::timeout(IO_TIMEOUT, conn.connect(addr)).await??;

    let mut wr = [0u8; 128];
    let mut rd = [0u8; 128];
    for _ in 0..3 {
        rand::thread_rng().fill(&mut wr);

        time::timeout(IO_TIMEOUT, conn.send(&wr)).await??;
        debug!("send");

        time::timeout(IO_TIMEOUT, conn.recv(&mut rd)).await??;
        debug!("recv");

        assert_eq!(wr, rd);
    }
    Ok(())
}

async fn tcp_pingpong_hitter(addr: &'static str) -> Result<()> {
    let mut conn = time::timeout(IO_TIMEOUT, TcpStream::connect(addr)).await??;

    let wr = PING.as_bytes();
    let mut rd = [0u8; PONG.len()];

    for _ in 0..100 {
        time::timeout(IO_TIMEOUT, conn.write_all(wr)).await??;
        time::timeout(IO_TIMEOUT, conn.read_exact(&mut rd)).await??;
        assert_eq!(rd, PONG.as_bytes());
    }

    Ok(())
}

async fn udp_pingpong_hitter(addr: &'static str) -> Result<()> {
    let conn = time::timeout(IO_TIMEOUT, UdpSocket::bind("127.0.0.1:0")).await??;
    time::timeout(IO_TIMEOUT, conn.connect(&addr)).await??;

    let wr = PING.as_bytes();
    let mut rd = [0u8; PONG.len()];

    for _ in 0..3 {
        time::timeout(IO_TIMEOUT, conn.send(wr)).await??;
        debug!("ping");

        time::timeout(IO_TIMEOUT, conn.recv(&mut rd)).await??;
        debug!("pong");

        assert_eq!(rd, PONG.as_bytes());
    }

    Ok(())
}

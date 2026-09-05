use crate::config::{Config, ServerConfig, ServerServiceConfig, ServiceType, TransportType};
use crate::config_watcher::{ConfigChange, ServerServiceChange};
use crate::constants::{listen_backoff, UDP_BUFFER_SIZE};
use crate::helper::{retry_notify_with_deadline, write_and_flush};
use crate::multi_map::MultiMap;
use crate::protocol::Hello::{ControlChannelHello, DataChannelHello};
use crate::protocol::{
    self, read_auth, read_hello, Ack, ControlChannelCmd, DataChannelCmd, Hello, UdpTraffic,
    HASH_WIDTH_IN_BYTES,
};
use crate::runtime_status::{self, RuntimeRegistry};
use crate::transport::{SocketOpts, TcpTransport, Transport};
use anyhow::{anyhow, bail, Context, Result};
use backoff::backoff::Backoff;
use backoff::ExponentialBackoff;

use rand::RngCore;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{self, copy_bidirectional, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{broadcast, mpsc, RwLock};
use tokio::time;
use tracing::{debug, error, info, info_span, instrument, warn, Instrument, Span};

#[cfg(feature = "noise")]
use crate::transport::NoiseTransport;
#[cfg(any(feature = "native-tls", feature = "rustls"))]
use crate::transport::TlsTransport;
#[cfg(any(feature = "websocket-native-tls", feature = "websocket-rustls"))]
use crate::transport::WebsocketTransport;

type ServiceDigest = protocol::Digest; // SHA256 of a service name
type Nonce = protocol::Digest; // Also called `session_key`

const TCP_POOL_SIZE: usize = 8; // The number of cached connections for TCP servies
                                // The UDP pool consumes exactly one data channel (see run_udp_connection_pool),
                                // so prefetch only one; a second channel would be created but never used
const UDP_POOL_SIZE: usize = 1;
const CHAN_SIZE: usize = 2048; // The capacity of various chans
const HANDSHAKE_TIMEOUT: u64 = 5; // Timeout for transport handshake
                                  // Timeout for the whole hello/auth phase of an incoming connection, so an
                                  // unauthenticated peer cannot hold the connection (and its fd) forever
const HELLO_AUTH_TIMEOUT: u64 = 10;
// Lower bound for data_channel_timeout
const MIN_DATA_CHANNEL_TIMEOUT: Duration = Duration::from_secs(10);

// A requested data channel must arrive within this budget. Otherwise the
// control channel is stuck (a half-open connection whose writes still
// succeed, or a client that cannot open new connections to the server) and
// is torn down with an error so the client reconnects. Visitors would
// otherwise hang forever waiting in the connection pool.
fn data_channel_timeout(heartbeat_interval: u64) -> Duration {
    std::cmp::max(
        Duration::from_secs(heartbeat_interval.saturating_mul(2)),
        MIN_DATA_CHANNEL_TIMEOUT,
    )
}

// The entrypoint of running a server
pub async fn run_server(
    config: Config,
    shutdown_rx: broadcast::Receiver<bool>,
    update_rx: mpsc::Receiver<ConfigChange>,
    runtime: RuntimeRegistry,
) -> Result<()> {
    let config = match config.server {
            Some(config) => config,
            None => {
                return Err(anyhow!("Try to run as a server, but the configuration is missing. Please add the `[server]` block"))
            }
        };

    match config.transport.transport_type {
        TransportType::Tcp => {
            let mut server = Server::<TcpTransport>::from(config, runtime).await?;
            server.run(shutdown_rx, update_rx).await?;
        }
        TransportType::Tls => {
            #[cfg(any(feature = "native-tls", feature = "rustls"))]
            {
                let mut server = Server::<TlsTransport>::from(config, runtime).await?;
                server.run(shutdown_rx, update_rx).await?;
            }
            #[cfg(not(any(feature = "native-tls", feature = "rustls")))]
            crate::helper::feature_neither_compile("native-tls", "rustls")
        }
        TransportType::Noise => {
            #[cfg(feature = "noise")]
            {
                let mut server = Server::<NoiseTransport>::from(config, runtime).await?;
                server.run(shutdown_rx, update_rx).await?;
            }
            #[cfg(not(feature = "noise"))]
            crate::helper::feature_not_compile("noise")
        }
        TransportType::Websocket => {
            #[cfg(any(feature = "websocket-native-tls", feature = "websocket-rustls"))]
            {
                let mut server = Server::<WebsocketTransport>::from(config, runtime).await?;
                server.run(shutdown_rx, update_rx).await?;
            }
            #[cfg(not(any(feature = "websocket-native-tls", feature = "websocket-rustls")))]
            crate::helper::feature_neither_compile("websocket-native-tls", "websocket-rustls")
        }
    }

    Ok(())
}

// A hash map of ControlChannelHandles, indexed by ServiceDigest or Nonce
// See also MultiMap
type ControlChannelMap<T> = MultiMap<ServiceDigest, Nonce, ControlChannelHandle<T>>;

// Server holds all states of running a server
struct Server<T: Transport> {
    // `[server]` config
    config: Arc<ServerConfig>,

    // `[server.services]` config, indexed by ServiceDigest
    services: Arc<RwLock<HashMap<ServiceDigest, ServerServiceConfig>>>,
    // Collection of contorl channels
    control_channels: Arc<RwLock<ControlChannelMap<T>>>,
    // Wrapper around the transport layer
    transport: Arc<T>,
    runtime: RuntimeRegistry,
}

// Generate a hash map of services which is indexed by ServiceDigest
fn generate_service_hashmap(
    server_config: &ServerConfig,
) -> HashMap<ServiceDigest, ServerServiceConfig> {
    let mut ret = HashMap::new();
    for u in &server_config.services {
        ret.insert(protocol::digest(u.0.as_bytes()), (*u.1).clone());
    }
    ret
}

impl<T: 'static + Transport> Server<T> {
    // Create a server from `[server]`
    pub async fn from(config: ServerConfig, runtime: RuntimeRegistry) -> Result<Server<T>> {
        let config = Arc::new(config);
        let services = Arc::new(RwLock::new(generate_service_hashmap(&config)));
        let control_channels = Arc::new(RwLock::new(ControlChannelMap::new()));
        let transport = Arc::new(T::new(&config.transport)?);
        Ok(Server {
            config,
            services,
            control_channels,
            transport,
            runtime,
        })
    }

    // The entry point of Server
    pub async fn run(
        &mut self,
        mut shutdown_rx: broadcast::Receiver<bool>,
        mut update_rx: mpsc::Receiver<ConfigChange>,
    ) -> Result<()> {
        // A fresh run must invalidate an older listener snapshot before a
        // transport bind is attempted.
        runtime_status::server_listener_pending(&self.runtime);
        let l = match self.transport.bind(&self.config.bind_addr).await {
            Ok(listener) => listener,
            Err(error) => {
                runtime_status::server_listener_error(&self.runtime, &error);
                return Err(error).with_context(|| "Failed to listen at `server.bind_addr`");
            }
        };
        runtime_status::server_listener_listening(&self.runtime);
        info!("Listening at {}", self.config.bind_addr);

        // Retry at least every 100ms
        let mut backoff = ExponentialBackoff {
            max_interval: Duration::from_millis(100),
            max_elapsed_time: None,
            ..Default::default()
        };

        // Wait for connections and shutdown signals
        loop {
            tokio::select! {
                // Wait for incoming control and data channels
                ret = self.transport.accept(&l) => {
                    match ret {
                        Err(err) => {
                            // Detects whether it's an IO error
                            if let Some(err) = err.downcast_ref::<io::Error>() {
                                // If it is an IO error, then it's possibly an
                                // EMFILE. So sleep for a while and retry
                                // TODO: Only sleep for EMFILE, ENFILE, ENOMEM, ENOBUFS
                                if let Some(d) = backoff.next_backoff() {
                                    error!("Failed to accept: {:#}. Retry in {:?}...", err, d);
                                    time::sleep(d).await;
                                } else {
                                    // This branch will never be executed according to the current retry policy
                                    error!("Too many retries. Aborting...");
                                    break;
                                }
                            }
                            // If it's not an IO error, then it comes from
                            // the transport layer, so just ignore it
                        }
                        Ok((conn, addr)) => {
                            backoff.reset();

                            // Do transport handshake with a timeout
                            match time::timeout(Duration::from_secs(HANDSHAKE_TIMEOUT), self.transport.handshake(conn)).await {
                                Ok(conn) => {
                                    match conn.with_context(|| "Failed to do transport handshake") {
                                        Ok(conn) => {
                                            let services = self.services.clone();
                                            let control_channels = self.control_channels.clone();
                                            let server_config = self.config.clone();
                                            let runtime = self.runtime.clone();
                                            tokio::spawn(async move {
                                                if let Err(err) = handle_connection(conn, addr, services, control_channels, server_config, runtime).await {
                                                    error!("{:#}", err);
                                                }
                                            }.instrument(info_span!("connection", %addr)));
                                        }, Err(e) => {
                                            error!("{:#}", e);
                                        }
                                    }
                                },
                                Err(e) => {
                                    error!("Transport handshake timeout: {}", e);
                                }
                            }
                        }
                    }
                },
                // Wait for the shutdown signal
                _ = shutdown_rx.recv() => {
                    info!("Shuting down gracefully...");
                    break;
                },
                e = update_rx.recv() => {
                    if let Some(e) = e {
                        self.handle_hot_reload(e).await;
                    }
                }
            }
        }

        runtime_status::server_listener_stopped(&self.runtime);
        info!("Shutdown");

        Ok(())
    }

    async fn handle_hot_reload(&mut self, e: ConfigChange) {
        match e {
            ConfigChange::ServerChange(server_change) => match server_change {
                ServerServiceChange::Add(cfg) => {
                    let hash = protocol::digest(cfg.name.as_bytes());
                    runtime_status::server_add(&self.runtime, cfg.name.clone());
                    let mut wg = self.services.write().await;
                    let _ = wg.insert(hash, cfg);

                    let mut wg = self.control_channels.write().await;
                    let _ = wg.remove1(&hash);
                }
                ServerServiceChange::Delete(s) => {
                    let hash = protocol::digest(s.as_bytes());
                    runtime_status::remove(&self.runtime, &s);
                    let _ = self.services.write().await.remove(&hash);

                    let mut wg = self.control_channels.write().await;
                    let _ = wg.remove1(&hash);
                }
            },
            ignored => warn!("Ignored {:?} since running as a server", ignored),
        }
    }
}
// Handle connections to `server.bind_addr`.
async fn handle_connection<T: 'static + Transport>(
    mut conn: T::Stream,
    source_addr: std::net::SocketAddr,
    services: Arc<RwLock<HashMap<ServiceDigest, ServerServiceConfig>>>,
    control_channels: Arc<RwLock<ControlChannelMap<T>>>,
    server_config: Arc<ServerConfig>,
    runtime: RuntimeRegistry,
) -> Result<()> {
    // The hello/auth phase must complete within a deadline; otherwise an
    // unauthenticated peer sending zero bytes could occupy the connection.
    let handshake = async {
        match read_hello(&mut conn).await? {
            ControlChannelHello(_, service_digest) => {
                do_control_channel_handshake(
                    conn,
                    source_addr,
                    services,
                    control_channels,
                    service_digest,
                    server_config,
                    runtime,
                )
                .await?;
            }
            DataChannelHello(_, nonce) => {
                do_data_channel_handshake(conn, control_channels, nonce).await?;
            }
        }
        Ok::<(), anyhow::Error>(())
    };
    time::timeout(Duration::from_secs(HELLO_AUTH_TIMEOUT), handshake)
        .await
        .with_context(|| "Hello/auth timed out")??;
    Ok(())
}

// Constant-time comparison of two digests. Comparing the session key with
// `!=` would leak timing information about the expected value
fn digest_eq(a: &protocol::Digest, b: &protocol::Digest) -> bool {
    let mut acc = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        acc |= x ^ y;
    }
    acc == 0
}

async fn do_control_channel_handshake<T: 'static + Transport>(
    mut conn: T::Stream,
    source_addr: std::net::SocketAddr,
    services: Arc<RwLock<HashMap<ServiceDigest, ServerServiceConfig>>>,
    control_channels: Arc<RwLock<ControlChannelMap<T>>>,
    service_digest: ServiceDigest,
    server_config: Arc<ServerConfig>,
    runtime: RuntimeRegistry,
) -> Result<()> {
    info!("Try to handshake a control channel");
    T::hint(&conn, SocketOpts::for_control_channel());

    // Generate a nonce
    let mut nonce = vec![0u8; HASH_WIDTH_IN_BYTES];
    rand::thread_rng().fill_bytes(&mut nonce);

    // Send hello
    let hello_send = Hello::ControlChannelHello(
        protocol::CURRENT_PROTO_VERSION,
        nonce.clone().try_into().unwrap(),
    );
    conn.write_all(&bincode::serialize(&hello_send).unwrap())
        .await?;
    conn.flush().await?;

    // Lookup the service
    let service_config = match services.read().await.get(&service_digest) {
        Some(v) => v,
        None => {
            conn.write_all(&bincode::serialize(&Ack::ServiceNotExist).unwrap())
                .await?;
            bail!("No such a service {}", hex::encode(service_digest));
        }
    }
    .to_owned();

    let service_name = service_config.name.clone();

    // Calculate the checksum
    let mut concat = Vec::from(service_config.token.as_ref().unwrap().as_bytes());
    concat.append(&mut nonce);

    // Read auth
    let protocol::Auth(d) = read_auth(&mut conn).await?;

    // Validate
    let session_key = protocol::digest(&concat);
    if !digest_eq(&session_key, &d) {
        conn.write_all(&bincode::serialize(&Ack::AuthFailed).unwrap())
            .await?;
        // Never log the session key: together with the nonce it allows
        // offline brute-forcing a weak token
        bail!("Service {} failed the authentication", service_name);
    } else {
        // Send the ack outside the write lock; the lock only guards the map
        conn.write_all(&bincode::serialize(&Ack::Ok).unwrap())
            .await?;
        conn.flush().await?;
        info!(service = %service_config.name, "Control channel established");
        let connected_since =
            runtime_status::server_connected(&runtime, &service_name, source_addr).ok_or_else(
                || anyhow!("service {} was removed during authentication", service_name),
            )?;
        let handle = ControlChannelHandle::new(
            conn,
            service_config,
            server_config.heartbeat_interval,
            data_channel_timeout(server_config.heartbeat_interval),
            runtime.clone(),
            source_addr,
            connected_since,
        );

        let mut h = control_channels.write().await;
        // A new authenticated control channel supersedes the old one.
        if h.remove1(&service_digest).is_some() {
            warn!(
                "Dropping previous control channel for service {}",
                service_name
            );
        }
        let _ = h.insert(service_digest, session_key, handle);
    }
    Ok(())
}

async fn do_data_channel_handshake<T: 'static + Transport>(
    conn: T::Stream,
    control_channels: Arc<RwLock<ControlChannelMap<T>>>,
    nonce: Nonce,
) -> Result<()> {
    debug!("Try to handshake a data channel");

    // Validate
    let control_channels_guard = control_channels.read().await;
    match control_channels_guard.get2(&nonce) {
        Some(handle) => {
            T::hint(&conn, SocketOpts::from_server_cfg(&handle.service));

            // Send the data channel to the corresponding control channel
            handle
                .data_ch_tx
                .send(conn)
                .await
                .with_context(|| "Data channel for a stale control channel")?;
            // Tell the control channel one of its requests was fulfilled,
            // so it can re-arm its data channel deadline
            let _ = handle.data_ch_arrived_tx.send(());
        }
        None => {
            warn!("Data channel has incorrect nonce");
        }
    }
    Ok(())
}

pub struct ControlChannelHandle<T: Transport> {
    // Shutdown the control channel by dropping it
    _shutdown_tx: broadcast::Sender<bool>,
    data_ch_tx: mpsc::Sender<T::Stream>,
    data_ch_arrived_tx: mpsc::UnboundedSender<()>,
    service: ServerServiceConfig,
}

impl<T> ControlChannelHandle<T>
where
    T: 'static + Transport,
{
    // Create a control channel handle, where the control channel handling task
    // and the connection pool task are created.
    fn new(
        conn: T::Stream,
        service: ServerServiceConfig,
        heartbeat_interval: u64,
        data_ch_timeout: Duration,
        runtime: RuntimeRegistry,
        source_addr: std::net::SocketAddr,
        connected_since: u64,
    ) -> ControlChannelHandle<T> {
        // Create a shutdown channel
        let (shutdown_tx, shutdown_rx) = broadcast::channel::<bool>(1);

        // Store data channels
        let (data_ch_tx, data_ch_rx) = mpsc::channel(CHAN_SIZE * 2);

        // Store data channel creation requests
        let (data_ch_req_tx, data_ch_req_rx) = mpsc::unbounded_channel();
        // Tracks data channels handed over in do_data_channel_handshake
        let (data_ch_arrived_tx, data_ch_arrived_rx) = mpsc::unbounded_channel();

        // Cache some data channels for later use
        let pool_size = match service.service_type {
            ServiceType::Tcp => TCP_POOL_SIZE,
            ServiceType::Udp => UDP_POOL_SIZE,
        };

        for _i in 0..pool_size {
            if let Err(e) = data_ch_req_tx.send(true) {
                error!("Failed to request data channel {}", e);
            };
        }

        let shutdown_rx_clone = shutdown_tx.subscribe();
        let bind_addr = service.bind_addr.clone();
        match service.service_type {
            ServiceType::Tcp => tokio::spawn(
                async move {
                    if let Err(e) = run_tcp_connection_pool::<T>(
                        bind_addr,
                        data_ch_rx,
                        data_ch_req_tx,
                        shutdown_rx_clone,
                    )
                    .await
                    .with_context(|| "Failed to run TCP connection pool")
                    {
                        error!("{:#}", e);
                    }
                }
                .instrument(Span::current()),
            ),
            ServiceType::Udp => tokio::spawn(
                async move {
                    if let Err(e) = run_udp_connection_pool::<T>(
                        bind_addr,
                        data_ch_rx,
                        data_ch_req_tx,
                        shutdown_rx_clone,
                    )
                    .await
                    .with_context(|| "Failed to run UDP connection pool")
                    {
                        error!("{:#}", e);
                    }
                }
                .instrument(Span::current()),
            ),
        };
        let ch = ControlChannel::<T> {
            conn,
            shutdown_rx,
            data_ch_req_rx,
            data_ch_arrived_rx,
            heartbeat_interval,
            data_ch_timeout,
            service_name: service.name.clone(),
            runtime,
            source_addr,
            connected_since,
        };

        // Run the control channel
        tokio::spawn(
            async move {
                let runtime = ch.runtime.clone();
                let service_name = ch.service_name.clone();
                let source_addr = ch.source_addr;
                let connected_since = ch.connected_since;
                let result = ch.run().await;
                if let Err(err) = &result {
                    error!("{:#}", err);
                    runtime_status::server_disconnected(
                        &runtime,
                        &service_name,
                        source_addr,
                        connected_since,
                        Some(err),
                    );
                } else {
                    runtime_status::server_disconnected(
                        &runtime,
                        &service_name,
                        source_addr,
                        connected_since,
                        Some("control channel stopped"),
                    );
                }
            }
            .instrument(Span::current()),
        );

        ControlChannelHandle {
            _shutdown_tx: shutdown_tx,
            data_ch_tx,
            data_ch_arrived_tx,
            service,
        }
    }
}
struct ControlChannel<T: Transport> {
    conn: T::Stream,                               // The connection of control channel
    shutdown_rx: broadcast::Receiver<bool>,        // Receives the shutdown signal
    data_ch_req_rx: mpsc::UnboundedReceiver<bool>, // Receives visitor connections
    data_ch_arrived_rx: mpsc::UnboundedReceiver<()>, // Fulfilled data channel requests
    heartbeat_interval: u64,                       // Application-layer heartbeat interval in secs
    data_ch_timeout: Duration,                     // Max wait for a requested data channel
    service_name: String,
    runtime: RuntimeRegistry,
    source_addr: std::net::SocketAddr,
    connected_since: u64,
}
impl<T: Transport> ControlChannel<T> {
    async fn write_and_flush(&mut self, data: &[u8]) -> Result<()> {
        write_and_flush(&mut self.conn, data)
            .await
            .with_context(|| "Failed to write control cmds")?;
        Ok(())
    }
    // Run a control channel
    #[instrument(skip_all)]
    async fn run(mut self) -> Result<()> {
        let create_ch_cmd = bincode::serialize(&ControlChannelCmd::CreateDataChannel).unwrap();
        let heartbeat = bincode::serialize(&ControlChannelCmd::HeartBeat).unwrap();

        // Outstanding CreateDataChannel requests. A deadline is armed while
        // any request is unfulfilled and re-armed by every arrival; if it
        // fires, the client is unable to deliver data channels over this
        // (possibly half-open) connection, so the run loop errors out and
        // the client is forced to reconnect.
        let mut pending_data_ch: u32 = 0;
        let mut data_ch_deadline: Option<std::pin::Pin<Box<time::Sleep>>> = None;
        let mut arrived_rx_closed = false;

        // Wait for data channel requests and the shutdown signal
        loop {
            tokio::select! {
                val = self.data_ch_req_rx.recv() => {
                    match val {
                        Some(_) => {
                            if let Err(e) = self.write_and_flush(&create_ch_cmd).await {
                                error!("{:#}", e);
                                break;
                            }
                            pending_data_ch += 1;
                            if data_ch_deadline.is_none() {
                                data_ch_deadline =
                                    Some(Box::pin(time::sleep(self.data_ch_timeout)));
                            }
                        }
                        None => {
                            break;
                        }
                    }
                },
                arrived = self.data_ch_arrived_rx.recv(), if !arrived_rx_closed => {
                    match arrived {
                        Some(()) => {
                            pending_data_ch = pending_data_ch.saturating_sub(1);
                            // Channels are still being delivered; give the
                            // oldest outstanding request a fresh budget
                            data_ch_deadline = (pending_data_ch > 0)
                                .then(|| Box::pin(time::sleep(self.data_ch_timeout)));
                        }
                        // The handle was dropped; the shutdown branch fires
                        // next and ends the loop
                        None => arrived_rx_closed = true,
                    }
                },
                _ = async {
                    match data_ch_deadline.as_mut() {
                        Some(deadline) => deadline.await,
                        None => std::future::pending().await,
                    }
                } => {
                    bail!(
                        "No data channel arrived within {:?}; the control channel is stuck",
                        self.data_ch_timeout
                    );
                },
                _ = time::sleep(Duration::from_secs(self.heartbeat_interval)), if self.heartbeat_interval != 0 => {
                            if let Err(e) = self.write_and_flush(&heartbeat).await {
                                error!("{:#}", e);
                                break;
                            }
                }
                // Wait for the shutdown signal
                _ = self.shutdown_rx.recv() => {
                    break;
                }
            }
        }

        info!("Control channel shutdown");

        Ok(())
    }
}

fn tcp_listen_and_send(
    addr: String,
    data_ch_req_tx: mpsc::UnboundedSender<bool>,
    mut shutdown_rx: broadcast::Receiver<bool>,
) -> mpsc::Receiver<TcpStream> {
    let (tx, rx) = mpsc::channel(CHAN_SIZE);

    tokio::spawn(async move {
        let l = retry_notify_with_deadline(listen_backoff(),  || async {
            Ok(TcpListener::bind(&addr).await?)
        }, |e, duration| {
            error!("{:#}. Retry in {:?}", e, duration);
        }, &mut shutdown_rx).await;

        let l: TcpListener = match l {
            Ok(Some(v)) => v,
            Ok(None) => {
                // Shutdown while retrying; not an error
                info!("Shutdown while listening");
                return;
            }
            Err(e) => {
                error!("{:#}", e.context("Failed to listen for the service"));
                return;
            }
        };

        info!("Listening at {}", &addr);

        // Retry at least every 1s
        let mut backoff = ExponentialBackoff {
            max_interval: Duration::from_secs(1),
            max_elapsed_time: None,
            ..Default::default()
        };

        // Wait for visitors and the shutdown signal
        loop {
            tokio::select! {
                val = l.accept() => {
                    match val {
                        Err(e) => {
                            // `l` is a TCP listener so this must be a IO error
                            // Possibly a EMFILE. So sleep for a while
                            error!("{}. Sleep for a while", e);
                            if let Some(d) = backoff.next_backoff() {
                                time::sleep(d).await;
                            } else {
                                // This branch will never be reached for current backoff policy
                                error!("Too many retries. Aborting...");
                                break;
                            }
                        }
                        Ok((incoming, addr)) => {
                            // For every visitor, request to create a data channel
                            if data_ch_req_tx.send(true).with_context(|| "Failed to send data chan create request").is_err() {
                                // An error indicates the control channel is broken
                                // So break the loop
                                break;
                            }

                            backoff.reset();

                            debug!("New visitor from {}", addr);

                            // Send the visitor to the connection pool
                            let _ = tx.send(incoming).await;
                        }
                    }
                },
                _ = shutdown_rx.recv() => {
                    break;
                }
            }
        }

        info!("TCPListener shutdown");
    }.instrument(Span::current()));

    rx
}

#[instrument(skip_all)]
async fn run_tcp_connection_pool<T: Transport>(
    bind_addr: String,
    mut data_ch_rx: mpsc::Receiver<T::Stream>,
    data_ch_req_tx: mpsc::UnboundedSender<bool>,
    shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()> {
    let mut visitor_rx = tcp_listen_and_send(bind_addr, data_ch_req_tx.clone(), shutdown_rx);
    let cmd = bincode::serialize(&DataChannelCmd::StartForwardTcp).unwrap();

    'pool: while let Some(mut visitor) = visitor_rx.recv().await {
        loop {
            if let Some(mut ch) = data_ch_rx.recv().await {
                if write_and_flush(&mut ch, &cmd).await.is_ok() {
                    tokio::spawn(async move {
                        if let Err(e) = copy_bidirectional(&mut ch, &mut visitor).await {
                            debug!(
                                "Failed to forward TCP between the visitor and the data channel: {:#}",
                                e
                            );
                        }
                    });
                    break;
                } else {
                    // Current data channel is broken. Request for a new one
                    if data_ch_req_tx.send(true).is_err() {
                        break 'pool;
                    }
                }
            } else {
                break 'pool;
            }
        }
    }

    info!("Shutdown");
    Ok(())
}

#[instrument(skip_all)]
async fn run_udp_connection_pool<T: Transport>(
    bind_addr: String,
    mut data_ch_rx: mpsc::Receiver<T::Stream>,
    data_ch_req_tx: mpsc::UnboundedSender<bool>,
    mut shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()> {
    // TODO: Load balance

    let cmd = bincode::serialize(&DataChannelCmd::StartForwardUdp).unwrap();
    let mut buf = [0u8; UDP_BUFFER_SIZE];

    // The socket is bound once for the pool's whole lifetime: per-datagram
    // errors (ICMP-triggered WSAECONNRESET on Windows) are tolerated below,
    // so only a dead data channel tears down a forwarding session — the
    // socket, and with it the visitors' endpoint, survives the rebuild.
    let l = match retry_notify_with_deadline(
        listen_backoff(),
        || async { Ok(UdpSocket::bind(&bind_addr).await?) },
        |e, duration| {
            warn!("{:#}. Retry in {:?}", e, duration);
        },
        &mut shutdown_rx,
    )
    .await
    {
        Ok(Some(l)) => l,
        Ok(None) => return Ok(()), // Shutdown while retrying; not an error
        Err(e) => return Err(e.context("Failed to listen for the service")),
    };

    info!("Listening at {}", &bind_addr);

    // Session loop: a broken data channel only tears down the current
    // forwarding session. A fresh data channel is acquired and forwarding
    // resumes, so a transient I/O error no longer kills the UDP service
    // until the control channel is rebuilt. Only the shutdown signal or
    // the control channel going away (closing `data_ch_rx` /
    // `data_ch_req_tx`) ends the pool.
    'pool: loop {
        // Receive one data channel
        let mut conn = match data_ch_rx.recv().await {
            Some(conn) => conn,
            // The control channel is gone. Nothing will feed us new data
            // channels, so there is no point in retrying
            None => break 'pool,
        };
        if let Err(e) = write_and_flush(&mut conn, &cmd).await {
            warn!("Failed to start forwarding on a data channel: {:#}", e);
            // Ask for a replacement channel and retry
            if data_ch_req_tx.send(true).is_err() {
                break 'pool;
            }
            continue 'pool;
        }

        loop {
            tokio::select! {
                // Forward inbound traffic to the client
                val = l.recv_from(&mut buf) => {
                    match val {
                        Ok((n, from)) => {
                            if let Err(e) = UdpTraffic::write_slice(&mut conn, from, &buf[..n]).await {
                                warn!("Failed to forward inbound traffic to the client: {:#}", e);
                                break;
                            }
                        }
                        // A UDP socket error is per-datagram and transient
                        // (on Windows a single ICMP port unreachable turns
                        // into WSAECONNRESET). Log and keep the session;
                        // only a dead data channel justifies a rebuild.
                        Err(e) => {
                            warn!("Failed to receive inbound traffic (ignored): {:#}", e);
                            // A persistent error (e.g. the interface is
                            // gone) returns immediately from every recv —
                            // back off instead of spinning the loop hot.
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        }
                    }
                },

                // Forward outbound traffic from the client to the visitor
                hdr_len = conn.read_u8() => {
                    let hdr_len = match hdr_len {
                        Ok(v) => v,
                        Err(e) => {
                            warn!("Failed to read from the data channel: {:#}", e);
                            break;
                        }
                    };
                    match UdpTraffic::read(&mut conn, hdr_len).await {
                        Ok(t) => {
                            if let Err(e) = l.send_to(&t.data, t.from).await {
                                // Per-datagram transient, see above
                                warn!("Failed to forward outbound traffic to the visitor (ignored): {:#}", e);
                            }
                        }
                        Err(e) => {
                            warn!("Failed to read outbound traffic from the data channel: {:#}", e);
                            break;
                        }
                    }
                }

                _ = shutdown_rx.recv() => {
                    break 'pool;
                }
            }
        }

        // The forwarding session is broken. Ask the client for a
        // replacement data channel and resume on the same socket
        if data_ch_req_tx.send(true).is_err() {
            break 'pool;
        }
    }

    debug!("UDP pool dropped");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_status::ServerListenerState;
    fn test_server_config(bind_addr: String) -> ServerConfig {
        ServerConfig {
            bind_addr,
            ..Default::default()
        }
    }

    async fn wait_for_listener_state(runtime: &RuntimeRegistry, expected: ServerListenerState) {
        time::timeout(Duration::from_secs(1), async {
            loop {
                if matches!(
                    runtime_status::snapshot(runtime).server_listener,
                    Some(crate::runtime_status::ServerListenerSnapshot { state, .. })
                        if state == expected
                ) {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("server listener did not reach expected state");
    }

    #[tokio::test]
    async fn listener_state_tracks_successful_bind_and_shutdown() {
        let runtime = runtime_status::server_registry(std::iter::empty());
        assert!(matches!(
            runtime_status::snapshot(&runtime).server_listener,
            Some(crate::runtime_status::ServerListenerSnapshot {
                state: ServerListenerState::Pending,
                ..
            })
        ));

        let mut server = Server::<TcpTransport>::from(
            test_server_config("127.0.0.1:0".to_owned()),
            runtime.clone(),
        )
        .await
        .unwrap();
        let (shutdown_tx, shutdown_rx) = broadcast::channel(1);
        let (_update_tx, update_rx) = mpsc::channel(1);
        let run = tokio::spawn(async move { server.run(shutdown_rx, update_rx).await });
        wait_for_listener_state(&runtime, ServerListenerState::Listening).await;
        assert_eq!(shutdown_tx.send(true).unwrap(), 1);
        run.await.unwrap().unwrap();
        assert!(matches!(
            runtime_status::snapshot(&runtime).server_listener,
            Some(crate::runtime_status::ServerListenerSnapshot {
                state: ServerListenerState::Stopped,
                last_error: None,
            })
        ));
    }

    #[tokio::test]
    async fn listener_state_tracks_bind_failure() {
        let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let runtime = runtime_status::server_registry(std::iter::empty());
        let mut server = Server::<TcpTransport>::from(
            test_server_config(occupied.local_addr().unwrap().to_string()),
            runtime.clone(),
        )
        .await
        .unwrap();
        let (_shutdown_tx, shutdown_rx) = broadcast::channel(1);
        let (_update_tx, update_rx) = mpsc::channel(1);

        assert!(server.run(shutdown_rx, update_rx).await.is_err());
        assert!(matches!(
            runtime_status::snapshot(&runtime).server_listener,
            Some(crate::runtime_status::ServerListenerSnapshot {
                state: ServerListenerState::Error,
                last_error: Some(_),
            })
        ));
    }

    #[test]
    fn digest_eq_matches_identical_digests() {
        let a = protocol::digest(b"token-and-nonce");
        assert!(digest_eq(&a, &a));

        let zero = [0u8; HASH_WIDTH_IN_BYTES];
        assert!(digest_eq(&zero, &zero));
    }

    #[test]
    fn digest_eq_rejects_a_difference_in_any_byte() {
        let a = protocol::digest(b"token-and-nonce");
        for i in 0..HASH_WIDTH_IN_BYTES {
            let mut b = a;
            b[i] ^= 0xff;
            assert!(!digest_eq(&a, &b), "difference at byte {} must fail", i);
        }
    }
    fn test_service_config(name: &str, bind_addr: &str) -> ServerServiceConfig {
        let mut cfg = ServerServiceConfig::with_name(name);
        cfg.bind_addr = bind_addr.to_owned();
        cfg
    }

    // A loopback pair standing in for the client end of a control channel
    async fn control_channel_pair() -> (TcpStream, TcpStream, std::net::SocketAddr) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client_side = TcpStream::connect(addr).await.unwrap();
        let (server_side, source_addr) = listener.accept().await.unwrap();
        (client_side, server_side, source_addr)
    }

    #[test]
    fn data_channel_timeout_scales_with_heartbeat_and_has_a_floor() {
        assert_eq!(data_channel_timeout(30), Duration::from_secs(60));
        assert_eq!(data_channel_timeout(1), MIN_DATA_CHANNEL_TIMEOUT);
        // A disabled heartbeat must not disable the timeout
        assert_eq!(data_channel_timeout(0), MIN_DATA_CHANNEL_TIMEOUT);
    }

    #[tokio::test]
    async fn stuck_control_channel_is_torn_down_when_no_data_channel_arrives() {
        use crate::protocol::read_control_cmd;
        use crate::runtime_status::{RuntimeServiceSnapshot, ServerControlState};

        let runtime = runtime_status::server_registry(std::iter::empty());
        runtime_status::server_add(&runtime, "svc".to_string());

        let (mut client_side, server_side, source_addr) = control_channel_pair().await;
        let connected_since =
            runtime_status::server_connected(&runtime, "svc", source_addr).unwrap();

        let handle = ControlChannelHandle::<TcpTransport>::new(
            server_side,
            test_service_config("svc", "127.0.0.1:0"),
            0,                          // heartbeat disabled
            Duration::from_millis(500), // test-scale data channel timeout
            runtime.clone(),
            source_addr,
            connected_since,
        );

        // The pool prefetches TCP_POOL_SIZE data channels; the fake client
        // reads the requests but never creates any.
        let mut requested = 0;
        while requested < TCP_POOL_SIZE {
            match time::timeout(Duration::from_secs(2), read_control_cmd(&mut client_side))
                .await
                .expect("server did not request a data channel")
                .unwrap()
            {
                ControlChannelCmd::CreateDataChannel => requested += 1,
                ControlChannelCmd::HeartBeat => {}
            }
        }

        // After the timeout the server must give up on the stuck channel and
        // close the connection instead of letting visitors hang forever.
        let closed = time::timeout(Duration::from_secs(5), async {
            let mut buf = [0u8; 64];
            loop {
                if client_side.read(&mut buf).await.unwrap() == 0 {
                    break;
                }
            }
        })
        .await;
        assert!(closed.is_ok(), "stuck control channel was not torn down");

        // The runtime status must show the service waiting with the timeout
        // recorded as the reason.
        time::timeout(Duration::from_secs(2), async {
            loop {
                let snapshot = runtime_status::snapshot(&runtime);
                if let Some(RuntimeServiceSnapshot::Server {
                    state,
                    last_disconnected_or_error: Some(event),
                    ..
                }) = snapshot.services.get("svc")
                {
                    assert_eq!(*state, ServerControlState::Waiting);
                    assert!(
                        event.message.contains("No data channel arrived"),
                        "unexpected disconnect reason: {}",
                        event.message
                    );
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("service status did not transition to waiting");

        drop(handle);
    }

    #[tokio::test]
    async fn control_channel_forwards_and_survives_when_data_channels_arrive() {
        use crate::protocol::{read_control_cmd, read_data_cmd};
        use crate::runtime_status::{RuntimeServiceSnapshot, ServerControlState};

        let bind_addr = "127.0.0.1:12370";
        let runtime = runtime_status::server_registry(std::iter::empty());
        runtime_status::server_add(&runtime, "svc2".to_string());

        let (mut client_side, server_side, source_addr) = control_channel_pair().await;
        let connected_since =
            runtime_status::server_connected(&runtime, "svc2", source_addr).unwrap();

        let handle = ControlChannelHandle::<TcpTransport>::new(
            server_side,
            test_service_config("svc2", bind_addr),
            0,                          // heartbeat disabled
            Duration::from_millis(500), // test-scale data channel timeout
            runtime.clone(),
            source_addr,
            connected_since,
        );

        // Route data channels to the handle, keyed by an arbitrary nonce
        let nonce: Nonce = [7u8; HASH_WIDTH_IN_BYTES];
        let control_channels = Arc::new(RwLock::new(ControlChannelMap::<TcpTransport>::new()));
        control_channels
            .write()
            .await
            .insert(protocol::digest(b"svc2"), nonce, handle);

        // Fake client: answer every CreateDataChannel request with a data
        // channel that echoes back whatever the visitor sends.
        let map = control_channels.clone();
        tokio::spawn(async move {
            while let Ok(ControlChannelCmd::CreateDataChannel) =
                read_control_cmd(&mut client_side).await
            {
                let map = map.clone();
                tokio::spawn(async move {
                    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                    let addr = listener.local_addr().unwrap();
                    let mut data_client = TcpStream::connect(addr).await.unwrap();
                    let (data_server, _) = listener.accept().await.unwrap();
                    do_data_channel_handshake::<TcpTransport>(data_server, map, nonce)
                        .await
                        .unwrap();
                    read_data_cmd(&mut data_client).await.unwrap();
                    let (mut rd, mut wr) = data_client.split();
                    let _ = io::copy(&mut rd, &mut wr).await;
                });
            }
        });

        // Wait for the visitor listener, then run an echo roundtrip
        let mut visitor = time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(conn) = TcpStream::connect(bind_addr).await {
                    break conn;
                }
                time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("visitor listener did not come up");
        visitor.write_all(b"ping").await.unwrap();
        let mut buf = [0u8; 4];
        time::timeout(Duration::from_secs(2), visitor.read_exact(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&buf, b"ping");

        // Idle well beyond the data channel timeout: with every request
        // fulfilled there is no pending deadline, so the channel must stay
        // up and keep forwarding.
        time::sleep(Duration::from_millis(1200)).await;
        let mut visitor2 = TcpStream::connect(bind_addr).await.unwrap();
        visitor2.write_all(b"pong").await.unwrap();
        time::timeout(Duration::from_secs(2), visitor2.read_exact(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&buf, b"pong");

        let snapshot = runtime_status::snapshot(&runtime);
        assert!(matches!(
            snapshot.services.get("svc2"),
            Some(RuntimeServiceSnapshot::Server {
                state: ServerControlState::Connected,
                ..
            })
        ));
    }
}

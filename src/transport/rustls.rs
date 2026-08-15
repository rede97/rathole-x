use crate::config::{TlsConfig, TransportConfig};
use crate::helper::host_port_pair;
use crate::transport::{AddrMaybeCached, SocketOpts, TcpTransport, Transport};
use std::fmt::Debug;
use std::fs;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream, ToSocketAddrs};
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer, ServerName};

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use p12::PFX;
use tokio_rustls::rustls::{ClientConfig, RootCertStore, ServerConfig};
pub(crate) use tokio_rustls::TlsStream;
use tokio_rustls::{TlsAcceptor, TlsConnector};

pub struct TlsTransport {
    tcp: TcpTransport,
    config: TlsConfig,
    connector: Option<TlsConnector>,
    tls_acceptor: Option<TlsAcceptor>,
}

// workaround for TlsConnector and TlsAcceptor not implementing Debug
impl Debug for TlsTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsTransport")
            .field("tcp", &self.tcp)
            .field("config", &self.config)
            .finish()
    }
}

fn load_server_config(config: &TlsConfig) -> Result<Option<ServerConfig>> {
    if let Some(pkcs12_path) = config.pkcs12.as_ref() {
        let buf = fs::read(pkcs12_path)
            .with_context(|| format!("Failed to read `tls.pkcs12` {}", pkcs12_path))?;
        let pfx = PFX::parse(buf.as_slice())?;
        let pass = config
            .pkcs12_password
            .as_ref()
            .ok_or_else(|| anyhow!("Missing `tls.pkcs12_password`"))?;

        let certs = pfx.cert_bags(pass)?;
        let keys = pfx.key_bags(pass)?;

        let chain: Vec<CertificateDer> = certs.into_iter().map(CertificateDer::from).collect();
        let key = PrivatePkcs8KeyDer::from(
            keys.into_iter()
                .next()
                .ok_or_else(|| anyhow!("No private key found in `tls.pkcs12` {}", pkcs12_path))?,
        );

        Ok(Some(
            ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(chain, key.into())?,
        ))
    } else {
        Ok(None)
    }
}

fn load_client_config(config: &TlsConfig) -> Result<Option<ClientConfig>> {
    let mut root_certs = RootCertStore::empty();

    if let Some(path) = config.trusted_root.as_ref() {
        let file = fs::File::open(path)
            .with_context(|| format!("Failed to open `tls.trusted_root` {}", path))?;
        // Trust every certificate in the PEM bundle, not just the first one
        for cert in rustls_pemfile::certs(&mut std::io::BufReader::new(file)) {
            let cert =
                cert.with_context(|| format!("Failed to parse a certificate in {}", path))?;
            root_certs
                .add(cert)
                .with_context(|| format!("Failed to trust a certificate in {}", path))?;
        }
    } else {
        // Read from the native root store; trust all of it, not just the
        // first certificate. An empty or unreadable store is a hard error
        // instead of a panic or a silently useless connector.
        let certs = rustls_native_certs::load_native_certs()
            .map_err(|e| anyhow!("Failed to load native certs: {}", e))?;
        for cert in certs {
            root_certs
                .add(cert)
                .with_context(|| "Failed to trust a native root certificate")?;
        }
    }

    if root_certs.is_empty() {
        bail!("No root certificate available to verify the server");
    }

    Ok(Some(
        ClientConfig::builder()
            .with_root_certificates(root_certs)
            .with_no_client_auth(),
    ))
}

#[async_trait]
impl Transport for TlsTransport {
    type Acceptor = TcpListener;
    type RawStream = TcpStream;
    type Stream = TlsStream<TcpStream>;

    fn new(config: &TransportConfig) -> Result<Self> {
        let tcp = TcpTransport::new(config)?;
        let config = config
            .tls
            .as_ref()
            .ok_or_else(|| anyhow!("Missing tls config"))?;

        // A client always needs a connector. A server-only setup (pkcs12
        // configured, no trusted_root) skips loading client roots here so it
        // also starts on hosts without a native CA store; if that transport
        // is ever used as a client, `connect` builds the connector on demand
        // and surfaces the real error.
        let connector = match (&config.trusted_root, &config.pkcs12) {
            (None, Some(_)) => None,
            _ => load_client_config(config)?.map(|c| Arc::new(c).into()),
        };
        let tls_acceptor = load_server_config(config)?.map(|c| Arc::new(c).into());

        Ok(TlsTransport {
            tcp,
            config: config.clone(),
            connector,
            tls_acceptor,
        })
    }

    fn hint(conn: &Self::Stream, opt: SocketOpts) {
        opt.apply(conn.get_ref().0);
    }

    async fn bind<A: ToSocketAddrs + Send + Sync>(&self, addr: A) -> Result<Self::Acceptor> {
        let l = TcpListener::bind(addr)
            .await
            .with_context(|| "Failed to create tcp listener")?;
        Ok(l)
    }

    async fn accept(&self, a: &Self::Acceptor) -> Result<(Self::RawStream, SocketAddr)> {
        self.tcp
            .accept(a)
            .await
            .with_context(|| "Failed to accept TCP connection")
    }

    async fn handshake(&self, conn: Self::RawStream) -> Result<Self::Stream> {
        let conn = self
            .tls_acceptor
            .as_ref()
            .ok_or_else(|| anyhow!("Missing `tls.pkcs12` for running as a server"))?
            .accept(conn)
            .await?;
        Ok(tokio_rustls::TlsStream::Server(conn))
    }

    async fn connect(&self, addr: &AddrMaybeCached) -> Result<Self::Stream> {
        let conn = self.tcp.connect(addr).await?;

        // Server-only transports reach this point without a connector; build
        // it now so the failure names the real cause (missing roots etc.)
        // instead of a vague "no client config".
        let built;
        let connector = match self.connector.as_ref() {
            Some(c) => c,
            None => {
                let client_config = load_client_config(&self.config)?.ok_or_else(|| {
                    anyhow!("No tls client config available for running as a client")
                })?;
                built = Arc::new(client_config).into();
                &built
            }
        };

        let host_name = self
            .config
            .hostname
            .as_deref()
            .unwrap_or(host_port_pair(&addr.addr)?.0);

        Ok(tokio_rustls::TlsStream::Client(
            connector
                .connect(ServerName::try_from(host_name)?.to_owned(), conn)
                .await?,
        ))
    }
}

pub(crate) fn get_tcpstream(s: &TlsStream<TcpStream>) -> &TcpStream {
    s.get_ref().0
}

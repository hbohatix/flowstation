use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use quinn::{Connection, Endpoint, RecvStream, SendStream, VarInt};
#[cfg(test)]
use rustls::pki_types::{CertificateDer, ServerName};

use super::{MAX_FRAME_BYTES, NetworkAddress, NetworkError, NetworkMessage, NetworkTransport, drain_length_prefixed};

/// Channel type for QUIC streams
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuicChannelType {
    /// Reliable ordered channel for signalling
    Reliable,
    /// Unreliable unordered channel for voice (low latency)
    Unreliable,
}

/// Configuration for creating a QUIC transport
#[derive(Clone)]
pub struct QuicTransportConfig {
    /// Server address to connect to
    pub server_addr: NetworkAddress,
    /// Connection timeout
    pub connect_timeout: Duration,
    /// Skip certificate verification. TEST BUILDS ONLY — the all-accepting verifier is not
    /// compiled into release binaries, so setting this outside `cfg(test)` fails the connect.
    pub skip_cert_verification: bool,
    /// Tokio runtime handle for async operations
    pub runtime: tokio::runtime::Handle,
}

impl QuicTransportConfig {
    /// Create a new QUIC transport configuration
    pub fn new(server_addr: NetworkAddress, runtime: tokio::runtime::Handle) -> Self {
        Self {
            server_addr,
            connect_timeout: Duration::from_secs(5),
            skip_cert_verification: false,
            runtime,
        }
    }

    /// Create an insecure QUIC transport configuration. Only usable in test builds — see
    /// [`QuicTransportConfig::skip_cert_verification`].
    pub fn insecure(server_addr: NetworkAddress, runtime: tokio::runtime::Handle) -> Self {
        Self {
            server_addr,
            connect_timeout: Duration::from_secs(5),
            skip_cert_verification: true,
            runtime,
        }
    }

    /// Set connection timeout
    pub fn with_connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }
}

/// ALPN this transport has always sent. Kept as the default so existing servers keep working.
pub const DEFAULT_ALPN: &[u8] = b"hq-29";

/// How long a non-blocking receive waits for data before returning nothing.
const RECEIVE_POLL: Duration = Duration::from_millis(10);

/// Which server certificates the client accepts.
#[derive(Clone, Debug, Default)]
pub enum QuicTrust {
    /// The operating system's root store (default; e.g. a Let's Encrypt certificate).
    #[default]
    System,
    /// Only these certificates (PEM). Used to pin a self-signed server, such as the
    /// flowstation-platform core's generated `core-cert.pem`.
    PinnedPem(Vec<u8>),
}

/// Optional client settings for [`QuicTransport::with_options`].
#[derive(Clone, Debug)]
pub struct QuicClientOptions {
    pub alpn: Vec<Vec<u8>>,
    pub trust: QuicTrust,
    /// TLS server name (SNI + certificate check). Defaults to the host part of the address.
    pub server_name: Option<String>,
}

impl Default for QuicClientOptions {
    fn default() -> Self {
        Self { alpn: vec![DEFAULT_ALPN.to_vec()], trust: QuicTrust::System, server_name: None }
    }
}

/// QUIC-based network transport
/// Provides both reliable signalling and unreliable voice channels
pub struct QuicTransport {
    /// TLS server name used on connect.
    server_name: String,
    endpoint: Option<Endpoint>,
    connection: Option<Connection>,
    server_addr: NetworkAddress,
    socket_addr: SocketAddr,
    connect_timeout: Duration,
    /// Cached reliable bi-directional stream for signalling
    reliable_send: Option<SendStream>,
    reliable_recv: Option<RecvStream>,
    /// Bytes read from the reliable stream that do not yet form a complete frame. Kept across
    /// calls: the read below is cancelled by its short timeout, and anything already consumed
    /// would otherwise be dropped, desyncing the length-prefixed framing.
    reliable_rx_buf: Vec<u8>,
    /// Frames decoded but not yet handed to the caller (one `receive_reliable` = one message)
    reliable_pending: std::collections::VecDeque<Vec<u8>>,
    /// Tokio runtime handle for async operations
    runtime: tokio::runtime::Runtime,
}

impl QuicTransport {
    /// Create a new QUIC transport
    ///
    /// # Arguments
    /// * `server_addr` - Server address to connect to
    /// * `connect_timeout` - Connection timeout duration
    /// * `skip_cert_verification` - Skip certificate verification (useful for testing)
    /// * `runtime` - Tokio runtimefor async operations
    pub fn new(
        server_addr: NetworkAddress,
        connect_timeout: Duration,
        skip_cert_verification: bool,
        runtime: tokio::runtime::Runtime,
    ) -> Result<Self, NetworkError> {
        Self::build(server_addr, connect_timeout, skip_cert_verification, QuicClientOptions::default(), runtime)
    }

    /// Like [`QuicTransport::new`], with ALPN, certificate pinning and TLS server name.
    pub fn with_options(
        server_addr: NetworkAddress,
        connect_timeout: Duration,
        options: QuicClientOptions,
        runtime: tokio::runtime::Runtime,
    ) -> Result<Self, NetworkError> {
        Self::build(server_addr, connect_timeout, false, options, runtime)
    }

    fn build(
        server_addr: NetworkAddress,
        connect_timeout: Duration,
        skip_cert_verification: bool,
        options: QuicClientOptions,
        runtime: tokio::runtime::Runtime,
    ) -> Result<Self, NetworkError> {
        let socket_addr = Self::parse_socket_addr(&server_addr)?;
        let server_name = match (&options.server_name, &server_addr) {
            (Some(name), _) => name.clone(),
            (None, NetworkAddress::Tcp { host, .. } | NetworkAddress::Udp { host, .. }) => host.clone(),
            (None, NetworkAddress::Custom { .. }) => {
                return Err(NetworkError::ConnectionFailed("Custom addresses not supported for QUIC".to_string()));
            }
        };

        // Create endpoint within the runtime context
        let endpoint = runtime.block_on(async {
            // Configure QUIC client
            let crypto = if skip_cert_verification {
                Self::configure_insecure_client()
            } else {
                Self::configure_client(&options)
            }?;

            let mut client_config = quinn::ClientConfig::new(Arc::new(crypto));

            // Configure transport for low latency
            let mut transport_config = quinn::TransportConfig::default();

            // Reduce keep-alive interval for faster detection of disconnects
            transport_config.keep_alive_interval(Some(Duration::from_secs(5)));

            // Lower max idle timeout
            transport_config.max_idle_timeout(Some(VarInt::from_u32(30_000).into()));

            client_config.transport_config(Arc::new(transport_config));

            // Create endpoint
            let mut endpoint = Endpoint::client("[::]:0".parse().unwrap())
                .map_err(|e| NetworkError::ConnectionFailed(format!("Failed to create endpoint: {}", e)))?;

            endpoint.set_default_client_config(client_config);

            Ok::<_, NetworkError>(endpoint)
        })?;

        Ok(Self {
            server_name,
            endpoint: Some(endpoint),
            connection: None,
            server_addr,
            socket_addr,
            connect_timeout,
            reliable_send: None,
            reliable_recv: None,
            reliable_rx_buf: Vec::new(),
            reliable_pending: std::collections::VecDeque::new(),
            runtime,
        })
    }

    /// Parse NetworkAddress to SocketAddr. IP literals parse directly; host names are resolved.
    fn parse_socket_addr(addr: &NetworkAddress) -> Result<SocketAddr, NetworkError> {
        match addr {
            NetworkAddress::Tcp { host, port } | NetworkAddress::Udp { host, port } => {
                if let Ok(sa) = format!("{}:{}", host, port).parse() {
                    return Ok(sa);
                }
                use std::net::ToSocketAddrs;
                (host.as_str(), *port)
                    .to_socket_addrs()
                    .map_err(|e| NetworkError::ConnectionFailed(format!("Invalid address {host}: {e}")))?
                    .next()
                    .ok_or_else(|| NetworkError::ConnectionFailed(format!("No address for {host}")))
            }
            NetworkAddress::Custom { .. } => Err(NetworkError::ConnectionFailed(
                "Custom addresses not supported for QUIC".to_string(),
            )),
        }
    }

    /// Configure client with certificate verification (production)
    fn configure_client(options: &QuicClientOptions) -> Result<quinn::crypto::rustls::QuicClientConfig, NetworkError> {
        let mut roots = rustls::RootCertStore::empty();

        match &options.trust {
            QuicTrust::System => {
                for cert in rustls_native_certs::load_native_certs()
                    .map_err(|e| NetworkError::ConnectionFailed(format!("Failed to load native certs: {}", e)))?
                {
                    roots
                        .add(cert)
                        .map_err(|e| NetworkError::ConnectionFailed(format!("Failed to add cert: {}", e)))?;
                }
            }
            QuicTrust::PinnedPem(pem) => {
                for cert in rustls_pemfile::certs(&mut pem.as_slice()) {
                    let cert = cert.map_err(|e| NetworkError::ConnectionFailed(format!("Bad pinned certificate: {}", e)))?;
                    roots
                        .add(cert)
                        .map_err(|e| NetworkError::ConnectionFailed(format!("Failed to add pinned cert: {}", e)))?;
                }
                if roots.is_empty() {
                    return Err(NetworkError::ConnectionFailed("Pinned certificate file contains no certificate".to_string()));
                }
            }
        }

        let mut crypto = rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();

        // Set ALPN protocol for QUIC
        crypto.alpn_protocols = options.alpn.clone();

        Ok(quinn::crypto::rustls::QuicClientConfig::try_from(crypto)
            .map_err(|e| NetworkError::ConnectionFailed(format!("Failed to build QUIC config: {}", e)))?)
    }

    /// Release builds must not be able to skip certificate validation at all: the
    /// all-accepting verifier below is `cfg(test)`-only, so asking for it here fails loudly
    /// instead of silently connecting to whoever answers.
    #[cfg(not(test))]
    fn configure_insecure_client() -> Result<quinn::crypto::rustls::QuicClientConfig, NetworkError> {
        Err(NetworkError::ConnectionFailed(
            "skip_cert_verification is only available in test builds".to_string(),
        ))
    }

    /// Configure insecure client (skip certificate verification - testing only)
    #[cfg(test)]
    fn configure_insecure_client() -> Result<quinn::crypto::rustls::QuicClientConfig, NetworkError> {
        let mut crypto = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(SkipServerVerification))
            .with_no_client_auth();

        // Set ALPN protocol for QUIC
        crypto.alpn_protocols = vec![b"hq-29".to_vec()];

        quinn::crypto::rustls::QuicClientConfig::try_from(crypto)
            .map_err(|e| NetworkError::ConnectionFailed(format!("Failed to build insecure QUIC config: {}", e)))
    }

    /// Send data on a specific channel type
    pub async fn send_on_channel(&mut self, payload: &[u8], channel: QuicChannelType) -> Result<(), NetworkError> {
        let conn = self
            .connection
            .as_ref()
            .ok_or_else(|| NetworkError::SendFailed("No active connection".to_string()))?;

        match channel {
            QuicChannelType::Reliable => {
                // Use cached bidirectional stream for signalling
                if self.reliable_send.is_none() {
                    let (send, recv) = conn
                        .open_bi()
                        .await
                        .map_err(|e| NetworkError::SendFailed(format!("Failed to open stream: {}", e)))?;
                    self.reliable_send = Some(send);
                    self.reliable_recv = Some(recv);
                }

                let send = self.reliable_send.as_mut().unwrap();

                // Send length-prefixed message
                let len = (payload.len() as u32).to_be_bytes();
                send.write_all(&len)
                    .await
                    .map_err(|e| NetworkError::SendFailed(format!("Failed to send length: {}", e)))?;
                send.write_all(payload)
                    .await
                    .map_err(|e| NetworkError::SendFailed(format!("Failed to send payload: {}", e)))?;
            }
            QuicChannelType::Unreliable => {
                // Send as unreliable datagram for low latency voice
                conn.send_datagram(payload.to_vec().into())
                    .map_err(|e| NetworkError::SendFailed(format!("Failed to send datagram: {}", e)))?;
            }
        }

        Ok(())
    }

    /// Receive data from a specific channel type (non-blocking)
    pub async fn receive_from_channel(&mut self, channel: QuicChannelType) -> Result<Option<Vec<u8>>, NetworkError> {
        let conn = self
            .connection
            .as_ref()
            .ok_or_else(|| NetworkError::ReceiveFailed("No active connection".to_string()))?;

        match channel {
            QuicChannelType::Reliable => {
                // Anything already decoded goes out first.
                if let Some(frame) = self.reliable_pending.pop_front() {
                    return Ok(Some(frame));
                }

                // One bounded read into our own buffer. `read_exact` was used here before, but
                // it is not cancel-safe: when the 10 ms timeout fired mid-message the bytes it
                // had consumed were thrown away and the framing never recovered.
                let mut chunk = vec![0u8; 8192];
                let read = match self.reliable_recv.as_mut() {
                    Some(recv) => match tokio::time::timeout(Duration::from_millis(10), recv.read(&mut chunk)).await {
                        Ok(Ok(Some(n))) => Some(n),
                        Ok(Ok(None)) => None,  // Stream finished
                        Ok(Err(_)) => return Ok(None), // Stream error
                        Err(_) => None,        // Timeout — nothing new this call
                    },
                    None => return Ok(None),
                };
                if let Some(n) = read {
                    self.reliable_rx_buf.extend_from_slice(&chunk[..n]);
                    if self.reliable_rx_buf.len() > 2 * MAX_FRAME_BYTES {
                        self.reliable_rx_buf.clear();
                        return Err(NetworkError::ReceiveFailed("reliable stream desynced, buffer reset".to_string()));
                    }
                }

                match drain_length_prefixed(&mut self.reliable_rx_buf) {
                    Ok(frames) => self.reliable_pending.extend(frames),
                    Err(e) => {
                        self.reliable_rx_buf.clear();
                        return Err(NetworkError::ReceiveFailed(e));
                    }
                }
                Ok(self.reliable_pending.pop_front())
            }
            QuicChannelType::Unreliable => {
                // Bounded wait like the reliable channel: without it a caller polling for voice
                // blocks forever when no datagram arrives. read_datagram is cancel-safe.
                match tokio::time::timeout(RECEIVE_POLL, conn.read_datagram()).await {
                    Err(_) => Ok(None),
                    Ok(Ok(data)) => Ok(Some(data.to_vec())),
                    Ok(Err(quinn::ConnectionError::ApplicationClosed(_))) => Ok(None),
                    Ok(Err(e)) => Err(NetworkError::ReceiveFailed(format!("Failed to receive datagram: {}", e))),
                }
            }
        }
    }

    /// Close reliable stream
    pub fn close_reliable_stream(&mut self) {
        if let Some(mut send) = self.reliable_send.take() {
            let _ = send.finish();
        }
        self.reliable_recv = None;
        // Buffered bytes belong to the stream we just closed.
        self.reliable_rx_buf.clear();
        self.reliable_pending.clear();
    }
}

impl NetworkTransport for QuicTransport {
    fn connect(&mut self) -> Result<(), NetworkError> {
        tracing::debug!("QuicTransport connecting to {:?}", self.server_addr);

        // Close existing connection
        if let Some(conn) = self.connection.take() {
            conn.close(VarInt::from_u32(0), b"reconnecting");
        }
        self.close_reliable_stream();

        let endpoint = self
            .endpoint
            .as_ref()
            .ok_or_else(|| NetworkError::ConnectionFailed("No endpoint".to_string()))?;

        let server_name = self.server_name.clone();

        // Connect with timeout
        let runtime = self.runtime.handle().clone();
        let connection = runtime.block_on(async {
            let connecting = endpoint
                .connect(self.socket_addr, &server_name)
                .map_err(|e| NetworkError::ConnectionFailed(format!("Failed to initiate connection: {}", e)))?;

            tokio::time::timeout(self.connect_timeout, connecting)
                .await
                .map_err(|_| NetworkError::Timeout)?
                .map_err(|e| NetworkError::ConnectionFailed(format!("Connection failed: {}", e)))
        })?;

        self.connection = Some(connection);

        Ok(())
    }

    fn send_reliable(&mut self, payload: &[u8]) -> Result<(), NetworkError> {
        // Synchronous wrapper around async send_on_channel
        let runtime = self.runtime.handle().clone();
        runtime.block_on(async { self.send_on_channel(payload, QuicChannelType::Reliable).await })
    }

    fn send_unreliable(&mut self, payload: &[u8]) -> Result<(), NetworkError> {
        // Synchronous wrapper around async send_on_channel
        let runtime = self.runtime.handle().clone();
        runtime.block_on(async { self.send_on_channel(payload, QuicChannelType::Unreliable).await })
    }

    fn receive_reliable(&mut self) -> Vec<NetworkMessage> {
        // Non-blocking receive from reliable channel
        let mut messages = Vec::new();

        let runtime = self.runtime.handle().clone();
        if let Ok(Some(payload)) = runtime.block_on(async { self.receive_from_channel(QuicChannelType::Reliable).await }) {
            messages.push(NetworkMessage {
                source: self.server_addr.clone(),
                payload,
                timestamp: Instant::now(),
            });
        }

        messages
    }

    fn receive_unreliable(&mut self) -> Vec<NetworkMessage> {
        // Non-blocking receive from unreliable channel (datagrams)
        let mut messages = Vec::new();

        let runtime = self.runtime.handle().clone();
        if let Ok(Some(payload)) = runtime.block_on(async { self.receive_from_channel(QuicChannelType::Unreliable).await }) {
            messages.push(NetworkMessage {
                source: self.server_addr.clone(),
                payload,
                timestamp: Instant::now(),
            });
        }

        messages
    }

    fn wait_for_response_reliable(&mut self) -> Result<NetworkMessage, NetworkError> {
        // Blocking receive from reliable channel
        let runtime = self.runtime.handle().clone();
        runtime.block_on(async {
            match self.receive_from_channel(QuicChannelType::Reliable).await? {
                Some(payload) => Ok(NetworkMessage {
                    source: self.server_addr.clone(),
                    payload,
                    timestamp: Instant::now(),
                }),
                None => Err(NetworkError::ReceiveFailed("Connection closed".to_string())),
            }
        })
    }
}

impl Drop for QuicTransport {
    fn drop(&mut self) {
        if let Some(conn) = self.connection.take() {
            conn.close(VarInt::from_u32(0), b"shutdown");
        }
        if let Some(endpoint) = self.endpoint.take() {
            endpoint.close(VarInt::from_u32(0), b"shutdown");
        }
    }
}

/// Certificate verifier that accepts any certificate (INSECURE - testing only).
/// Deliberately `cfg(test)`: it must never exist in a shipped binary.
#[cfg(test)]
#[derive(Debug)]
struct SkipServerVerification;

#[cfg(test)]
impl rustls::client::danger::ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::ED25519,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestServer {
        addr: SocketAddr,
        cert_pem: String,
        endpoint: Endpoint,
    }

    /// QUIC echo-ish server on 127.0.0.1 with a fresh self-signed cert for "localhost".
    fn start_server(rt: &tokio::runtime::Runtime, alpn: &[u8]) -> TestServer {
        let rcgen::CertifiedKey { cert, key_pair } = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let key = rustls::pki_types::PrivateKeyDer::Pkcs8(key_pair.serialize_der().into());
        let mut crypto = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert.der().clone()], key)
            .unwrap();
        crypto.alpn_protocols = vec![alpn.to_vec()];
        let server = quinn::ServerConfig::with_crypto(Arc::new(quinn::crypto::rustls::QuicServerConfig::try_from(crypto).unwrap()));
        let endpoint = rt.block_on(async { Endpoint::server(server, "127.0.0.1:0".parse().unwrap()).unwrap() });
        let addr = endpoint.local_addr().unwrap();
        let ep = endpoint.clone();
        rt.spawn(async move {
            while let Some(incoming) = ep.accept().await {
                tokio::spawn(async move {
                    if let Ok(conn) = incoming.await {
                        // Echo the first datagram back, then keep the connection open.
                        if let Ok(d) = conn.read_datagram().await {
                            let _ = conn.send_datagram(d);
                        }
                        conn.closed().await;
                    }
                });
            }
        });
        TestServer { addr, cert_pem: cert.pem(), endpoint }
    }

    fn client(server: &TestServer, trust: QuicTrust, alpn: &[u8]) -> QuicTransport {
        let addr = NetworkAddress::Udp { host: server.addr.ip().to_string(), port: server.addr.port() };
        let options = QuicClientOptions { alpn: vec![alpn.to_vec()], trust, server_name: Some("localhost".into()) };
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        QuicTransport::with_options(addr, Duration::from_secs(3), options, rt).unwrap()
    }

    fn server_rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap()
    }

    #[test]
    fn pinned_certificate_connects_and_datagrams_flow() {
        let rt = server_rt();
        let server = start_server(&rt, b"flowstation-dispatch-v0");
        let mut t = client(&server, QuicTrust::PinnedPem(server.cert_pem.clone().into_bytes()), b"flowstation-dispatch-v0");
        t.connect().expect("connects with pinned cert");

        // Nothing sent yet: polling must return quickly instead of blocking.
        let start = Instant::now();
        assert!(t.receive_unreliable().is_empty());
        assert!(start.elapsed() < Duration::from_millis(500), "datagram poll blocked for {:?}", start.elapsed());

        t.send_unreliable(b"voice").unwrap();
        let mut echoed = Vec::new();
        for _ in 0..100 {
            echoed = t.receive_unreliable();
            if !echoed.is_empty() {
                break;
            }
        }
        assert_eq!(echoed.first().map(|m| m.payload.as_slice()), Some(&b"voice"[..]));
        server.endpoint.close(VarInt::from_u32(0), b"done");
    }

    #[test]
    fn other_certificate_is_refused() {
        let rt = server_rt();
        let server = start_server(&rt, DEFAULT_ALPN);
        let other = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap().cert.pem();
        let mut t = client(&server, QuicTrust::PinnedPem(other.into_bytes()), DEFAULT_ALPN);
        assert!(t.connect().is_err(), "must not connect to a server with a different certificate");
    }

    #[test]
    fn alpn_mismatch_is_refused() {
        let rt = server_rt();
        let server = start_server(&rt, b"flowstation-dispatch-v0");
        let mut t = client(&server, QuicTrust::PinnedPem(server.cert_pem.clone().into_bytes()), b"something-else");
        assert!(t.connect().is_err());
    }

    #[test]
    fn empty_pin_file_is_an_error() {
        let rt = server_rt();
        let server = start_server(&rt, DEFAULT_ALPN);
        let addr = NetworkAddress::Udp { host: server.addr.ip().to_string(), port: server.addr.port() };
        let options = QuicClientOptions { trust: QuicTrust::PinnedPem(b"not a pem".to_vec()), ..Default::default() };
        let result = QuicTransport::with_options(addr, Duration::from_secs(1), options, server_rt());
        assert!(result.is_err());
    }

    #[test]
    fn default_options_keep_legacy_alpn() {
        assert_eq!(QuicClientOptions::default().alpn, vec![b"hq-29".to_vec()]);
    }
}

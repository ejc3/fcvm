//! The coordinator's port 443 with Lambda tiles (`--lambda aws`): one listener per public
//! address of the task, routed by TLS server name (fleet/PROTOCOL.md, "Lambda tile"):
//!
//! - the tunnel endpoint's name (`tunnel.<subzone>`): TLS ends here, with the subzone
//!   certificate, and the connection is a Lambda tile's tunnel ([`tunnel::Endpoint::serve`]);
//! - a Lambda tile's launch name (`lam-<n>-<token>.<subzone>`, or its `-v4` name): the
//!   connection, its ClientHello included, goes byte for byte over that tile's tunnel to the
//!   renderer's TLS listener, so TLS ends in the renderer as it does on an EC2 tile;
//! - any other name: the EC2 half's aggregator, which listens behind the front on loopback.
//!
//! The ClientHello is read with the GPU host's IPv4 front's parser (`agent::sni`). A
//! connection whose first bytes are not a ClientHello naming a server is closed without a
//! byte sent.

use crate::fleet::agent::sni;
use crate::fleet::aggregate::tls::CertFile;
use crate::fleet::tunnel::{Endpoint, Port};
use anyhow::{bail, Context, Result};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;

/// The tunnel endpoint's label under the subzone.
pub const TUNNEL_LABEL: &str = "tunnel";
/// The loopback port the EC2 aggregator's TLS listener moves to behind the front.
pub const AGGREGATOR_BEHIND: u16 = 9443;
/// Connections handled at once by one front; past it a connection is closed at once.
pub const MAX_CONNECTIONS: usize = 8192;
/// How long a connection may take to send its ClientHello.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the aggregator behind the front may take to accept.
const BACKEND_TIMEOUT: Duration = Duration::from_secs(5);

/// Where a server name goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    Tunnel,
    /// A Lambda tile's renderer, by slot (`lam-<n>`).
    Tile(String),
    Aggregator,
}

/// The route of `name` (lowercase, no trailing dot) under `zone`.
pub fn route(name: &str, zone: &str) -> Route {
    let Some(label) = name
        .strip_suffix(zone)
        .and_then(|l| l.strip_suffix('.'))
        .filter(|l| !l.contains('.'))
    else {
        return Route::Aggregator;
    };
    if label == TUNNEL_LABEL {
        return Route::Tunnel;
    }
    let label = label.strip_suffix("-v4").unwrap_or(label);
    let Some((id, token)) = label.rsplit_once('-') else {
        return Route::Aggregator;
    };
    let token_ok = token.len() == 8
        && token
            .bytes()
            .all(|b| b.is_ascii_lowercase() || (b'2'..=b'7').contains(&b));
    match id.strip_prefix("lam-") {
        Some(n) if token_ok && !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => {
            Route::Tile(id.to_string())
        }
        _ => Route::Aggregator,
    }
}

/// A stream that yields `prefix` before reading on: the ClientHello the front already read.
pub struct Prefixed<S> {
    prefix: Vec<u8>,
    at: usize,
    inner: S,
}

impl<S> Prefixed<S> {
    pub fn new(prefix: Vec<u8>, inner: S) -> Self {
        Prefixed {
            prefix,
            at: 0,
            inner,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Prefixed<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.at < self.prefix.len() {
            let n = (self.prefix.len() - self.at).min(buf.remaining());
            let at = self.at;
            buf.put_slice(&self.prefix[at..at + n]);
            self.at += n;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Prefixed<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

pub struct Front {
    pub zone: String,
    pub endpoint: Arc<Endpoint>,
    pub cert: Arc<CertFile>,
    /// The aggregator's TLS listener behind the front; None: other names are closed.
    pub aggregator: Option<SocketAddr>,
    slots: Arc<Semaphore>,
}

impl Front {
    pub fn new(
        zone: String,
        endpoint: Arc<Endpoint>,
        cert: Arc<CertFile>,
        aggregator: Option<SocketAddr>,
    ) -> Arc<Self> {
        Arc::new(Front {
            zone,
            endpoint,
            cert,
            aggregator,
            slots: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
        })
    }

    /// Accept on `listener` for as long as the process runs.
    pub async fn serve(self: Arc<Self>, listener: TcpListener) {
        loop {
            let (conn, peer) = match listener.accept().await {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(error = %e, "the Lambda front could not accept");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let Ok(permit) = self.slots.clone().try_acquire_owned() else {
                continue;
            };
            let front = self.clone();
            tokio::spawn(async move {
                let _permit = permit;
                if let Err(e) = front.handle(conn).await {
                    tracing::debug!(%peer, error = %format!("{e:#}"), "a connection to the Lambda front ended");
                }
            });
        }
    }

    async fn handle(&self, mut conn: TcpStream) -> Result<()> {
        conn.set_nodelay(true)?;
        let mut hello = Vec::new();
        let name = tokio::time::timeout(HELLO_TIMEOUT, sni::read_hello(&mut conn, &mut hello))
            .await
            .context("no ClientHello in time")??;
        match route(&name, &self.zone) {
            Route::Tunnel => {
                let acceptor = tokio_rustls::TlsAcceptor::from(self.cert.config());
                let tls = tokio::time::timeout(
                    HELLO_TIMEOUT,
                    acceptor.accept(Prefixed::new(hello, conn)),
                )
                .await
                .context("the tunnel handshake timed out")??;
                self.endpoint.serve(tls).await
            }
            Route::Tile(slot) => {
                let mut tile = self.endpoint.open(&slot, Port::Tls).await?;
                tile.write_all(&hello).await?;
                tokio::io::copy_bidirectional(&mut conn, &mut tile).await?;
                Ok(())
            }
            Route::Aggregator => {
                let Some(addr) = self.aggregator else {
                    bail!("no listener for {name}");
                };
                let mut back = tokio::time::timeout(BACKEND_TIMEOUT, TcpStream::connect(addr))
                    .await
                    .context("the aggregator did not accept in time")??;
                back.set_nodelay(true)?;
                back.write_all(&hello).await?;
                tokio::io::copy_bidirectional(&mut conn, &mut back).await?;
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_routed_by_label() {
        let z = "x.fcvm.dev";
        assert_eq!(route("tunnel.x.fcvm.dev", z), Route::Tunnel);
        assert_eq!(
            route("lam-3-abcdefgh.x.fcvm.dev", z),
            Route::Tile("lam-3".into())
        );
        assert_eq!(
            route("lam-12-abcd2345-v4.x.fcvm.dev", z),
            Route::Tile("lam-12".into())
        );
        assert_eq!(route("agg-ec2-abcdefgh.x.fcvm.dev", z), Route::Aggregator);
        assert_eq!(route("ec2-1-abcdefgh-v4.x.fcvm.dev", z), Route::Aggregator);
        // Not a launch name: no token, a bad token, another zone, a deeper name.
        assert_eq!(route("lam-3.x.fcvm.dev", z), Route::Aggregator);
        assert_eq!(route("lam-3-ABCDEFGH.x.fcvm.dev", z), Route::Aggregator);
        assert_eq!(route("lam-3-abcdefg1.x.fcvm.dev", z), Route::Aggregator);
        assert_eq!(route("lam-x-abcdefgh.x.fcvm.dev", z), Route::Aggregator);
        assert_eq!(route("lam-3-abcdefgh.y.fcvm.dev", z), Route::Aggregator);
        assert_eq!(route("a.lam-3-abcdefgh.x.fcvm.dev", z), Route::Aggregator);
        assert_eq!(route("tunnel.y.fcvm.dev", z), Route::Aggregator);
    }

    #[tokio::test]
    async fn a_prefixed_stream_yields_the_prefix_first() {
        use tokio::io::AsyncReadExt;
        let (a, mut b) = tokio::io::duplex(64);
        b.write_all(b" world").await.unwrap();
        drop(b);
        let mut p = Prefixed::new(b"hello".to_vec(), a);
        let mut s = String::new();
        p.read_to_string(&mut s).await.unwrap();
        assert_eq!(s, "hello world");
    }

    fn provider() -> Arc<rustls::crypto::CryptoProvider> {
        Arc::new(rustls::crypto::ring::default_provider())
    }

    /// The first flight of a TLS client opening `name`.
    fn client_hello(name: &str) -> Vec<u8> {
        let config = rustls::ClientConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(rustls::RootCertStore::empty())
            .with_no_client_auth();
        let mut c = rustls::ClientConnection::new(
            Arc::new(config),
            rustls::pki_types::ServerName::try_from(name.to_string()).unwrap(),
        )
        .unwrap();
        let mut out = Vec::new();
        c.write_tls(&mut out).unwrap();
        out
    }

    /// A TCP stand-in that records what it reads first and answers `reply`.
    async fn stand_in(reply: &'static [u8]) -> (SocketAddr, Arc<std::sync::Mutex<Vec<u8>>>) {
        use tokio::io::AsyncReadExt;
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let got: Arc<std::sync::Mutex<Vec<u8>>> = Arc::default();
        let seen = got.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut c, _)) = l.accept().await else {
                    return;
                };
                let seen = seen.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 16384];
                    let n = c.read(&mut buf).await.unwrap_or(0);
                    seen.lock().unwrap().extend_from_slice(&buf[..n]);
                    let _ = c.write_all(reply).await;
                });
            }
        });
        (addr, got)
    }

    struct TunnelDialer {
        front: SocketAddr,
        connector: tokio_rustls::TlsConnector,
    }

    #[async_trait::async_trait]
    impl crate::fleet::tunnel::Dialer for TunnelDialer {
        async fn dial(&self) -> Result<crate::fleet::tunnel::Stream> {
            let tcp = TcpStream::connect(self.front).await?;
            let name = rustls::pki_types::ServerName::try_from("tunnel.x.example".to_string())?;
            Ok(Box::new(self.connector.connect(name, tcp).await?))
        }
    }

    struct To(SocketAddr);

    #[async_trait::async_trait]
    impl crate::fleet::tunnel::Local for To {
        async fn connect(&self, _: Port) -> Result<crate::fleet::tunnel::Stream> {
            Ok(Box::new(TcpStream::connect(self.0).await?))
        }
    }

    async fn answer(front: SocketAddr, first: &[u8]) -> Vec<u8> {
        use tokio::io::AsyncReadExt;
        let mut c = TcpStream::connect(front).await.unwrap();
        c.write_all(first).await.unwrap();
        let mut got = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(10), c.read_to_end(&mut got)).await;
        got
    }

    #[tokio::test]
    async fn the_front_routes_the_tunnel_a_tiles_viewers_and_the_aggregator() {
        use crate::fleet::tunnel::{self, Identity};
        // The subzone certificate, as the coordinator's tls.json.
        let ck = rcgen::generate_simple_self_signed(vec!["*.x.example".to_string()]).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("tls.json");
        std::fs::write(
            &file,
            serde_json::json!({"cert": ck.cert.pem(), "key": ck.signing_key.serialize_pem()})
                .to_string(),
        )
        .unwrap();
        let cert = CertFile::load(&file).unwrap();
        let key = tunnel::derive_key("token-for-tests");
        let endpoint = Endpoint::new(tunnel::derive_key("token-for-tests"));
        let (agg, agg_seen) = stand_in(b"aggregator").await;
        let (renderer, renderer_seen) = stand_in(b"renderer").await;
        let front = Front::new("x.example".into(), endpoint.clone(), cert, Some(agg));
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let front_addr = l.local_addr().unwrap();
        tokio::spawn(front.serve(l));
        // The tile dials its tunnel through the front, TLS to tunnel.x.example.
        let mut roots = rustls::RootCertStore::empty();
        roots.add(ck.cert.der().clone()).unwrap();
        let connector = tokio_rustls::TlsConnector::from(Arc::new(
            rustls::ClientConfig::builder_with_provider(provider())
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        ));
        let id = Identity {
            slot: "lam-1".into(),
            generation: 1,
            env: "env-a".into(),
            started_ms: 1,
            credential: tunnel::mint(&key, "lam-1", 1, crate::fleet::now_ms() + 60_000),
            instance: None,
            instance_boot_ms: None,
        };
        let (up, mut up_rx) = tokio::sync::watch::channel(false);
        let (_stop, stop_rx) = tokio::sync::watch::channel(false);
        tokio::spawn(tunnel::run_client(
            id,
            Arc::new(TunnelDialer {
                front: front_addr,
                connector,
            }),
            Arc::new(To(renderer)),
            up,
            stop_rx,
        ));
        tokio::time::timeout(Duration::from_secs(10), up_rx.wait_for(|u| *u))
            .await
            .expect("the tunnel comes up through the front")
            .unwrap();
        // A viewer of the tile: its ClientHello reaches the renderer unchanged.
        let hello = client_hello("lam-1-abcdefgh.x.example");
        assert_eq!(answer(front_addr, &hello).await, b"renderer");
        assert_eq!(renderer_seen.lock().unwrap().as_slice(), hello.as_slice());
        // The -v4 name goes to the same tile.
        let v4 = client_hello("lam-1-abcdefgh-v4.x.example");
        assert_eq!(answer(front_addr, &v4).await, b"renderer");
        // The EC2 aggregator's name: to the aggregator behind the front.
        let a = client_hello("agg-ec2-abcdefgh.x.example");
        assert_eq!(answer(front_addr, &a).await, b"aggregator");
        assert_eq!(agg_seen.lock().unwrap().as_slice(), a.as_slice());
        // A tile with no tunnel, and bytes that are not TLS: closed without a byte.
        assert!(
            answer(front_addr, &client_hello("lam-2-abcdefgh.x.example"))
                .await
                .is_empty()
        );
        assert!(answer(front_addr, b"GET / HTTP/1.1\r\n\r\n")
            .await
            .is_empty());
    }
}

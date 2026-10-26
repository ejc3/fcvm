//! The aggregator's public listeners (`--tls-listen`): browsers on an https page open
//! `wss://`, so a region stream they read is served with TLS. The certificate is the
//! fleet's wildcard in the file the agent writes for its renderers (`--tls-file`,
//! `tls.json`: a JSON object `{"cert", "key"}`, PEM; src/fleet/agent/tls.rs). The file is
//! read again every 2 s, and a new version is used for the connections that follow, so a
//! renewal needs no restart; a version that does not load is logged and the previous one
//! kept.
//!
//! A public listener serves only `/frames` and `/healthz` ([`super::public_router`]):
//! `/layout` and `/stats` stay on `--listen`, which the agent and the coordinator keep on
//! an address of their own.

use anyhow::{bail, Context, Result};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::ServerConfig;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Semaphore};
use tokio_rustls::server::TlsStream;
use tokio_rustls::TlsAcceptor;

/// How often the certificate file is read again.
pub const RELOAD: Duration = Duration::from_secs(2);
/// A client that has not finished its handshake by then is dropped.
const HANDSHAKE: Duration = Duration::from_secs(10);
/// Handshakes in progress at once per listener; further connections wait to be accepted.
const HANDSHAKES: usize = 256;

/// A server configuration for the certificate file's contents.
pub fn server_config(bytes: &[u8]) -> Result<Arc<ServerConfig>> {
    #[derive(serde::Deserialize)]
    struct File {
        cert: String,
        key: String,
    }
    let f: File =
        serde_json::from_slice(bytes).context("not a JSON object with `cert` and `key`")?;
    let certs = CertificateDer::pem_slice_iter(f.cert.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .context("`cert` is not PEM certificates")?;
    if certs.is_empty() {
        bail!("`cert` holds no certificate");
    }
    let key = PrivateKeyDer::from_pem_slice(f.key.as_bytes())
        .context("`key` is not a PEM private key")?;
    let mut c =
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .context("the certificate and key do not form a server identity")?;
    // WebSocket upgrades are HTTP/1.1.
    c.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(c))
}

/// The certificate file and the configuration loaded from its current version.
pub struct CertFile {
    path: PathBuf,
    current: RwLock<(Vec<u8>, Arc<ServerConfig>)>,
}

impl CertFile {
    /// Load the file; one that cannot be read or does not load is an error.
    pub fn load(path: &Path) -> Result<Arc<CertFile>> {
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let config =
            server_config(&bytes).with_context(|| format!("loading {}", path.display()))?;
        Ok(Arc::new(CertFile {
            path: path.to_path_buf(),
            current: RwLock::new((bytes, config)),
        }))
    }

    /// The configuration new connections get.
    pub fn config(&self) -> Arc<ServerConfig> {
        self.current.read().unwrap().1.clone()
    }

    /// Read the file again. Returns whether a new version was loaded; a version that does
    /// not load is an error and the previous one stays in use.
    pub fn reload(&self) -> Result<bool> {
        let bytes = std::fs::read(&self.path)
            .with_context(|| format!("reading {}", self.path.display()))?;
        if self.current.read().unwrap().0 == bytes {
            return Ok(false);
        }
        let config =
            server_config(&bytes).with_context(|| format!("loading {}", self.path.display()))?;
        *self.current.write().unwrap() = (bytes, config);
        Ok(true)
    }

    /// Reload every `every` for as long as the process runs.
    pub async fn watch(self: Arc<Self>, every: Duration) {
        loop {
            tokio::time::sleep(every).await;
            match self.reload() {
                Ok(true) => {
                    tracing::info!(file = %self.path.display(), "aggregator certificate updated")
                }
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "aggregator certificate not reloaded; keeping the previous one")
                }
            }
        }
    }
}

/// A TLS listener for `axum::serve`. Connections are accepted and their handshakes run on
/// a task of its own, each bounded by a deadline, so a client that stalls its handshake
/// delays no other.
pub struct TlsListener {
    rx: mpsc::Receiver<(TlsStream<TcpStream>, SocketAddr)>,
    local: SocketAddr,
    acceptor: tokio::task::JoinHandle<()>,
}

impl Drop for TlsListener {
    fn drop(&mut self) {
        self.acceptor.abort();
    }
}

impl TlsListener {
    /// The address it is bound to.
    pub fn address(&self) -> SocketAddr {
        self.local
    }

    pub async fn bind(addr: &str, certs: Arc<CertFile>) -> Result<TlsListener> {
        let tcp = TcpListener::bind(addr)
            .await
            .with_context(|| format!("listening on {addr}"))?;
        let local = tcp.local_addr()?;
        let (tx, rx) = mpsc::channel(64);
        let acceptor = tokio::spawn(accept_loop(tcp, certs, tx));
        Ok(TlsListener {
            rx,
            local,
            acceptor,
        })
    }
}

async fn accept_loop(
    tcp: TcpListener,
    certs: Arc<CertFile>,
    tx: mpsc::Sender<(TlsStream<TcpStream>, SocketAddr)>,
) {
    let slots = Arc::new(Semaphore::new(HANDSHAKES));
    loop {
        let Ok(permit) = slots.clone().acquire_owned().await else {
            return;
        };
        let (stream, peer) = match tcp.accept().await {
            Ok(c) => c,
            Err(e) => {
                // EMFILE and the like: back off instead of spinning.
                tracing::warn!(error = %e, "aggregator TLS accept failed");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let acceptor = TlsAcceptor::from(certs.config());
        let tx = tx.clone();
        tokio::spawn(async move {
            let _permit = permit;
            match tokio::time::timeout(HANDSHAKE, acceptor.accept(stream)).await {
                Ok(Ok(s)) => {
                    let _ = tx.send((s, peer)).await;
                }
                Ok(Err(e)) => tracing::debug!(%peer, error = %e, "TLS handshake failed"),
                Err(_) => tracing::debug!(%peer, "TLS handshake timed out"),
            }
        });
    }
}

impl axum::serve::Listener for TlsListener {
    type Io = TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.rx.recv().await {
            Some(c) => c,
            // The accept task only ends when the listener is dropped.
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        Ok(self.local)
    }
}

#[cfg(test)]
mod tests {
    use super::server_config;

    #[test]
    fn a_file_without_a_usable_identity_is_refused() {
        let ck = rcgen::generate_simple_self_signed(vec!["agg.example.test".into()]).unwrap();
        let good =
            serde_json::json!({"cert": ck.cert.pem(), "key": ck.signing_key.serialize_pem()});
        assert!(server_config(good.to_string().as_bytes()).is_ok());
        for bad in [
            serde_json::json!({"cert": ck.cert.pem()}),
            serde_json::json!({"cert": "", "key": ck.signing_key.serialize_pem()}),
            serde_json::json!({"cert": ck.cert.pem(), "key": "not a key"}),
        ] {
            assert!(server_config(bad.to_string().as_bytes()).is_err(), "{bad}");
        }
        // A key that is not the certificate's.
        let other = rcgen::generate_simple_self_signed(vec!["x.example.test".into()]).unwrap();
        let mixed =
            serde_json::json!({"cert": ck.cert.pem(), "key": other.signing_key.serialize_pem()});
        assert!(server_config(mixed.to_string().as_bytes()).is_err());
    }
}

//! What the GPU host's IPv4 front (src/fleet/agent/sni.rs) adds to a viewer's first frame:
//! TCP connect, TLS handshake, WebSocket upgrade and the first binary message, timed direct to
//! a place's TLS listener and through the front, the arms interleaved. With
//! `SNI_BENCH_EXTERNAL=<ip:port>` a third arm goes through another relay there (an nginx
//! `stream` server with `ssl_preread` mapping the bench's name to the place's port, which
//! `SNI_BENCH_PLACE_PORT` fixes).
//!
//!   make bench-sni-front [SNI_BENCH_N=2000] [SNI_BENCH_EXTERNAL=127.0.0.1:8443 SNI_BENCH_PLACE_PORT=28443]
//!
//! The place, the front and the client each run on their own runtime threads, as the place's
//! fcvm, the agent and a browser are separate processes. The place sends a 64 KiB binary
//! message as soon as the upgrade completes, the size of a tile's keyframe at the wall's
//! smallest rendition.

use fcvm::fleet::agent::sni::Front;
use fleet_core::protocol::{AgentMsg, SlotReport};
use futures_util::{SinkExt, StreamExt};
use rustls::pki_types::{PrivateKeyDer, ServerName};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;

const ZONE: &str = "tiles.bench.test";
const NAME: &str = "slot-1-abcdefgh-v4.tiles.bench.test";
const FRAME: usize = 64 * 1024;

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn runtime(threads: usize) -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(threads)
        .enable_all()
        .build()
        .unwrap()
}

/// The place: TLS, the WebSocket upgrade, one binary frame, then wait for the client to close.
async fn place(listener: TcpListener, config: Arc<rustls::ServerConfig>) {
    let acceptor = tokio_rustls::TlsAcceptor::from(config);
    loop {
        let (sock, _) = listener.accept().await.unwrap();
        sock.set_nodelay(true).unwrap();
        let acceptor = acceptor.clone();
        tokio::spawn(async move {
            let Ok(tls) = acceptor.accept(sock).await else {
                return;
            };
            let Ok(mut ws) = tokio_tungstenite::accept_async(tls).await else {
                return;
            };
            let _ = ws.send(Message::Binary(vec![7u8; FRAME].into())).await;
            while let Some(Ok(_)) = ws.next().await {}
        });
    }
}

/// One viewer: the time to the TLS session and to the first frame.
async fn viewer(addr: SocketAddr, client: Arc<rustls::ClientConfig>) -> (Duration, Duration) {
    let start = Instant::now();
    let tcp = TcpStream::connect(addr).await.unwrap();
    tcp.set_nodelay(true).unwrap();
    let tls = tokio_rustls::TlsConnector::from(client)
        .connect(ServerName::try_from(NAME).unwrap(), tcp)
        .await
        .unwrap();
    let handshake = start.elapsed();
    let (mut ws, _) = tokio_tungstenite::client_async(format!("wss://{NAME}/frames"), tls)
        .await
        .unwrap();
    loop {
        match ws.next().await.unwrap().unwrap() {
            Message::Binary(b) => {
                assert_eq!(b.len(), FRAME);
                break;
            }
            _ => continue,
        }
    }
    let frame = start.elapsed();
    let _ = ws.close(None).await;
    (handshake, frame)
}

fn quantile(sorted: &[f64], q: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * q).round() as usize]
}

fn main() {
    // `cargo bench` passes --bench; nothing else is taken.
    let n: usize = std::env::var("SNI_BENCH_N")
        .ok()
        .map(|v| v.parse().expect("SNI_BENCH_N is a count"))
        .unwrap_or(2000);
    let external: Option<SocketAddr> = std::env::var("SNI_BENCH_EXTERNAL")
        .ok()
        .map(|v| v.parse().expect("SNI_BENCH_EXTERNAL is <ip:port>"));
    let place_port: u16 = std::env::var("SNI_BENCH_PLACE_PORT")
        .ok()
        .map(|v| v.parse().expect("SNI_BENCH_PLACE_PORT is a port"))
        .unwrap_or(0);

    let ck = rcgen::generate_simple_self_signed(vec![format!("*.{ZONE}")]).unwrap();
    let server = Arc::new(
        rustls::ServerConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![ck.cert.der().clone()],
                PrivateKeyDer::Pkcs8(ck.signing_key.serialize_der().into()),
            )
            .unwrap(),
    );
    let mut roots = rustls::RootCertStore::empty();
    roots.add(ck.cert.der().clone()).unwrap();
    let client = Arc::new(
        rustls::ClientConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    );

    let place_rt = runtime(2);
    let place_addr = place_rt.block_on(async {
        let l = TcpListener::bind(("127.0.0.1", place_port)).await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(place(l, server));
        addr
    });
    let front_rt = runtime(2);
    let front = front_rt.block_on(async {
        let f = Front::bind("127.0.0.1:0".parse().unwrap(), 443, None)
            .await
            .unwrap();
        f.publish(AgentMsg::Slot(SlotReport {
            id: "slot-1".into(),
            ipv4: Some("192.0.2.1".into()),
            v4_port: Some(place_addr.port()),
            ..Default::default()
        }));
        f
    });

    let mut arms: Vec<(&str, SocketAddr)> =
        vec![("direct", place_addr), ("front", front.local_addr())];
    if let Some(e) = external {
        arms.push(("external", e));
    }
    let client_rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut times: Vec<(Vec<f64>, Vec<f64>)> = vec![(Vec::new(), Vec::new()); arms.len()];
    client_rt.block_on(async {
        // Warm-up: the first connections of each arm pay for page faults and lazy setup.
        for _ in 0..50 {
            for (_, addr) in &arms {
                viewer(*addr, client.clone()).await;
            }
        }
        for i in 0..n {
            // Rotate the order, so no arm always follows another.
            for k in 0..arms.len() {
                let a = (i + k) % arms.len();
                let (h, f) = viewer(arms[a].1, client.clone()).await;
                times[a].0.push(h.as_secs_f64() * 1e6);
                times[a].1.push(f.as_secs_f64() * 1e6);
            }
        }
    });
    println!("n={n} per arm, microseconds (p50 / p90 / p99 / mean)");
    let mut medians = Vec::new();
    for ((name, _), (h, f)) in arms.iter().zip(times.iter_mut()) {
        h.sort_by(f64::total_cmp);
        f.sort_by(f64::total_cmp);
        let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
        println!(
            "{name:>8}  tls {:7.0} / {:7.0} / {:7.0} / {:7.0}   first frame {:7.0} / {:7.0} / {:7.0} / {:7.0}",
            quantile(h, 0.5),
            quantile(h, 0.9),
            quantile(h, 0.99),
            mean(h),
            quantile(f, 0.5),
            quantile(f, 0.9),
            quantile(f, 0.99),
            mean(f),
        );
        medians.push((quantile(h, 0.5), quantile(f, 0.5)));
    }
    for (k, (name, _)) in arms.iter().enumerate().skip(1) {
        println!(
            "{name:>8}  added at p50: tls {:+.0} us, first frame {:+.0} us",
            medians[k].0 - medians[0].0,
            medians[k].1 - medians[0].1
        );
    }
}

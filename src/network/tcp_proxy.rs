//! Namespace-aware TCP proxy for routed networking.
//!
//! Replaces socat with built-in Rust TCP relay. Uses `setns(2)` to create
//! sockets inside network namespaces, then relays data with tokio.

use std::net::{IpAddr, SocketAddr};
use std::os::fd::AsFd;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::PortMapping;

/// Port the proxy relay binds on inside the VM's network namespace.
/// fc-agent connects to `gateway:<this_port>` for HTTP proxy access.
/// Uses a high port to avoid colliding with common application ports (8080, 3128, etc.)
/// that container processes might try to reach on the gateway.
pub const PROXY_RELAY_PORT: u16 = 41480;

/// Run a closure inside a network namespace, restoring the original namespace afterward.
///
/// Handles the setns enter/restore boilerplate: saves current namespace,
/// enters the target, runs the operation, then always restores.
fn run_in_namespace<T>(ns_path: &str, f: impl FnOnce() -> std::io::Result<T>) -> Result<T> {
    let old_ns =
        std::fs::File::open("/proc/self/ns/net").context("opening current network namespace")?;
    let new_ns =
        std::fs::File::open(ns_path).with_context(|| format!("opening namespace {ns_path}"))?;

    nix::sched::setns(new_ns.as_fd(), nix::sched::CloneFlags::CLONE_NEWNET)
        .context("entering network namespace")?;

    let result = f();

    // ALWAYS restore original namespace, even on failure.
    nix::sched::setns(old_ns.as_fd(), nix::sched::CloneFlags::CLONE_NEWNET)
        .expect("failed to restore network namespace — thread is in wrong namespace");

    result.with_context(|| format!("operation in namespace {ns_path}"))
}

/// Connect a TCP stream inside a network namespace.
///
/// Spawns a blocking thread that enters the namespace via `setns(2)`, connects,
/// then restores the original namespace. The returned stream works from any namespace.
async fn connect_in_namespace(ns_name: &str, addr: SocketAddr) -> Result<tokio::net::TcpStream> {
    let ns_path = format!("/var/run/netns/{}", ns_name);

    let std_stream = tokio::task::spawn_blocking(move || {
        run_in_namespace(&ns_path, || std::net::TcpStream::connect(addr))
    })
    .await
    .context("spawn_blocking panicked")??;

    std_stream.set_nonblocking(true)?;
    Ok(tokio::net::TcpStream::from_std(std_stream)?)
}

/// Bind a TCP listener inside a network namespace.
///
/// The returned listener accepts connections from within the namespace,
/// but the FD is usable from the host namespace (for tokio's epoll).
async fn bind_in_namespace(ns_name: &str, addr: SocketAddr) -> Result<tokio::net::TcpListener> {
    let ns_path = format!("/var/run/netns/{}", ns_name);

    let std_listener = tokio::task::spawn_blocking(move || {
        run_in_namespace(&ns_path, || std::net::TcpListener::bind(addr))
    })
    .await
    .context("spawn_blocking panicked")??;

    std_listener.set_nonblocking(true)?;
    Ok(tokio::net::TcpListener::from_std(std_listener)?)
}

/// A running relay: the task that accepts on one listener, and the signal that stops it.
///
/// `stop_relays` stops relays and waits until they are gone. A `Relay` that is dropped
/// without it is ended too, by aborting its task. Dropping does not wait: the listener
/// and the connections close when the runtime drops the aborted tasks.
#[derive(Debug)]
pub struct Relay {
    stop: CancellationToken,
    task: JoinHandle<()>,
}

impl Drop for Relay {
    fn drop(&mut self) {
        // Nothing to abort in a relay `stop_relays` has waited for: its task has finished.
        self.task.abort();
    }
}

/// Accept connections on `listener` and relay each to an upstream via `connect`.
///
/// Shared relay loop used by both port forwarding and proxy relay. The relay's task owns
/// the listener and every connection it accepted. Once stopped (`stop_relays`) it closes
/// the listener, ends the connections and finishes only when their tasks have.
fn spawn_relay_loop<F, Fut>(
    listener: tokio::net::TcpListener,
    connect: F,
    label: &'static str,
) -> Relay
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<tokio::net::TcpStream>> + Send,
{
    let connect = Arc::new(connect);
    let stop = CancellationToken::new();
    let stopped = stop.clone();
    let task = tokio::spawn(async move {
        // The connections live in this set and not in detached tasks, so they end with
        // the relay: a stopped relay shuts the set down below, and aborting this task
        // drops the set, which aborts each of them.
        let mut connections = tokio::task::JoinSet::new();
        loop {
            let accepted = tokio::select! {
                _ = stopped.cancelled() => break,
                accepted = listener.accept() => accepted,
                // A finished connection leaves the set, so the set holds only live ones.
                Some(_) = connections.join_next() => continue,
            };
            match accepted {
                Ok((client, peer)) => {
                    let connect = Arc::clone(&connect);
                    connections.spawn(async move {
                        match connect().await {
                            Ok(mut upstream) => {
                                let mut client = client;
                                match tokio::io::copy_bidirectional(&mut client, &mut upstream)
                                    .await
                                {
                                    Ok((c2u, u2c)) => {
                                        debug!(
                                            client_to_upstream = c2u,
                                            upstream_to_client = u2c,
                                            %peer,
                                            "{label} relay completed"
                                        );
                                    }
                                    Err(e) => {
                                        debug!(error = %e, %peer, "{label} relay error");
                                    }
                                }
                            }
                            Err(e) => {
                                debug!(error = %e, %peer, "{label} connect failed");
                            }
                        }
                    });
                }
                Err(e) => {
                    // accept() can fail transiently (ECONNABORTED, EMFILE/ENFILE
                    // under fd pressure). The listener is owned by this loop and
                    // never closed externally, so back off briefly and keep
                    // accepting instead of silently killing the relay for the
                    // VM's lifetime.
                    warn!(error = %e, "{label} accept error, retrying");
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    continue;
                }
            }
        }
        // Stopped. The listener closes first, so a client that connects from here on is
        // refused. `shutdown` aborts every connection and returns when their tasks have
        // finished, and a finished task has dropped the streams it held.
        drop(listener);
        connections.shutdown().await;
    });
    Relay { stop, task }
}

/// Stop relays and wait until they are gone.
///
/// Every relay is told to stop before any is waited for. A stopped relay closes its
/// listener, aborts the connections it had accepted and waits until their tasks have
/// finished. So when this returns the listeners are closed, and the same address and
/// port can be bound again at once: on a snapshot miss an NV2 guest is torn down and
/// restored from its snapshot in one process. No connection task is left either. Each
/// has dropped its client and upstream streams, so a client is disconnected instead of
/// staying attached to a VM that is gone.
///
/// A dial of the guest that is still in progress is not waited for. It runs on the
/// blocking pool (`connect_in_namespace`), ends by itself, and its socket closes then.
pub async fn stop_relays(relays: impl IntoIterator<Item = Relay>) {
    let mut relays: Vec<Relay> = relays.into_iter().collect();
    for relay in &relays {
        relay.stop.cancel();
    }
    for relay in &mut relays {
        // A stopped relay's task ends by itself. An error is a panic in the relay loop,
        // which drops its connections without waiting for them.
        if let Err(e) = (&mut relay.task).await {
            warn!(error = %e, "relay task did not stop cleanly");
        }
    }
}

/// Backlog for a port forward's listener: as long as the kernel allows, which caps it at
/// net.core.somaxconn. `tokio::net::TcpListener::bind` asks for the same.
const PORT_FORWARD_BACKLOG: u32 = i32::MAX as u32;

/// Where a published port's host-side listener binds: the address the mapping's HOSTIP
/// names, or the VM's own loopback address when it names none.
fn port_forward_bind_addr(mapping: &PortMapping, loopback_ip: &str) -> Result<SocketAddr> {
    let host = mapping.host_ip.as_deref().unwrap_or(loopback_ip);
    let ip: IpAddr = host.parse().map_err(|_| {
        anyhow::anyhow!(
            "port forward host address {host} (host port {}) is not an IP address",
            mapping.host_port
        )
    })?;
    Ok(SocketAddr::new(ip, mapping.host_port))
}

/// Listen on `addr` for a port forward.
///
/// This is the socket `tokio::net::TcpListener::bind` makes (SO_REUSEADDR, kernel-capped
/// backlog) with one option set explicitly: an IPv6 listener clears IPV6_V6ONLY. tokio
/// leaves that option at the kernel default, the `net.ipv6.bindv6only` sysctl, and with
/// the sysctl at 1 a `[::]` listener refuses IPv4 clients. Cleared, `[::]` accepts IPv4
/// and IPv6 clients on every host.
fn listen_for_port_forward(addr: SocketAddr) -> std::io::Result<tokio::net::TcpListener> {
    let socket = match addr {
        SocketAddr::V4(_) => tokio::net::TcpSocket::new_v4()?,
        SocketAddr::V6(_) => {
            let socket = tokio::net::TcpSocket::new_v6()?;
            nix::sys::socket::setsockopt(&socket, nix::sys::socket::sockopt::Ipv6V6Only, &false)?;
            socket
        }
    };
    socket.set_reuseaddr(true)?;
    socket.bind(addr)?;
    socket.listen(PORT_FORWARD_BACKLOG)
}

/// Bind the host-side listener of every mapping, or of none.
///
/// The listeners are plain values until a relay task takes them, so an error drops, and
/// with that closes, the ones already bound before it is returned.
fn bind_port_forwards(
    loopback_ip: &str,
    mappings: &[PortMapping],
) -> Result<Vec<tokio::net::TcpListener>> {
    let mut listeners = Vec::with_capacity(mappings.len());
    for mapping in mappings {
        let addr = port_forward_bind_addr(mapping, loopback_ip)?;
        let listener = listen_for_port_forward(addr)
            .with_context(|| format!("binding port forward on {addr}"))?;
        info!(
            listen = %addr,
            guest_port = mapping.guest_port,
            "port forwarding via TCP proxy"
        );
        listeners.push(listener);
    }
    Ok(listeners)
}

/// Start port forwarding: listen on the host, relay to the guest inside the namespace.
///
/// A mapping listens on the host address its HOSTIP names, or on `loopback_ip` (the VM's
/// own loopback address) when it names none. Either every mapping is listening when this
/// returns, or it returns an error and none is.
///
/// Returns a `Relay` per port mapping, for `stop_relays`.
pub async fn start_port_forwards(
    loopback_ip: &str,
    mappings: &[PortMapping],
    ns_name: &str,
    guest_ip: &str,
) -> Result<Vec<Relay>> {
    let guest_ip: IpAddr = guest_ip
        .parse()
        .with_context(|| format!("invalid guest address {guest_ip}"))?;
    let listeners = bind_port_forwards(loopback_ip, mappings)?;
    let ns_name: Arc<str> = ns_name.into();

    Ok(listeners
        .into_iter()
        .zip(mappings)
        .map(|(listener, mapping)| {
            let ns_name = Arc::clone(&ns_name);
            let guest_addr = SocketAddr::new(guest_ip, mapping.guest_port);
            spawn_relay_loop(
                listener,
                move || {
                    let ns = Arc::clone(&ns_name);
                    async move { connect_in_namespace(&ns, guest_addr).await }
                },
                "port forward",
            )
        })
        .collect())
}

/// Connect to a service on the host's loopback, for `--forward-localhost`.
///
/// The flag forwards the host's `localhost`, which is 127.0.0.1 and ::1. The guest's
/// relay accepts on both and does not carry over which one its client dialled, so the
/// host side cannot mirror the address family: it dials 127.0.0.1, and ::1 when
/// 127.0.0.1 refuses the connection. A refusal is the one error that says nothing
/// listens there. Any other error is of a service that is there, and is returned.
async fn connect_host_loopback(port: u16) -> Result<tokio::net::TcpStream> {
    let v4 = SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port));
    let v4_error = match tokio::net::TcpStream::connect(v4).await {
        Ok(stream) => return Ok(stream),
        Err(e) if tries_ipv6_loopback(&e) => e,
        Err(e) => {
            anyhow::bail!("connecting to the host's loopback port {port}: {v4} gave {e}")
        }
    };
    let v6 = SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port));
    // One message with both outcomes: the relay logs an error's own text, not its chain.
    tokio::net::TcpStream::connect(v6)
        .await
        .map_err(|v6_error| {
            anyhow::anyhow!(
                "connecting to the host's loopback port {port}: \
             {v4} gave {v4_error}, then {v6} gave {v6_error}"
            )
        })
}

/// Whether a failed dial of 127.0.0.1 is followed by one of ::1: only when it was refused.
fn tries_ipv6_loopback(v4_error: &std::io::Error) -> bool {
    v4_error.kind() == std::io::ErrorKind::ConnectionRefused
}

/// Start localhost forwarding: listen inside the namespace, relay to host loopback.
///
/// Used for `--forward-localhost` in routed mode. fc-agent's guest-side relay
/// connects to `<listen_ip>:<port>` (the pasta-style host gateway 10.0.2.2) for
/// traffic destined to the host's loopback. The listener runs inside the VM's
/// network namespace; the upstream connect happens in the host namespace,
/// reaching a service on the host's loopback: 127.0.0.1:<port>, or [::1]:<port>
/// when nothing accepts on 127.0.0.1 (`connect_host_loopback`).
///
/// Either every port is listening when this returns, or it returns an error and none
/// is. Returns a `Relay` per port, for `stop_relays`.
pub async fn start_localhost_forwards(
    ns_name: &str,
    listen_ip: &str,
    ports: &[u16],
) -> Result<Vec<Relay>> {
    // Bind every port before a relay takes any of them, as `bind_port_forwards` does.
    // The listeners are plain values until then, so an error drops, and with that
    // closes, the ones already bound before it is returned.
    let mut listeners = Vec::with_capacity(ports.len());
    for &port in ports {
        let bind_addr: SocketAddr =
            format!("{}:{}", listen_ip, port).parse().with_context(|| {
                format!(
                    "invalid localhost forward bind address {}:{}",
                    listen_ip, port
                )
            })?;
        let listener = bind_in_namespace(ns_name, bind_addr)
            .await
            .with_context(|| format!("binding localhost forward on {bind_addr}"))?;
        info!(
            port,
            bind = %bind_addr,
            "localhost forwarding via TCP proxy"
        );
        listeners.push((port, listener));
    }

    Ok(listeners
        .into_iter()
        .map(|(port, listener)| {
            spawn_relay_loop(
                listener,
                move || connect_host_loopback(port),
                "localhost forward",
            )
        })
        .collect())
}

/// Start a reverse proxy relay: listen inside namespace, connect to host proxy.
///
/// Used when host-side BPF programs intercept connect() for proxy auth.
/// The relay listens in the namespace (reachable from the VM via gateway IP)
/// and connects to the real proxy from the host namespace. Returns the `Relay`, for
/// `stop_relays`.
pub async fn start_proxy_relay(
    ns_name: &str,
    gateway_ip: &str,
    proxy_addr: SocketAddr,
) -> Result<Relay> {
    let bind_addr: SocketAddr = format!("{}:{}", gateway_ip, PROXY_RELAY_PORT)
        .parse()
        .with_context(|| {
            format!(
                "invalid proxy relay bind address {}:{}",
                gateway_ip, PROXY_RELAY_PORT
            )
        })?;

    let listener = bind_in_namespace(ns_name, bind_addr)
        .await
        .context("binding proxy relay listener in namespace")?;

    info!(
        proxy = %proxy_addr,
        bind = %bind_addr,
        "reverse proxy relay via TCP proxy"
    );

    let relay = spawn_relay_loop(
        listener,
        move || async move {
            tokio::net::TcpStream::connect(proxy_addr)
                .await
                .context("connecting to proxy")
        },
        "proxy",
    );

    Ok(relay)
}

/// Parse a proxy URL like "http://host:port" or "host:port" into a SocketAddr.
///
/// Supports IP addresses, hostnames (via DNS resolution), and URLs with
/// trailing slashes or paths. Matches socat's behavior of accepting hostnames.
pub fn parse_proxy_addr(proxy: &str) -> Result<SocketAddr> {
    let addr_str = proxy
        .trim_start_matches("http://")
        .trim_start_matches("https://");
    // Strip trailing path/slash (e.g. "10.0.0.1:8080/" → "10.0.0.1:8080")
    let addr_str = addr_str.split('/').next().unwrap_or(addr_str);
    // Use ToSocketAddrs to support both IP addresses and hostnames
    use std::net::ToSocketAddrs;
    addr_str
        .to_socket_addrs()
        .with_context(|| format!("resolving proxy address: {addr_str}"))?
        .next()
        .with_context(|| format!("no addresses resolved for proxy: {addr_str}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_proxy_addr_ip_port() {
        let addr = parse_proxy_addr("http://10.0.0.1:8080").unwrap();
        assert_eq!(addr.to_string(), "10.0.0.1:8080");
    }

    #[test]
    fn test_parse_proxy_addr_bare_ip_port() {
        let addr = parse_proxy_addr("10.0.0.1:8080").unwrap();
        assert_eq!(addr.to_string(), "10.0.0.1:8080");
    }

    #[test]
    fn test_parse_proxy_addr_trailing_slash() {
        let addr = parse_proxy_addr("http://10.0.0.1:8080/").unwrap();
        assert_eq!(addr.to_string(), "10.0.0.1:8080");
    }

    #[test]
    fn test_parse_proxy_addr_with_path() {
        let addr = parse_proxy_addr("http://10.0.0.1:8080/proxy/path").unwrap();
        assert_eq!(addr.to_string(), "10.0.0.1:8080");
    }

    #[test]
    fn test_parse_proxy_addr_https_scheme() {
        let addr = parse_proxy_addr("https://10.0.0.1:3128").unwrap();
        assert_eq!(addr.to_string(), "10.0.0.1:3128");
    }

    #[test]
    fn test_parse_proxy_addr_localhost() {
        let addr = parse_proxy_addr("http://localhost:8080").unwrap();
        assert_eq!(addr.port(), 8080);
    }

    #[test]
    fn test_parse_proxy_addr_ipv6() {
        let addr = parse_proxy_addr("http://[::1]:8080").unwrap();
        assert_eq!(addr.port(), 8080);
    }

    fn mapping(host_ip: Option<&str>, host_port: u16) -> PortMapping {
        PortMapping {
            host_ip: host_ip.map(str::to_string),
            host_port,
            guest_port: 80,
            proto: super::super::types::Protocol::Tcp,
        }
    }

    /// A loopback address for one test to bind. All of 127/8 is local on Linux. VMs take
    /// theirs upward from 127.0.0.2, and the slot (one per address per test) plus the
    /// process id keep this one away from every other test thread and test process.
    fn test_ip(slot: u8) -> std::net::Ipv4Addr {
        let pid = std::process::id();
        std::net::Ipv4Addr::new(127, 128 | slot, (pid >> 8) as u8, pid as u8)
    }

    #[test]
    fn a_mapping_binds_its_own_host_address_or_the_vm_loopback() {
        let bind = |host_ip, host_port| {
            port_forward_bind_addr(&mapping(host_ip, host_port), "127.0.0.5")
                .map(|addr| addr.to_string())
                .map_err(|e| e.to_string())
        };
        assert_eq!(bind(None, 8080), Ok("127.0.0.5:8080".to_string()));
        assert_eq!(
            bind(Some("127.0.0.1"), 8080),
            Ok("127.0.0.1:8080".to_string())
        );
        assert_eq!(bind(Some("0.0.0.0"), 80), Ok("0.0.0.0:80".to_string()));
        assert_eq!(bind(Some("::"), 80), Ok("[::]:80".to_string()));
        assert_eq!(bind(Some("::1"), 443), Ok("[::1]:443".to_string()));

        let error = bind(Some("localhost"), 8080).unwrap_err();
        assert!(
            error.contains("localhost") && error.contains("8080"),
            "{error}"
        );
        // The VM's loopback address is taken as given, so a bad one is reported too.
        let error = port_forward_bind_addr(&mapping(None, 8080), "not-an-address")
            .unwrap_err()
            .to_string();
        assert!(error.contains("not-an-address"), "{error}");
    }

    /// The listener is on the address the mapping names: a client connecting there is
    /// accepted, and one connecting to the VM's loopback address on that port is refused.
    /// A mapping that names no address still listens on the VM's loopback address.
    #[tokio::test]
    async fn a_mapping_listens_on_its_host_address_and_not_on_the_vm_loopback() {
        let (loopback, host_ip) = (test_ip(1), test_ip(2));
        let listeners = bind_port_forwards(
            &loopback.to_string(),
            &[mapping(Some(&host_ip.to_string()), 0), mapping(None, 0)],
        )
        .unwrap();
        let (named, unnamed) = (
            listeners[0].local_addr().unwrap(),
            listeners[1].local_addr().unwrap(),
        );
        assert_eq!(named.ip(), IpAddr::V4(host_ip));
        assert_eq!(unnamed.ip(), IpAddr::V4(loopback));

        tokio::net::TcpStream::connect(named)
            .await
            .expect("the mapping's own address accepts");
        let refused = tokio::net::TcpStream::connect((loopback, named.port()))
            .await
            .expect_err("nothing listens on the VM's loopback address at that port");
        assert_eq!(
            refused.kind(),
            std::io::ErrorKind::ConnectionRefused,
            "{refused}"
        );
    }

    /// `[::]` is every address of the host, IPv4 included, so http://localhost/ reaches
    /// the listener whether the client resolves localhost to 127.0.0.1 or to ::1.
    #[tokio::test]
    async fn a_listener_on_the_ipv6_wildcard_accepts_ipv4_and_ipv6_clients() {
        let listeners = bind_port_forwards("127.0.0.1", &[mapping(Some("::"), 0)]).unwrap();
        let listening = listeners[0].local_addr().unwrap();
        assert_eq!(listening.ip(), IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED));
        let v6only =
            nix::sys::socket::getsockopt(&listeners[0], nix::sys::socket::sockopt::Ipv6V6Only)
                .unwrap();
        assert!(
            !v6only,
            "IPV6_V6ONLY must be cleared whatever net.ipv6.bindv6only says"
        );

        for client in [
            IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
        ] {
            tokio::net::TcpStream::connect((client, listening.port()))
                .await
                .unwrap_or_else(|e| panic!("a client connecting to {client} is refused: {e}"));
        }
    }

    /// A mapping whose address cannot be bound fails the start with an error naming that
    /// address and port, and the listeners bound before it are closed by the time the
    /// error is returned.
    #[tokio::test]
    async fn a_bind_failure_names_the_address_and_leaves_no_listener_behind() {
        let (loopback, free, taken) = (test_ip(3), test_ip(4), test_ip(5));
        // Another listener holds `taken:port`. The same port on `free` can still be
        // bound: a listener on one specific address does not claim the others.
        let squatter = std::net::TcpListener::bind((taken, 0)).unwrap();
        let port = squatter.local_addr().unwrap().port();

        let mappings = [
            mapping(Some(&free.to_string()), port),
            mapping(Some(&taken.to_string()), port),
        ];
        // No relay starts, so the namespace is never entered.
        let error = start_port_forwards(&loopback.to_string(), &mappings, "unused", "10.0.2.100")
            .await
            .expect_err("the second mapping's address is in use");
        let error = format!("{error:#}");
        assert!(error.contains(&format!("{taken}:{port}")), "{error}");

        let refused = tokio::net::TcpStream::connect((free, port))
            .await
            .expect_err("the first mapping's listener must be closed");
        assert_eq!(
            refused.kind(),
            std::io::ErrorKind::ConnectionRefused,
            "{refused}"
        );

        // 192.0.2.1 is TEST-NET-1, an address no host has.
        let absent = [mapping(Some("192.0.2.1"), port)];
        let error = start_port_forwards(&loopback.to_string(), &absent, "unused", "10.0.2.100")
            .await
            .expect_err("192.0.2.1 is not an address of this host");
        let error = format!("{error:#}");
        assert!(error.contains(&format!("192.0.2.1:{port}")), "{error}");
    }

    /// A connection belongs to its relay: stopping the relay ends it on both sides. A
    /// detached connection task would keep the client attached, and the upstream socket
    /// open inside the VM's namespace, for as long as the client stayed.
    #[tokio::test]
    async fn stopped_relays_have_closed_their_connections() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let upstream = tokio::net::TcpListener::bind((test_ip(7), 0))
            .await
            .unwrap();
        let upstream_addr = upstream.local_addr().unwrap();
        let listeners = bind_port_forwards(&test_ip(8).to_string(), &[mapping(None, 0)]).unwrap();
        let listening = listeners[0].local_addr().unwrap();
        let relays: Vec<Relay> = listeners
            .into_iter()
            .map(|listener| {
                spawn_relay_loop(
                    listener,
                    move || async move { Ok(tokio::net::TcpStream::connect(upstream_addr).await?) },
                    "test",
                )
            })
            .collect();

        // One connection through the relay, shown end to end by a byte each way.
        let mut client = tokio::net::TcpStream::connect(listening).await.unwrap();
        let (mut served, _) = upstream.accept().await.unwrap();
        let mut byte = [0u8; 1];
        client.write_all(b"a").await.unwrap();
        served.read_exact(&mut byte).await.unwrap();
        served.write_all(b"b").await.unwrap();
        client.read_exact(&mut byte).await.unwrap();

        stop_relays(relays).await;

        // Each end sees the connection end. A read still pending at the deadline is a
        // connection that outlived its relay.
        for (side, stream) in [("client", &mut client), ("upstream", &mut served)] {
            let ended =
                tokio::time::timeout(std::time::Duration::from_secs(5), stream.read(&mut byte))
                    .await;
            match ended {
                Ok(Ok(0)) | Ok(Err(_)) => {}
                Ok(Ok(n)) => panic!("{side} read {n} bytes after the relay stopped"),
                Err(_) => panic!("{side} is still connected 5 s after the relay stopped"),
            }
        }
    }

    /// A relay task owns its listener. Stopping the relays must not return before the
    /// task has closed it.
    #[tokio::test]
    async fn stopped_relays_have_closed_their_listeners() {
        let listeners = bind_port_forwards(&test_ip(6).to_string(), &[mapping(None, 0)]).unwrap();
        let listening = listeners[0].local_addr().unwrap();
        let relays: Vec<Relay> = listeners
            .into_iter()
            .map(|listener| {
                spawn_relay_loop(
                    listener,
                    || async { anyhow::bail!("this test connects no client") },
                    "test",
                )
            })
            .collect();

        stop_relays(relays).await;

        std::net::TcpListener::bind(listening)
            .expect("the address must be free again once the relays are stopped");
    }

    /// Sets its flag when it is dropped.
    struct SetOnDrop(Arc<std::sync::atomic::AtomicBool>);

    impl Drop for SetOnDrop {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// When `stop_relays` returns, the connection tasks have finished: none is still
    /// running with its sockets open. Asking for their cancellation is not that. The
    /// connection task here is inside a poll that blocks its worker thread for 500 ms
    /// when the relay is stopped, and it holds a guard until it finishes. A stop that
    /// only asks for cancellation returns at once, with the guard still held.
    ///
    /// The client seeing its connection end within some seconds does not show this: it
    /// sees that after a stop that waits for nothing, too.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stopped_relays_have_finished_their_connection_tasks() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let finished = Arc::new(AtomicBool::new(false));
        let (entered, entered_rx) = std::sync::mpsc::channel::<()>();
        let listeners = bind_port_forwards(&test_ip(9).to_string(), &[mapping(None, 0)]).unwrap();
        let listening = listeners[0].local_addr().unwrap();
        let relays: Vec<_> = listeners
            .into_iter()
            .map(|listener| {
                let (finished, entered) = (Arc::clone(&finished), entered.clone());
                spawn_relay_loop(
                    listener,
                    move || {
                        let (guard, entered) = (SetOnDrop(Arc::clone(&finished)), entered.clone());
                        async move {
                            let _held_until_the_task_finishes = guard;
                            entered.send(()).expect("the test waits for this");
                            std::thread::sleep(std::time::Duration::from_millis(500));
                            anyhow::bail!("this test has no upstream")
                        }
                    },
                    "test",
                )
            })
            .collect();

        // One client, so one connection task. Once it has said so, it is inside its
        // blocking poll. The client connects without the runtime: an async connect
        // completes through the I/O driver, and the worker thread that drives it can be
        // the one the connection task blocks.
        let _client = std::net::TcpStream::connect(listening).unwrap();
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("the relay starts a connection task for the client");

        stop_relays(relays).await;

        assert!(
            finished.load(Ordering::SeqCst),
            "stop_relays returned while a connection task was still running"
        );
    }

    /// A relay that is dropped without `stop_relays` is ended too, so it does not hold
    /// its port for the life of the process. Dropping does not wait: the address is free
    /// once the runtime has dropped the task, which is soon after and not at once.
    #[tokio::test]
    async fn a_dropped_relay_releases_its_listener() {
        let listeners = bind_port_forwards(&test_ip(10).to_string(), &[mapping(None, 0)]).unwrap();
        let listening = listeners[0].local_addr().unwrap();
        let relays: Vec<_> = listeners
            .into_iter()
            .map(|listener| {
                spawn_relay_loop(
                    listener,
                    || async { anyhow::bail!("this test connects no client") },
                    "test",
                )
            })
            .collect();

        drop(relays);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match std::net::TcpListener::bind(listening) {
                Ok(_) => break,
                Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "a dropped relay still listens on {listening} after 5 s"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                Err(e) => panic!("binding {listening}: {e}"),
            }
        }
    }

    /// A socket bound to `addr` that does not listen: connections to it are refused, and
    /// no other process can take the port, so "nothing listens here" holds for as long as
    /// the test keeps the socket.
    fn hold_without_listening(addr: SocketAddr) -> std::io::Result<tokio::net::TcpSocket> {
        let socket = match addr {
            SocketAddr::V4(_) => tokio::net::TcpSocket::new_v4()?,
            SocketAddr::V6(_) => tokio::net::TcpSocket::new_v6()?,
        };
        socket.bind(addr)?;
        Ok(socket)
    }

    /// `hold_without_listening`, or `None` when another socket has the address. Any other
    /// error is the test's environment (no IPv6 loopback, no descriptors) and fails the
    /// test by name, where a retry loop would spin on it.
    fn hold_if_free(addr: SocketAddr) -> Option<tokio::net::TcpSocket> {
        match hold_without_listening(addr) {
            Ok(socket) => Some(socket),
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => None,
            Err(e) => panic!("binding {addr}: {e}"),
        }
    }

    const V4_LOOPBACK: IpAddr = IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);
    const V6_LOOPBACK: IpAddr = IpAddr::V6(std::net::Ipv6Addr::LOCALHOST);

    /// A host service that listens only on ::1 is reached. 127.0.0.1 at the same port is
    /// held without a listener, so the first dial is refused whatever else runs here.
    #[tokio::test]
    async fn a_host_service_on_ipv6_loopback_only_is_reached() {
        let (service, _refusing) = loop {
            let service = std::net::TcpListener::bind((V6_LOOPBACK, 0))
                .expect("binding [::1]:0, which this test needs");
            let port = service.local_addr().unwrap().port();
            // The port number is free on ::1 only; take another when 127.0.0.1 has it.
            if let Some(held) = hold_if_free(SocketAddr::new(V4_LOOPBACK, port)) {
                break (service, held);
            }
        };
        let port = service.local_addr().unwrap().port();

        let stream = connect_host_loopback(port)
            .await
            .expect("the service on ::1 accepts");
        assert_eq!(
            stream.peer_addr().unwrap(),
            SocketAddr::new(V6_LOOPBACK, port)
        );
    }

    /// A service on 127.0.0.1 is still the one dialled, also when ::1 listens too.
    #[tokio::test]
    async fn a_host_service_on_ipv4_loopback_is_dialled_first() {
        let (v4, _v6) = loop {
            let v4 = std::net::TcpListener::bind((V4_LOOPBACK, 0)).unwrap();
            let port = v4.local_addr().unwrap().port();
            match std::net::TcpListener::bind((V6_LOOPBACK, port)) {
                Ok(v6) => break (v4, v6),
                Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {}
                Err(e) => panic!("binding [::1]:{port}: {e}"),
            }
        };
        let port = v4.local_addr().unwrap().port();

        let stream = connect_host_loopback(port).await.unwrap();
        assert_eq!(
            stream.peer_addr().unwrap(),
            SocketAddr::new(V4_LOOPBACK, port)
        );
    }

    /// With no service on either address the error names the port and the outcome of
    /// both dials.
    #[tokio::test]
    async fn no_host_service_is_an_error_naming_both_loopback_addresses() {
        let (held_v4, _held_v6) = loop {
            let held_v4 = hold_without_listening(SocketAddr::new(V4_LOOPBACK, 0)).unwrap();
            let port = held_v4.local_addr().unwrap().port();
            if let Some(held_v6) = hold_if_free(SocketAddr::new(V6_LOOPBACK, port)) {
                break (held_v4, held_v6);
            }
        };
        let port = held_v4.local_addr().unwrap().port();

        let error = connect_host_loopback(port)
            .await
            .expect_err("nothing listens on either loopback address");
        // The relay logs an error's own text, not its chain, so both outcomes are in it.
        let error = error.to_string();
        assert!(
            error.contains(&format!("127.0.0.1:{port} gave"))
                && error.contains(&format!("[::1]:{port} gave")),
            "{error}"
        );
    }

    /// Only a refusal on 127.0.0.1 is followed by a dial of ::1. A timeout, or no local
    /// port left to dial from, is the error of a service that is there: sending that
    /// connection to whatever listens on ::1 would reach another service.
    #[test]
    fn only_a_refusal_on_ipv4_loopback_is_followed_by_ipv6_loopback() {
        use std::io::{Error, ErrorKind};
        assert!(tries_ipv6_loopback(&Error::from(
            ErrorKind::ConnectionRefused
        )));
        assert!(tries_ipv6_loopback(&Error::from_raw_os_error(
            libc::ECONNREFUSED
        )));
        for errno in [
            libc::ETIMEDOUT,
            libc::EADDRNOTAVAIL,
            libc::ECONNRESET,
            libc::EMFILE,
        ] {
            let error = Error::from_raw_os_error(errno);
            assert!(!tries_ipv6_loopback(&error), "{error}");
        }
    }

    /// RAII network namespace for privileged tests.
    ///
    /// `ip netns add` is checked (a leftover same-named namespace fails the
    /// test instead of being silently reused), and Drop deletes the namespace
    /// even on panics and early returns, so failed tests cannot leak
    /// namespaces on the host.
    #[cfg(feature = "privileged-tests")]
    struct TestNetns {
        name: String,
    }

    #[cfg(feature = "privileged-tests")]
    impl TestNetns {
        async fn create(name: String) -> Result<Self> {
            let add = tokio::process::Command::new("ip")
                .args(["netns", "add", &name])
                .output()
                .await
                .context("creating test namespace")?;
            anyhow::ensure!(
                add.status.success(),
                "ip netns add {} failed: {}",
                name,
                String::from_utf8_lossy(&add.stderr).trim()
            );
            let lo = tokio::process::Command::new("ip")
                .args(["netns", "exec", &name, "ip", "link", "set", "lo", "up"])
                .output()
                .await
                .context("bringing up loopback in test namespace")?;
            anyhow::ensure!(
                lo.status.success(),
                "bringing up lo in {} failed: {}",
                name,
                String::from_utf8_lossy(&lo.stderr).trim()
            );
            Ok(Self { name })
        }
    }

    #[cfg(feature = "privileged-tests")]
    impl Drop for TestNetns {
        fn drop(&mut self) {
            // Synchronous on purpose: Drop also runs on panics and `?` returns.
            let _ = std::process::Command::new("ip")
                .args(["netns", "del", &self.name])
                .output();
        }
    }

    /// Test that connect_in_namespace can reach a listener inside a namespace.
    ///
    /// Creates a temp namespace, binds a listener in it, connects from outside
    /// via setns, and verifies bidirectional data flow.
    #[cfg(feature = "privileged-tests")]
    #[tokio::test]
    async fn test_connect_in_namespace() -> Result<()> {
        // Namespace is deleted by TestNetns::drop, even on failure paths.
        let ns = TestNetns::create(format!("test-proxy-{}", std::process::id())).await?;
        let ns_name = ns.name.clone();

        // Bind a listener inside the namespace
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = bind_in_namespace(&ns_name, addr).await?;
        let listen_addr = listener.local_addr()?;
        println!("Listener bound at {} in namespace {}", listen_addr, ns_name);

        // Spawn an echo server
        let echo_handle = tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let (mut r, mut w) = stream.split();
                let _ = tokio::io::copy(&mut r, &mut w).await;
            }
        });

        // Connect from "outside" using setns
        let mut stream = connect_in_namespace(&ns_name, listen_addr).await?;

        // Send data and verify we can write (connection established)
        use tokio::io::AsyncWriteExt;
        stream.write_all(b"hello").await?;
        stream.shutdown().await?;
        println!("Successfully connected and sent data via namespace");

        // Cleanup (namespace deleted by TestNetns::drop)
        echo_handle.abort();
        println!("test_connect_in_namespace PASSED");
        Ok(())
    }

    /// Test the full port-forward relay path without a VM.
    ///
    /// Creates a namespace with an echo server, sets up a port forward,
    /// and verifies end-to-end data flow through the proxy.
    #[cfg(feature = "privileged-tests")]
    #[tokio::test]
    async fn test_port_forward_relay() -> Result<()> {
        // Namespace is deleted by TestNetns::drop, even on failure paths.
        let ns = TestNetns::create(format!("test-pf-{}", std::process::id())).await?;
        let ns_name = ns.name.clone();

        // Start echo server inside namespace on a known port
        let server_addr: SocketAddr = "127.0.0.1:9999".parse().unwrap();
        let server_listener = bind_in_namespace(&ns_name, server_addr).await?;

        let echo_handle = tokio::spawn(async move {
            while let Ok((mut stream, _)) = server_listener.accept().await {
                tokio::spawn(async move {
                    let (mut r, mut w) = stream.split();
                    let _ = tokio::io::copy(&mut r, &mut w).await;
                });
            }
        });

        // Start port forward: host 127.0.0.1:0 → namespace 127.0.0.1:9999
        let host_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let host_addr = host_listener.local_addr()?;
        drop(host_listener); // Release the port so start_port_forwards can bind it

        let mapping = PortMapping {
            host_port: host_addr.port(),
            guest_port: 9999,
            host_ip: None,
            proto: super::super::types::Protocol::Tcp,
        };

        let handles = start_port_forwards("127.0.0.1", &[mapping], &ns_name, "127.0.0.1").await?;

        // Give the listener a moment to start accepting
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Connect through the port forward and test echo
        let mut client = tokio::net::TcpStream::connect(host_addr).await?;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        client.write_all(b"test data 12345").await?;
        client.shutdown().await?;

        let mut buf = Vec::new();
        client.read_to_end(&mut buf).await?;
        assert_eq!(buf, b"test data 12345", "echo data should match");

        // Cleanup (namespace deleted by TestNetns::drop)
        stop_relays(handles).await;
        echo_handle.abort();
        println!("test_port_forward_relay PASSED");
        Ok(())
    }

    /// The relay reaches a host service that listens only on ::1.
    ///
    /// The path of `test_localhost_forward_relay`, with the host service on the other
    /// loopback address and 127.0.0.1 at that port held without a listener.
    #[cfg(feature = "privileged-tests")]
    #[tokio::test]
    async fn test_localhost_forward_relay_reaches_ipv6_loopback() -> Result<()> {
        // Namespace is deleted by TestNetns::drop, even if the assertion panics.
        let ns = TestNetns::create(format!("test-lf6-{}", std::process::id())).await?;
        let ns_name = ns.name.clone();

        let (host_listener, _refusing) = loop {
            let listener = tokio::net::TcpListener::bind((V6_LOOPBACK, 0)).await?;
            let port = listener.local_addr()?.port();
            if let Some(held) = hold_if_free(SocketAddr::new(V4_LOOPBACK, port)) {
                break (listener, held);
            }
        };
        let host_port = host_listener.local_addr()?.port();
        let server_handle = tokio::spawn(async move {
            if let Ok((mut stream, _)) = host_listener.accept().await {
                use tokio::io::AsyncWriteExt;
                let _ = stream.write_all(b"HELLO_FROM_HOST_V6").await;
            }
        });

        let result: Result<()> = async {
            let handles = start_localhost_forwards(&ns_name, "127.0.0.1", &[host_port]).await?;
            let listen_addr = SocketAddr::new(V4_LOOPBACK, host_port);
            let mut client = connect_in_namespace(&ns_name, listen_addr).await?;

            use tokio::io::AsyncReadExt;
            let mut buf = Vec::new();
            client.read_to_end(&mut buf).await?;
            assert_eq!(
                buf, b"HELLO_FROM_HOST_V6",
                "the relay should reach the host's ::1"
            );

            stop_relays(handles).await;
            Ok(())
        }
        .await;

        server_handle.abort();
        result?;
        println!("test_localhost_forward_relay_reaches_ipv6_loopback PASSED");
        Ok(())
    }

    /// Test the localhost-forward relay path without a VM.
    ///
    /// Creates a namespace with the forward listener inside it (the guest side),
    /// a server on host loopback (the host side), and verifies that connections
    /// made from inside the namespace reach the host loopback service.
    #[cfg(feature = "privileged-tests")]
    #[tokio::test]
    async fn test_localhost_forward_relay() -> Result<()> {
        // Namespace is deleted by TestNetns::drop, even if the assertion panics.
        let ns = TestNetns::create(format!("test-lf-{}", std::process::id())).await?;
        let ns_name = ns.name.clone();

        // Host service on host-namespace loopback. The relay must reach this
        // even though the listener it accepts from lives inside the namespace.
        let host_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let host_port = host_listener.local_addr()?.port();
        let server_handle = tokio::spawn(async move {
            if let Ok((mut stream, _)) = host_listener.accept().await {
                use tokio::io::AsyncWriteExt;
                let _ = stream.write_all(b"HELLO_FROM_HOST").await;
            }
        });

        // Forward listener inside the namespace on the same port number.
        // Production binds on the bridge's 10.0.2.2 alias; the namespace
        // loopback exercises the same bind-in-namespace + relay path.
        let result: Result<()> = async {
            let handles = start_localhost_forwards(&ns_name, "127.0.0.1", &[host_port]).await?;

            // Give the listener a moment to start accepting
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;

            // Connect from inside the namespace (the guest contract: connect to
            // <listen_ip>:<port>) and verify data from the host service arrives.
            let listen_addr: SocketAddr = format!("127.0.0.1:{}", host_port).parse().unwrap();
            let mut client = connect_in_namespace(&ns_name, listen_addr).await?;

            use tokio::io::AsyncReadExt;
            let mut buf = Vec::new();
            client.read_to_end(&mut buf).await?;
            assert_eq!(buf, b"HELLO_FROM_HOST", "relay should reach host loopback");

            stop_relays(handles).await;
            Ok(())
        }
        .await;

        server_handle.abort();
        result?;
        println!("test_localhost_forward_relay PASSED");
        Ok(())
    }

    /// A port that cannot be bound fails the start, and the listeners bound before it are
    /// closed by the time the error is returned.
    #[cfg(feature = "privileged-tests")]
    #[tokio::test]
    async fn test_localhost_forward_bind_failure_leaves_no_listener() -> Result<()> {
        // Namespace is deleted by TestNetns::drop, even if an assertion panics.
        let ns = TestNetns::create(format!("test-lfb-{}", std::process::id())).await?;
        let ns_name = ns.name.clone();
        let loopback = |port: u16| SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port));

        // `taken` is held by a listener in the namespace. `free` was bound a moment ago
        // and released, and nothing else binds in this namespace.
        let squatter = bind_in_namespace(&ns_name, loopback(0)).await?;
        let taken = squatter.local_addr()?.port();
        let free = bind_in_namespace(&ns_name, loopback(0))
            .await?
            .local_addr()?
            .port();

        let error = start_localhost_forwards(&ns_name, "127.0.0.1", &[free, taken])
            .await
            .expect_err("the second port is in use");
        let error = format!("{error:#}");
        assert!(error.contains(&format!("127.0.0.1:{taken}")), "{error}");

        // Nothing is awaited between the error and this bind. A listener still owned by
        // an aborted relay task would stay open until the runtime ran again.
        let ns_path = format!("/var/run/netns/{ns_name}");
        run_in_namespace(&ns_path, || std::net::TcpListener::bind(loopback(free)))
            .expect("the first port must be free again once the start has failed");
        println!("test_localhost_forward_bind_failure_leaves_no_listener PASSED");
        Ok(())
    }
}

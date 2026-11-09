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
/// The port of the relay for the HTTPS proxy when it is not the HTTP one.
pub const HTTPS_PROXY_RELAY_PORT: u16 = 41481;

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
/// A socket belongs to the namespace it was made in, so only making it needs the
/// namespace: a blocking thread enters it via `setns(2)`, makes the socket and restores
/// its own namespace. The connect then runs on the runtime, where dropping the future
/// abandons it. A blocking connect could not be abandoned, and the runtime waits for its
/// blocking threads when the process exits: a dial of a guest that had just been killed
/// kept fcvm alive for 4.4 to 5.1 s after its teardown had finished, until the dial gave
/// up (the fleet agent's `teardown_ms`, 2026-10-30).
async fn connect_in_namespace(ns_name: &str, addr: SocketAddr) -> Result<tokio::net::TcpStream> {
    let ns_path = format!("/var/run/netns/{}", ns_name);

    let socket = tokio::task::spawn_blocking(move || {
        run_in_namespace(&ns_path, || match addr {
            SocketAddr::V4(_) => tokio::net::TcpSocket::new_v4(),
            SocketAddr::V6(_) => tokio::net::TcpSocket::new_v6(),
        })
    })
    .await
    .context("spawn_blocking panicked")??;

    Ok(socket.connect(addr).await?)
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

/// How many connections the relays of one call may hold at once: those relaying and
/// those whose upstream dial is still in progress. Each holds two descriptors (fcvm
/// raises RLIMIT_NOFILE to the hard limit, at least 65536, at startup) and a task. At the limit a relay stops
/// accepting, so a new client waits in its listener's accept queue (capped by
/// net.core.somaxconn), and once that is full the kernel stops answering its SYN and
/// the client retries. A relay accepts again as soon as one of its connections ends.
pub const RELAY_CONNECTION_LIMIT: usize = 4096;

/// The connections a set of relays may hold at once, shared by every relay given it.
pub type RelayLimit = Arc<tokio::sync::Semaphore>;

/// A limit of `connections` for the relays it is given.
pub fn relay_limit(connections: usize) -> RelayLimit {
    Arc::new(tokio::sync::Semaphore::new(connections))
}

/// Accept connections on `listener` and relay each to an upstream via `connect`, with a
/// limit of its own ([`RELAY_CONNECTION_LIMIT`]).
fn spawn_relay_loop<F, Fut>(
    listener: tokio::net::TcpListener,
    connect: F,
    label: &'static str,
) -> Relay
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<tokio::net::TcpStream>> + Send,
{
    spawn_limited_relay_loop(
        listener,
        relay_limit(RELAY_CONNECTION_LIMIT),
        connect,
        label,
    )
}

/// Accept connections on `listener` and relay each to an upstream via `connect`.
///
/// Shared relay loop used by both port forwarding and proxy relay. The relay's task owns
/// the listener and every connection it accepted. Once stopped (`stop_relays`) it closes
/// the listener, ends the connections and finishes only when their tasks have.
///
/// A connection holds one of `limit`'s permits from just after it is accepted until its
/// task ends, its dial of the upstream included. With none free the relay holds the
/// client it accepted until one frees and accepts no other, so a relay waiting for a
/// client holds no permit, and at the limit each relay holds at most one waiting client.
/// The dial is bounded by the kernel's SYN retries (`net.ipv4.tcp_syn_retries`, 6 by
/// default: about 127 s), and a stopped relay ends a dial in progress with its task.
fn spawn_limited_relay_loop<F, Fut>(
    listener: tokio::net::TcpListener,
    limit: RelayLimit,
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
        let mut at_limit = false;
        'relay: loop {
            let accepted = tokio::select! {
                _ = stopped.cancelled() => break,
                accepted = listener.accept() => accepted,
                // A finished connection leaves the set, so the set holds only live ones.
                Some(_) = connections.join_next() => continue,
            };
            let (client, peer) = match accepted {
                Ok(accepted) => accepted,
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
            };
            // The permit is taken after the accept, so a relay that waits for a client
            // holds none: the limit counts connections, not listeners. At the limit
            // this relay holds the one client it accepted until a permit frees, and
            // the clients after it wait in the listen queue.
            let permit = match Arc::clone(&limit).try_acquire_owned() {
                Ok(permit) => {
                    if at_limit {
                        at_limit = false;
                        info!("{label} relay below its connection limit, accepting again");
                    }
                    permit
                }
                Err(_) => {
                    if !at_limit {
                        at_limit = true;
                        warn!(
                            held = connections.len(),
                            "{label} relay at its connection limit; new clients wait in the \
                             listen queue"
                        );
                    }
                    loop {
                        tokio::select! {
                            _ = stopped.cancelled() => break 'relay,
                            permit = Arc::clone(&limit).acquire_owned() => match permit {
                                Ok(permit) => break permit,
                                Err(_) => break 'relay,
                            },
                            // Reaped while waiting; the accepted client stays held.
                            Some(_) = connections.join_next() => {}
                        }
                    }
                }
            };
            let connect = Arc::clone(&connect);
            connections.spawn(async move {
                let _permit = permit;
                match connect().await {
                    Ok(mut upstream) => {
                        let mut client = client;
                        // A relay writes each read as it arrives and must not
                        // coalesce: with Nagle on, a write that follows one the
                        // peer has not acknowledged waits for its (delayed) ACK,
                        // 40 ms on Linux and longer on macOS, on every message
                        // sent in more than one piece. The endpoints keep their
                        // own setting.
                        for (side, stream) in [("client", &client), ("upstream", &upstream)] {
                            if let Err(e) = stream.set_nodelay(true) {
                                debug!(error = %e, %peer, "{label} relay: TCP_NODELAY on the {side} socket");
                            }
                        }
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
/// A dial of the guest that is still in progress is a connection task's future
/// (`connect_in_namespace`), so it ends with the task and its socket closes.
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

/// The host-side listeners of a VM's published ports, one per mapping and in mapping
/// order, bound and listening.
///
/// A relay accepts on a duplicate of its listener's descriptor (`start_port_forwards`),
/// so the socket stays open, and keeps queueing connections in its backlog, for as long
/// as this value holds it, also after the relay has stopped. Dropping it closes every
/// listener whose relay has stopped.
#[derive(Debug, Default)]
pub struct PortListeners {
    listeners: Vec<std::net::TcpListener>,
}

impl std::ops::Index<usize> for PortListeners {
    type Output = std::net::TcpListener;
    fn index(&self, index: usize) -> &Self::Output {
        &self.listeners[index]
    }
}

impl PortListeners {
    /// Keep the listeners `keep` selects, by position, and close the others.
    pub fn retain(&mut self, mut keep: impl FnMut(usize) -> bool) {
        let mut index = 0;
        self.listeners.retain(|_| {
            let kept = keep(index);
            index += 1;
            kept
        });
    }

    /// Take the listener bound to exactly `addr`, if there is one.
    fn take(&mut self, addr: SocketAddr) -> Option<std::net::TcpListener> {
        let at = self
            .listeners
            .iter()
            .position(|listener| listener.local_addr().is_ok_and(|bound| bound == addr))?;
        Some(self.listeners.remove(at))
    }

    #[cfg(test)]
    fn into_tokio(self) -> Vec<tokio::net::TcpListener> {
        self.listeners
            .into_iter()
            .map(|listener| tokio::net::TcpListener::from_std(listener).unwrap())
            .collect()
    }
}

/// Bind the host-side listener of every mapping, or of none.
///
/// A mapping listens on the host address its HOSTIP names, or on `loopback_ip` (the VM's
/// own loopback address) when it names none. A listener in `handed` that is bound to a
/// mapping's address is used for that mapping instead of binding again: a VM that is
/// torn down and restored in one process hands its listeners to the restore, so its
/// published ports stay open in between (#1025). The listeners of `handed` that no
/// mapping uses are closed.
///
/// No relay runs yet, so an error drops, and with that closes, the listeners already
/// bound before it is returned.
pub fn bind_port_forwards(
    loopback_ip: &str,
    mappings: &[PortMapping],
    mut handed: PortListeners,
) -> Result<PortListeners> {
    let mut listeners = Vec::with_capacity(mappings.len());
    for mapping in mappings {
        let addr = port_forward_bind_addr(mapping, loopback_ip)?;
        let listener = match handed.take(addr) {
            Some(listener) => {
                info!(
                    listen = %addr,
                    guest_port = mapping.guest_port,
                    "port forwarding via TCP proxy, on the listener the torn-down VM handed over"
                );
                listener
            }
            None => {
                let listener = listen_for_port_forward(addr)
                    .and_then(|listener| listener.into_std())
                    .with_context(|| format!("binding port forward on {addr}"))?;
                info!(
                    listen = %addr,
                    guest_port = mapping.guest_port,
                    "port forwarding via TCP proxy"
                );
                listener
            }
        };
        listeners.push(listener);
    }
    Ok(PortListeners { listeners })
}

/// Start port forwarding on `listeners` (`bind_port_forwards` of the same `mappings`):
/// relay each connection to the guest inside the namespace.
///
/// Each relay accepts on a duplicate of its listener, so `listeners` still holds every
/// socket when the relays stop (`PortListeners`). Returns a `Relay` per port mapping, for
/// `stop_relays`.
pub fn start_port_forwards(
    listeners: &PortListeners,
    mappings: &[PortMapping],
    ns_name: &str,
    guest_ip: &str,
) -> Result<Vec<Relay>> {
    anyhow::ensure!(
        listeners.listeners.len() == mappings.len(),
        "{} port forward listeners for {} mappings",
        listeners.listeners.len(),
        mappings.len()
    );
    let guest_ip: IpAddr = guest_ip
        .parse()
        .with_context(|| format!("invalid guest address {guest_ip}"))?;
    // Every duplicate before any relay starts, so an error starts none.
    let accepting = listeners
        .listeners
        .iter()
        .map(|listener| {
            listener
                .try_clone()
                .and_then(tokio::net::TcpListener::from_std)
                .context("duplicating a port forward listener for its relay")
        })
        .collect::<Result<Vec<_>>>()?;
    let ns_name: Arc<str> = ns_name.into();
    // One limit for the VM's published ports together, so a VM holds at most this many
    // relayed connections and pending guest dials however many ports it publishes. A
    // mapping with a HOSTIP can be reachable from other machines (`--publish [::]:80:80`).
    let limit = relay_limit(RELAY_CONNECTION_LIMIT);

    Ok(accepting
        .into_iter()
        .zip(mappings)
        .map(|(listener, mapping)| {
            let ns_name = Arc::clone(&ns_name);
            let guest_addr = SocketAddr::new(guest_ip, mapping.guest_port);
            spawn_limited_relay_loop(
                listener,
                Arc::clone(&limit),
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
    port: u16,
    upstream: Vec<SocketAddr>,
) -> Result<Relay> {
    let bind_addr: SocketAddr = format!("{gateway_ip}:{port}")
        .parse()
        .with_context(|| format!("invalid proxy relay bind address {gateway_ip}:{port}"))?;

    let listener = bind_in_namespace(ns_name, bind_addr)
        .await
        .context("binding proxy relay listener in namespace")?;

    info!(
        proxy = ?upstream,
        bind = %bind_addr,
        "reverse proxy relay via TCP proxy"
    );

    Ok(proxy_relay(listener, upstream))
}

/// Relay each connection on `listener` to the host's proxy, trying its addresses in the
/// order they resolved (a `localhost` proxy can resolve to ::1 first and listen only on
/// 127.0.0.1).
fn proxy_relay(listener: tokio::net::TcpListener, upstream: Vec<SocketAddr>) -> Relay {
    let upstream: Arc<[SocketAddr]> = upstream.into();
    spawn_relay_loop(
        listener,
        move || {
            let upstream = Arc::clone(&upstream);
            async move {
                tokio::net::TcpStream::connect(&upstream[..])
                    .await
                    .context("connecting to proxy")
            }
        },
        "proxy",
    )
}

/// The host's `(HTTP, HTTPS)` proxies, each from its own scheme's variables.
pub fn host_scheme_proxies_from_env() -> (Option<String>, Option<String>) {
    let var = |names: [&str; 2]| {
        names
            .into_iter()
            .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()))
    };
    (
        var(["HTTP_PROXY", "http_proxy"]),
        var(["HTTPS_PROXY", "https_proxy"]),
    )
}

/// A relay for the host's proxy: the address it connects to, and the URL a guest uses
/// to reach it. The relay passes bytes through, so the guest still authenticates to the
/// proxy itself, and the URL keeps the proxy's credentials.
#[derive(Debug, PartialEq)]
pub struct ProxyRelayPlan {
    /// Every address the proxy's host resolved to, in order; never empty.
    pub upstream: Vec<SocketAddr>,
    pub guest_url: String,
}

/// Plan a relay for `proxy` that listens on `relay_ip:port`.
pub fn plan_proxy_relay(proxy: &str, relay_ip: &str, port: u16) -> Result<ProxyRelayPlan> {
    let url = proxy_url(proxy)?;
    let upstream = resolve_proxy(&url)?;
    // The relay passes bytes through, so the guest speaks the proxy's own scheme.
    let mut guest = url;
    guest
        .set_host(Some(relay_ip))
        .with_context(|| format!("relay address {relay_ip} is not a URL host"))?;
    guest
        .set_port(Some(port))
        .map_err(|()| anyhow::anyhow!("the proxy URL cannot take a port"))?;
    Ok(ProxyRelayPlan {
        upstream,
        guest_url: guest.to_string(),
    })
}

/// One relay of a `SchemeProxies` plan.
#[derive(Debug, PartialEq)]
pub struct PlannedRelay {
    pub port: u16,
    pub upstream: Vec<SocketAddr>,
}

/// The relays a guest needs for the host's HTTP and HTTPS proxies, and the proxy URL
/// the guest is given for each scheme.
#[derive(Debug, Default, PartialEq)]
pub struct SchemeProxies {
    pub relays: Vec<PlannedRelay>,
    /// The guest's HTTP and HTTPS proxy URLs. Both None when nothing is relayed: the
    /// guest then takes the host's variables as they are. When something is relayed,
    /// a scheme whose proxy needs no relay gets that proxy's own URL.
    pub http_url: Option<String>,
    pub https_url: Option<String>,
}

/// Plan the relays for the host's HTTP proxy (`http`) and HTTPS proxy (`https`), each
/// the value of that scheme's variable. A scheme without a variable uses the other's
/// proxy, as the guest's URL for both schemes was before they could differ.
///
/// A proxy `needs_relay` judges from the addresses it resolved to. The proxy a scheme
/// uses is relayed on its own port: `PROXY_RELAY_PORT` for HTTP, and
/// `HTTPS_PROXY_RELAY_PORT` for HTTPS when it is a different proxy. Both schemes on
/// one proxy share one relay and one URL. A proxy that does not resolve is passed
/// through with a warning, like one that needs no relay.
pub fn plan_scheme_proxies(
    http: Option<&str>,
    https: Option<&str>,
    relay_ip: &str,
    needs_relay: impl Fn(&[SocketAddr]) -> bool,
) -> SchemeProxies {
    let (http, https) = (http.or(https), https.or(http));
    let (Some(http), Some(https)) = (http, https) else {
        return SchemeProxies::default();
    };
    let mut plan = SchemeProxies::default();
    let relay_for = |proxy: &str, port: u16, plan: &mut SchemeProxies| -> String {
        match plan_proxy_relay(proxy, relay_ip, port) {
            Ok(relay) if needs_relay(&relay.upstream) => {
                plan.relays.push(PlannedRelay {
                    port,
                    upstream: relay.upstream,
                });
                relay.guest_url
            }
            Ok(_) => proxy.to_string(),
            Err(e) => {
                warn!(error = %format!("{e:#}"), "cannot relay the host's proxy");
                proxy.to_string()
            }
        }
    };
    let (http_url, https_url) = if http == https {
        let url = relay_for(http, PROXY_RELAY_PORT, &mut plan);
        (url.clone(), url)
    } else {
        (
            relay_for(http, PROXY_RELAY_PORT, &mut plan),
            relay_for(https, HTTPS_PROXY_RELAY_PORT, &mut plan),
        )
    };
    if !plan.relays.is_empty() {
        plan.http_url = Some(http_url);
        plan.https_url = Some(https_url);
    }
    plan
}

/// Parse a proxy URL like "http://user:password@host:port" or "host:port" into the
/// address to connect to. Hostnames are resolved. The credentials are not part of the
/// address, and no error repeats them.
pub fn parse_proxy_addr(proxy: &str) -> Result<SocketAddr> {
    Ok(resolve_proxy(&proxy_url(proxy)?)?[0])
}

fn proxy_url(proxy: &str) -> Result<url::Url> {
    let normalized = if proxy.contains("://") {
        proxy.to_string()
    } else {
        format!("http://{proxy}")
    };
    // url's parse errors name the problem, not the input.
    url::Url::parse(&normalized).map_err(|e| anyhow::anyhow!("the proxy URL does not parse: {e}"))
}

/// Every address the proxy's host resolves to, in order; an error when there is none.
fn resolve_proxy(url: &url::Url) -> Result<Vec<SocketAddr>> {
    use std::net::ToSocketAddrs;
    let host = url
        .host_str()
        .context("the proxy URL has no host")?
        .trim_start_matches('[')
        .trim_end_matches(']');
    let port = url
        .port_or_known_default()
        .context("the proxy URL has no port")?;
    let addresses: Vec<SocketAddr> = (host, port)
        .to_socket_addrs()
        .with_context(|| format!("resolving proxy address {host}:{port}"))?
        .collect();
    anyhow::ensure!(
        !addresses.is_empty(),
        "no addresses resolved for proxy {host}:{port}"
    );
    Ok(addresses)
}

/// Start a relay for the host's proxy that listens on `bind_ip:port` in fcvm's own
/// network namespace, where the guest reaches it through its veth, and connects to
/// `upstream`.
pub async fn start_host_proxy_relay(
    bind_ip: &str,
    port: u16,
    upstream: Vec<SocketAddr>,
) -> Result<Relay> {
    let bind_addr: SocketAddr = format!("{bind_ip}:{port}")
        .parse()
        .with_context(|| format!("invalid proxy relay bind address {bind_ip}:{port}"))?;
    let listener = tokio::net::TcpListener::bind(bind_addr)
        .await
        .with_context(|| format!("binding proxy relay listener on {bind_addr}"))?;
    info!(proxy = ?upstream, bind = %bind_addr, "proxy relay on the host");
    Ok(proxy_relay(listener, upstream))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wait until `count` reaches `want`, then check it stays there for a while.
    async fn settles_at(count: &std::sync::atomic::AtomicUsize, want: usize) {
        use std::sync::atomic::Ordering;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while count.load(Ordering::SeqCst) < want {
            assert!(
                std::time::Instant::now() < deadline,
                "only {} of {want} connections reached the upstream",
                count.load(Ordering::SeqCst)
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert_eq!(
            count.load(Ordering::SeqCst),
            want,
            "the relay dialled more upstreams than its limit allows"
        );
    }

    /// A relay holds at most its limit of connections; a client over it waits in the
    /// listen queue and is relayed once a connection ends (#1040).
    #[tokio::test]
    async fn a_relay_holds_no_more_connections_than_its_limit() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = upstream.local_addr().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let relay_addr = listener.local_addr().unwrap();
        let dialled = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&dialled);
        let relay = spawn_limited_relay_loop(
            listener,
            relay_limit(2),
            move || {
                let counter = Arc::clone(&counter);
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok(tokio::net::TcpStream::connect(upstream_addr).await?)
                }
            },
            "test",
        );
        let mut clients = Vec::new();
        for _ in 0..3 {
            clients.push(tokio::net::TcpStream::connect(relay_addr).await.unwrap());
        }
        settles_at(&dialled, 2).await;
        // The two relayed connections end on both sides; the waiting client is relayed.
        let (first, _) = upstream.accept().await.unwrap();
        let (second, _) = upstream.accept().await.unwrap();
        drop((first, second, clients.remove(0), clients.remove(0)));
        settles_at(&dialled, 3).await;
        stop_relays([relay]).await;
    }

    /// A relay waiting for a client holds no permit: with more relays on one limit than
    /// the limit allows, a client of any of them is still relayed while the others are
    /// idle.
    #[tokio::test]
    async fn idle_relays_hold_no_permit_of_their_shared_limit() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = upstream.local_addr().unwrap();
        let limit = relay_limit(2);
        let dialled = Arc::new(AtomicUsize::new(0));
        let mut relays = Vec::new();
        let mut addrs = Vec::new();
        for _ in 0..3 {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            addrs.push(listener.local_addr().unwrap());
            let counter = Arc::clone(&dialled);
            relays.push(spawn_limited_relay_loop(
                listener,
                Arc::clone(&limit),
                move || {
                    let counter = Arc::clone(&counter);
                    async move {
                        counter.fetch_add(1, Ordering::SeqCst);
                        Ok(tokio::net::TcpStream::connect(upstream_addr).await?)
                    }
                },
                "test",
            ));
        }
        // Every relay has started and is waiting in accept, and none holds a permit.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(
            limit.available_permits(),
            2,
            "relays waiting for a client hold permits of their shared limit"
        );
        for addr in &addrs {
            let _client = tokio::net::TcpStream::connect(addr).await.unwrap();
            let before = dialled.load(Ordering::SeqCst);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while dialled.load(Ordering::SeqCst) == before {
                assert!(
                    std::time::Instant::now() < deadline,
                    "a client of the relay on {addr} was not relayed while the others were idle"
                );
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            let (_served, _) = upstream.accept().await.unwrap();
        }
        stop_relays(relays).await;
    }

    /// A dial of the upstream that has not finished holds its place under the limit,
    /// and stopping the relay ends it.
    #[tokio::test]
    async fn a_dial_in_progress_counts_toward_the_limit_and_ends_with_the_relay() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        struct Dropped(Arc<AtomicBool>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let relay_addr = listener.local_addr().unwrap();
        let dialled = Arc::new(AtomicUsize::new(0));
        let ended = Arc::new(AtomicBool::new(false));
        let (counter, flag) = (Arc::clone(&dialled), Arc::clone(&ended));
        let relay = spawn_limited_relay_loop(
            listener,
            relay_limit(1),
            move || {
                let counter = Arc::clone(&counter);
                let guard = Dropped(Arc::clone(&flag));
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    let _guard = guard;
                    std::future::pending::<Result<tokio::net::TcpStream>>().await
                }
            },
            "test",
        );
        let _first = tokio::net::TcpStream::connect(relay_addr).await.unwrap();
        let _second = tokio::net::TcpStream::connect(relay_addr).await.unwrap();
        settles_at(&dialled, 1).await;
        assert!(!ended.load(Ordering::SeqCst));
        stop_relays([relay]).await;
        assert!(
            ended.load(Ordering::SeqCst),
            "the dial in progress outlived its stopped relay"
        );
    }

    /// A message the upstream writes in two parts reaches the relay's client without
    /// waiting for the client to acknowledge the first part. The relay writes each read
    /// as it comes, so without TCP_NODELAY on its client socket Nagle holds the second
    /// part until the client's (delayed) ACK of the first arrives: on the GPU wall a
    /// renderer frame is TLS records written back to back, and through the relay every
    /// fcvm frame's tail waited for that ACK, while the EC2 tiles' frames did not.
    #[tokio::test]
    async fn a_message_written_in_two_parts_crosses_the_relay_without_waiting_for_an_ack() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        const ROUNDS: usize = 30;
        const FIRST: usize = 16_384;
        const SECOND: usize = 12_000;
        let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = upstream.local_addr().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let relay_addr = listener.local_addr().unwrap();
        let relay = spawn_relay_loop(
            listener,
            move || async move { Ok(tokio::net::TcpStream::connect(upstream_addr).await?) },
            "test",
        );
        let mut client = tokio::net::TcpStream::connect(relay_addr).await.unwrap();
        let (mut server, _) = upstream.accept().await.unwrap();
        // The renderer sets TCP_NODELAY on its own sockets (fleet/renderer/src/server.cpp).
        server.set_nodelay(true).unwrap();
        // A browser's receiver delays its ACKs (macOS by up to 100 ms or more). Loopback
        // starts in quick-ACK mode, so it is turned off before every message, as the
        // kernel turns it back on: then the client acknowledges a lone segment only when
        // its delayed-ACK timer fires (40 ms here).
        let quickack_off = |s: &tokio::net::TcpStream| {
            use std::os::fd::AsRawFd;
            let off: libc::c_int = 0;
            // SAFETY: a valid socket fd and a c_int option value that outlives the call.
            let rc = unsafe {
                libc::setsockopt(
                    s.as_raw_fd(),
                    libc::IPPROTO_TCP,
                    libc::TCP_QUICKACK,
                    &off as *const libc::c_int as *const libc::c_void,
                    std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                )
            };
            assert_eq!(rc, 0, "TCP_QUICKACK: {}", std::io::Error::last_os_error());
        };
        let mut took = Vec::with_capacity(ROUNDS);
        let mut buf = vec![0u8; FIRST + SECOND];
        for _ in 0..ROUNDS {
            quickack_off(&client);
            let started = std::time::Instant::now();
            server.write_all(&[1u8; FIRST]).await.unwrap();
            // A moment apart, as two TLS records are: the relay reads them separately.
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            server.write_all(&[2u8; SECOND]).await.unwrap();
            client.read_exact(&mut buf).await.unwrap();
            took.push(started.elapsed());
            tokio::time::sleep(std::time::Duration::from_millis(33)).await;
        }
        stop_relays([relay]).await;
        took.sort();
        let median = took[ROUNDS / 2];
        assert!(
            median < std::time::Duration::from_millis(15),
            "median {median:?} for a message in two parts through the relay; all: {took:?}"
        );
    }

    /// A proxy URL carries credentials (`user:password@host:port`). They are not part of
    /// the address, and an error must not repeat them: routed mode logged the whole URL
    /// and skipped its relay (#1461).
    #[test]
    fn a_proxy_with_credentials_resolves_to_its_address() {
        let addr = parse_proxy_addr("http://user:secret@127.0.0.1:3128").unwrap();
        assert_eq!(addr.to_string(), "127.0.0.1:3128");
        let error = parse_proxy_addr("http://user:secret@no-such-host.invalid:3128").unwrap_err();
        assert!(!format!("{error:#}").contains("secret"), "{error:#}");
    }

    /// The guest's URL for a relay keeps the proxy's credentials and names the relay.
    #[test]
    fn a_relay_url_keeps_the_proxys_credentials() {
        let plan = plan_proxy_relay(
            "http://user:secret@127.0.0.1:3128/",
            "10.0.2.1",
            PROXY_RELAY_PORT,
        )
        .unwrap();
        assert_eq!(plan.upstream, vec!["127.0.0.1:3128".parse().unwrap()]);
        assert_eq!(
            plan.guest_url,
            format!("http://user:secret@10.0.2.1:{PROXY_RELAY_PORT}/")
        );
        let plan = plan_proxy_relay("127.0.0.1:3128", "172.30.0.1", PROXY_RELAY_PORT).unwrap();
        assert_eq!(
            plan.guest_url,
            format!("http://172.30.0.1:{PROXY_RELAY_PORT}/")
        );
    }

    /// `plan_scheme_proxies` with a relay for every proxy on 127.0.0.1.
    fn plan(http: Option<&str>, https: Option<&str>) -> SchemeProxies {
        plan_scheme_proxies(http, https, "10.0.0.1", |upstream| {
            upstream.iter().all(|a| a.ip().is_loopback())
        })
    }

    /// No proxy variable, no relay and no URL.
    #[test]
    fn no_proxy_plans_nothing() {
        assert_eq!(plan(None, None), SchemeProxies::default());
    }

    /// A scheme with no variable uses the other's proxy, and both schemes on one proxy
    /// share one relay and one URL.
    #[test]
    fn a_shared_proxy_is_one_relay() {
        let proxy = "http://127.0.0.1:3128";
        for (http, https) in [
            (Some(proxy), Some(proxy)),
            (Some(proxy), None),
            (None, Some(proxy)),
        ] {
            let plan = plan(http, https);
            assert_eq!(
                plan.relays,
                vec![PlannedRelay {
                    port: PROXY_RELAY_PORT,
                    upstream: vec!["127.0.0.1:3128".parse().unwrap()],
                }]
            );
            let url = format!("http://10.0.0.1:{PROXY_RELAY_PORT}/");
            assert_eq!(plan.http_url.as_deref(), Some(url.as_str()));
            assert_eq!(plan.https_url.as_deref(), Some(url.as_str()));
        }
    }

    /// Two different proxies get two relays on distinct ports, each to its own upstream.
    #[test]
    fn different_proxies_get_distinct_relay_ports() {
        let plan = plan(
            Some("http://127.0.0.1:3128"),
            Some("https://127.0.0.1:3129"),
        );
        assert_eq!(plan.relays.len(), 2);
        assert_eq!(plan.relays[0].port, PROXY_RELAY_PORT);
        assert_eq!(plan.relays[1].port, HTTPS_PROXY_RELAY_PORT);
        assert_ne!(plan.relays[0].port, plan.relays[1].port);
        assert_eq!(
            plan.relays[0].upstream,
            vec!["127.0.0.1:3128".parse().unwrap()]
        );
        assert_eq!(
            plan.relays[1].upstream,
            vec!["127.0.0.1:3129".parse().unwrap()]
        );
        assert_eq!(
            plan.http_url,
            Some(format!("http://10.0.0.1:{PROXY_RELAY_PORT}/"))
        );
        // The scheme of the proxy's URL stays the proxy's own.
        assert_eq!(
            plan.https_url,
            Some(format!("https://10.0.0.1:{HTTPS_PROXY_RELAY_PORT}/"))
        );
    }

    /// A proxy's host can resolve to several addresses (`localhost` to ::1 and then
    /// 127.0.0.1 on some hosts) while the proxy listens on one. The relay tries them in
    /// order instead of keeping the first.
    #[tokio::test]
    async fn a_proxy_relay_tries_every_address_of_its_proxy() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let refused = {
            let gone = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            gone.local_addr().unwrap()
        };
        let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let live = proxy.local_addr().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let relay_addr = listener.local_addr().unwrap();
        let relay = proxy_relay(listener, vec![refused, live]);

        let mut client = tokio::net::TcpStream::connect(relay_addr).await.unwrap();
        client.write_all(b"CONNECT").await.unwrap();
        let (mut upstream, _) =
            tokio::time::timeout(std::time::Duration::from_secs(5), proxy.accept())
                .await
                .expect("the relay never reached the proxy at its second address")
                .unwrap();
        let mut got = [0u8; 7];
        upstream.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"CONNECT");
        stop_relays([relay]).await;
    }

    /// The relay passes bytes through, so a TLS proxy stays TLS for the guest.
    #[test]
    fn a_relay_url_keeps_the_proxys_scheme() {
        let plan =
            plan_proxy_relay("https://127.0.0.1:3128", "10.0.2.1", PROXY_RELAY_PORT).unwrap();
        assert_eq!(plan.upstream, vec!["127.0.0.1:3128".parse().unwrap()]);
        assert_eq!(
            plan.guest_url,
            format!("https://10.0.2.1:{PROXY_RELAY_PORT}/")
        );
    }

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
            loopback_client: false,
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
            Default::default(),
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
        let listeners =
            bind_port_forwards("127.0.0.1", &[mapping(Some("::"), 0)], Default::default()).unwrap();
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
        let error = bind_port_forwards(&loopback.to_string(), &mappings, Default::default())
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
        let error = bind_port_forwards(&loopback.to_string(), &absent, Default::default())
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
        let listeners = bind_port_forwards(
            &test_ip(8).to_string(),
            &[mapping(None, 0)],
            Default::default(),
        )
        .unwrap()
        .into_tokio();
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
        let listeners = bind_port_forwards(
            &test_ip(6).to_string(),
            &[mapping(None, 0)],
            Default::default(),
        )
        .unwrap()
        .into_tokio();
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
        let listeners = bind_port_forwards(
            &test_ip(9).to_string(),
            &[mapping(None, 0)],
            Default::default(),
        )
        .unwrap()
        .into_tokio();
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
        let listeners = bind_port_forwards(
            &test_ip(10).to_string(),
            &[mapping(None, 0)],
            Default::default(),
        )
        .unwrap()
        .into_tokio();
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

    /// The inode of a socket's descriptor: two descriptors with one inode are one socket.
    fn socket_inode(socket: &impl AsFd) -> u64 {
        nix::sys::stat::fstat(socket.as_fd()).unwrap().st_ino
    }

    /// A VM torn down and restored in one process hands its listeners to the restore
    /// (#1025). From the teardown to the restore's bind, a client of the published address
    /// is queued, not refused, and no other socket can take the port; the restore binds
    /// the very socket the torn-down VM listened on, and accepts that client. A listener
    /// whose address the restore does not publish is closed.
    #[tokio::test]
    async fn handed_over_listeners_stay_open_from_the_teardown_to_the_restore() {
        let (loopback, host, other) = (test_ip(11), test_ip(12), test_ip(13));
        let probe = std::net::TcpListener::bind((host, 0)).unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let published = [mapping(Some(&host.to_string()), port)];

        // The torn-down VM: its relay ran on a duplicate and has stopped.
        let first_mappings = [published[0].clone(), mapping(Some(&other.to_string()), 0)];
        let first =
            bind_port_forwards(&loopback.to_string(), &first_mappings, Default::default()).unwrap();
        let first_inode = socket_inode(&first[0]);
        let dropped_addr = first[1].local_addr().unwrap();
        let relays = start_port_forwards(&first, &first_mappings, "unused", "10.0.2.100").unwrap();
        stop_relays(relays).await;

        // The gap: no relay accepts, and the socket still listens.
        let gap_client = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tokio::net::TcpStream::connect((host, port)),
        )
        .await
        .expect("a connect in the gap completes")
        .expect("a client in the gap is queued, not refused");
        let squatter = std::net::TcpListener::bind((host, port))
            .expect_err("no other socket may take the port in the gap");
        assert_eq!(squatter.kind(), std::io::ErrorKind::AddrInUse, "{squatter}");

        // The restore publishes only the first mapping.
        let second = bind_port_forwards(&loopback.to_string(), &published, first).unwrap();
        assert_eq!(
            socket_inode(&second[0]),
            first_inode,
            "the restore must listen on the torn-down VM's socket"
        );
        let (_, peer) = second[0].accept().unwrap();
        assert_eq!(
            peer,
            gap_client.local_addr().unwrap(),
            "the client queued in the gap is accepted by the restore"
        );
        let refused = tokio::net::TcpStream::connect(dropped_addr)
            .await
            .expect_err("a handed listener the restore does not publish is closed");
        assert_eq!(
            refused.kind(),
            std::io::ErrorKind::ConnectionRefused,
            "{refused}"
        );
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

    /// A dial of the guest that is still in progress does not hold the process when it exits.
    ///
    /// fcvm's `main` drops its runtime on the way out, and a runtime's drop waits for its
    /// blocking threads. The dial target here is a listener in the namespace whose accept
    /// queue is full, so the kernel drops every SYN and a connect waits for its SYN
    /// retries (over two minutes), as a dial of a guest killed under it does. The runtime
    /// must be gone at once.
    #[cfg(feature = "privileged-tests")]
    #[test]
    fn a_dial_in_progress_does_not_hold_the_runtime_when_it_is_dropped() -> Result<()> {
        use std::os::fd::AsRawFd;
        let setup = tokio::runtime::Runtime::new()?;
        let ns = setup.block_on(TestNetns::create(format!(
            "test-dial-{}",
            std::process::id()
        )))?;
        drop(setup);
        let ns_path = format!("/var/run/netns/{}", ns.name);
        let listener = run_in_namespace(&ns_path, || {
            let l = std::net::TcpListener::bind("127.0.0.1:0")?;
            // SAFETY: listen(2) on a socket this test owns; backlog 0 keeps one queued
            // connection at most.
            if unsafe { libc::listen(l.as_raw_fd(), 0) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(l)
        })?;
        let target = listener.local_addr()?;
        // Fill the accept queue until a connect gets no answer.
        let mut queued = Vec::new();
        let full = run_in_namespace(&ns_path, || {
            for _ in 0..8 {
                match std::net::TcpStream::connect_timeout(
                    &target,
                    std::time::Duration::from_millis(500),
                ) {
                    Ok(s) => queued.push(s),
                    Err(e) if e.kind() == std::io::ErrorKind::TimedOut => return Ok(true),
                    Err(e) => return Err(e),
                }
            }
            Ok(false)
        })?;
        anyhow::ensure!(full, "the accept queue never filled, so no dial would wait");

        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        let ns_name = ns.name.clone();
        let dial = rt.spawn(async move { connect_in_namespace(&ns_name, target).await });
        std::thread::sleep(std::time::Duration::from_millis(300));
        anyhow::ensure!(
            !dial.is_finished(),
            "the dial ended before the runtime was dropped"
        );
        let (tx, rx) = std::sync::mpsc::channel();
        let started = std::time::Instant::now();
        std::thread::spawn(move || {
            drop(rt);
            let _ = tx.send(());
        });
        let dropped = rx.recv_timeout(std::time::Duration::from_secs(3));
        assert!(
            dropped.is_ok(),
            "the runtime was still waiting for a dial of a guest that does not answer {:?} after it was dropped",
            started.elapsed()
        );
        drop(queued);
        drop(listener);
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
        drop(host_listener); // Release the port so bind_port_forwards can bind it

        let mapping = PortMapping {
            host_port: host_addr.port(),
            guest_port: 9999,
            host_ip: None,
            proto: super::super::types::Protocol::Tcp,
            loopback_client: false,
        };

        let mappings = [mapping];
        let listeners = bind_port_forwards("127.0.0.1", &mappings, Default::default())?;
        let handles = start_port_forwards(&listeners, &mappings, &ns_name, "127.0.0.1")?;

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

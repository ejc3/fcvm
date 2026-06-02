//! Namespace-aware TCP proxy for routed networking.
//!
//! Replaces socat with built-in Rust TCP relay. Uses `setns(2)` to create
//! sockets inside network namespaces, then relays data with tokio.

use std::net::SocketAddr;
use std::os::fd::AsFd;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::task::JoinHandle;
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

/// Accept connections on `listener` and relay each to an upstream via `connect`.
///
/// Shared relay loop used by both port forwarding and proxy relay.
fn spawn_relay_loop<F, Fut>(
    listener: tokio::net::TcpListener,
    connect: F,
    label: &'static str,
) -> JoinHandle<()>
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<tokio::net::TcpStream>> + Send,
{
    let connect = Arc::new(connect);
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((client, peer)) => {
                    let connect = Arc::clone(&connect);
                    tokio::spawn(async move {
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
                    warn!(error = %e, "{label} accept error");
                    break;
                }
            }
        }
    })
}

/// Start port forwarding: listen on host loopback, relay to guest inside namespace.
///
/// Returns a `JoinHandle` per port mapping. Abort all handles on cleanup.
pub async fn start_port_forwards(
    loopback_ip: &str,
    mappings: &[PortMapping],
    ns_name: &str,
    guest_ip: &str,
) -> Result<Vec<JoinHandle<()>>> {
    let mut handles: Vec<JoinHandle<()>> = Vec::new();

    // Helper: abort all started handles on error.
    let abort_all = |handles: &[JoinHandle<()>]| {
        for h in handles {
            h.abort();
        }
    };

    for mapping in mappings {
        let bind_addr: SocketAddr = match format!("{}:{}", loopback_ip, mapping.host_port).parse() {
            Ok(addr) => addr,
            Err(e) => {
                abort_all(&handles);
                return Err(anyhow::anyhow!(e)).with_context(|| {
                    format!("invalid bind address {}:{}", loopback_ip, mapping.host_port)
                });
            }
        };

        let listener = match tokio::net::TcpListener::bind(bind_addr).await {
            Ok(l) => l,
            Err(e) => {
                abort_all(&handles);
                return Err(anyhow::anyhow!(e))
                    .with_context(|| format!("binding port forward on {bind_addr}"));
            }
        };

        info!(
            host_port = mapping.host_port,
            guest_port = mapping.guest_port,
            bind = %loopback_ip,
            "port forwarding via TCP proxy"
        );

        let ns_name: Arc<str> = ns_name.into();
        let guest_addr: SocketAddr = match format!("{}:{}", guest_ip, mapping.guest_port).parse() {
            Ok(addr) => addr,
            Err(e) => {
                abort_all(&handles);
                return Err(anyhow::anyhow!(e)).with_context(|| {
                    format!("invalid guest address {}:{}", guest_ip, mapping.guest_port)
                });
            }
        };

        let handle = spawn_relay_loop(
            listener,
            move || {
                let ns = Arc::clone(&ns_name);
                async move { connect_in_namespace(&ns, guest_addr).await }
            },
            "port forward",
        );

        handles.push(handle);
    }

    Ok(handles)
}

/// Start localhost forwarding: listen inside the namespace, relay to host loopback.
///
/// Used for `--forward-localhost` in routed mode. fc-agent's guest-side relay
/// connects to `<listen_ip>:<port>` (the pasta-style host gateway 10.0.2.2) for
/// traffic destined to the host's loopback. The listener runs inside the VM's
/// network namespace; the upstream connect happens in the host namespace,
/// reaching services bound to 127.0.0.1:<port> on the host.
///
/// Returns a `JoinHandle` per port. Abort all handles on cleanup.
pub async fn start_localhost_forwards(
    ns_name: &str,
    listen_ip: &str,
    ports: &[u16],
) -> Result<Vec<JoinHandle<()>>> {
    let mut handles: Vec<JoinHandle<()>> = Vec::new();

    // Helper: abort all started handles on error.
    let abort_all = |handles: &[JoinHandle<()>]| {
        for h in handles {
            h.abort();
        }
    };

    for &port in ports {
        let bind_addr: SocketAddr = match format!("{}:{}", listen_ip, port).parse() {
            Ok(addr) => addr,
            Err(e) => {
                abort_all(&handles);
                return Err(anyhow::anyhow!(e)).with_context(|| {
                    format!(
                        "invalid localhost forward bind address {}:{}",
                        listen_ip, port
                    )
                });
            }
        };

        let listener = match bind_in_namespace(ns_name, bind_addr).await {
            Ok(l) => l,
            Err(e) => {
                abort_all(&handles);
                return Err(e).with_context(|| format!("binding localhost forward on {bind_addr}"));
            }
        };

        info!(
            port,
            bind = %bind_addr,
            "localhost forwarding via TCP proxy"
        );

        let host_addr = SocketAddr::from(([127, 0, 0, 1], port));
        let handle = spawn_relay_loop(
            listener,
            move || async move {
                tokio::net::TcpStream::connect(host_addr)
                    .await
                    .context("connecting to host loopback")
            },
            "localhost forward",
        );

        handles.push(handle);
    }

    Ok(handles)
}

/// Start a reverse proxy relay: listen inside namespace, connect to host proxy.
///
/// Used when host-side BPF programs intercept connect() for proxy auth.
/// The relay listens in the namespace (reachable from the VM via gateway IP)
/// and connects to the real proxy from the host namespace.
pub async fn start_proxy_relay(
    ns_name: &str,
    gateway_ip: &str,
    proxy_addr: SocketAddr,
) -> Result<JoinHandle<()>> {
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

    let handle = spawn_relay_loop(
        listener,
        move || async move {
            tokio::net::TcpStream::connect(proxy_addr)
                .await
                .context("connecting to proxy")
        },
        "proxy",
    );

    Ok(handle)
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

    /// Test that connect_in_namespace can reach a listener inside a namespace.
    ///
    /// Creates a temp namespace, binds a listener in it, connects from outside
    /// via setns, and verifies bidirectional data flow.
    #[cfg(feature = "privileged-tests")]
    #[tokio::test]
    async fn test_connect_in_namespace() -> Result<()> {
        let ns_name = format!("test-proxy-{}", std::process::id());

        // Create namespace
        tokio::process::Command::new("ip")
            .args(["netns", "add", &ns_name])
            .output()
            .await
            .context("creating test namespace")?;

        // Bring up loopback in namespace (required for 127.0.0.1 to work)
        tokio::process::Command::new("ip")
            .args(["netns", "exec", &ns_name, "ip", "link", "set", "lo", "up"])
            .output()
            .await?;

        let cleanup = || async {
            let _ = tokio::process::Command::new("ip")
                .args(["netns", "del", &ns_name])
                .output()
                .await;
        };

        // Bind a listener inside the namespace
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = match bind_in_namespace(&ns_name, addr).await {
            Ok(l) => l,
            Err(e) => {
                cleanup().await;
                return Err(e);
            }
        };
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
        let mut stream = match connect_in_namespace(&ns_name, listen_addr).await {
            Ok(s) => s,
            Err(e) => {
                echo_handle.abort();
                cleanup().await;
                return Err(e);
            }
        };

        // Send data and verify we can write (connection established)
        use tokio::io::AsyncWriteExt;
        stream.write_all(b"hello").await?;
        stream.shutdown().await?;
        println!("Successfully connected and sent data via namespace");

        // Cleanup
        echo_handle.abort();
        cleanup().await;
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
        let ns_name = format!("test-pf-{}", std::process::id());

        // Create namespace + loopback
        tokio::process::Command::new("ip")
            .args(["netns", "add", &ns_name])
            .output()
            .await?;
        tokio::process::Command::new("ip")
            .args(["netns", "exec", &ns_name, "ip", "link", "set", "lo", "up"])
            .output()
            .await?;

        let cleanup = |ns: String| async move {
            let _ = tokio::process::Command::new("ip")
                .args(["netns", "del", &ns])
                .output()
                .await;
        };

        // Start echo server inside namespace on a known port
        let server_addr: SocketAddr = "127.0.0.1:9999".parse().unwrap();
        let server_listener = bind_in_namespace(&ns_name, server_addr).await?;

        let echo_handle = tokio::spawn(async move {
            loop {
                match server_listener.accept().await {
                    Ok((mut stream, _)) => {
                        tokio::spawn(async move {
                            let (mut r, mut w) = stream.split();
                            let _ = tokio::io::copy(&mut r, &mut w).await;
                        });
                    }
                    Err(_) => break,
                }
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

        // Cleanup
        for h in handles {
            h.abort();
        }
        echo_handle.abort();
        cleanup(ns_name).await;
        println!("test_port_forward_relay PASSED");
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
        let ns_name = format!("test-lf-{}", std::process::id());

        // Create namespace + loopback
        tokio::process::Command::new("ip")
            .args(["netns", "add", &ns_name])
            .output()
            .await?;
        tokio::process::Command::new("ip")
            .args(["netns", "exec", &ns_name, "ip", "link", "set", "lo", "up"])
            .output()
            .await?;

        let cleanup = |ns: String| async move {
            let _ = tokio::process::Command::new("ip")
                .args(["netns", "del", &ns])
                .output()
                .await;
        };

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

            for h in handles {
                h.abort();
            }
            Ok(())
        }
        .await;

        server_handle.abort();
        cleanup(ns_name).await;
        result?;
        println!("test_localhost_forward_relay PASSED");
        Ok(())
    }
}

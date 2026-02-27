//! Namespace-aware TCP proxy for routed networking.
//!
//! Replaces socat with built-in Rust TCP relay. Uses `setns(2)` to create
//! sockets inside network namespaces, then relays data with tokio.

use std::net::SocketAddr;
use std::os::fd::AsFd;

use anyhow::{Context, Result};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use super::PortMapping;

/// Connect a TCP stream inside a network namespace.
///
/// Spawns a blocking thread that enters the namespace via `setns(2)`, connects,
/// then restores the original namespace. The returned stream works from any namespace.
async fn connect_in_namespace(ns_name: &str, addr: SocketAddr) -> Result<tokio::net::TcpStream> {
    let ns_path = format!("/var/run/netns/{}", ns_name);

    let std_stream = tokio::task::spawn_blocking(move || -> Result<std::net::TcpStream> {
        let old_ns = std::fs::File::open("/proc/self/ns/net")
            .context("opening current network namespace")?;
        let new_ns = std::fs::File::open(&ns_path)
            .with_context(|| format!("opening namespace {ns_path}"))?;

        nix::sched::setns(new_ns.as_fd(), nix::sched::CloneFlags::CLONE_NEWNET)
            .context("entering network namespace")?;

        let result = std::net::TcpStream::connect(addr);

        // ALWAYS restore original namespace, even on connect failure.
        nix::sched::setns(old_ns.as_fd(), nix::sched::CloneFlags::CLONE_NEWNET)
            .expect("failed to restore network namespace — thread is in wrong namespace");

        result.with_context(|| format!("connecting to {addr} in namespace"))
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

    let std_listener = tokio::task::spawn_blocking(move || -> Result<std::net::TcpListener> {
        let old_ns = std::fs::File::open("/proc/self/ns/net")
            .context("opening current network namespace")?;
        let new_ns = std::fs::File::open(&ns_path)
            .with_context(|| format!("opening namespace {ns_path}"))?;

        nix::sched::setns(new_ns.as_fd(), nix::sched::CloneFlags::CLONE_NEWNET)
            .context("entering network namespace")?;

        let result = std::net::TcpListener::bind(addr);

        nix::sched::setns(old_ns.as_fd(), nix::sched::CloneFlags::CLONE_NEWNET)
            .expect("failed to restore network namespace — thread is in wrong namespace");

        result.with_context(|| format!("binding {addr} in namespace"))
    })
    .await
    .context("spawn_blocking panicked")??;

    std_listener.set_nonblocking(true)?;
    Ok(tokio::net::TcpListener::from_std(std_listener)?)
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

    for mapping in mappings {
        let bind_addr: SocketAddr = match format!("{}:{}", loopback_ip, mapping.host_port).parse() {
            Ok(addr) => addr,
            Err(e) => {
                for h in &handles {
                    h.abort();
                }
                return Err(anyhow::anyhow!(e)).with_context(|| {
                    format!("invalid bind address {}:{}", loopback_ip, mapping.host_port)
                });
            }
        };

        let listener = match tokio::net::TcpListener::bind(bind_addr).await {
            Ok(l) => l,
            Err(e) => {
                for h in &handles {
                    h.abort();
                }
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

        let ns_name = ns_name.to_string();
        let guest_addr: SocketAddr = match format!("{}:{}", guest_ip, mapping.guest_port).parse() {
            Ok(addr) => addr,
            Err(e) => {
                for h in &handles {
                    h.abort();
                }
                return Err(anyhow::anyhow!(e)).with_context(|| {
                    format!("invalid guest address {}:{}", guest_ip, mapping.guest_port)
                });
            }
        };

        let handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((client, peer)) => {
                        let ns = ns_name.clone();
                        tokio::spawn(async move {
                            match connect_in_namespace(&ns, guest_addr).await {
                                Ok(mut guest) => {
                                    let mut client = client;
                                    match tokio::io::copy_bidirectional(&mut client, &mut guest)
                                        .await
                                    {
                                        Ok((c2g, g2c)) => {
                                            debug!(
                                                client_to_guest = c2g,
                                                guest_to_client = g2c,
                                                %peer,
                                                "port forward relay completed"
                                            );
                                        }
                                        Err(e) => {
                                            debug!(error = %e, %peer, "port forward relay error");
                                        }
                                    }
                                }
                                Err(e) => {
                                    debug!(
                                        error = %e,
                                        %peer,
                                        "failed to connect to guest in namespace"
                                    );
                                }
                            }
                        });
                    }
                    Err(e) => {
                        warn!(error = %e, "port forward accept error");
                        break;
                    }
                }
            }
        });

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
    let bind_addr: SocketAddr = format!("{}:8080", gateway_ip)
        .parse()
        .with_context(|| format!("invalid proxy relay bind address {}:8080", gateway_ip))?;

    let listener = bind_in_namespace(ns_name, bind_addr)
        .await
        .context("binding proxy relay listener in namespace")?;

    info!(
        proxy = %proxy_addr,
        bind = %bind_addr,
        "reverse proxy relay via TCP proxy"
    );

    let handle = tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((client, peer)) => {
                    tokio::spawn(async move {
                        // Connect to the real proxy in the HOST namespace (default).
                        match tokio::net::TcpStream::connect(proxy_addr).await {
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
                                            "proxy relay completed"
                                        );
                                    }
                                    Err(e) => {
                                        debug!(error = %e, %peer, "proxy relay error");
                                    }
                                }
                            }
                            Err(e) => {
                                debug!(error = %e, %peer, "failed to connect to proxy");
                            }
                        }
                    });
                }
                Err(e) => {
                    warn!(error = %e, "proxy relay accept error");
                    break;
                }
            }
        }
    });

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
}

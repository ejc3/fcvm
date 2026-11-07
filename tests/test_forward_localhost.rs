//! Test --forward-localhost flag: VM localhost reaches host services.
//!
//! Starts a TCP server on host 127.0.0.1, runs a VM with --forward-localhost
//! that connects to localhost:port from inside the container.

#![cfg(feature = "integration-fast")]

mod common;

use anyhow::{Context, Result};
use std::io::Write;
use std::net::TcpListener;

/// A host service for one test: its port, the greeting only it sends, the accept
/// thread, and, for a service on ::1, the IPv4 loopback socket that holds the same
/// port.
struct HostServer {
    port: u16,
    greeting: String,
    accept: std::thread::JoinHandle<bool>,
    /// Bound and not listening, so a connection to 127.0.0.1 on the port is refused
    /// for as long as the test runs: the relay, which tries 127.0.0.1 first, must reach
    /// the ::1 service and nothing another test or the host runs on that port.
    _ipv4_reservation: Option<std::os::fd::OwnedFd>,
}

/// Bind `host` on a port nobody uses on 127.0.0.1 either, when `host` is ::1.
fn bind_host_listener(
    host: std::net::IpAddr,
) -> Result<(TcpListener, Option<std::os::fd::OwnedFd>)> {
    use nix::sys::socket::{bind, socket, AddressFamily, SockFlag, SockType, SockaddrIn};
    if !host.is_ipv6() {
        return Ok((TcpListener::bind((host, 0))?, None));
    }
    for _ in 0..100 {
        let listener = TcpListener::bind((host, 0))?;
        let port = listener.local_addr()?.port();
        let reservation = socket(
            AddressFamily::Inet,
            SockType::Stream,
            SockFlag::SOCK_CLOEXEC,
            None,
        )?;
        let ipv4 = SockaddrIn::from(std::net::SocketAddrV4::new(
            std::net::Ipv4Addr::LOCALHOST,
            port,
        ));
        if bind(std::os::fd::AsRawFd::as_raw_fd(&reservation), &ipv4).is_ok() {
            return Ok((listener, Some(reservation)));
        }
    }
    anyhow::bail!("no port free on both ::1 and 127.0.0.1")
}

/// Start a TCP server on host loopback address `host` that accepts one connection and
/// replies with a greeting no other server sends.
fn spawn_host_server(host: std::net::IpAddr) -> Result<HostServer> {
    // Only reachable via loopback.
    let (listener, reservation) = bind_host_listener(host)?;
    let port = listener.local_addr()?.port();
    let greeting = format!("HELLO_FROM_HOST_{}_{port}", std::process::id());
    println!("  Host server on {}", listener.local_addr()?);

    // Accept one connection in background (with timeout)
    let reply = format!("{greeting}\n");
    let accept_handle = std::thread::spawn(move || -> bool {
        listener.set_nonblocking(false).expect("set_nonblocking");
        // Accept timeout must cover a full cold VM boot plus image export under
        // parallel CI load (minutes, see the 600s nextest slow-timeout) and nc's
        // 30s timeout — not just nc. If it expires the listener closes and the
        // relay's later connect is refused, failing the test even though
        // forwarding works.
        unsafe {
            let tv = libc::timeval {
                tv_sec: 570,
                tv_usec: 0,
            };
            libc::setsockopt(
                std::os::unix::io::AsRawFd::as_raw_fd(&listener),
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                &tv as *const _ as *const libc::c_void,
                std::mem::size_of_val(&tv) as u32,
            );
        }
        match listener.accept() {
            Ok((mut conn, _)) => {
                let _ = conn.write_all(reply.as_bytes());
                true
            }
            Err(_) => false,
        }
    });

    Ok(HostServer {
        port,
        greeting,
        accept: accept_handle,
        _ipv4_reservation: reservation,
    })
}

/// A service on ::1 holds its port on 127.0.0.1 too, so the relay's first try there is
/// refused rather than reaching whatever else listens on that port.
#[test]
fn an_ipv6_loopback_service_holds_its_port_on_ipv4_loopback() {
    let server = spawn_host_server(std::net::Ipv6Addr::LOCALHOST.into()).unwrap();
    assert!(
        TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, server.port)).is_err(),
        "127.0.0.1:{} is free beside the ::1 service",
        server.port
    );
    let _ = std::net::TcpStream::connect((std::net::Ipv6Addr::LOCALHOST, server.port));
    let _ = server.accept.join();
}

/// Run a VM with --forward-localhost that connects to localhost:port from the
/// container, and assert the host server's greeting reaches the container.
async fn run_forward_localhost_case(extra_args: &[&str], name_prefix: &str) -> Result<()> {
    run_forward_localhost_case_to(
        std::net::Ipv4Addr::LOCALHOST.into(),
        extra_args,
        name_prefix,
    )
    .await
}

async fn run_forward_localhost_case_to(
    host: std::net::IpAddr,
    extra_args: &[&str],
    name_prefix: &str,
) -> Result<()> {
    let server = spawn_host_server(host)?;
    let port = server.port;

    let port_str = port.to_string();
    let (vm_name, _, _, _) = common::unique_names(name_prefix);

    // Run container command that connects to localhost:port
    // This matches the exact manual test that works
    let fcvm_path = common::find_fcvm_binary()?;
    let mut args = vec![
        "podman",
        "run",
        "--name",
        &vm_name,
        "--forward-localhost",
        &port_str,
        "--no-snapshot",
    ];
    args.extend_from_slice(extra_args);
    let cmd = format!("nc -w30 127.0.0.1 {} 2>&1 || echo FAILED", port);
    args.extend_from_slice(&[common::TEST_IMAGE, "--", "sh", "-c", &cmd]);

    let output = tokio::process::Command::new(&fcvm_path)
        .args(&args)
        .output()
        .await
        .context("running fcvm")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    println!("  stdout: {}", stdout.trim());

    let accepted = if server.accept.is_finished() {
        server.accept.join().unwrap_or(false)
    } else {
        // fcvm exited without the container ever connecting (boot failure or a
        // forwarding regression). Unblock the accept thread with a local
        // connection so the test reports promptly instead of waiting out the
        // accept timeout.
        let _ = std::net::TcpStream::connect((host, port));
        let _ = server.accept.join();
        false
    };
    println!("  server accepted: {}", accepted);

    assert!(
        stdout.contains(&server.greeting),
        "VM localhost should reach host (got stdout={}, stderr={})",
        stdout.trim(),
        &stderr[..std::cmp::min(200, stderr.len())]
    );

    Ok(())
}

/// Test that --forward-localhost makes container's 127.0.0.1 reach host services.
#[tokio::test]
async fn test_forward_localhost() -> Result<()> {
    println!("\nTest --forward-localhost");
    println!("========================");

    run_forward_localhost_case(&[], "fwd-localhost").await?;

    println!("✅ FORWARD LOCALHOST TEST PASSED!");
    Ok(())
}

/// A host service listening on ::1 alone (as a local test database can) is reached
/// through --forward-localhost in rootless mode: pasta maps the guest's IPv4 gateway to
/// the host's 127.0.0.1 and its IPv6 gateway to the host's ::1, and the guest's relay
/// tries the second when the first refuses. On a host without a global IPv6 address
/// (the CI container) pasta runs IPv6 in local mode, so the guest still has its IPv6
/// gateway (#1026).
#[tokio::test]
async fn test_forward_localhost_reaches_a_host_service_on_ipv6_loopback_only() -> Result<()> {
    run_forward_localhost_case_to(
        std::net::Ipv6Addr::LOCALHOST.into(),
        &[],
        "fwd-localhost-v6",
    )
    .await
}

/// Test --forward-localhost with routed networking.
///
/// Routed mode has no pasta gateway mapping: fcvm assigns 10.0.2.2 to the
/// namespace bridge and relays connections to the host's loopback via the
/// built-in TCP proxy. Requires root (network namespaces, veth pairs).
#[cfg(feature = "privileged-tests")]
#[tokio::test]
async fn test_forward_localhost_routed() -> Result<()> {
    println!("\nTest --forward-localhost (routed)");
    println!("=================================");

    run_forward_localhost_case(&["--network", "routed"], "fwd-localhost-routed").await?;

    println!("✅ FORWARD LOCALHOST ROUTED TEST PASSED!");
    Ok(())
}

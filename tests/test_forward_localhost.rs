//! Test --forward-localhost flag: VM localhost reaches host services.
//!
//! Starts a TCP server on host 127.0.0.1, runs a VM with --forward-localhost
//! that connects to localhost:port from inside the container.

#![cfg(feature = "integration-fast")]

mod common;

use anyhow::{Context, Result};
use std::io::Write;
use std::net::TcpListener;

/// Start a TCP server on host 127.0.0.1 that accepts one connection and replies
/// with a greeting. Returns the bound port and the accept thread handle.
fn spawn_host_server() -> Result<(u16, std::thread::JoinHandle<bool>)> {
    // Start a TCP server on host 127.0.0.1 (only reachable via loopback)
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    println!("  Host server on 127.0.0.1:{}", port);

    // Accept one connection in background (with timeout)
    let accept_handle = std::thread::spawn(move || -> bool {
        listener.set_nonblocking(false).expect("set_nonblocking");
        // 45s accept timeout (must exceed nc timeout inside VM)
        unsafe {
            let tv = libc::timeval {
                tv_sec: 45,
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
                let _ = conn.write_all(b"HELLO_FROM_HOST\n");
                true
            }
            Err(_) => false,
        }
    });

    Ok((port, accept_handle))
}

/// Run a VM with --forward-localhost that connects to localhost:port from the
/// container, and assert the host server's greeting reaches the container.
async fn run_forward_localhost_case(extra_args: &[&str], name_prefix: &str) -> Result<()> {
    let (port, accept_handle) = spawn_host_server()?;

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

    let accepted = accept_handle.join().unwrap_or(false);
    println!("  server accepted: {}", accepted);

    assert!(
        stdout.contains("HELLO_FROM_HOST"),
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

/// Test --forward-localhost with routed networking.
///
/// Routed mode has no pasta gateway mapping: fcvm assigns 10.0.2.2 to the
/// namespace bridge and relays connections to the host's 127.0.0.1 via the
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

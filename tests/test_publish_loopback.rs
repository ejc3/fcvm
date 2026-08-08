//! A published port must reach a guest service bound to 127.0.0.1 ONLY.
//!
//! `--publish` delivers the packet to the guest's external interface, so a
//! service on `0.0.0.0` was reachable and a service on `127.0.0.1` was not, with
//! no way to bridge the gap: `--forward-localhost` runs guest -> host, so aiming
//! it at such a port hijacks the guest's own listener instead of exposing it.
//!
//! That gap is why the Chromium benchmark had to run a userspace relay inside
//! every clone — Chromium ignores `--remote-debugging-address` and binds
//! `127.0.0.1:9222` regardless (measured on 151.0.7922.71: the flag is present in
//! `/proc/<pid>/cmdline` while `/proc/net/tcp` shows `0100007F:2406` and nothing
//! else). The relay was an extra process in the byte path, on the one arm that
//! dropped connections.
//!
//! RED WITHOUT THE FIX: the listener below binds `127.0.0.1` only — asserted
//! against the guest's own `/proc/net/tcp` before the host ever connects, so a
//! pass cannot come from a wildcard bind that would have worked anyway. Without
//! `fc-agent::network::publish_to_loopback` the host connect is refused.

// The entire file is privileged-only. Gating just the test fn leaves the
// imports and GUEST_PORT unused in the default build, which `-D warnings`
// rejects.
#![cfg(feature = "privileged-tests")]

mod common;

use anyhow::{Context, Result};
use std::process::Command;
use std::time::Duration;

/// Guest port the listener binds on 127.0.0.1. Deliberately not a port any other
/// test publishes, so a stray VM cannot answer for us.
const GUEST_PORT: u16 = 9231;

#[test]
fn test_published_port_reaches_a_loopback_only_listener() -> Result<()> {
    println!("\ntest_published_port_reaches_a_loopback_only_listener");

    let fcvm_path = common::find_fcvm_binary()?;
    let vm_name = format!("publoop-{}", std::process::id());
    let host_port = common::find_available_high_port()?;

    // busybox nc, loopback-bound, answering a fixed body. `-s 127.0.0.1` is the
    // whole point: a wildcard bind would be reachable without the feature.
    let guest_cmd = format!(
        "while true; do printf 'HTTP/1.1 200 OK\\r\\nContent-Length: 12\\r\\n\\r\\nLOOPBACK_OK' \
         | nc -l -s 127.0.0.1 -p {GUEST_PORT}; done"
    );

    let mut cmd = Command::new(&fcvm_path);
    cmd.args([
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "bridged",
        "--publish",
        &format!("{host_port}:{GUEST_PORT}"),
        common::TEST_IMAGE,
        "sh",
        "-c",
        &guest_cmd,
    ]);
    common::set_test_pdeathsig_std(&mut cmd);
    let mut fcvm = cmd.spawn().context("spawning fcvm")?;
    let fcvm_pid = fcvm.id();

    let result = (|| -> Result<()> {
        // Wait for the VM itself; the container command is a bare listener, so
        // health is process-level rather than an HTTP probe.
        let start = std::time::Instant::now();
        let mut healthy = false;
        while start.elapsed() < Duration::from_secs(180) {
            std::thread::sleep(common::POLL_INTERVAL);
            let out = Command::new(&fcvm_path)
                .args(["ls", "--json", "--pid", &fcvm_pid.to_string()])
                .output()?;
            if out.status.success()
                && String::from_utf8_lossy(&out.stdout).contains("\"health_status\"")
            {
                healthy = true;
                break;
            }
        }
        anyhow::ensure!(healthy, "VM never reported a health status within 180s");

        // The listener must be loopback-only, or this test proves nothing.
        // /proc/net/tcp encodes the local address little-endian hex:
        // 127.0.0.1 -> 0100007F, 0.0.0.0 -> 00000000.
        let tcp = Command::new(&fcvm_path)
            .args([
                "exec",
                "--pid",
                &fcvm_pid.to_string(),
                "--",
                "cat",
                "/proc/net/tcp",
            ])
            .output()
            .context("reading guest /proc/net/tcp")?;
        let tcp = String::from_utf8_lossy(&tcp.stdout).to_uppercase();
        let loopback_entry = format!("0100007F:{:04X}", GUEST_PORT);
        let wildcard_entry = format!("00000000:{:04X}", GUEST_PORT);

        anyhow::ensure!(
            tcp.contains(&loopback_entry),
            "guest has no listener on 127.0.0.1:{GUEST_PORT} ({loopback_entry} absent from \
             /proc/net/tcp). The fixture is wrong, and the assertion below would be \
             meaningless:\n{tcp}"
        );
        anyhow::ensure!(
            !tcp.contains(&wildcard_entry),
            "guest listener is bound to 0.0.0.0 ({wildcard_entry} present). A wildcard bind is \
             reachable WITHOUT publish_to_loopback, so this test would pass with the feature \
             reverted:\n{tcp}"
        );

        // The claim under test. Assert on the BODY, not just a 200: a proxy or a
        // stray listener could answer without the packet ever reaching the guest's
        // loopback service, and the marker only exists inside that service.
        let curl = Command::new("curl")
            .args([
                "-sS",
                "--max-time",
                "15",
                &format!("http://127.0.0.1:{host_port}/"),
            ])
            .output()
            .context("running curl against the published port")?;
        let body = String::from_utf8_lossy(&curl.stdout).to_string();
        let err = String::from_utf8_lossy(&curl.stderr).to_string();

        anyhow::ensure!(
            body.contains("LOOPBACK_OK"),
            "host connect to published port {host_port} did not reach the guest's \
             127.0.0.1:{GUEST_PORT} listener. fc-agent must DNAT every published port to \
             loopback and enable route_localnet \
             (fc-agent/src/network.rs::publish_to_loopback); without it a loopback-only guest \
             service is unreachable no matter what is published. curl stdout={body:?} \
             stderr={err:?}"
        );
        Ok(())
    })();

    tokio::runtime::Runtime::new()?.block_on(common::kill_process(fcvm_pid));
    let _ = fcvm.wait();
    result
}

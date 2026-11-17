//! A rootless guest runs eth0 at the namespace links' MTU (`pasta::GUEST_MTU`), when it
//! boots and in a clone restored from its snapshot (the source is set back to 1500 before
//! the snapshot, so the clone's MTU comes from its restore document), and a bulk transfer
//! from a host service arrives whole at that MTU.
//!
//! At 1500 a freshly restored web-server clone took 45,000 to 80,000 frames a second on one
//! vCPU while its daemons pulled about 1 GB from host services, and its TAP dropped 75
//! to 86 percent of the frames pasta sent, the ARP and TCP probes of fcvm's own
//! reachability check among them (2026-11-16).

#![cfg(feature = "integration-fast")]

mod common;

use anyhow::{Context, Result};
use std::io::Write;
use std::net::TcpListener;

/// What the guest is sent per connection: enough 64 KiB frames that a transfer which
/// stalls or truncates shows as a short count.
const BULK_BYTES: usize = 64 << 20;

/// A host service on 127.0.0.1, reached from the guest as 10.0.2.2 through pasta, that
/// sends `BULK_BYTES` zero bytes to each connection and closes it. The accept thread is
/// left to the process: a test that fails before connecting must not hang on it.
fn spawn_bulk_server() -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").context("binding the bulk server")?;
    let port = listener.local_addr()?.port();
    std::thread::spawn(move || {
        let chunk = vec![0u8; 1 << 20];
        for mut connection in listener.incoming().flatten() {
            let mut left = BULK_BYTES;
            while left > 0 {
                let n = left.min(chunk.len());
                if connection.write_all(&chunk[..n]).is_err() {
                    break;
                }
                left -= n;
            }
        }
    });
    Ok(port)
}

/// eth0's MTU in the guest, and the byte count of one transfer from the bulk server.
async fn guest_mtu_and_bulk_count(pid: u32, port: u16) -> Result<(String, String)> {
    let mtu = common::exec_in_vm(pid, &["cat /sys/class/net/eth0/mtu"]).await?;
    let count = common::exec_in_vm(
        pid,
        &[&format!("nc -w 30 10.0.2.2 {port} < /dev/null | wc -c")],
    )
    .await?;
    Ok((mtu.trim().to_string(), count.trim().to_string()))
}

/// Boot a rootless guest and check it, set its eth0 back to 1500, snapshot it and
/// check a clone of the snapshot: the clone must come up at the namespace MTU from the
/// restore document, not inherit the source's 1500. Returns the booted guest's and the
/// clone's (MTU, bytes) and the source's MTU at the snapshot. Every process it starts
/// goes into `procs`, which the caller stops whatever happens.
async fn boot_then_restore(
    procs: &mut Vec<(tokio::process::Child, u32)>,
    port: u16,
) -> Result<([(String, String); 2], String)> {
    let (vm, clone, snapshot, _) = common::unique_names("guest-mtu");
    let (child, pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm,
        "--network",
        "rootless",
        common::TEST_IMAGE,
    ])
    .await?;
    procs.push((child, pid));
    common::poll_health_by_pid(pid, 180).await?;
    let booted = guest_mtu_and_bulk_count(pid, port).await?;
    let at_snapshot = common::exec_in_vm(
        pid,
        &["/usr/sbin/ip link set eth0 mtu 1500 && cat /sys/class/net/eth0/mtu"],
    )
    .await?
    .trim()
    .to_string();

    common::create_snapshot_by_pid(pid, &snapshot).await?;
    let (serve, serve_pid) = common::start_memory_server(&snapshot).await?;
    procs.push((serve, serve_pid));
    let (clone_child, clone_pid) = common::spawn_clone(serve_pid, &clone).await?;
    procs.push((clone_child, clone_pid));
    common::poll_health_by_pid(clone_pid, 120).await?;
    let restored = guest_mtu_and_bulk_count(clone_pid, port).await?;
    Ok(([booted, restored], at_snapshot))
}

#[tokio::test]
async fn test_rootless_guest_and_its_snapshot_clone_run_eth0_at_the_namespace_mtu() -> Result<()> {
    let port = spawn_bulk_server()?;
    let mut procs = Vec::new();
    let result = boot_then_restore(&mut procs, port).await;
    for (mut child, pid) in procs.into_iter().rev() {
        common::kill_process(pid).await;
        let _ = child.wait().await;
    }
    let ([booted, restored], at_snapshot) = result?;
    assert_eq!(
        at_snapshot, "1500",
        "the source's eth0 MTU when it was snapshotted"
    );
    let want = (
        fcvm::network::pasta::GUEST_MTU.to_string(),
        BULK_BYTES.to_string(),
    );
    assert_eq!(
        booted, want,
        "(eth0 MTU, bytes received) in the booted guest"
    );
    assert_eq!(
        restored, want,
        "(eth0 MTU, bytes received) in the restored clone"
    );
    Ok(())
}

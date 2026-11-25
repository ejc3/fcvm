//! The snapshot gate's TCP rule reads the packet, not connection tracking (#2106).
//!
//! Both tests run a routed VM, where nothing but the gate ever attached connection
//! tracking in the guest: no egress proxy and no published port puts a nat rule there.

#![cfg(feature = "integration-slow")]
#![cfg(feature = "privileged-tests")]

mod common;

use anyhow::{Context, Result};
use std::net::{Ipv6Addr, SocketAddr};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const IP6TABLES: &str = "/usr/sbin/ip6tables";

/// The address a routed VM's state records for its guest.
async fn guest_address(pid: u32) -> Result<Ipv6Addr> {
    let recorded = common::get_network_field(pid, "guest_ipv6").await?;
    recorded
        .trim_end_matches("/128")
        .parse()
        .with_context(|| format!("the VM's guest_ipv6 is {recorded:?}"))
}

/// Connect to `target` from local port `port` (0 for one the kernel picks).
async fn connect_from(port: u16, target: SocketAddr) -> std::io::Result<tokio::net::TcpStream> {
    let socket = tokio::net::TcpSocket::new_v6()?;
    socket.set_reuseaddr(true)?;
    socket.bind(SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), port))?;
    tokio::time::timeout(Duration::from_secs(3), socket.connect(target))
        .await
        .unwrap_or_else(|_| Err(std::io::ErrorKind::TimedOut.into()))
}

/// The guest's tracked connections with what time changes left out (the countdown in
/// the fifth column), sorted. Empty when the guest tracks nothing or never loaded the
/// tracker.
async fn tracked(pid: u32) -> Result<String> {
    common::exec_in_vm(
        pid,
        &["cat /proc/net/nf_conntrack 2>/dev/null | awk '{$5=\"\"; print}' | sort"],
    )
    .await
}

/// Wait, up to two minutes, until the log at `path` contains `marker`.
async fn wait_for_log(path: &std::path::Path, marker: &str) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(120);
    while !std::fs::read_to_string(path)
        .unwrap_or_default()
        .contains(marker)
    {
        anyhow::ensure!(
            Instant::now() < deadline,
            "{} never logged {marker:?}",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Ok(())
}

/// Leave the guest tracking a connection from the host's port `port` to its port 80 as
/// ESTABLISHED although the connection is over: attach connection tracking with a rule
/// that uses it, connect and hold the connection, delete the rule, and end the
/// connection once the tracker has stopped watching. The entry's state says whether the
/// end went unseen; when it was seen, try again.
async fn leave_a_stale_established_entry(pid: u32, target: SocketAddr) -> Result<u16> {
    let rule = "INPUT -m conntrack --ctstate ESTABLISHED -j ACCEPT";
    for _ in 0..10 {
        common::exec_in_vm(pid, &[&format!("{IP6TABLES} -w -I {rule}")]).await?;
        let held = connect_from(0, target).await;
        let removed = common::exec_in_vm(pid, &[&format!("{IP6TABLES} -w -D {rule}")]).await;
        let held = held.context("connecting to the guest's port 80")?;
        removed?;
        let port = held.local_addr()?.port();
        // The rule's deletion detaches the tracker a moment after it returns.
        tokio::time::sleep(Duration::from_millis(300)).await;
        // A reset, so the host keeps no TIME-WAIT socket on this port.
        held.set_zero_linger()?;
        drop(held);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let entry = tracked(pid)
            .await?
            .lines()
            .find(|line| line.contains(&format!("sport={port} dport=80")))
            .map(str::to_string);
        if entry.is_some_and(|line| line.contains(" ESTABLISHED ")) {
            return Ok(port);
        }
    }
    anyhow::bail!("ten connections all ended while the guest still tracked them")
}

/// #2106: a restored clone's closed gate refuses a connection from a tuple its tracker
/// holds as ESTABLISHED, as it refuses one from a fresh port.
///
/// The snapshot's guest tracks a finished connection from one host port as ESTABLISHED
/// (`leave_a_stale_established_entry`). The clone is given the same address and is held
/// before it verifies its inherited gate (`restore.pre_gate_verify`), so the gate is
/// closed for the whole check. A fresh port is the control: it must be refused, or the
/// clone was not reachable and nothing was tested.
///
/// RED BEFORE THE FIX: the gate's rule was `--ctstate NEW`, a SYN on the tracked tuple
/// was not NEW, and the connection from the stale port was accepted.
#[tokio::test]
async fn a_stale_tracked_tuple_does_not_pass_a_restored_clones_closed_gate() -> Result<()> {
    let (golden_name, clone_name, snapshot_name, _) = common::unique_names("staletuple");
    let (mut golden_child, golden_pid, _golden_log) = common::spawn_fcvm_with_env_and_log_path(
        &[
            "podman",
            "run",
            "--name",
            &golden_name,
            "--network",
            "routed",
            "--health-check",
            "http://localhost:80",
            common::TEST_IMAGE,
        ],
        &[
            (
                "FCVM_GUEST_FAILPOINT",
                "restore.pre_gate_verify:sleep:15000",
            ),
            ("FCVM_NO_SNAPSHOT", "1"),
        ],
    )
    .await
    .context("spawning the golden VM with the gate hold armed")?;
    let prepared: Result<(Ipv6Addr, u16)> = async {
        common::poll_health_by_pid(golden_pid, 180).await?;
        let address = guest_address(golden_pid).await?;
        let stale = leave_a_stale_established_entry(golden_pid, (address, 80).into()).await?;
        common::create_snapshot_by_pid(golden_pid, &snapshot_name).await?;
        Ok((address, stale))
    }
    .await;
    common::kill_process(golden_pid).await;
    let _ = golden_child.wait().await;
    let (address, stale) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            let _ = common::delete_snapshot(&snapshot_name).await;
            return Err(error);
        }
    };
    let target: SocketAddr = (address, 80).into();

    let (mut clone_child, clone_pid, clone_log) = common::spawn_fcvm_with_env_and_log_path(
        &[
            "snapshot",
            "run",
            "--snapshot",
            &snapshot_name,
            "--name",
            &clone_name,
            "--ipv6-address",
            &address.to_string(),
        ],
        &[],
    )
    .await
    .context("spawning the restored clone at the golden's address")?;
    let checked: Result<()> = async {
        wait_for_log(&clone_log, "FAILPOINT restore.pre_gate_verify reached").await?;
        // The control, retried while the host learns the clone's link address.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match connect_from(0, target).await {
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => break,
                Ok(_) => anyhow::bail!("a fresh port connected to the clone before publication"),
                Err(error) => anyhow::ensure!(
                    Instant::now() < deadline,
                    "the held clone never refused a fresh port, so its gate was not reached: \
                     {error}"
                ),
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let from_stale = connect_from(stale, target).await;
        let log = std::fs::read_to_string(&clone_log).unwrap_or_default();
        anyhow::ensure!(
            !log.contains("FAILPOINT restore.pre_gate_verify released"),
            "the hold ended before the stale port was tried, so nothing was tested"
        );
        match from_stale {
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {}
            Ok(_) => anyhow::bail!(
                "a connection from port {stale}, which the guest tracks as ESTABLISHED, was \
                 accepted through the clone's closed gate"
            ),
            Err(error) => anyhow::bail!("the stale port was not refused but failed: {error}"),
        }
        // Published, the clone serves both.
        common::poll_health_by_pid(clone_pid, 120).await?;
        connect_from(0, target)
            .await
            .context("a fresh port after publication")?;
        let rules = common::exec_in_vm(clone_pid, &[&format!("{IP6TABLES} -w -S")]).await?;
        anyhow::ensure!(
            !rules.contains("FCVM_SNAPSHOT"),
            "the published clone kept a gate chain:\n{rules}"
        );
        Ok(())
    }
    .await;
    common::kill_process(clone_pid).await;
    let _ = clone_child.wait().await;
    let deleted = common::delete_snapshot(&snapshot_name).await;
    checked?;
    deleted
}

/// A snapshot leaves the source's connection tracking as it found it, and a connection
/// that carries data through the snapshot is not reset.
///
/// One HTTP connection to the guest's nginx sends a request every 25 ms from before
/// `snapshot create` until after it returns, so segments of an open connection arrive
/// while the gate is closed, the resumed guest's backlog among them. The guest's
/// tracked connections are compared across the snapshot.
///
/// RED BEFORE THE FIX: closing the gate attached the tracker (its rule was
/// `-m conntrack`), the connection's next segment was the first the tracker saw of it,
/// which is NEW, and the gate reset it; the tracker's table held entries it did not hold
/// before.
#[tokio::test]
async fn a_snapshot_leaves_the_sources_tracked_connections_as_it_found_them() -> Result<()> {
    let (vm_name, _, snapshot_name, _) = common::unique_names("snapconn");
    let (mut child, pid) = common::spawn_fcvm_with_logs(
        &[
            "podman",
            "run",
            "--name",
            &vm_name,
            "--network",
            "routed",
            "--no-snapshot",
            "--health-check",
            "http://localhost:80",
            common::TEST_IMAGE,
        ],
        &vm_name,
    )
    .await
    .context("spawning the source VM")?;
    let outcome: Result<()> = async {
        common::poll_health_by_pid(pid, 180).await?;
        let target: SocketAddr = (guest_address(pid).await?, 80).into();
        let mut connection = connect_from(0, target)
            .await
            .context("connecting to the guest's nginx")?;
        let (stop, mut stopped) = tokio::sync::oneshot::channel::<()>();
        // One keep-alive connection; every answer is read to its end before the next
        // request, and any error ends the task with it.
        let traffic = tokio::spawn(async move {
            let mut answered = 0u32;
            let mut buffer = vec![0u8; 16 * 1024];
            loop {
                // nginx closes a connection after 1000 requests: stay under that
                if stopped.try_recv().is_ok() || answered == 900 {
                    return Ok::<u32, anyhow::Error>(answered);
                }
                connection
                    .write_all(b"HEAD / HTTP/1.1\r\nHost: guest\r\n\r\n")
                    .await
                    .with_context(|| format!("request {answered}"))?;
                let mut answer = Vec::new();
                while !answer.ends_with(b"\r\n\r\n") {
                    let read = tokio::time::timeout(
                        Duration::from_secs(30),
                        connection.read(&mut buffer),
                    )
                    .await
                    .with_context(|| format!("no answer to request {answered} in 30 s"))?
                    .with_context(|| format!("reading the answer to request {answered}"))?;
                    anyhow::ensure!(read > 0, "the guest closed the connection at {answered}");
                    answer.extend_from_slice(&buffer[..read]);
                }
                answered += 1;
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        let before = tracked(pid).await?;
        let created = common::create_snapshot_by_pid(pid, &snapshot_name).await;
        // The source has resumed; let the connection run on past its reopened gate.
        tokio::time::sleep(Duration::from_secs(2)).await;
        let after = tracked(pid).await;
        let _ = stop.send(());
        let answered = traffic.await.context("the traffic task")?;
        created?;
        let answered = answered.context(
            "the connection that carried data through the snapshot did not survive it",
        )?;
        anyhow::ensure!(
            (20..900).contains(&answered),
            "{answered} requests were answered: the connection did not span the snapshot"
        );
        let after = after?;
        anyhow::ensure!(
            before == after,
            "the snapshot changed the source's tracked connections.\nbefore:\n{before}\nafter:\n{after}"
        );
        let rules = common::exec_in_vm(pid, &[&format!("{IP6TABLES} -w -S")]).await?;
        anyhow::ensure!(
            !rules.contains("FCVM_SNAPSHOT"),
            "the resumed source kept a gate chain:\n{rules}"
        );
        Ok(())
    }
    .await;
    common::kill_process(pid).await;
    let _ = child.wait().await;
    let deleted = common::delete_snapshot(&snapshot_name).await;
    outcome?;
    deleted
}

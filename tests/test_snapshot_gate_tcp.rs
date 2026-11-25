//! The snapshot gate's TCP rule reads the packet, not connection tracking (#2106), and
//! what a SYN cookie lets past it does not outlive a restore (#2123).
//!
//! The tests run a routed VM, where nothing but the gate ever attached connection
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

const SYN: u8 = 0x02;
const RST: u8 = 0x04;
const ACK: u8 = 0x10;

/// TCP segments written by hand to one peer, over a raw IPv6 socket: a client that can
/// hold its handshake's last ACK back, which a kernel socket never does.
struct RawTcp(std::os::fd::OwnedFd);

impl RawTcp {
    fn open(peer: Ipv6Addr) -> std::io::Result<RawTcp> {
        use std::os::fd::{AsRawFd, FromRawFd};
        let check = |result: libc::c_int| match result {
            -1 => Err(std::io::Error::last_os_error()),
            _ => Ok(()),
        };
        // SAFETY: plain socket calls on a descriptor this function owns, with pointers
        // to locals that outlive each call.
        unsafe {
            let fd = libc::socket(libc::AF_INET6, libc::SOCK_RAW, libc::IPPROTO_TCP);
            check(fd)?;
            let socket = RawTcp(std::os::fd::OwnedFd::from_raw_fd(fd));
            let fd = socket.0.as_raw_fd();
            // The kernel fills in the checksum, 16 bytes into the TCP header.
            let offset: libc::c_int = 16;
            check(libc::setsockopt(
                fd,
                libc::IPPROTO_IPV6,
                libc::IPV6_CHECKSUM,
                (&raw const offset).cast(),
                std::mem::size_of_val(&offset) as libc::socklen_t,
            ))?;
            let wait = libc::timeval {
                tv_sec: 2,
                tv_usec: 0,
            };
            check(libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                (&raw const wait).cast(),
                std::mem::size_of_val(&wait) as libc::socklen_t,
            ))?;
            // Connected: the kernel picks the source address, and only the peer's
            // segments are read.
            let mut address: libc::sockaddr_in6 = std::mem::zeroed();
            address.sin6_family = libc::AF_INET6 as libc::sa_family_t;
            address.sin6_addr.s6_addr = peer.octets();
            check(libc::connect(
                fd,
                (&raw const address).cast(),
                std::mem::size_of_val(&address) as libc::socklen_t,
            ))?;
            Ok(socket)
        }
    }

    /// One segment with no data. A SYN carries an MSS option.
    fn send(&self, from: u16, to: u16, seq: u32, ack: u32, flags: u8) -> std::io::Result<()> {
        use std::os::fd::AsRawFd;
        let options: &[u8] = if flags & SYN != 0 {
            &[2, 4, 0x04, 0xc4]
        } else {
            &[]
        };
        let mut segment = Vec::with_capacity(24);
        segment.extend(from.to_be_bytes());
        segment.extend(to.to_be_bytes());
        segment.extend(seq.to_be_bytes());
        segment.extend(ack.to_be_bytes());
        segment.push((((20 + options.len()) / 4) as u8) << 4);
        segment.push(flags);
        segment.extend(64240u16.to_be_bytes());
        segment.extend([0u8; 4]);
        segment.extend(options);
        // SAFETY: the buffer is valid for its length.
        let sent = unsafe {
            libc::send(
                self.0.as_raw_fd(),
                segment.as_ptr().cast(),
                segment.len(),
                0,
            )
        };
        match sent {
            -1 => Err(std::io::Error::last_os_error()),
            _ => Ok(()),
        }
    }

    /// The sequence number and flags of the peer's next segment from its port `from` to
    /// our port `to`, or None when two seconds pass without one.
    fn answer(&self, from: u16, to: u16) -> std::io::Result<Option<(u32, u8)>> {
        use std::os::fd::AsRawFd;
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut segment = [0u8; 128];
        while Instant::now() < deadline {
            // SAFETY: the buffer is valid for its length.
            let read = unsafe {
                libc::recv(
                    self.0.as_raw_fd(),
                    segment.as_mut_ptr().cast(),
                    segment.len(),
                    0,
                )
            };
            if read < 0 {
                let error = std::io::Error::last_os_error();
                return match error.kind() {
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => Ok(None),
                    _ => Err(error),
                };
            }
            let port = |at: usize| u16::from_be_bytes([segment[at], segment[at + 1]]);
            if read >= 20 && port(0) == from && port(2) == to {
                let seq = u32::from_be_bytes([segment[4], segment[5], segment[6], segment[7]]);
                return Ok(Some((seq, segment[13])));
            }
        }
        Ok(None)
    }
}

/// #2123: a listener that answers a SYN with a cookie keeps nothing for that client, and
/// the client's bare ACK, which the gate passes, is the whole handshake to it.
///
/// The golden's listener answers every SYN with a cookie (`tcp_syncookies=2`, what the
/// default setting does once a SYN queue is full). Two clients get a cookie each and hold
/// their ACK. The clone takes the golden's address and has a restore hook that keeps
/// publication eight seconds off.
///
/// - The first ACK arrives while the clone is held before the restore's first step. The
///   listener takes it. The socket it makes is in no manifest, and the restore's own
///   list finds and retires it.
/// - The second arrives after that list, while the hook runs. The clone no longer takes
///   cookies, so the listener answers with a reset, as the gate answers a SYN.
///
/// RED BEFORE THE FIX: the first client's socket is established in the published clone,
/// and the second client's ACK is not reset.
#[tokio::test]
async fn a_syn_cookie_handshake_does_not_reach_a_restored_clone() -> Result<()> {
    const BEFORE_THE_LIST: u16 = 40003;
    const AFTER_THE_LIST: u16 = 40004;
    const ISN: u32 = 5000;
    let (golden_name, clone_name, snapshot_name, _) = common::unique_names("syncookie");
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
            "--on-restore",
            "sleep 8",
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
    let prepared: Result<(Ipv6Addr, [u32; 2])> = async {
        common::poll_health_by_pid(golden_pid, 180).await?;
        let address = guest_address(golden_pid).await?;
        common::exec_in_vm(golden_pid, &["sysctl -qw net.ipv4.tcp_syncookies=2"]).await?;
        let client = RawTcp::open(address).context("opening a raw TCP socket to the guest")?;
        let mut cookies = [0; 2];
        for (cookie, port) in cookies.iter_mut().zip([BEFORE_THE_LIST, AFTER_THE_LIST]) {
            client.send(port, 80, ISN, 0, SYN)?;
            let (seq, flags) = client
                .answer(80, port)?
                .with_context(|| format!("no answer to the SYN from port {port}"))?;
            anyhow::ensure!(
                flags & (SYN | ACK) == SYN | ACK,
                "the SYN from port {port} was answered with flags {flags:#04x}"
            );
            *cookie = seq;
        }
        let sent = common::exec_in_vm(
            golden_pid,
            &["nstat -az TcpExtSyncookiesSent | awk 'END {print $2}'"],
        )
        .await?;
        anyhow::ensure!(
            sent.trim().parse::<u64>().is_ok_and(|sent| sent >= 2),
            "the listener did not answer with cookies (TcpExtSyncookiesSent {sent:?}), so \
             nothing would be tested"
        );
        common::create_snapshot_by_pid(golden_pid, &snapshot_name).await?;
        Ok((address, cookies))
    }
    .await;
    common::kill_process(golden_pid).await;
    let _ = golden_child.wait().await;
    let (address, cookies) = match prepared {
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
        let logged = |marker: &str| {
            std::fs::read_to_string(&clone_log)
                .unwrap_or_default()
                .contains(marker)
        };
        wait_for_log(&clone_log, "FAILPOINT restore.pre_gate_verify reached").await?;
        // The clone is reachable once a fresh port is refused by its gate.
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
        let client = RawTcp::open(address).context("opening a raw TCP socket to the clone")?;
        let ack = |port: u16, cookie: u32| {
            client.send(port, 80, ISN + 1, cookie.wrapping_add(1), ACK)?;
            client.answer(80, port)
        };

        // Before the restore's first step: the listener takes the cookie and says nothing.
        let taken = ack(BEFORE_THE_LIST, cookies[0])?;
        anyhow::ensure!(
            !logged("FAILPOINT restore.pre_gate_verify released"),
            "the hold ended before the first ACK was answered, so nothing was tested"
        );
        anyhow::ensure!(
            taken.is_none(),
            "the held clone's listener did not take the first cookie (it answered with \
             flags {:#04x}), so nothing was tested",
            taken.map_or(0, |(_, flags)| flags)
        );

        // After the restore's list, while the hook keeps publication off.
        wait_for_log(&clone_log, "snapshot network cleanup complete").await?;
        let refused = ack(AFTER_THE_LIST, cookies[1])?;
        anyhow::ensure!(
            !logged("snapshot network published"),
            "the clone was published before the second ACK was answered, so nothing was tested"
        );

        common::poll_health_by_pid(clone_pid, 120).await?;
        let established =
            common::exec_in_vm(clone_pid, &["ss -Htn state established '( sport = :80 )'"]).await?;
        let mut wrong = Vec::new();
        if established.contains(&format!(":{BEFORE_THE_LIST}")) {
            wrong.push(format!(
                "the socket a cookie made behind the closed gate is established in the \
                 published clone:\n{established}"
            ));
        }
        if !matches!(refused, Some((_, flags)) if flags & RST != 0) {
            wrong.push(format!(
                "a cookie's ACK after the restore's list was not reset before publication \
                 (answer: {refused:?})"
            ));
        }
        if established.contains(&format!(":{AFTER_THE_LIST}")) {
            wrong.push(format!(
                "a cookie handshake completed between the restore's list and publication:\n\
                 {established}"
            ));
        }
        anyhow::ensure!(wrong.is_empty(), "{}", wrong.join("\n"));
        // Published, the clone has its own setting back.
        let setting =
            common::exec_in_vm(clone_pid, &["cat /proc/sys/net/ipv4/tcp_syncookies"]).await?;
        anyhow::ensure!(
            setting.trim() == "2",
            "the published clone's tcp_syncookies is {setting:?}, not the 2 it was restored with"
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

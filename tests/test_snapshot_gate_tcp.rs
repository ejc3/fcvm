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

/// Wait for `path` to hold `marker` somewhere after its first `after`. The log is a VM's,
/// and a VM that is gone will log nothing more: fail two seconds after it went (what it
/// wrote last may still be on its way to the file), not at the deadline.
async fn wait_for_log_after(
    path: &std::path::Path,
    vm_pid: u32,
    after: &str,
    marker: &str,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut gone_since: Option<Instant> = None;
    loop {
        if gone_since.is_none() && !std::path::Path::new(&format!("/proc/{vm_pid}")).exists() {
            gone_since = Some(Instant::now());
        }
        let logged = std::fs::read_to_string(path).unwrap_or_default();
        if logged
            .split_once(after)
            .is_some_and(|(_, rest)| rest.contains(marker))
        {
            return Ok(());
        }
        anyhow::ensure!(
            gone_since.is_none_or(|gone| gone.elapsed() < Duration::from_secs(2)),
            "the VM is gone and {} holds no {marker:?} after {after:?}",
            path.display()
        );
        anyhow::ensure!(
            Instant::now() < deadline,
            "{} never logged {marker:?} after {after:?}",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
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

/// A connection the source held from a client of a port published with `/loopback` does
/// not outlive a restore.
///
/// Such a client is on the host, and the guest's nat rules rewrite both its addresses to
/// 127.0.0.1 (`fc-agent/src/network.rs`, `publish_as_loopback_client`), so the server's
/// socket records a loopback peer. The snapshot's manifest kept every socket with a
/// loopback peer out, as a connection inside the guest, and each clone woke with the
/// source's client connections on that port still established. What says a connection is
/// inside the guest is that both its sockets are there.
///
/// RED BEFORE THE FIX: the clone holds the source's connection from the host.
#[tokio::test]
async fn a_loopback_ports_client_connection_does_not_outlive_a_restore() -> Result<()> {
    let (golden_name, clone_name, snapshot_name, _) = common::unique_names("loopclient");
    let host_port = common::find_available_high_port()?;
    let (mut golden_child, golden_pid, _golden_log) = common::spawn_fcvm_with_env_and_log_path(
        &[
            "podman",
            "run",
            "--name",
            &golden_name,
            "--network",
            "routed",
            "--publish",
            &format!("{host_port}:80/loopback"),
            "--health-check",
            "http://localhost:80",
            common::TEST_IMAGE,
        ],
        &[("FCVM_NO_SNAPSHOT", "1")],
    )
    .await
    .context("spawning the golden VM with a /loopback port")?;
    // The server's connections on port 80 with a peer on 127.0.0.1, by the peer's port.
    // TIME-WAIT is left out: what a finished health check leaves behind is no connection.
    let loopback_peers = |listing: &str| -> std::collections::BTreeSet<u16> {
        listing
            .lines()
            .filter_map(|line| line.split_whitespace().last())
            .filter_map(|peer| peer.strip_prefix("127.0.0.1:"))
            .filter_map(|port| port.parse().ok())
            .collect()
    };
    const SERVER_SOCKETS: &str = "ss -Htn state connected exclude time-wait '( sport = :80 )'";
    let mut held = None;
    let prepared: Result<std::collections::BTreeSet<u16>> = async {
        common::poll_health_by_pid(golden_pid, 180).await?;
        let published: std::net::IpAddr = common::get_network_field(golden_pid, "loopback_ip")
            .await?
            .parse()
            .context("the VM's loopback_ip")?;
        let deadline = Instant::now() + Duration::from_secs(30);
        let client = loop {
            match tokio::net::TcpStream::connect((published, host_port)).await {
                Ok(client) => break client,
                Err(error) => anyhow::ensure!(
                    Instant::now() < deadline,
                    "connecting to the published port: {error}"
                ),
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        // The connection the test holds is the one that is there both times; a health
        // check's comes and goes.
        let mut ours = None;
        for _ in 0..50 {
            let first = loopback_peers(&common::exec_in_vm(golden_pid, &[SERVER_SOCKETS]).await?);
            tokio::time::sleep(Duration::from_millis(1500)).await;
            let second = loopback_peers(&common::exec_in_vm(golden_pid, &[SERVER_SOCKETS]).await?);
            let both: std::collections::BTreeSet<u16> =
                first.intersection(&second).copied().collect();
            if !both.is_empty() {
                ours = Some(both);
                break;
            }
        }
        let ours = ours.context(
            "the source holds no connection from 127.0.0.1 on port 80, so the /loopback port \
             did not rewrite its client and nothing would be tested",
        )?;
        // The rewrite gives the client a port of its reserved range, above the ports the
        // guest gives its own connections (#2152). With the client's own port kept, a
        // client could make its socket the mirror image of itself.
        anyhow::ensure!(
            ours.iter().all(|port| (61000..=65535).contains(port)),
            "a /loopback port's client keeps a source port outside the reserved range: {ours:?}"
        );
        let ephemeral =
            common::exec_in_vm(golden_pid, &["cat /proc/sys/net/ipv4/ip_local_port_range"]).await?;
        let top: u16 = ephemeral
            .split_whitespace()
            .nth(1)
            .and_then(|top| top.parse().ok())
            .with_context(|| format!("the guest's ip_local_port_range is {ephemeral:?}"))?;
        anyhow::ensure!(
            top < 61000,
            "the guest gives its own connections ports up to {top}, inside the range reserved \
             for /loopback clients"
        );
        held = Some(client);
        common::create_snapshot_by_pid(golden_pid, &snapshot_name).await?;
        Ok(ours)
    }
    .await;
    common::kill_process(golden_pid).await;
    let _ = golden_child.wait().await;
    drop(held);
    let ours = match prepared {
        Ok(ours) => ours,
        Err(error) => {
            let _ = common::delete_snapshot(&snapshot_name).await;
            return Err(error);
        }
    };

    let (mut clone_child, clone_pid, _clone_log) = common::spawn_fcvm_with_env_and_log_path(
        &[
            "snapshot",
            "run",
            "--snapshot",
            &snapshot_name,
            "--name",
            &clone_name,
        ],
        &[],
    )
    .await
    .context("spawning the restored clone")?;
    let checked: Result<()> = async {
        common::poll_health_by_pid(clone_pid, 120).await?;
        let listing = common::exec_in_vm(clone_pid, &[SERVER_SOCKETS]).await?;
        let kept: Vec<u16> = loopback_peers(&listing)
            .intersection(&ours)
            .copied()
            .collect();
        anyhow::ensure!(
            kept.is_empty(),
            "the clone holds the source's connection from the host on its /loopback port \
             (peer ports {kept:?}):\n{listing}"
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

/// The guest's gate chains, in both families: empty when the gate is open.
const GATE_CHAINS: &str =
    "{ /usr/sbin/iptables -w -S; /usr/sbin/ip6tables -w -S; } | grep FCVM_SNAPSHOT || true";

/// A source whose agent dies between a snapshot's prepare and its resume is reopened
/// all the same.
///
/// The prepare closes the source's gate and freezes its container, and the resume
/// one-shot that undoes both arrives over the agent's exec server. With the agent dead
/// the one-shot could not run, `snapshot create` gave up after one try, and on
/// Firecracker the agent systemd started next waited for ever for its boot plan, which
/// comes over MMDS and so through the closed egress half: the source refused every new
/// TCP connection for the rest of its life. Now an agent that starts with a boundary
/// owed serves the host's resume before anything else, and the host asks until the gate
/// is open.
///
/// systemd starts the next agent about a second after the kill (`RestartSec=1`), which is
/// inside the 54 s the host's exec keeps trying to connect, so the first ask lands.
///
/// What says the gate is open is read on the host, from two logs:
///
/// - the resume one-shot's own report, which the host logs when the one-shot succeeds;
/// - the new agent fetching its boot plan after it started with the boundary owed. That
///   fetch is a TCP connection out of the guest, which the gate's egress half refuses.
///
/// Nothing is asked of the guest after the restart, because it does not live: systemd
/// killed the container with the agent, the new agent cannot start it again, and the VM
/// powers off about a second after the plan arrives, as it does after any death of the
/// agent with no snapshot in sight (#2158). An exec sent then is a race with the
/// poweroff, which this test lost in 3 runs of 6 while it sent one.
///
/// RED BEFORE THE FIX: `snapshot create` fails after its one ask and the gate stays.
#[tokio::test]
async fn a_sources_gate_is_reopened_after_its_agent_dies_between_the_prepare_and_the_resume(
) -> Result<()> {
    let (golden_name, _, snapshot_name, _) = common::unique_names("agentdies");
    let (mut golden_child, golden_pid, golden_log) = common::spawn_fcvm_with_env_and_log_path(
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
        &[("FCVM_NO_SNAPSHOT", "1")],
    )
    .await
    .context("spawning the source VM")?;
    let scratch = tempfile::tempdir()?;
    let hold = scratch.path().join("reopen");
    let create_log = scratch.path().join("create.log");
    let checked: Result<()> = async {
        common::poll_health_by_pid(golden_pid, 180).await?;
        let log = std::fs::File::create(&create_log)?;
        let mut create = tokio::process::Command::new(common::find_fcvm_binary()?)
            .args([
                "snapshot",
                "create",
                "--pid",
                &golden_pid.to_string(),
                "--tag",
                &snapshot_name,
            ])
            .env(
                "FCVM_FAILPOINT",
                format!(
                    "snapshot.post_resume_pre_reopen:block_until_file:{}",
                    hold.display()
                ),
            )
            // The host logs the one-shot's report at debug.
            .env("RUST_LOG", "fcvm=debug")
            .stdout(log.try_clone()?)
            .stderr(log)
            .kill_on_drop(true)
            .spawn()
            .context("spawning snapshot create")?;
        wait_for_log(
            &create_log,
            "FAILPOINT snapshot.post_resume_pre_reopen reached",
        )
        .await?;

        // The source runs behind its closed gate, with a boundary owed.
        let gated = common::exec_in_vm(golden_pid, &[GATE_CHAINS]).await?;
        anyhow::ensure!(
            gated.contains("FCVM_SNAPSHOT_IN") && gated.contains("FCVM_SNAPSHOT_OUT"),
            "the source's gate is not closed between the prepare and the resume, so nothing \
             would be tested:\n{gated}"
        );
        // The exec's own shell dies with the agent's cgroup, so its result says nothing.
        let _ = common::exec_in_vm(
            golden_pid,
            &["kill -KILL $(systemctl show -p MainPID --value fc-agent)"],
        )
        .await;
        std::fs::write(&hold, b"")?;

        let status = tokio::time::timeout(Duration::from_secs(240), create.wait())
            .await
            .context("snapshot create did not end within 240 s")??;
        let logged = std::fs::read_to_string(&create_log).unwrap_or_default();
        anyhow::ensure!(
            status.success(),
            "snapshot create failed after the source's agent died between its prepare and \
             its resume ({status}):\n{logged}"
        );
        // The guest's own account of the resume, as the host logged it.
        anyhow::ensure!(
            logged.contains("source VM snapshot network reopened: gate=open"),
            "snapshot create succeeded without the guest reporting its gate open:\n{logged}"
        );
        // The resume was served by an agent that started behind the closed gate, and
        // that agent then reached MMDS: a connection out of the guest.
        const OWED: &str =
            "a snapshot's boundary is owed its resume and the agent that served it is gone";
        anyhow::ensure!(
            std::fs::read_to_string(&golden_log)
                .unwrap_or_default()
                .contains(OWED),
            "no agent started with the boundary owed, so the agent the test killed served the \
             resume itself and nothing was tested:\n{logged}"
        );
        wait_for_log_after(
            &golden_log,
            golden_pid,
            OWED,
            "received container plan successfully",
        )
        .await
        .context("the restarted agent never got its boot plan through the gate")?;
        Ok(())
    }
    .await;
    common::kill_process(golden_pid).await;
    let _ = golden_child.wait().await;
    let deleted = common::delete_snapshot(&snapshot_name).await;
    checked?;
    deleted
}

/// A clone whose restore is under way refuses a snapshot, and its container stays frozen
/// until the restore thaws it, however long the restore takes.
///
/// The clone is held for 75 s before its restore verifies the gate, with both halves of
/// the gate closed and the container frozen as the snapshot left it.
///
/// - `fcvm snapshot create` of the clone fails within 20 s and changes nothing in the
///   guest: the gate is still closed and the container still frozen (#2138). The guest
///   refuses the prepare, and refuses the reopen the host sends after a failed prepare,
///   each before it waits for the boundary lock, with the status the host does not
///   retry. The prepare used to wait for the restore's lock, then close the published
///   clone's gate; the reopen waited for the lock too.
/// - 66 s into the hold the container is still frozen (#2140). The freeze watchdog used
///   to thaw it at its 60th look, behind the closed gate, where each connection the
///   workload made was answered with a reset.
///
/// RED BEFORE THE FIX: the create does not end within 20 s, and the container's
/// `cgroup.freeze` reads 0 at 66 s.
#[tokio::test]
async fn a_restoring_clone_refuses_a_prepare_and_stays_frozen_past_the_watchdogs_deadline(
) -> Result<()> {
    const FREEZE: &str =
        "cat /sys/fs/cgroup/system.slice/fc-agent.service/libpod-payload-*/cgroup.freeze";
    let (golden_name, clone_name, snapshot_name, _) = common::unique_names("heldrestore");
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
                "restore.pre_gate_verify:sleep:75000",
            ),
            ("FCVM_NO_SNAPSHOT", "1"),
        ],
    )
    .await
    .context("spawning the golden VM with the gate hold armed")?;
    let prepared: Result<()> = async {
        common::poll_health_by_pid(golden_pid, 180).await?;
        let running = common::exec_in_vm(golden_pid, &[FREEZE]).await?;
        anyhow::ensure!(
            running.trim() == "0",
            "the source's container cgroup is not where the test reads it: {running:?}"
        );
        common::create_snapshot_by_pid(golden_pid, &snapshot_name).await
    }
    .await;
    common::kill_process(golden_pid).await;
    let _ = golden_child.wait().await;
    if let Err(error) = prepared {
        let _ = common::delete_snapshot(&snapshot_name).await;
        return Err(error);
    }

    let (mut clone_child, clone_pid, clone_log) = common::spawn_fcvm_with_env_and_log_path(
        &[
            "snapshot",
            "run",
            "--snapshot",
            &snapshot_name,
            "--name",
            &clone_name,
        ],
        &[],
    )
    .await
    .context("spawning the restored clone")?;
    let checked: Result<()> = async {
        wait_for_log(&clone_log, "FAILPOINT restore.pre_gate_verify reached").await?;
        let held = Instant::now();
        let logged = |marker: &str| {
            std::fs::read_to_string(&clone_log)
                .unwrap_or_default()
                .contains(marker)
        };

        let stray = format!("{snapshot_name}-stray");
        let create = tokio::process::Command::new(common::find_fcvm_binary()?)
            .args([
                "snapshot",
                "create",
                "--pid",
                &clone_pid.to_string(),
                "--tag",
                &stray,
            ])
            .kill_on_drop(true)
            .output();
        let create = tokio::time::timeout(Duration::from_secs(20), create)
            .await
            .context(
                "snapshot create of a clone whose restore is under way did not end within \
                 20 s: a one-shot is waiting for the restore's lock, to act on the published \
                 clone",
            )??;
        if create.status.success() {
            let _ = common::delete_snapshot(&stray).await;
            anyhow::bail!("a snapshot was taken of a clone whose restore is under way");
        }
        let refused = String::from_utf8_lossy(&create.stderr);
        anyhow::ensure!(
            refused.contains("restore"),
            "snapshot create of a restoring clone failed for another reason: {refused}"
        );
        let frozen = common::exec_in_vm(clone_pid, &[FREEZE]).await?;
        anyhow::ensure!(
            frozen.trim() == "1",
            "the refused snapshot thawed the restoring clone's container: {frozen:?}"
        );
        let gate = common::exec_in_vm(clone_pid, &[GATE_CHAINS]).await?;
        anyhow::ensure!(
            gate.contains("FCVM_SNAPSHOT_IN") && gate.contains("FCVM_SNAPSHOT_OUT"),
            "the refused snapshot opened the restoring clone's gate:\n{gate}"
        );
        anyhow::ensure!(
            !logged("FAILPOINT restore.pre_gate_verify released"),
            "the hold ended before the snapshot was refused, so nothing was tested"
        );

        tokio::time::sleep(Duration::from_secs(66).saturating_sub(held.elapsed())).await;
        let frozen = common::exec_in_vm(clone_pid, &[FREEZE]).await?;
        anyhow::ensure!(
            !logged("FAILPOINT restore.pre_gate_verify released"),
            "the hold ended before the watchdog's deadline, so nothing was tested"
        );
        anyhow::ensure!(
            frozen.trim() == "1",
            "the container was thawed behind the closed gate, {:?} into its restore: {frozen:?}",
            held.elapsed()
        );

        // The restore goes on, thaws the container itself and publishes.
        common::poll_health_by_pid(clone_pid, 180).await?;
        let thawed = common::exec_in_vm(clone_pid, &[FREEZE]).await?;
        anyhow::ensure!(
            thawed.trim() == "0",
            "the published clone's container is frozen: {thawed:?}"
        );
        let marks = common::exec_in_vm(
            clone_pid,
            &["ls /run/fcvm/restore-under-way /run/fcvm/snapshot-boundary-owed 2>/dev/null || true"],
        )
        .await?;
        anyhow::ensure!(marks.trim().is_empty(), "the published clone still holds {marks}");
        Ok(())
    }
    .await;
    common::kill_process(clone_pid).await;
    let _ = clone_child.wait().await;
    let deleted = common::delete_snapshot(&snapshot_name).await;
    checked?;
    deleted
}

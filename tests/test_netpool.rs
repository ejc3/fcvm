//! The namespace pool (`fcvm netpool serve`) against a real pool process: who gets which
//! namespace, when a namespace may be lent again, what the next tenant can see of the last,
//! and what is left when the pool dies.
//!
//! Each test runs its own pool in a data directory of its own, so no other test's clones
//! claim from it. A tenant is played by this test binary started again inside the leased
//! namespace ([`helper_in_the_namespace`]): it attaches to the VMM's TAP as a VMM does and
//! sends and records Ethernet frames as a guest would.

#![cfg(feature = "integration-fast")]

mod common;

use fcvm::netpool::client::{self, Lease};
use fcvm::netpool::proto::Stats;
use std::io::{BufRead, Read, Write};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

const GUEST_IP: [u8; 4] = [10, 0, 2, 100];
const NAMESPACE_IP: [u8; 4] = [10, 0, 2, 1];
/// The guest's gateway, which pasta answers for and maps to the host's loopback.
const GATEWAY_IP: [u8; 4] = [10, 0, 2, 2];
/// The MAC the first tenant answers ARP with. No VM has it.
const POISON_MAC: [u8; 6] = [0x02, 0, 0, 0, 0, 0xaa];

// ---------------------------------------------------------------------------------------
// The helper: this binary, started inside a leased namespace.
// ---------------------------------------------------------------------------------------

/// Not a test of its own. With `NETPOOL_TEST_TAP` set this is the body of the process a test
/// starts inside a leased namespace: it attaches to the TAP and serves the commands on its
/// standard input, one line each, answering each with one line `@@=ANSWER`.
///
/// - `capture MS`: record every frame for MS milliseconds, print `@@frame HEX` for each.
/// - `poison`: play a guest that leaves marks. Answer ARP for the guest's address with
///   [`POISON_MAC`], accept the first TCP connection, and answer its first data segment
///   with an ICMP "fragmentation needed" for 576 bytes, never acknowledging the data.
/// - `udp PORT`: send one UDP datagram to the gateway's PORT, as a guest that starts a flow
///   through pasta.
/// - `detach`: close the TAP.
#[test]
fn helper_in_the_namespace() {
    let Ok(tap) = std::env::var("NETPOOL_TEST_TAP") else {
        return;
    };
    let mut tap = Some(attach(&tap));
    println!("@@=attached");
    for line in std::io::stdin().lock().lines() {
        let line = line.expect("reading a command");
        let mut words = line.split_whitespace();
        match (words.next(), words.next()) {
            (Some("capture"), Some(ms)) => {
                let budget = Duration::from_millis(ms.parse().expect("a number of milliseconds"));
                let mut count = 0;
                let deadline = Instant::now() + budget;
                while let Some(frame) = read_frame(tap.as_mut().unwrap(), deadline) {
                    println!("@@frame {}", hex(&frame));
                    count += 1;
                }
                println!("@@=captured {count}");
            }
            (Some("poison"), _) => println!("@@={}", poison(tap.as_mut().unwrap())),
            (Some("udp"), Some(port)) => {
                let port = port.parse().expect("a port");
                println!("@@={}", udp_to_gateway(tap.as_mut().unwrap(), port));
            }
            (Some("detach"), _) => {
                tap = None;
                println!("@@=detached");
            }
            other => println!("@@=unknown {other:?}"),
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Attach to the TAP `name` the way a VMM does: `TUNSETIFF` on a new descriptor.
fn attach(name: &str) -> std::fs::File {
    #[repr(C)]
    struct IfReq {
        name: [u8; libc::IFNAMSIZ],
        flags: libc::c_short,
        pad: [u8; 22],
    }
    const TUNSETIFF: libc::c_ulong = 0x4004_54ca;
    const IFF_TAP: libc::c_short = 0x0002;
    const IFF_NO_PI: libc::c_short = 0x1000;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/net/tun")
        .expect("opening /dev/net/tun");
    let mut request = IfReq {
        name: [0; libc::IFNAMSIZ],
        flags: IFF_TAP | IFF_NO_PI,
        pad: [0; 22],
    };
    request.name[..name.len()].copy_from_slice(name.as_bytes());
    // SAFETY: `request` is a `struct ifreq` that outlives the call.
    let rc = unsafe { libc::ioctl(file.as_raw_fd(), TUNSETIFF as _, &request) };
    assert_eq!(
        rc,
        0,
        "TUNSETIFF {name}: {}",
        std::io::Error::last_os_error()
    );
    file
}

/// The next frame from the TAP, or `None` at `deadline`.
fn read_frame(tap: &mut std::fs::File, deadline: Instant) -> Option<Vec<u8>> {
    let left = deadline.checked_duration_since(Instant::now())?;
    let mut poll = libc::pollfd {
        fd: tap.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: `poll` is one valid pollfd for the whole call.
    if unsafe { libc::poll(&mut poll, 1, left.as_millis() as libc::c_int) } <= 0 {
        return None;
    }
    let mut frame = vec![0u8; 65536];
    let len = tap.read(&mut frame).expect("reading a frame");
    frame.truncate(len);
    Some(frame)
}

fn checksum(parts: &[&[u8]]) -> [u8; 2] {
    let mut sum = 0u32;
    for part in parts {
        for pair in part.chunks(2) {
            sum += u32::from(pair[0]) << 8 | u32::from(*pair.get(1).unwrap_or(&0));
        }
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    (!(sum as u16)).to_be_bytes()
}

/// An IPv4 header from the guest to `to` for `payload_len` bytes of `protocol`.
fn ipv4_header(to: [u8; 4], protocol: u8, payload_len: usize) -> Vec<u8> {
    let mut header = vec![0x45, 0, 0, 0, 0, 0, 0x40, 0, 64, protocol, 0, 0];
    header[2..4].copy_from_slice(&((20 + payload_len) as u16).to_be_bytes());
    header.extend_from_slice(&GUEST_IP);
    header.extend_from_slice(&to);
    let sum = checksum(&[&header]);
    header[10..12].copy_from_slice(&sum);
    header
}

fn ethernet(to: &[u8], ethertype: u16, payload: &[u8]) -> Vec<u8> {
    let mut frame = to.to_vec();
    frame.extend_from_slice(&POISON_MAC);
    frame.extend_from_slice(&ethertype.to_be_bytes());
    frame.extend_from_slice(payload);
    frame
}

/// Leave the marks a guest can leave in a namespace, and say what was left.
fn poison(tap: &mut std::fs::File) -> String {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut connected = false;
    while let Some(frame) = read_frame(tap, deadline) {
        if frame.len() < 34 {
            continue;
        }
        let (from, ethertype) = (&frame[6..12], u16::from_be_bytes([frame[12], frame[13]]));
        let body = &frame[14..];
        // An ARP request for the guest's address: answer with a MAC that is no VM's.
        if ethertype == 0x0806 && body.len() >= 28 && body[7] == 1 && body[24..28] == GUEST_IP {
            let mut reply = vec![0, 1, 8, 0, 6, 4, 0, 2];
            reply.extend_from_slice(&POISON_MAC);
            reply.extend_from_slice(&GUEST_IP);
            reply.extend_from_slice(&body[8..18]);
            tap.write_all(&ethernet(from, 0x0806, &reply)).unwrap();
            continue;
        }
        if ethertype != 0x0800 || body[9] != 6 || body[16..20] != GUEST_IP {
            continue;
        }
        let ip_len = usize::from(body[0] & 0xf) * 4;
        let tcp = &body[ip_len..];
        let (flags, data_at) = (tcp[13], usize::from(tcp[12] >> 4) * 4);
        if flags & 0x02 != 0 {
            // SYN: accept it. Ports swapped, our sequence number 1000, theirs plus one.
            let mut reply = Vec::new();
            reply.extend_from_slice(&tcp[2..4]);
            reply.extend_from_slice(&tcp[0..2]);
            reply.extend_from_slice(&1000u32.to_be_bytes());
            let theirs = u32::from_be_bytes(tcp[4..8].try_into().unwrap());
            reply.extend_from_slice(&theirs.wrapping_add(1).to_be_bytes());
            reply.extend_from_slice(&[0x50, 0x12, 0xff, 0xff, 0, 0, 0, 0]);
            let pseudo = [&GUEST_IP[..], &NAMESPACE_IP[..], &[0, 6, 0, 20][..]].concat();
            let sum = checksum(&[&pseudo, &reply]);
            reply[16..18].copy_from_slice(&sum);
            let packet = [ipv4_header(NAMESPACE_IP, 6, reply.len()), reply].concat();
            tap.write_all(&ethernet(from, 0x0800, &packet)).unwrap();
            connected = true;
        } else if connected && tcp.len() > data_at {
            // Data: never acknowledged, so the namespace's socket keeps retransmitting it.
            // Answered with "fragmentation needed, next hop MTU 576", quoting the segment.
            let quoted = &body[..(ip_len + 8).min(body.len())];
            let mut icmp = vec![3, 4, 0, 0, 0, 0, 2, 0x40];
            icmp.extend_from_slice(quoted);
            let sum = checksum(&[&icmp]);
            icmp[2..4].copy_from_slice(&sum);
            let packet = [ipv4_header(NAMESPACE_IP, 1, icmp.len()), icmp].concat();
            tap.write_all(&ethernet(from, 0x0800, &packet)).unwrap();
            return "poisoned".to_string();
        }
    }
    format!("no data segment arrived (connected: {connected})")
}

/// Ask who has the gateway's address, then send one UDP datagram to `port` there.
fn udp_to_gateway(tap: &mut std::fs::File, port: u16) -> String {
    let mut request = vec![0, 1, 8, 0, 6, 4, 0, 1];
    request.extend_from_slice(&POISON_MAC);
    request.extend_from_slice(&GUEST_IP);
    request.extend_from_slice(&[0; 6]);
    request.extend_from_slice(&GATEWAY_IP);
    tap.write_all(&ethernet(&[0xff; 6], 0x0806, &request))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let gateway = loop {
        let Some(frame) = read_frame(tap, deadline) else {
            return "no ARP reply for the gateway".to_string();
        };
        let body = &frame[14.min(frame.len())..];
        if frame.len() >= 42
            && frame[12..14] == [0x08, 0x06]
            && body[7] == 2
            && body[14..18] == GATEWAY_IP
        {
            break body[8..14].to_vec();
        }
    };
    let payload = b"from the first tenant";
    let mut udp = Vec::new();
    udp.extend_from_slice(&40000u16.to_be_bytes());
    udp.extend_from_slice(&port.to_be_bytes());
    udp.extend_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
    udp.extend_from_slice(&[0, 0]);
    udp.extend_from_slice(payload);
    let packet = [ipv4_header(GATEWAY_IP, 17, udp.len()), udp].concat();
    tap.write_all(&ethernet(&gateway, 0x0800, &packet)).unwrap();
    "sent".to_string()
}

/// The frames of a capture that are not the namespace's own multicast membership reports.
///
/// A tenant that sends nothing still receives those: the bridge joins the snoopers' groups
/// (224.0.0.106, ff02::6a) and the kernel reports its memberships again whenever the bridge's
/// carrier changes, in a namespace no VM has used as in a recycled one. Everything else a
/// silent tenant receives was caused by someone: an ARP request, a TCP segment, a datagram,
/// a neighbour solicitation.
fn not_membership_reports(frames: &[String]) -> Vec<String> {
    let is_report = |frame: &[u8]| -> bool {
        if frame.len() < 14 + 20 {
            return false;
        }
        let ethertype = u16::from_be_bytes([frame[12], frame[13]]);
        let ip = &frame[14..];
        match (ethertype, &frame[..3], &frame[..2]) {
            // IGMP to an IPv4 multicast group.
            (0x0800, [0x01, 0x00, 0x5e], _) => ip[9] == 2,
            // MLD to an IPv6 multicast group: ICMPv6 after a hop-by-hop header, a report
            // (v1 131, v2 143) or a done (132).
            (0x86dd, _, [0x33, 0x33]) => {
                ip.len() >= 40 + 8 + 4
                    && ip[6] == 0
                    && ip[40] == 58
                    && ip
                        .get(40 + (usize::from(ip[41]) + 1) * 8)
                        .is_some_and(|kind| [131, 132, 143].contains(kind))
            }
            _ => false,
        }
    };
    frames
        .iter()
        .filter(|hex| {
            let bytes: Vec<u8> = (0..hex.len() / 2)
                .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap())
                .collect();
            !is_report(&bytes)
        })
        .cloned()
        .collect()
}

/// A helper process inside a leased namespace.
struct Helper {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    lines: tokio::io::Lines<tokio::io::BufReader<tokio::process::ChildStdout>>,
}

impl Helper {
    /// Start the helper in the namespace of `lease`, attached to its TAP. With `keep_lease`
    /// it inherits the lease as a VMM does; without, it holds the TAP and no lease.
    async fn start(lease: &Lease, keep_lease: bool) -> Self {
        let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "helper_in_the_namespace", "--nocapture"])
            .env("NETPOOL_TEST_TAP", &lease.grant.tap)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true);
        fcvm::utils::install_namespace_pre_exec(
            &mut command,
            &fcvm::utils::NamespaceParams {
                vm_id: "netpool-test".to_string(),
                user_namespace_path: Some(lease.user_namespace_path()),
                net_namespace_path: Some(lease.net_namespace_path()),
                keep_fds: if keep_lease {
                    vec![lease.fd()]
                } else {
                    Vec::new()
                },
                ..Default::default()
            },
        )
        .expect("preparing the helper's namespaces");
        let mut child = command.spawn().expect("starting the helper");
        let stdin = child.stdin.take().unwrap();
        let lines = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
        let mut helper = Self {
            child,
            stdin,
            lines,
        };
        assert_eq!(helper.answer().await.0, "attached");
        helper
    }

    /// The helper's next answer and the frames it printed before it.
    async fn answer(&mut self) -> (String, Vec<String>) {
        let mut frames = Vec::new();
        loop {
            let line = tokio::time::timeout(Duration::from_secs(60), self.lines.next_line())
                .await
                .expect("the helper went silent")
                .expect("reading the helper")
                .expect("the helper exited");
            // The marker can follow text of the test harness on the same line ("test NAME
            // ... " when it runs its tests on one thread).
            let Some((_, said)) = line.split_once("@@") else {
                continue;
            };
            if let Some(answer) = said.strip_prefix('=') {
                return (answer.to_string(), frames);
            }
            if let Some(frame) = said.strip_prefix("frame ") {
                frames.push(frame.to_string());
            }
        }
    }

    async fn command(&mut self, command: &str) -> (String, Vec<String>) {
        self.stdin
            .write_all(format!("{command}\n").as_bytes())
            .await
            .expect("writing to the helper");
        self.answer().await
    }

    async fn kill(mut self) {
        let _ = self.child.kill().await;
    }
}

// ---------------------------------------------------------------------------------------
// The pool under test.
// ---------------------------------------------------------------------------------------

struct Pool {
    child: tokio::process::Child,
    pid: u32,
    data_dir: PathBuf,
    socket: PathBuf,
    key: String,
}

impl Pool {
    /// Start `fcvm netpool serve --slots SLOTS` in a data directory of its own and wait
    /// until every namespace is ready.
    async fn start(slots: usize) -> Self {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let data_dir = PathBuf::from(format!(
            "/mnt/fcvm-btrfs/netpool-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&data_dir).expect("creating the test's data directory");
        let mut command = tokio::process::Command::new(common::find_fcvm_binary().unwrap());
        command
            .args(["netpool", "serve", "--slots", &slots.to_string()])
            .env("FCVM_DATA_DIR", &data_dir);
        common::set_test_pdeathsig(&mut command);
        let child = command.spawn().expect("starting the pool");
        let pid = child.id().unwrap();
        // The key of a default restored clone's network, which is the pool's.
        let key =
            fcvm::network::PastaNetwork::new("key".into(), "tap".into(), Vec::new()).pool_key();
        let mut pool = Self {
            child,
            pid,
            data_dir,
            socket: PathBuf::new(),
            key,
        };
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            assert!(Instant::now() < deadline, "the pool did not come up");
            assert!(
                pool.child.try_wait().unwrap().is_none(),
                "the pool exited while starting"
            );
            let sockets: Vec<PathBuf> = std::fs::read_dir(pool.data_dir.join("netpool"))
                .into_iter()
                .flatten()
                .flatten()
                .map(|owner| owner.path().join("pool.sock"))
                .filter(|socket| socket.exists())
                .collect();
            if let [socket] = sockets.as_slice() {
                pool.socket = socket.clone();
                if let Ok(Some(stats)) = client::stats_at(socket).await {
                    if stats.ready == slots {
                        return pool;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn stats(&self) -> Stats {
        client::stats_at(&self.socket)
            .await
            .expect("asking the pool for its counters")
            .expect("the pool is not listening")
    }

    /// Wait until the pool's counters satisfy `reached`, and return them.
    async fn wait_for(&self, what: &str, reached: impl Fn(&Stats) -> bool) -> Stats {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let stats = self.stats().await;
            if reached(&stats) {
                return stats;
            }
            assert!(
                Instant::now() < deadline,
                "the pool never had {what}: {stats:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn claim(&self) -> Option<Lease> {
        client::claim_at(&self.socket, &self.key)
            .await
            .expect("claiming a namespace")
    }

    /// The pool's child processes: its holders and pastas.
    fn children(&self) -> Vec<i32> {
        let mut children = Vec::new();
        for task in std::fs::read_dir(format!("/proc/{}/task", self.pid))
            .unwrap()
            .flatten()
        {
            let list = std::fs::read_to_string(task.path().join("children")).unwrap_or_default();
            children.extend(
                list.split_whitespace()
                    .map(|pid| pid.parse::<i32>().unwrap()),
            );
        }
        children
    }

    /// SIGTERM the pool, wait for it to destroy its namespaces, remove its data directory.
    async fn stop(mut self) {
        // SAFETY: kill takes no pointers; the pid is this test's own child.
        unsafe { libc::kill(self.pid as i32, libc::SIGTERM) };
        let status = tokio::time::timeout(Duration::from_secs(60), self.child.wait())
            .await
            .expect("the pool did not stop")
            .unwrap();
        assert!(status.success(), "the pool ended with {status}");
        let _ = std::fs::remove_dir_all(&self.data_dir);
    }
}

/// Whether the process `pid` is gone: reaped, or dead and waiting to be.
fn gone(pid: i32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        Ok(stat) => stat
            .rsplit(')')
            .next()
            .is_some_and(|rest| rest.trim_start().starts_with('Z')),
    }
}

async fn all_gone(pids: &[i32], what: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while let Some(alive) = pids.iter().find(|pid| !gone(**pid)) {
        assert!(
            Instant::now() < deadline,
            "{what}: process {alive} is still running"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// ---------------------------------------------------------------------------------------
// Tests without a VM.
// ---------------------------------------------------------------------------------------

/// Invariant 1, end to end: every claim gets a namespace of its own, a full pool lends
/// nothing, and a namespace given back is lent again, as a recycled one.
#[tokio::test(flavor = "current_thread")]
async fn each_lease_has_a_namespace_of_its_own() {
    let pool = Pool::start(3).await;
    let mut leases = Vec::new();
    for _ in 0..3 {
        leases.push(pool.claim().await.expect("a free namespace"));
    }
    let mut holders: Vec<u32> = leases.iter().map(|lease| lease.grant.holder_pid).collect();
    holders.sort_unstable();
    holders.dedup();
    assert_eq!(holders.len(), 3, "two leases share a namespace");
    assert!(leases.iter().all(|lease| lease.grant.generation == 1));
    assert!(
        pool.claim().await.is_none(),
        "a fourth lease from three namespaces"
    );
    let stats = pool.stats().await;
    assert_eq!(
        (stats.leased, stats.ready, stats.grants, stats.misses),
        (3, 0, 3, 1)
    );

    // A claim for a namespace built another way gets none, even with one free.
    let returned = leases.pop().unwrap();
    let (slot, holder) = (returned.grant.slot, returned.grant.holder_pid);
    drop(returned);
    pool.wait_for("a reset namespace", |s| s.ready == 1 && s.resets == 1)
        .await;
    let other = client::claim_at(&pool.socket, "another key").await.unwrap();
    assert!(
        other.is_none(),
        "a namespace was lent to a claim with another key"
    );

    let again = pool
        .claim()
        .await
        .expect("the namespace that was given back");
    assert_eq!(
        (
            again.grant.slot,
            again.grant.holder_pid,
            again.grant.generation
        ),
        (slot, holder, 2)
    );
    let stats = pool.stats().await;
    assert_eq!(
        (stats.recycled_grants, stats.destroys, stats.builds),
        (1, 0, 3)
    );
    drop((again, leases));
    pool.stop().await;
}

/// Invariant 2: a namespace is lent again only when every process that held its lease is
/// gone. The claimer dies first here, as a killed clone does, and the process it left in
/// the namespace (a VMM, attached to the TAP) lives on.
#[tokio::test(flavor = "current_thread")]
async fn a_namespace_is_not_lent_while_a_process_of_its_last_lease_runs() {
    let pool = Pool::start(1).await;
    let lease = pool.claim().await.expect("a free namespace");
    let holder = lease.grant.holder_pid;
    let vmm = Helper::start(&lease, true).await;
    drop(lease);

    for _ in 0..25 {
        let stats = pool.stats().await;
        assert_eq!(
            (stats.leased, stats.ready, stats.resets),
            (1, 0, 0),
            "the namespace was taken back while a holder of its lease was running"
        );
        assert!(pool.claim().await.is_none());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    vmm.kill().await;
    pool.wait_for("the namespace back", |s| s.ready == 1 && s.resets == 1)
        .await;
    let next = pool
        .claim()
        .await
        .expect("the namespace, once its VMM is gone");
    assert_eq!((next.grant.holder_pid, next.grant.generation), (holder, 2));
    assert_eq!(pool.stats().await.destroys, 0);
    drop(next);
    pool.stop().await;
}

/// The other half of invariant 2: a process that is still attached to the TAP when the
/// lease ends (it never had the lease, or gave it up) keeps the namespace from being lent.
/// The reset finds the TAP with a carrier, and the namespace is destroyed and replaced.
#[tokio::test(flavor = "current_thread")]
async fn a_namespace_with_a_process_still_on_its_tap_is_destroyed() {
    let pool = Pool::start(1).await;
    let lease = pool.claim().await.expect("a free namespace");
    let holder = lease.grant.holder_pid;
    let stray = Helper::start(&lease, false).await;
    drop(lease);

    let stats = pool
        .wait_for("a replacement namespace", |s| s.ready == 1 && s.builds == 2)
        .await;
    assert_eq!((stats.destroys, stats.resets), (1, 0));
    let next = pool.claim().await.expect("the replacement");
    assert_ne!(next.grant.holder_pid, holder);
    assert_eq!(next.grant.generation, 1);
    stray.kill().await;
    drop(next);
    pool.stop().await;
}

/// Invariant 5: a namespace that does not read as it was built is not lent again. A route
/// through loopback is something no reset removes.
#[tokio::test(flavor = "current_thread")]
async fn a_namespace_with_a_foreign_route_is_destroyed() {
    let pool = Pool::start(1).await;
    let lease = pool.claim().await.expect("a free namespace");
    let holder = lease.grant.holder_pid;
    let added = std::process::Command::new("nsenter")
        .args([
            "--preserve-credentials",
            "-U",
            "-n",
            "-t",
            &holder.to_string(),
            "--",
        ])
        .args(["ip", "route", "add", "192.0.2.0/24", "dev", "lo"])
        .status()
        .expect("running ip in the namespace");
    assert!(added.success());
    drop(lease);

    let stats = pool
        .wait_for("a replacement namespace", |s| s.ready == 1 && s.builds == 2)
        .await;
    assert_eq!((stats.destroys, stats.resets), (1, 0));
    let next = pool.claim().await.expect("the replacement");
    assert_ne!(next.grant.holder_pid, holder);
    drop(next);
    pool.stop().await;
}

/// Invariant 4: the next tenant sees nothing of the last. The first tenant answers ARP
/// with a MAC of its own choosing, takes a TCP connection from the namespace and leaves its
/// data unacknowledged, reports a path MTU of 576, and starts a UDP flow through pasta to a
/// host service that answers late. The second attaches to the same TAP and sends nothing:
/// it must receive what a silent tenant of an unused namespace receives (the namespace's own
/// multicast membership reports) and nothing else, and the namespace must have been reset,
/// not replaced.
#[tokio::test(flavor = "current_thread")]
async fn the_next_tenant_gets_no_frame_from_the_last() {
    let pool = Pool::start(1).await;

    // What a tenant that sends nothing receives in a namespace no one has used.
    let lease = pool.claim().await.expect("a free namespace");
    let holder = lease.grant.holder_pid;
    let mut first = Helper::start(&lease, true).await;
    let (_, unused) = first.command("capture 1500").await;
    assert_eq!(
        not_membership_reports(&unused),
        Vec::<String>::new(),
        "an unused namespace sent a silent tenant something"
    );

    // The marks. The connection is made from a socket of the namespace, as the health
    // probe's and pasta's inbound connections are.
    let socket = lease
        .agent
        .tcp_socket(&"10.0.2.100".parse().unwrap())
        .await
        .expect("a socket in the namespace");
    let connection = tokio::spawn(async move {
        let mut stream = socket.connect("10.0.2.100:80".parse().unwrap()).await?;
        stream.write_all(&[0x55; 1200]).await?;
        Ok::<_, std::io::Error>(stream)
    });
    assert_eq!(first.command("poison").await.0, "poisoned");
    let stream = connection
        .await
        .unwrap()
        .expect("connecting to the first tenant");

    // A flow through pasta: the tenant's datagram reaches a service on the host's loopback,
    // which now knows where to answer.
    let service = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let port = service.local_addr().unwrap().port();
    assert_eq!(first.command(&format!("udp {port}")).await.0, "sent");
    let mut datagram = [0u8; 64];
    let (len, pasta_flow) =
        tokio::time::timeout(Duration::from_secs(10), service.recv_from(&mut datagram))
            .await
            .expect("the tenant's datagram never reached the host")
            .unwrap();
    assert_eq!(&datagram[..len], b"from the first tenant");

    // The tenant leaves: its VMM exits and its clone's descriptors close.
    first.kill().await;
    drop(stream);
    drop(lease);
    let stats = pool
        .wait_for("the namespace back", |s| {
            s.ready == 1 && (s.resets == 1 || s.destroys == 1)
        })
        .await;
    assert_eq!(
        (stats.resets, stats.destroys),
        (1, 0),
        "the reset did not leave the namespace as it was built"
    );

    let lease = pool.claim().await.expect("the recycled namespace");
    assert_eq!(
        (lease.grant.holder_pid, lease.grant.generation),
        (holder, 2)
    );
    let mut second = Helper::start(&lease, true).await;
    // The host service answers the first tenant now. The pasta that had the flow is gone,
    // so the answer goes nowhere; the send itself may be refused.
    let _ = service.send_to(b"a late answer", pasta_flow).await;
    // Long enough for a retransmission of the first tenant's data, were its socket alive.
    let (_, frames) = second.command("capture 3500").await;
    assert_eq!(
        not_membership_reports(&frames),
        Vec::<String>::new(),
        "the second tenant received frames the first caused"
    );
    second.kill().await;
    drop(lease);
    pool.stop().await;
}

/// Invariant 3, the pool's side: a killed pool leaves no holder and no pasta, and every
/// lease ends.
#[tokio::test(flavor = "current_thread")]
async fn a_killed_pool_leaves_no_process_and_ends_every_lease() {
    let mut pool = Pool::start(3).await;
    let leases = [pool.claim().await.unwrap(), pool.claim().await.unwrap()];
    let children = pool.children();
    assert_eq!(
        children.len(),
        6,
        "three holders and three pastas: {children:?}"
    );
    for lease in &leases {
        assert!(children.contains(&(lease.grant.holder_pid as i32)));
    }

    // SAFETY: kill takes no pointers; the pid is this test's own child.
    unsafe { libc::kill(pool.pid as i32, libc::SIGKILL) };
    pool.child.wait().await.unwrap();
    all_gone(&children, "after the pool was killed").await;
    for lease in &leases {
        tokio::time::timeout(Duration::from_secs(10), lease.ended())
            .await
            .expect("a lease outlived its pool");
    }
    let _ = std::fs::remove_dir_all(&pool.data_dir);
}

// ---------------------------------------------------------------------------------------
// Tests with VMs.
// ---------------------------------------------------------------------------------------

/// The state of the VM named `name` in `data_dir`, once it has one.
fn vm_state(data_dir: &std::path::Path, name: &str) -> Option<serde_json::Value> {
    for entry in std::fs::read_dir(data_dir.join("state")).ok()?.flatten() {
        if entry.path().extension().is_some_and(|ext| ext == "json") {
            let Ok(text) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            if let Ok(state) = serde_json::from_str::<serde_json::Value>(&text) {
                if state["name"] == name {
                    return Some(state);
                }
            }
        }
    }
    None
}

async fn healthy(
    data_dir: &std::path::Path,
    name: &str,
    fcvm: &mut tokio::process::Child,
) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        if let Some(state) = vm_state(data_dir, name) {
            if state["health_status"] == "healthy" {
                return state;
            }
        }
        assert!(
            fcvm.try_wait().unwrap().is_none(),
            "{name}'s fcvm exited before it was healthy"
        );
        assert!(Instant::now() < deadline, "{name} never became healthy");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The socket inodes `pid` has open.
fn sockets_of(pid: u32) -> Vec<String> {
    std::fs::read_dir(format!("/proc/{pid}/fd"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|fd| std::fs::read_link(fd.path()).ok())
        .map(|target| target.to_string_lossy().into_owned())
        .filter(|target| target.starts_with("socket:"))
        .collect()
}

/// The Firecracker process that is a child of the fcvm process `pid`.
fn vmm_of(pid: u32) -> u32 {
    for task in std::fs::read_dir(format!("/proc/{pid}/task"))
        .unwrap()
        .flatten()
    {
        let list = std::fs::read_to_string(task.path().join("children")).unwrap_or_default();
        for child in list.split_whitespace() {
            let comm = std::fs::read_to_string(format!("/proc/{child}/comm")).unwrap_or_default();
            // The binary is named after its build (`firecracker-<hash>.bin`).
            if comm.trim().starts_with("firecracker") {
                return child.parse().unwrap();
            }
        }
    }
    panic!("fcvm {pid} has no firecracker child");
}

/// A data directory with a pool and one snapshot of a healthy nginx VM to clone.
struct Fixture {
    pool: Pool,
    tag: String,
    env: Vec<(String, String)>,
}

/// A clone publishes its snapshot's ports unless told otherwise, and a clone that publishes
/// a port is not pooled. The fixture's snapshot publishes one, so the tests have both kinds.
const NO_PORTS: [&str; 2] = ["--publish", "none"];

impl Fixture {
    async fn new(slots: usize) -> Self {
        let pool = Pool::start(slots).await;
        let env = vec![(
            "FCVM_DATA_DIR".to_string(),
            pool.data_dir.to_string_lossy().into_owned(),
        )];
        let (base, _, tag, _) = common::unique_names("netpool");
        let fixture = Self { pool, tag, env };
        // A cold boot builds its own namespace whether a pool runs or not.
        let publish = format!("{}:80", common::find_available_high_port().unwrap());
        let (mut fcvm, pid) = fixture
            .run(
                &[
                    "podman",
                    "run",
                    "--name",
                    &base,
                    "--publish",
                    &publish,
                    common::TEST_IMAGE,
                ],
                &[],
            )
            .await;
        let state = healthy(&fixture.pool.data_dir, &base, &mut fcvm).await;
        assert!(
            state["netpool"].is_null(),
            "a cold boot took a pooled namespace"
        );
        let created = tokio::process::Command::new(common::find_fcvm_binary().unwrap())
            .args([
                "snapshot",
                "create",
                "--pid",
                &pid.to_string(),
                "--tag",
                &fixture.tag,
            ])
            .envs(fixture.env.iter().cloned())
            .output()
            .await
            .unwrap();
        assert!(
            created.status.success(),
            "snapshot create: {}",
            String::from_utf8_lossy(&created.stderr)
        );
        common::kill_process(pid).await;
        let _ = fcvm.wait().await;
        assert_eq!(fixture.pool.stats().await.grants, 0);
        fixture
    }

    async fn run(&self, args: &[&str], extra_env: &[(&str, &str)]) -> (tokio::process::Child, u32) {
        let mut env: Vec<(&str, &str)> = self
            .env
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        env.extend_from_slice(extra_env);
        common::spawn_fcvm_with_env(args, &env)
            .await
            .expect("starting fcvm")
    }

    async fn clone(
        &self,
        name: &str,
        extra: &[&str],
        extra_env: &[(&str, &str)],
    ) -> (tokio::process::Child, u32) {
        let mut args = vec!["snapshot", "run", "--snapshot", &self.tag, "--name", name];
        args.extend_from_slice(extra);
        self.run(&args, extra_env).await
    }
}

/// A restored clone runs in a pooled namespace and gives it back; the next clone runs in
/// the recycled one. The clone's VMM holds the lease. A clone finds the pool's one namespace
/// lent and builds its own. A clone with a published port builds its own namespace without
/// asking (invariant 6, until a pooled pasta can take forwards).
#[tokio::test]
async fn clones_run_in_recycled_namespaces() {
    let fixture = Fixture::new(1).await;
    let data_dir = fixture.pool.data_dir.clone();

    let (mut first, first_pid) = fixture.clone("pooled-1", &NO_PORTS, &[]).await;
    let state = healthy(&data_dir, "pooled-1", &mut first).await;
    assert_eq!(state["netpool"]["generation"], 1, "{state}");
    let holder = state["holder_pid"].as_u64().unwrap() as i32;
    assert!(
        fixture.pool.children().contains(&holder),
        "the holder is not the pool's"
    );
    // The lease is a socket both the clone and its VMM have, and the pool's connection to
    // nothing else: the VMM has no other socket in common with fcvm.
    let vmm = vmm_of(first_pid);
    let shared: Vec<String> = sockets_of(vmm)
        .into_iter()
        .filter(|socket| sockets_of(first_pid).contains(socket))
        .collect();
    assert_eq!(
        shared.len(),
        1,
        "the VMM does not hold the lease: {shared:?}"
    );

    // With the one namespace lent, a second clone builds its own.
    let (mut second_own, second_own_pid) = fixture.clone("own", &NO_PORTS, &[]).await;
    let state = healthy(&data_dir, "own", &mut second_own).await;
    assert!(
        state["netpool"].is_null(),
        "two clones in one namespace: {state}"
    );
    common::kill_process(second_own_pid).await;
    let _ = second_own.wait().await;
    assert_eq!(fixture.pool.stats().await.misses, 1);

    common::kill_process(first_pid).await;
    let _ = first.wait().await;
    let stats = fixture
        .pool
        .wait_for("the namespace back", |s| s.ready == 1 && s.resets == 1)
        .await;
    assert_eq!((stats.destroys, stats.grants), (0, 1));

    // A clone that publishes a port builds its own namespace with one free in the pool, and
    // does not ask: the snapshot's port by default, or one of its own choosing.
    let publish = format!("{}:80", common::find_available_high_port().unwrap());
    for (name, ports) in [
        ("inherits", &[][..]),
        ("publishes", &["--publish", &publish][..]),
    ] {
        let (mut publishing, publishing_pid) = fixture.clone(name, ports, &[]).await;
        let state = healthy(&data_dir, name, &mut publishing).await;
        assert!(
            state["netpool"].is_null(),
            "a clone with a published port was pooled: {state}"
        );
        common::kill_process(publishing_pid).await;
        let _ = publishing.wait().await;
    }
    let stats = fixture.pool.stats().await;
    assert_eq!(
        (stats.grants, stats.misses),
        (1, 1),
        "a publishing clone asked the pool"
    );

    let (mut second, second_pid) = fixture.clone("pooled-2", &NO_PORTS, &[]).await;
    let state = healthy(&data_dir, "pooled-2", &mut second).await;
    assert_eq!(state["netpool"]["generation"], 2, "{state}");
    assert_eq!(state["holder_pid"].as_u64().unwrap() as i32, holder);
    common::kill_process(second_pid).await;
    let _ = second.wait().await;
    let stats = fixture
        .pool
        .wait_for("the namespace back", |s| s.ready == 1 && s.resets == 2)
        .await;
    assert_eq!((stats.destroys, stats.recycled_grants), (0, 1));
    fixture.pool.stop().await;
}

/// Invariants 2 and 3 with real clones. A clone killed right after its claim, one killed
/// before its VMM exists, and one killed while its VM runs each give the namespace back,
/// the last only once its VMM is gone. A clone whose pool is killed exits with an error
/// that names the lease.
#[tokio::test]
async fn killed_clones_give_their_namespace_back_and_a_killed_pool_ends_its_clones() {
    let fixture = Fixture::new(1).await;
    let data_dir = fixture.pool.data_dir.clone();

    for (n, point) in ["pool.post_claim", "restore.pre_spawn_vmm"]
        .iter()
        .enumerate()
    {
        let name = format!("killed-{n}");
        let spec = format!("{point}:sleep:60000");
        let (mut fcvm, pid) = fixture
            .clone(&name, &NO_PORTS, &[("FCVM_FAILPOINT", &spec)])
            .await;
        fixture
            .pool
            .wait_for("a lease", |s| s.leased == 1 && s.grants == n as u64 + 1)
            .await;
        // Parked inside the failpoint: the lease is held and no VMM exists yet.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            fcvm.try_wait().unwrap().is_none(),
            "{name} did not park at {point}"
        );
        // SAFETY: kill takes no pointers; the pid is this test's own child.
        unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        let _ = fcvm.wait().await;
        let stats = fixture
            .pool
            .wait_for("the namespace back", |s| {
                s.ready == 1 && s.resets == n as u64 + 1
            })
            .await;
        assert_eq!(stats.destroys, 0, "after a clone killed at {point}");
    }

    let (mut running, pid) = fixture.clone("killed-running", &NO_PORTS, &[]).await;
    healthy(&data_dir, "killed-running", &mut running).await;
    let vmm = vmm_of(pid);
    // SAFETY: as above.
    unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    let _ = running.wait().await;
    let stats = fixture
        .pool
        .wait_for("the namespace back", |s| s.resets == 3)
        .await;
    assert!(
        gone(vmm as i32),
        "the namespace was reset while its VMM was running"
    );
    assert_eq!(stats.destroys, 0);

    let (mut orphan, _) = fixture.clone("orphaned", &NO_PORTS, &[]).await;
    healthy(&data_dir, "orphaned", &mut orphan).await;
    let children = fixture.pool.children();
    // SAFETY: as above.
    unsafe { libc::kill(fixture.pool.pid as i32, libc::SIGKILL) };
    let status = tokio::time::timeout(Duration::from_secs(60), orphan.wait())
        .await
        .expect("a clone outlived its pool")
        .unwrap();
    assert!(
        !status.success(),
        "a clone whose pool was killed exited with {status}"
    );
    all_gone(&children, "after the pool was killed").await;
    let Fixture { mut pool, .. } = fixture;
    let _ = pool.child.wait().await;
    let _ = std::fs::remove_dir_all(&pool.data_dir);
}

//! The host's limits on public Minecraft joins (src/fleet/agent/joinguard.rs), enforced by the
//! kernel: the agent's own rules, installed on INPUT in a network namespace of the test's own,
//! against a listener on [::1]:25565 there. Root is needed for the namespace and for ip6tables.
#![cfg(feature = "privileged-tests")]

use fcvm::fleet::agent::joinguard;
use std::io::Read;
use std::net::{Ipv6Addr, SocketAddr, TcpListener, TcpStream};
use std::time::Duration;

/// Run `f` on a thread in a new network namespace with loopback up and the join limits on INPUT.
fn in_guarded_namespace<T: Send + 'static>(f: impl FnOnce(SocketAddr) -> T + Send + 'static) -> T {
    std::thread::spawn(move || {
        assert_eq!(
            unsafe { libc::unshare(libc::CLONE_NEWNET) },
            0,
            "unshare: {}",
            std::io::Error::last_os_error()
        );
        let up = std::process::Command::new("/usr/bin/ip")
            .args(["link", "set", "lo", "up"])
            .status()
            .unwrap();
        assert!(up.success());
        joinguard::install("INPUT", joinguard::JOIN_PORT).expect("installing the join limits");
        f(SocketAddr::from((
            Ipv6Addr::LOCALHOST,
            joinguard::JOIN_PORT,
        )))
    })
    .join()
    .unwrap()
}

fn connect(addr: SocketAddr) -> std::io::Result<TcpStream> {
    TcpStream::connect_timeout(&addr, Duration::from_secs(2))
}

#[test]
fn one_source_holds_at_most_the_open_connection_cap() {
    let refused_at = in_guarded_namespace(|addr| {
        let l = TcpListener::bind(addr).unwrap();
        let mut held = Vec::new();
        for n in 1..=joinguard::MAX_CONNECTIONS + 1 {
            match connect(addr) {
                Ok(c) => {
                    held.push((c, l.accept().unwrap().0));
                }
                Err(e) => {
                    assert_eq!(e.kind(), std::io::ErrorKind::ConnectionRefused, "{e}");
                    return Some(n);
                }
            }
        }
        None
    });
    assert_eq!(
        refused_at,
        Some(joinguard::MAX_CONNECTIONS + 1),
        "the connection past the cap was let in"
    );
}

#[test]
fn one_source_opens_at_most_its_burst_of_new_connections_at_once() {
    let refused_at = in_guarded_namespace(|addr| {
        // The open-connection rule leaves the chain, so only the rate can refuse. Finishing each
        // connection's close is not enough on its own: before Linux 7.0 (de8a70cefcb2), connlimit
        // does not collect closed connections on a SYN in the same jiffy as its previous one
        // (`jiffies == list->last_gc` in net/netfilter/nf_conncount.c, since 5.19), so a loop
        // faster than a jiffy still counts the connections it closed, and the cap refuses the
        // 5th. CI's x64 runners boot Ubuntu's 6.14.0-1018-aws, which has that code.
        let mut delete = joinguard::rules()
            .into_iter()
            .find(|r| r.iter().any(|a| a == "connlimit"))
            .expect("the open-connection rule");
        delete[0] = "-D".into();
        let out = std::process::Command::new("/usr/sbin/ip6tables")
            .args(&delete)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let l = TcpListener::bind(addr).unwrap();
        // One connection at a time, each fully closed before the next: the client half-closes,
        // the server reads that and closes, and the client reads the server's FIN.
        for n in 1..=joinguard::NEW_BURST + 2 {
            match connect(addr) {
                Ok(mut c) => {
                    let (mut s, _) = l.accept().unwrap();
                    c.shutdown(std::net::Shutdown::Write).unwrap();
                    let mut rest = Vec::new();
                    s.read_to_end(&mut rest).unwrap();
                    drop(s);
                    c.read_to_end(&mut rest).unwrap();
                    drop(c);
                }
                Err(e) => {
                    assert_eq!(e.kind(), std::io::ErrorKind::ConnectionRefused, "{e}");
                    return Some(n);
                }
            }
        }
        None
    });
    assert_eq!(
        refused_at,
        Some(joinguard::NEW_BURST + 1),
        "a new connection past the burst was let in"
    );
}

#[test]
fn installing_twice_leaves_one_jump_and_one_set_of_rules() {
    let listing = in_guarded_namespace(|_| {
        joinguard::install("INPUT", joinguard::JOIN_PORT).unwrap();
        let out = std::process::Command::new("/usr/sbin/ip6tables")
            .args(["-S"])
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap()
    });
    assert_eq!(listing.matches("-j FCVM-MC-JOIN").count(), 1, "{listing}");
    assert_eq!(listing.matches("-A FCVM-MC-JOIN").count(), 2, "{listing}");
}

#[test]
fn a_jump_inserted_ahead_later_is_moved_behind_the_limits() {
    // Netavark inserts its jump at position 1 after the agent's (2026-11-15, a staging host).
    // Its chain there accepts only the bridge network's own traffic; this one accepts joins, so
    // a join it sees first skips the limits. The keeper's move puts the limits back first.
    let (bypassed, listing, refused) = in_guarded_namespace(|addr| {
        let ip6 = |args: &[&str]| {
            let out = std::process::Command::new("/usr/sbin/ip6tables")
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "ip6tables {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8(out.stdout).unwrap()
        };
        ip6(&["-N", "NETAVARK_FORWARD"]);
        ip6(&[
            "-A",
            "NETAVARK_FORWARD",
            "-p",
            "tcp",
            "--dport",
            "25565",
            "-j",
            "ACCEPT",
        ]);
        ip6(&[
            "-I",
            "INPUT",
            "1",
            "-m",
            "comment",
            "--comment",
            "netavark firewall plugin rules",
            "-j",
            "NETAVARK_FORWARD",
        ]);
        let l = TcpListener::bind(addr).unwrap();
        let mut held = Vec::new();
        for _ in 0..=joinguard::MAX_CONNECTIONS {
            let c = connect(addr).expect("a join the limits never saw");
            held.push((c, l.accept().unwrap().0));
        }
        let bypassed = held.len();
        let done = joinguard::ensure_first("INPUT", joinguard::JOIN_PORT).unwrap();
        assert_eq!(done, joinguard::First::Move { copies: 1 });
        let listing = ip6(&["-S", "INPUT"]);
        // connlimit counts the connections it has seen, so the ones that went past it are not in
        // its count: from the move on, the cap applies afresh.
        let mut refused = None;
        for n in 1..=joinguard::MAX_CONNECTIONS + 1 {
            match connect(addr) {
                Ok(c) => held.push((c, l.accept().unwrap().0)),
                Err(e) => {
                    refused = Some((n, e.kind()));
                    break;
                }
            }
        }
        (bypassed, listing, refused)
    });
    assert_eq!(
        bypassed as u32,
        joinguard::MAX_CONNECTIONS + 1,
        "the netavark rule did not bypass"
    );
    let rules: Vec<&str> = listing
        .lines()
        .filter(|l| l.starts_with("-A INPUT "))
        .collect();
    assert_eq!(
        rules[0], "-A INPUT -p tcp -m tcp --dport 25565 -j FCVM-MC-JOIN",
        "{listing}"
    );
    assert_eq!(listing.matches("-j FCVM-MC-JOIN").count(), 1, "{listing}");
    assert_eq!(
        refused,
        Some((
            joinguard::MAX_CONNECTIONS + 1,
            std::io::ErrorKind::ConnectionRefused
        )),
        "the cap does not hold after the move"
    );
}

//! The host's limits on public Minecraft joins (src/fleet/agent/joinguard.rs), enforced by the
//! kernel: the agent's own rules, installed on INPUT in a network namespace of the test's own,
//! against a listener on [::1]:25565 there. Root is needed for the namespace and for ip6tables.
#![cfg(feature = "privileged-tests")]

use fcvm::fleet::agent::joinguard;
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
        // The open-connection rule leaves the chain, so only the rate can refuse. Closing each
        // connection before the next is not enough: before Linux 7.0, connlimit does not
        // collect closed connections on a SYN in the same jiffy as its previous one
        // (`jiffies == list->last_gc` in net/netfilter/nf_conncount.c), so connections closed
        // a moment ago still count, and the cap refuses the 5th of a fast loop.
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
        // one connection at a time, each closed before the next
        for n in 1..=joinguard::NEW_BURST + 2 {
            match connect(addr) {
                Ok(c) => {
                    let (s, _) = l.accept().unwrap();
                    drop(c);
                    drop(s);
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

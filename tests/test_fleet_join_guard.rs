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
        let l = TcpListener::bind(addr).unwrap();
        // One connection at a time, each closed before the next: only the rate can refuse them.
        // Closed means its close handshake has finished: the per-source cap (connlimit) counts a
        // connection until conntrack has seen it reach TIME_WAIT, so dropping both ends and
        // connecting again at once let the cap refuse the 5th or 6th connection on CI's x64
        // runners (the burst allows 6). The client half-closes, the server reads that and
        // closes, and the client's read of the server's FIN returns only after the kernel has
        // sent the last ACK, which moves the connection to TIME_WAIT, uncounted.
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

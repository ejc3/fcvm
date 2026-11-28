//! Addresses and ports that tests ask the kernel for.
//!
//! A test that needs an address nothing listens on, or a port for a program it starts,
//! used to bind a listener to port 0, read its address and drop it. Once the listener is
//! closed the kernel can give that port to any socket on the host that binds port 0, so
//! the address is not the test's any more. In the unit lane the other sockets are the
//! other tests' listeners: a connect the test expects to be refused reaches one of them
//! and succeeds, or the program the port was for fails to bind it. That is how
//! `a_proxy_relay_tries_every_address_of_its_proxy` failed (#2259): 5 runs in 2000 with
//! another process binding and closing loopback listeners beside it, none in 2000 alone.
//!
//! Each address here is the test's for as long as the test holds what came with it:
//!
//! * [`refused_address`]: nothing listens there, and nothing can.
//! * [`reserved_port`]: a program the test starts can bind it, and nothing else can.
//! * [`reserved_ipv6_loopback_port`]: the same for `::1`, which has no private address.
//!
//! `tests/test_listener_ports.rs` fails on a listener bound only for its address anywhere
//! in the tree, this file included.
//!
//! Unit tests reach this as a `cfg(test)` module of the library. Integration tests and the
//! other crates' tests include the file by path, since they link the library without its
//! `cfg(test)` modules.
#![allow(dead_code)]

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

/// What a test holds to keep an address its own. Dropping it gives the address up.
pub type Held = Vec<OwnedFd>;

/// Where the sockets that hold a reserved port are bound. No test or program listens on
/// it: VM loopback addresses are allocated upward from 127.0.0.2 and private ones start at
/// 127.64.0.0.
const GUARD_IP: Ipv4Addr = Ipv4Addr::new(127, 63, 255, 254);

/// A socket of `kind` bound to `ip:port` (port 0 for one the kernel picks) that never
/// listens, and the port it got. It is bound without `SO_REUSEADDR`, so while it is open
/// no other socket can bind that port on `ip` or on the wildcard address, whether it names
/// the port or asks for port 0.
fn bound_socket(kind: libc::c_int, ip: Ipv4Addr, port: u16) -> std::io::Result<(OwnedFd, u16)> {
    // SAFETY: socket() takes no pointers; a descriptor it returns is owned by nothing else.
    let socket = unsafe {
        let fd = libc::socket(libc::AF_INET, kind | libc::SOCK_CLOEXEC, 0);
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        OwnedFd::from_raw_fd(fd)
    };
    // SAFETY: an all-zero sockaddr_in is a valid value of the type.
    let mut name: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    name.sin_family = libc::AF_INET as _;
    name.sin_addr.s_addr = u32::from(ip).to_be();
    name.sin_port = port.to_be();
    let mut len = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
    // SAFETY: `name` is a sockaddr_in of `len` bytes that outlives both calls.
    unsafe {
        let name = std::ptr::addr_of_mut!(name).cast::<libc::sockaddr>();
        if libc::bind(socket.as_raw_fd(), name, len) != 0
            || libc::getsockname(socket.as_raw_fd(), name, &mut len) != 0
        {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok((socket, u16::from_be(name.sin_port)))
}

/// A loopback address that refuses connections for as long as `Held` is.
///
/// Its socket is bound and never listens, so a connect to the address is reset, and no
/// other socket can bind the port.
pub fn refused_address() -> (Held, SocketAddr) {
    let ip = Ipv4Addr::LOCALHOST;
    let (socket, port) =
        bound_socket(libc::SOCK_STREAM, ip, 0).unwrap_or_else(|err| panic!("bind {ip}:0: {err}"));
    (vec![socket], SocketAddr::V4(SocketAddrV4::new(ip, port)))
}

/// A loopback address that only this process's tests use. All of 127/8 is local on Linux,
/// and the process id keeps test processes apart: `pid_max` is at most 2^22.
pub fn private_loopback_ip() -> Ipv4Addr {
    let pid = std::process::id();
    Ipv4Addr::new(
        127,
        64 | ((pid >> 16) as u8 & 0x3f),
        (pid >> 8) as u8,
        pid as u8,
    )
}

/// TCP and UDP sockets on [`GUARD_IP`] for `count` port numbers in a row, the first of
/// which the kernel picked. The numbers are theirs alone among every socket on the host
/// that is bound to that address or to the wildcard one.
fn guarded_ports(count: u16) -> (Held, u16) {
    // A first port that did not work out stays held, so the kernel does not offer it again.
    let mut passed_over = Vec::new();
    'first: for _ in 0..200 {
        let (tcp, first) = bound_socket(libc::SOCK_STREAM, GUARD_IP, 0)
            .unwrap_or_else(|err| panic!("bind {GUARD_IP}:0: {err}"));
        let mut held = vec![tcp];
        for index in 0..count {
            let Some(port) = first.checked_add(index) else {
                passed_over.append(&mut held);
                continue 'first;
            };
            let kinds = [libc::SOCK_STREAM, libc::SOCK_DGRAM];
            // The first port's TCP socket is the one that picked it.
            for kind in kinds.into_iter().skip(usize::from(index == 0)) {
                match bound_socket(kind, GUARD_IP, port) {
                    Ok((socket, _)) => held.push(socket),
                    Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => {
                        passed_over.append(&mut held);
                        continue 'first;
                    }
                    Err(err) => panic!("bind {GUARD_IP}:{port}: {err}"),
                }
            }
        }
        return (held, first);
    }
    panic!("no {count} ports in a row were free of every wildcard socket in 200 tries");
}

/// An address for a program under test to bind, TCP or UDP, that nothing else on the host
/// can take while `Held` is held.
///
/// The address is [`private_loopback_ip`], which no other process binds. A socket bound to
/// the wildcard address would take the port on every address, so `Held` keeps the port
/// number bound on [`GUARD_IP`]: the kernel refuses a wildcard bind of a port that is
/// bound on any address, and never picks it for one that asks for port 0. Two calls in one
/// process get different ports for the same reason.
///
/// The program must bind the address it is given. A wildcard bind of the port fails
/// against `Held`.
pub fn reserved_port() -> (Held, SocketAddrV4) {
    reserved_ports_in_a_row(1)
}

/// [`reserved_port`] for `count` ports in a row on one address; the first is returned.
///
/// A test binds its own listeners on these by number. A port 0 bind on
/// [`private_loopback_ip`] could be given a port another test of the process has reserved.
pub fn reserved_ports_in_a_row(count: u16) -> (Held, SocketAddrV4) {
    let (held, first) = guarded_ports(count);
    (held, SocketAddrV4::new(private_loopback_ip(), first))
}

/// A port for a program under test to bind on `::1`, which is the one IPv6 loopback
/// address, so a test cannot have a private one.
///
/// The port is below the range the kernel assigns from, so no socket that asks for port 0
/// gets it, on any address. It is the offset of a guarded port in that range, so two
/// holders never have the same one. What is left is a socket that binds this number by
/// name on `::1` or `[::]` in the host's network namespace, which no test in the tree does.
pub fn reserved_ipv6_loopback_port() -> (Held, u16) {
    let range = std::fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range")
        .expect("reading ip_local_port_range");
    let mut bounds = range.split_whitespace().map(|n| n.parse::<u16>().unwrap());
    let (low, high) = (bounds.next().unwrap(), bounds.next().unwrap());
    assert!(
        high - low < low - 1024,
        "the assigned range {low}-{high} is wider than the ports below it"
    );
    let (held, port) = guarded_ports(1);
    assert!(
        (low..=high).contains(&port),
        "{port} is outside {low}-{high}"
    );
    (held, 1024 + (port - low))
}

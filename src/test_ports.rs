//! Addresses and ports that tests ask the kernel for.
//!
//! A test that needs an address nothing listens on used to bind a listener to port 0, read
//! its address and drop it. Once the listener is closed the kernel can give that port to
//! any socket on the host that binds port 0, so the address is not the test's any more. In
//! the unit lane the other sockets are the other tests' listeners: a connect the test
//! expects to be refused reaches one of them and succeeds. That is how
//! `a_proxy_relay_tries_every_address_of_its_proxy` failed (#2259): 5 runs in 2000 with
//! another process binding and closing loopback listeners beside it, none in 2000 alone.
//!
//! [`refused_address`] is the address for that: it is the test's for as long as the test
//! holds what it returns. [`released_port`] is the one place that still closes a listener
//! and hands out its port, for the case a held socket cannot serve.
//!
//! `tests/test_listener_ports.rs` fails on a listener bound only for its address anywhere
//! else in the tree.
//!
//! Unit tests reach this as a `cfg(test)` module of the library. Integration tests and the
//! other crates' tests include the file by path, since they link the library without its
//! `cfg(test)` modules.
#![allow(dead_code)]

use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

/// A loopback address that refuses connections for as long as the returned socket is held.
///
/// The socket is bound and never listens, so a connect to its address is reset. It is
/// bound without `SO_REUSEADDR`, so no other socket can bind its port, whether it names
/// the port or asks for port 0.
pub fn refused_address() -> (OwnedFd, SocketAddr) {
    refused_address_on(Ipv4Addr::LOCALHOST)
}

/// [`refused_address`] on a loopback address of the caller's choosing.
pub fn refused_address_on(ip: Ipv4Addr) -> (OwnedFd, SocketAddr) {
    // SAFETY: socket() takes no pointers; a descriptor it returns is owned by nothing else.
    let held = unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
        assert!(fd >= 0, "socket: {}", std::io::Error::last_os_error());
        OwnedFd::from_raw_fd(fd)
    };
    // SAFETY: an all-zero sockaddr_in is a valid value of the type.
    let mut name: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    name.sin_family = libc::AF_INET as _;
    name.sin_addr.s_addr = u32::from(ip).to_be();
    let mut len = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
    // SAFETY: `name` is a sockaddr_in of `len` bytes that outlives both calls.
    unsafe {
        let name = std::ptr::addr_of_mut!(name).cast::<libc::sockaddr>();
        assert_eq!(
            libc::bind(held.as_raw_fd(), name, len),
            0,
            "bind {ip}:0: {}",
            std::io::Error::last_os_error()
        );
        assert_eq!(
            libc::getsockname(held.as_raw_fd(), name, &mut len),
            0,
            "getsockname: {}",
            std::io::Error::last_os_error()
        );
    }
    let port = u16::from_be(name.sin_port);
    (held, SocketAddr::V4(SocketAddrV4::new(ip, port)))
}

/// A port on `ip` that was free when this returned, for a program under test that binds
/// the port itself: a child process given `--listen`, pasta given a mapping.
///
/// Nothing holds the port. The kernel can give it to another socket before the program
/// binds it, and the program then fails to bind and says so. A caller that expects the
/// address to refuse, or to stay silent, must not use this: a stranger that takes the port
/// answers there. That caller wants [`refused_address`].
pub fn released_port(ip: IpAddr) -> u16 {
    let listener = std::net::TcpListener::bind((ip, 0))
        .unwrap_or_else(|err| panic!("binding a port on {ip}: {err}"));
    listener.local_addr().unwrap().port()
}

//! Destroy every TCP socket of the calling process's network namespace.
//!
//! A pooled namespace outlives the processes that used it. A TCP connection one of them
//! had to the guest does not end when its descriptor closes: the kernel keeps the socket to
//! retransmit what the guest never acknowledged, for minutes, and those segments would reach
//! the next guest at the same address. `SOCK_DESTROY` (`CONFIG_INET_DIAG_DESTROY`) drops a
//! socket at once and sends nothing more. The namespace's reset destroys every one of them
//! instead of tracking which sockets a VM's processes made.

use super::rtnl::Rtnl;
use std::io;

const NETLINK_SOCK_DIAG: libc::c_int = 4;
const SOCK_DIAG_BY_FAMILY: u16 = 20;
const SOCK_DESTROY: u16 = 21;
/// `sizeof(struct inet_diag_sockid)`: ports, addresses, interface, cookie.
const SOCKID_LEN: usize = 48;
/// The socket's identity within `struct inet_diag_msg`: after family, state, timer, retrans.
const MSG_SOCKID_AT: usize = 4;

/// `struct inet_diag_req_v2` for TCP sockets of `family` in every state.
fn request(family: u8, sockid: &[u8]) -> Vec<u8> {
    let mut request = vec![family, libc::IPPROTO_TCP as u8, 0, 0];
    request.extend_from_slice(&u32::MAX.to_ne_bytes());
    request.extend_from_slice(sockid);
    request
}

/// Destroy every TCP socket in this namespace, in any state, and return how many there
/// were. A socket that went away between the listing and its destruction is not an error.
pub fn destroy_tcp_sockets() -> io::Result<usize> {
    let mut diag = Rtnl::open_protocol(NETLINK_SOCK_DIAG)?;
    let mut destroyed = 0;
    for family in [libc::AF_INET as u8, libc::AF_INET6 as u8] {
        let sockets = diag.dump(SOCK_DIAG_BY_FAMILY, &request(family, &[0u8; SOCKID_LEN]))?;
        for socket in sockets {
            let Some(sockid) = socket
                .payload
                .get(MSG_SOCKID_AT..MSG_SOCKID_AT + SOCKID_LEN)
            else {
                continue;
            };
            match diag.change(SOCK_DESTROY, 0, &request(family, sockid)) {
                Ok(()) => destroyed += 1,
                Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {}
                Err(error) => return Err(error),
            }
        }
    }
    Ok(destroyed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The request is the kernel's `inet_diag_req_v2`: 8 bytes of selector, then the
    /// 48-byte socket identity.
    #[test]
    fn the_request_is_an_inet_diag_req_v2() {
        let all = request(libc::AF_INET as u8, &[0u8; SOCKID_LEN]);
        assert_eq!(all.len(), 56);
        assert_eq!(&all[..8], &[2, 6, 0, 0, 0xff, 0xff, 0xff, 0xff]);
        let one = request(libc::AF_INET6 as u8, &[7u8; SOCKID_LEN]);
        assert_eq!(one[0], 10);
        assert_eq!(&one[8..], &[7u8; SOCKID_LEN]);
    }
}

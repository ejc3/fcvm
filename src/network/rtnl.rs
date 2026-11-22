//! The few rtnetlink requests the namespace agent makes, spoken directly: a bridge, link
//! attributes, an address, and dumps to read back what a namespace holds.
//!
//! Every request is acknowledged (`NLM_F_ACK`) and its error returned, so a refused step
//! is the caller's error at that step, as a failed `ip` command was. The socket subscribes
//! to no multicast group: the kernel walks every NETLINK_ROUTE multicast subscriber on the
//! host for each link notification it sends, in any namespace, so a listener per namespace
//! lengthens every `rtnl_mutex` hold on the host.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

const NLMSG_HDRLEN: usize = 16;
const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;

const NLM_F_REQUEST: u16 = 0x1;
const NLM_F_ACK: u16 = 0x4;
const NLM_F_DUMP: u16 = 0x300;
const NLM_F_EXCL: u16 = 0x200;
const NLM_F_CREATE: u16 = 0x400;

pub const RTM_NEWLINK: u16 = 16;
pub const RTM_GETLINK: u16 = 18;
pub const RTM_NEWADDR: u16 = 20;
pub const RTM_GETADDR: u16 = 22;
pub const RTM_GETROUTE: u16 = 26;
pub const RTM_NEWNEIGH: u16 = 28;
pub const RTM_DELNEIGH: u16 = 29;
pub const RTM_GETNEIGH: u16 = 30;
pub const RTM_GETQDISC: u16 = 38;

const IFLA_ADDRESS: u16 = 1;
const IFLA_IFNAME: u16 = 3;
const IFLA_MTU: u16 = 4;
const IFLA_MASTER: u16 = 10;
const IFLA_LINKINFO: u16 = 18;
const IFLA_INFO_KIND: u16 = 1;
const IFLA_AF_SPEC: u16 = 26;
const IFLA_INET6_ADDR_GEN_MODE: u16 = 8;
const IN6_ADDR_GEN_MODE_NONE: u8 = 1;

const IFA_ADDRESS: u16 = 1;
const IFA_LOCAL: u16 = 2;

/// One attribute: `rtattr` header, the value, padding to four bytes.
fn attr(out: &mut Vec<u8>, kind: u16, value: &[u8]) {
    let len = 4 + value.len();
    out.extend_from_slice(&(len as u16).to_ne_bytes());
    out.extend_from_slice(&kind.to_ne_bytes());
    out.extend_from_slice(value);
    out.resize(out.len().next_multiple_of(4), 0);
}

/// `struct ifinfomsg` for the link `index`, setting the `flags` named in `change`.
fn ifinfomsg(index: u32, flags: u32, change: u32) -> Vec<u8> {
    let mut out = vec![libc::AF_UNSPEC as u8, 0, 0, 0];
    out.extend_from_slice(&(index as i32).to_ne_bytes());
    out.extend_from_slice(&flags.to_ne_bytes());
    out.extend_from_slice(&change.to_ne_bytes());
    out
}

/// What [`Rtnl::set_link`] changes about one link.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LinkChange {
    pub mtu: Option<u32>,
    /// The bridge the link becomes a port of.
    pub master: Option<u32>,
    pub up: bool,
    /// No automatic IPv6 link-local address (`addrgenmode none`): nothing in a VM's
    /// namespace speaks IPv6 from the namespace's own stack, and the address, its
    /// duplicate detection and its multicast memberships are work on every carrier change.
    pub no_ipv6_link_local: bool,
}

/// A message the kernel sent back: its type and its payload after the netlink header.
pub struct Message {
    pub kind: u16,
    pub payload: Vec<u8>,
}

impl Message {
    /// The attributes that follow a fixed header of `skip` bytes, as `(type, value)`.
    pub fn attrs(&self, skip: usize) -> Vec<(u16, &[u8])> {
        let mut found = Vec::new();
        let mut rest = self.payload.get(skip.next_multiple_of(4)..).unwrap_or(&[]);
        while rest.len() >= 4 {
            let len = u16::from_ne_bytes([rest[0], rest[1]]) as usize;
            let kind = u16::from_ne_bytes([rest[2], rest[3]]) & 0x3fff;
            if len < 4 || len > rest.len() {
                break;
            }
            found.push((kind, &rest[4..len]));
            rest = rest.get(len.next_multiple_of(4)..).unwrap_or(&[]);
        }
        found
    }
}

/// A NETLINK_ROUTE socket in the calling process's network namespace.
pub struct Rtnl {
    socket: OwnedFd,
    sequence: u32,
}

impl Rtnl {
    pub fn open() -> io::Result<Self> {
        // SAFETY: socket takes no pointers.
        let fd = unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC,
                libc::NETLINK_ROUTE,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: socket returned a new descriptor nothing else owns.
        let socket = unsafe { OwnedFd::from_raw_fd(fd) };
        // Strict checking makes the kernel refuse a dump request it would otherwise answer
        // with every object, filter ignored.
        let on: libc::c_int = 1;
        // SAFETY: the option value is one int that outlives the call.
        unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                libc::SOL_NETLINK,
                libc::NETLINK_GET_STRICT_CHK,
                (&on as *const libc::c_int).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
        Ok(Self {
            socket,
            sequence: 0,
        })
    }

    fn send(&mut self, kind: u16, flags: u16, payload: &[u8]) -> io::Result<u32> {
        self.sequence += 1;
        let mut message = Vec::with_capacity(NLMSG_HDRLEN + payload.len());
        message.extend_from_slice(&((NLMSG_HDRLEN + payload.len()) as u32).to_ne_bytes());
        message.extend_from_slice(&kind.to_ne_bytes());
        message.extend_from_slice(&(NLM_F_REQUEST | flags).to_ne_bytes());
        message.extend_from_slice(&self.sequence.to_ne_bytes());
        message.extend_from_slice(&0u32.to_ne_bytes());
        message.extend_from_slice(payload);
        // SAFETY: the buffer outlives the call.
        let sent = unsafe {
            libc::send(
                self.socket.as_raw_fd(),
                message.as_ptr().cast(),
                message.len(),
                0,
            )
        };
        if sent < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(self.sequence)
    }

    /// Read replies to request `sequence` until its acknowledgement or the end of its dump,
    /// collecting everything else it was sent. A kernel error becomes this call's error.
    fn replies(&mut self, sequence: u32) -> io::Result<Vec<Message>> {
        let mut messages = Vec::new();
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            // SAFETY: the buffer is writable for its whole length.
            let got = unsafe {
                libc::recv(
                    self.socket.as_raw_fd(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                    0,
                )
            };
            if got < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            let mut rest = &buffer[..got as usize];
            while rest.len() >= NLMSG_HDRLEN {
                let len = u32::from_ne_bytes(rest[0..4].try_into().unwrap()) as usize;
                let kind = u16::from_ne_bytes(rest[4..6].try_into().unwrap());
                let seq = u32::from_ne_bytes(rest[8..12].try_into().unwrap());
                if len < NLMSG_HDRLEN || len > rest.len() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "truncated netlink message",
                    ));
                }
                let payload = &rest[NLMSG_HDRLEN..len];
                rest = rest.get(len.next_multiple_of(4)..).unwrap_or(&[]);
                if seq != sequence {
                    continue;
                }
                match kind {
                    NLMSG_ERROR => {
                        let code = payload
                            .get(0..4)
                            .map(|b| i32::from_ne_bytes(b.try_into().unwrap()))
                            .unwrap_or(-libc::EIO);
                        if code != 0 {
                            return Err(io::Error::from_raw_os_error(-code));
                        }
                        return Ok(messages);
                    }
                    NLMSG_DONE => return Ok(messages),
                    _ => messages.push(Message {
                        kind,
                        payload: payload.to_vec(),
                    }),
                }
            }
        }
    }

    /// A change request: sent with `NLM_F_ACK`, done when the kernel acknowledges it.
    fn change(&mut self, kind: u16, flags: u16, payload: &[u8]) -> io::Result<()> {
        let sequence = self.send(kind, NLM_F_ACK | flags, payload)?;
        self.replies(sequence).map(drop)
    }

    /// Every object of one kind in this namespace (`RTM_GET*` with `NLM_F_DUMP`).
    /// `header` is the request's fixed header, zeroed but for the address family.
    pub fn dump(&mut self, kind: u16, header: &[u8]) -> io::Result<Vec<Message>> {
        let sequence = self.send(kind, NLM_F_DUMP, header)?;
        self.replies(sequence)
    }

    /// `ip link add NAME address MAC type bridge`.
    pub fn add_bridge(&mut self, name: &str, mac: [u8; 6]) -> io::Result<()> {
        let mut payload = ifinfomsg(0, 0, 0);
        let mut ifname = name.as_bytes().to_vec();
        ifname.push(0);
        attr(&mut payload, IFLA_IFNAME, &ifname);
        attr(&mut payload, IFLA_ADDRESS, &mac);
        let mut info = Vec::new();
        attr(&mut info, IFLA_INFO_KIND, b"bridge");
        attr(&mut payload, IFLA_LINKINFO, &info);
        self.change(RTM_NEWLINK, NLM_F_CREATE | NLM_F_EXCL, &payload)
    }

    /// `ip link set dev INDEX ...`: every change of `change` in one request, so in one
    /// `rtnl_mutex` hold. The kernel applies the MTU, then the master, then the flags, then
    /// the address generation mode.
    pub fn set_link(&mut self, index: u32, change: &LinkChange) -> io::Result<()> {
        let up = if change.up { libc::IFF_UP as u32 } else { 0 };
        let mut payload = ifinfomsg(index, up, up);
        if let Some(mtu) = change.mtu {
            attr(&mut payload, IFLA_MTU, &mtu.to_ne_bytes());
        }
        if let Some(master) = change.master {
            attr(&mut payload, IFLA_MASTER, &master.to_ne_bytes());
        }
        if change.no_ipv6_link_local {
            let mut mode = Vec::new();
            attr(
                &mut mode,
                IFLA_INET6_ADDR_GEN_MODE,
                &[IN6_ADDR_GEN_MODE_NONE],
            );
            let mut inet6 = Vec::new();
            attr(&mut inet6, libc::AF_INET6 as u16, &mode);
            attr(&mut payload, IFLA_AF_SPEC, &inet6);
        }
        self.change(RTM_NEWLINK, 0, &payload)
    }

    /// `ip addr add ADDRESS/PREFIX dev INDEX`.
    pub fn add_address(
        &mut self,
        index: u32,
        address: std::net::Ipv4Addr,
        prefix: u8,
    ) -> io::Result<()> {
        // struct ifaddrmsg: family, prefix length, flags, scope, index.
        let mut payload = vec![libc::AF_INET as u8, prefix, 0, 0];
        payload.extend_from_slice(&index.to_ne_bytes());
        attr(&mut payload, IFA_LOCAL, &address.octets());
        attr(&mut payload, IFA_ADDRESS, &address.octets());
        self.change(RTM_NEWADDR, NLM_F_CREATE | NLM_F_EXCL, &payload)
    }
}

/// The index of the link `name` in this namespace.
pub fn link_index(name: &str) -> io::Result<u32> {
    let name = std::ffi::CString::new(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "link name holds a NUL"))?;
    // SAFETY: the name is NUL-terminated and outlives the call.
    match unsafe { libc::if_nametoindex(name.as_ptr()) } {
        0 => Err(io::Error::last_os_error()),
        index => Ok(index),
    }
}

const TUNSETIFF: libc::c_ulong = 0x4004_54ca;
const TUNSETPERSIST: libc::c_ulong = 0x4004_54cb;
const IFF_TAP: libc::c_short = 0x0002;
const IFF_NO_PI: libc::c_short = 0x1000;

/// `ip tuntap add NAME mode tap`: a TAP device that stays when the descriptor that made it
/// closes. Whoever opens it later attaches to this device instead of making one.
pub fn add_tap(name: &str) -> io::Result<()> {
    #[repr(C)]
    struct IfReq {
        name: [u8; libc::IFNAMSIZ],
        flags: libc::c_short,
        pad: [u8; 22],
    }
    if name.len() >= libc::IFNAMSIZ {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("TAP name {name} is too long"),
        ));
    }
    let mut request = IfReq {
        name: [0; libc::IFNAMSIZ],
        flags: IFF_TAP | IFF_NO_PI,
        pad: [0; 22],
    };
    request.name[..name.len()].copy_from_slice(name.as_bytes());
    // SAFETY: the path is a NUL-terminated literal.
    let fd = unsafe { libc::open(c"/dev/net/tun".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: open returned a new descriptor nothing else owns.
    let tun = unsafe { OwnedFd::from_raw_fd(fd) };
    // The request's type is the libc's choice (c_ulong on glibc, c_int on musl).
    // SAFETY: `request` is a `struct ifreq` that outlives the call.
    if unsafe { libc::ioctl(tun.as_raw_fd(), TUNSETIFF as _, &request) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: TUNSETPERSIST takes its argument by value.
    if unsafe { libc::ioctl(tun.as_raw_fd(), TUNSETPERSIST as _, 1 as libc::c_ulong) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An attribute is its four-byte header, its value, and zeros up to a multiple of four;
    /// the length in the header does not count the padding.
    #[test]
    fn attributes_are_padded_to_four_bytes_and_parsed_back() {
        let mut payload = ifinfomsg(7, libc::IFF_UP as u32, libc::IFF_UP as u32);
        assert_eq!(payload.len(), 16);
        attr(&mut payload, IFLA_IFNAME, b"br0\0");
        attr(&mut payload, IFLA_ADDRESS, &[2, 0xfc, 0, 0, 2, 1]);
        attr(&mut payload, IFLA_MTU, &65520u32.to_ne_bytes());
        assert_eq!(payload.len(), 16 + 8 + 12 + 8);
        assert_eq!(&payload[16..20], &[8, 0, IFLA_IFNAME as u8, 0]);
        assert_eq!(&payload[24..28], &[10, 0, IFLA_ADDRESS as u8, 0]);
        assert_eq!(&payload[34..36], &[0, 0], "padding after a six-byte value");
        let message = Message {
            kind: RTM_NEWLINK,
            payload,
        };
        let attrs = message.attrs(16);
        assert_eq!(
            attrs,
            vec![
                (IFLA_IFNAME, &b"br0\0"[..]),
                (IFLA_ADDRESS, &[2, 0xfc, 0, 0, 2, 1][..]),
                (IFLA_MTU, &65520u32.to_ne_bytes()[..]),
            ]
        );
    }

    /// A nested attribute's type carries flag bits (`NLA_F_NESTED`) that are not part of
    /// the type, and a truncated attribute ends the list instead of reading past it.
    #[test]
    fn attribute_parsing_masks_flags_and_stops_at_a_short_attribute() {
        let mut payload = vec![0u8; 4];
        payload.extend_from_slice(&[8, 0, 18, 0x80, 1, 2, 3, 4]);
        payload.extend_from_slice(&[40, 0, 1, 0, 9, 9]);
        let message = Message { kind: 0, payload };
        assert_eq!(message.attrs(4), vec![(IFLA_LINKINFO, &[1u8, 2, 3, 4][..])]);
    }
}

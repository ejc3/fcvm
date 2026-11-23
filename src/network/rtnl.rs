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
pub const RTM_DELADDR: u16 = 21;
pub const RTM_GETADDR: u16 = 22;
pub const RTM_GETROUTE: u16 = 26;
pub const RTM_NEWNEIGH: u16 = 28;
pub const RTM_DELNEIGH: u16 = 29;
pub const RTM_GETNEIGH: u16 = 30;
pub const RTM_GETRULE: u16 = 34;
pub const RTM_GETQDISC: u16 = 38;
pub const RTM_GETNETCONF: u16 = 82;

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

/// How many times [`Rtnl::dump`] asks for a dump the kernel keeps interrupting.
const DUMP_ATTEMPTS: usize = 4;
/// `NLM_F_DUMP_INTR`: set on a dump's messages when the table changed under it.
const NLM_F_DUMP_INTR: u16 = 0x10;
const INTERRUPTED_DUMP: &str =
    "the kernel interrupted the dump: the table changed while it was read";

fn is_interrupted_dump(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::Interrupted && error.to_string().contains(INTERRUPTED_DUMP)
}

/// Take the messages of request `sequence` out of one datagram into `messages`, and say
/// whether the request is finished (its acknowledgement or the end of its dump was in the
/// datagram).
///
/// Errors: a kernel error, in an `NLMSG_ERROR` or in the code an `NLMSG_DONE` carries (a
/// dump that failed part-way ends with one), and a dump any of whose messages has
/// `NLM_F_DUMP_INTR`. The last is reported at the dump's end, so the socket is left with
/// nothing of this request unread.
fn take_replies(
    datagram: &[u8],
    sequence: u32,
    messages: &mut Vec<Message>,
    interrupted: &mut bool,
) -> io::Result<bool> {
    let mut rest = datagram;
    while rest.len() >= NLMSG_HDRLEN {
        let len = u32::from_ne_bytes(rest[0..4].try_into().unwrap()) as usize;
        let kind = u16::from_ne_bytes(rest[4..6].try_into().unwrap());
        let flags = u16::from_ne_bytes(rest[6..8].try_into().unwrap());
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
        if flags & NLM_F_DUMP_INTR != 0 {
            *interrupted = true;
        }
        let code = || {
            payload
                .get(0..4)
                .map(|b| i32::from_ne_bytes(b.try_into().unwrap()))
        };
        match kind {
            NLMSG_ERROR => {
                let code = code().unwrap_or(-libc::EIO);
                if code != 0 {
                    return Err(io::Error::from_raw_os_error(-code));
                }
                return Ok(true);
            }
            NLMSG_DONE => {
                // The end of a dump carries the dump's own result; an old kernel sends
                // none, which is success.
                if let Some(code) = code().filter(|code| *code < 0) {
                    return Err(io::Error::from_raw_os_error(-code));
                }
                if *interrupted {
                    return Err(io::Error::new(io::ErrorKind::Interrupted, INTERRUPTED_DUMP));
                }
                return Ok(true);
            }
            _ => messages.push(Message {
                kind,
                payload: payload.to_vec(),
            }),
        }
    }
    Ok(false)
}

/// A NETLINK_ROUTE socket in the calling process's network namespace.
pub struct Rtnl {
    socket: OwnedFd,
    sequence: u32,
}

impl Rtnl {
    pub fn open() -> io::Result<Self> {
        Self::open_protocol(libc::NETLINK_ROUTE)
    }

    /// A netlink socket of another protocol (`NETLINK_NETFILTER`, `NETLINK_SOCK_DIAG`),
    /// with the same request, acknowledgement and dump framing.
    pub fn open_protocol(protocol: libc::c_int) -> io::Result<Self> {
        // SAFETY: socket takes no pointers.
        let fd = unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC,
                protocol,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: socket returned a new descriptor nothing else owns.
        let socket = unsafe { OwnedFd::from_raw_fd(fd) };
        // Strict checking makes the kernel refuse a dump request it would otherwise answer
        // with every object, filter ignored. A socket without it is not used: a dump that
        // silently ignored its filter would read as a namespace's whole state.
        let on: libc::c_int = 1;
        // SAFETY: the option value is one int that outlives the call.
        let set = unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                libc::SOL_NETLINK,
                libc::NETLINK_GET_STRICT_CHK,
                (&on as *const libc::c_int).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if set != 0 {
            return Err(io::Error::last_os_error());
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
    /// collecting everything else it was sent. A kernel error becomes this call's error,
    /// and so does a dump the kernel says was interrupted ([`take_replies`]).
    fn replies(&mut self, sequence: u32) -> io::Result<Vec<Message>> {
        let mut messages = Vec::new();
        let mut interrupted = false;
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
            if take_replies(
                &buffer[..got as usize],
                sequence,
                &mut messages,
                &mut interrupted,
            )? {
                return Ok(messages);
            }
        }
    }

    /// A change request: sent with `NLM_F_ACK`, done when the kernel acknowledges it.
    pub fn change(&mut self, kind: u16, flags: u16, payload: &[u8]) -> io::Result<()> {
        let sequence = self.send(kind, NLM_F_ACK | flags, payload)?;
        self.replies(sequence).map(drop)
    }

    /// Every object of one kind in this namespace (`RTM_GET*` with `NLM_F_DUMP`).
    /// `header` is the request's fixed header, zeroed but for the address family.
    ///
    /// A dump the kernel marks interrupted (the table changed while it was read, so the
    /// listing may miss or repeat objects) is asked for again, a few times, and is an error
    /// after that: a listing known to be inconsistent is never returned.
    pub fn dump(&mut self, kind: u16, header: &[u8]) -> io::Result<Vec<Message>> {
        let mut last = None;
        for _ in 0..DUMP_ATTEMPTS {
            let sequence = self.send(kind, NLM_F_DUMP, header)?;
            match self.replies(sequence) {
                Err(error) if is_interrupted_dump(&error) => last = Some(error),
                other => return other,
            }
        }
        Err(last.expect("at least one attempt was made"))
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
    /// the address generation mode: a link that can have a carrier when it comes up must
    /// get `no_ipv6_link_local` in an earlier request than `up`, or it generates an address
    /// in between (`netns_agent::build` does that for the bridge).
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

const NDA_DST: u16 = 1;
impl Rtnl {
    /// Delete the neighbour entry for `address` (4 or 16 bytes) on link `index`, whatever
    /// its state: `ip neigh del ADDRESS dev INDEX`.
    pub fn del_neighbour(&mut self, index: u32, family: u8, address: &[u8]) -> io::Result<()> {
        // struct ndmsg: family, two pad bytes, a pad short, index, state, flags, type.
        let mut payload = vec![family, 0, 0, 0];
        payload.extend_from_slice(&(index as i32).to_ne_bytes());
        payload.extend_from_slice(&[0, 0, 0, 0]);
        attr(&mut payload, NDA_DST, address);
        self.change(RTM_DELNEIGH, 0, &payload)
    }

    /// Delete the address `mac` that the bridge learned on its port `index`:
    /// `bridge fdb del MAC dev INDEX master`.
    pub fn del_learned_address(&mut self, index: u32, mac: &[u8]) -> io::Result<()> {
        const NDA_LLADDR: u16 = 2;
        const NTF_MASTER: u8 = 0x04;
        let mut payload = vec![libc::AF_BRIDGE as u8, 0, 0, 0];
        payload.extend_from_slice(&(index as i32).to_ne_bytes());
        payload.extend_from_slice(&[0, 0, NTF_MASTER, 0]);
        attr(&mut payload, NDA_LLADDR, mac);
        self.change(RTM_DELNEIGH, 0, &payload)
    }

    /// Remove every IPv4 address of this namespace that is not loopback's and add it
    /// again, and return how many there were.
    ///
    /// The kernel keeps what it learned about a destination from ICMP (a path MTU from
    /// "fragmentation needed", a gateway from a redirect) on the route that leads there, for
    /// ten minutes, and no netlink request deletes such an entry. Removing an address
    /// removes its connected and local routes and everything kept on them; adding it again
    /// makes the routes anew. `ip route flush cache` does the same through a file under
    /// /proc/sys, which a namespace of an unprivileged user does not have and whose lookup
    /// takes a lock every other namespace's lookups take.
    pub fn renew_addresses(&mut self) -> io::Result<usize> {
        const IFA_BROADCAST: u16 = 4;
        const RT_SCOPE_HOST: u8 = 254;
        let addresses = self.dump(RTM_GETADDR, &[libc::AF_INET as u8, 0, 0, 0, 0, 0, 0, 0])?;
        let mut renewed = 0;
        for address in addresses {
            let Some(header) = address.payload.get(..8) else {
                continue;
            };
            if header[3] == RT_SCOPE_HOST {
                continue;
            }
            // struct ifaddrmsg as dumped, without the flags the kernel set on its own
            // (permanent), and the attributes that say what the address is.
            let mut payload = vec![header[0], header[1], 0, header[3]];
            payload.extend_from_slice(&header[4..8]);
            for (kind, value) in address.attrs(8) {
                if [IFA_ADDRESS, IFA_LOCAL, IFA_BROADCAST].contains(&kind) {
                    attr(&mut payload, kind, value);
                }
            }
            self.change(RTM_DELADDR, 0, &payload)?;
            self.change(RTM_NEWADDR, NLM_F_CREATE | NLM_F_EXCL, &payload)?;
            renewed += 1;
        }
        Ok(renewed)
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

    /// One netlink message: header and payload, padded.
    fn message(kind: u16, flags: u16, sequence: u32, payload: &[u8]) -> Vec<u8> {
        let mut bytes = ((NLMSG_HDRLEN + payload.len()) as u32)
            .to_ne_bytes()
            .to_vec();
        bytes.extend_from_slice(&kind.to_ne_bytes());
        bytes.extend_from_slice(&flags.to_ne_bytes());
        bytes.extend_from_slice(&sequence.to_ne_bytes());
        bytes.extend_from_slice(&0u32.to_ne_bytes());
        bytes.extend_from_slice(payload);
        bytes.resize(bytes.len().next_multiple_of(4), 0);
        bytes
    }

    fn taken(datagrams: &[Vec<u8>]) -> io::Result<(bool, usize)> {
        let (mut messages, mut interrupted, mut done) = (Vec::new(), false, false);
        for datagram in datagrams {
            done = take_replies(datagram, 7, &mut messages, &mut interrupted)?;
        }
        Ok((done, messages.len()))
    }

    /// A dump is whole only when the kernel says so: its end carries no error and none of
    /// its messages was marked interrupted. Anything else is an error, never a shorter
    /// listing: the pool's check of a namespace is made of these listings.
    #[test]
    fn a_dump_that_failed_or_was_interrupted_is_an_error() {
        const NLM_F_MULTI: u16 = 0x2;
        let object = |flags| message(RTM_NEWLINK, NLM_F_MULTI | flags, 7, &[1u8; 16]);
        let done = |code: i32| message(NLMSG_DONE, NLM_F_MULTI, 7, &code.to_ne_bytes());

        let whole = [[object(0), object(0)].concat(), done(0)];
        assert_eq!(taken(&whole).unwrap(), (true, 2));
        // Not finished until the end arrives; another request's messages are not counted.
        assert_eq!(taken(&whole[..1]).unwrap(), (false, 2));
        let other = message(RTM_NEWLINK, NLM_F_MULTI, 8, &[1u8; 16]);
        assert_eq!(
            taken(&[[other, object(0)].concat(), done(0)]).unwrap(),
            (true, 1)
        );
        // A kernel before the code existed ends a dump with an empty payload.
        let bare = message(NLMSG_DONE, NLM_F_MULTI, 7, &[]);
        assert_eq!(taken(&[object(0), bare]).unwrap(), (true, 1));

        let failed = taken(&[object(0), done(-libc::ENOMEM)]).unwrap_err();
        assert_eq!(failed.raw_os_error(), Some(libc::ENOMEM));

        // Interrupted on an object in an earlier datagram, reported at the end.
        let marked = [object(0), object(NLM_F_DUMP_INTR), object(0)];
        assert_eq!(taken(&marked).unwrap(), (false, 3));
        let interrupted = taken(&[marked.concat(), done(0)]).unwrap_err();
        assert!(is_interrupted_dump(&interrupted), "{interrupted}");
        // Or on the end itself.
        let end = message(
            NLMSG_DONE,
            NLM_F_MULTI | NLM_F_DUMP_INTR,
            7,
            &0i32.to_ne_bytes(),
        );
        assert!(is_interrupted_dump(&taken(&[object(0), end]).unwrap_err()));
        // EINTR from recv is another thing.
        assert!(!is_interrupted_dump(&io::Error::from_raw_os_error(
            libc::EINTR
        )));

        let refused = message(NLMSG_ERROR, 0, 7, &(-libc::EPERM).to_ne_bytes());
        assert_eq!(
            taken(&[refused]).unwrap_err().raw_os_error(),
            Some(libc::EPERM)
        );
        let acknowledged = message(NLMSG_ERROR, 0, 7, &0i32.to_ne_bytes());
        assert_eq!(taken(&[acknowledged]).unwrap(), (true, 0));
    }

    /// Every socket has strict checking on, or is not handed out.
    #[test]
    fn a_socket_has_strict_checking_on() {
        let rtnl = Rtnl::open().unwrap();
        let mut on: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: both pointers are to locals that outlive the call.
        let rc = unsafe {
            libc::getsockopt(
                rtnl.socket.as_raw_fd(),
                libc::SOL_NETLINK,
                libc::NETLINK_GET_STRICT_CHK,
                (&mut on as *mut libc::c_int).cast(),
                &mut len,
            )
        };
        assert_eq!((rc, on), (0, 1));
    }

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

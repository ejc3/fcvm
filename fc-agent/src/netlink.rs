//! Netlink framing shared by the snapshot socket cleanup (NETLINK_SOCK_DIAG)
//! and the restored clone's IPv6 swap (NETLINK_ROUTE): the message header,
//! datagram walking, acknowledgement decoding, and a bound, connected kernel
//! socket.

use std::io;
use std::os::fd::OwnedFd;
use std::sync::atomic::AtomicU32;

use anyhow::{bail, Context, Result};

pub(crate) const NLMSG_NOOP: u16 = 1;
pub(crate) const NLMSG_ERROR: u16 = 2;
pub(crate) const NLMSG_DONE: u16 = 3;
pub(crate) const NLMSG_OVERRUN: u16 = 4;
pub(crate) const NLM_F_REQUEST: u16 = 0x01;
pub(crate) const NLM_F_ACK: u16 = 0x04;
pub(crate) const NLM_F_DUMP_INTR: u16 = 0x10;
pub(crate) const NLM_F_REPLACE: u16 = 0x100;
pub(crate) const NLM_F_EXCL: u16 = 0x200;
pub(crate) const NLM_F_DUMP: u16 = 0x300;
pub(crate) const NLM_F_CREATE: u16 = 0x400;

/// Sequence numbers for every netlink request this process sends, so each
/// reply can be matched to the request it answers.
pub(crate) static NEXT_NETLINK_SEQUENCE: AtomicU32 = AtomicU32::new(1);

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct NetlinkHeader {
    pub(crate) length: u32,
    pub(crate) message_type: u16,
    pub(crate) flags: u16,
    pub(crate) sequence: u32,
    pub(crate) port_id: u32,
}

const _: () = assert!(std::mem::size_of::<NetlinkHeader>() == 16);

/// Largest datagram a receive accepts. A longer one fails closed instead of
/// being read truncated.
const RECEIVE_BUFFER_BYTES: usize = 256 * 1024;

pub(crate) fn read_unaligned<T: Copy>(bytes: &[u8]) -> Result<T> {
    if bytes.len() < std::mem::size_of::<T>() {
        bail!(
            "short netlink payload: {} bytes, need {}",
            bytes.len(),
            std::mem::size_of::<T>()
        );
    }
    // SAFETY: length was checked and read_unaligned permits any byte alignment.
    Ok(unsafe { bytes.as_ptr().cast::<T>().read_unaligned() })
}

pub(crate) fn append_netlink_header(bytes: &mut Vec<u8>, header: NetlinkHeader) {
    bytes.extend_from_slice(&header.length.to_ne_bytes());
    bytes.extend_from_slice(&header.message_type.to_ne_bytes());
    bytes.extend_from_slice(&header.flags.to_ne_bytes());
    bytes.extend_from_slice(&header.sequence.to_ne_bytes());
    bytes.extend_from_slice(&header.port_id.to_ne_bytes());
}

pub(crate) const fn netlink_align(length: usize) -> usize {
    (length + 3) & !3
}

/// Visit every message in a netlink datagram until `callback` returns true.
/// Every message's length is validated, including messages the callback skips.
pub(crate) fn for_each_netlink_message(
    bytes: &[u8],
    mut callback: impl FnMut(NetlinkHeader, &[u8]) -> Result<bool>,
) -> Result<bool> {
    let header_size = std::mem::size_of::<NetlinkHeader>();
    let mut offset = 0usize;
    while offset < bytes.len() {
        let header: NetlinkHeader = read_unaligned(&bytes[offset..])?;
        let length = header.length as usize;
        if length < header_size || offset + length > bytes.len() {
            bail!("invalid netlink message length {length} at offset {offset}");
        }
        let payload = &bytes[offset + header_size..offset + length];
        if callback(header, payload)? {
            return Ok(true);
        }
        offset = offset
            .checked_add(netlink_align(length))
            .context("netlink message offset overflow")?;
    }
    Ok(false)
}

pub(crate) fn decode_netlink_error(payload: &[u8]) -> Result<(i32, NetlinkHeader)> {
    let error = read_unaligned::<i32>(payload)?;
    let request = read_unaligned::<NetlinkHeader>(&payload[std::mem::size_of::<i32>()..])?;
    Ok((error, request))
}

pub(crate) fn validate_error_request(
    request: NetlinkHeader,
    sequence: u32,
    expected_message_type: u16,
) -> Result<()> {
    if request.sequence != sequence || request.message_type != expected_message_type {
        bail!(
            "netlink acknowledgement described the wrong request: type={} sequence={} \
             (expected type={} sequence={})",
            request.message_type,
            request.sequence,
            expected_message_type,
            sequence
        );
    }
    Ok(())
}

/// The two halves of a netlink socket a request needs, so the protocols built
/// on it can be driven against a model of the kernel.
pub(crate) trait NetlinkChannel {
    fn send(&mut self, datagram: &[u8]) -> Result<()>;
    /// The next datagram the kernel queued on the socket.
    fn receive(&mut self) -> Result<&[u8]>;
    /// Ask for a receive buffer of `bytes`, and return the size the kernel
    /// reports for the buffer afterwards.
    fn reserve_receive_buffer(&mut self, bytes: usize) -> Result<usize>;
}

/// A bound, connected netlink socket and the buffer its replies are received
/// into.
pub(crate) struct KernelChannel {
    fd: OwnedFd,
    buffer: Vec<u8>,
    /// The protocol, for diagnostics.
    name: &'static str,
}

impl KernelChannel {
    /// Open a socket for netlink `protocol`. Every receive gives up after five
    /// seconds, so no wait for the kernel can hang the caller.
    pub(crate) fn open(protocol: i32, name: &'static str) -> Result<Self> {
        use std::os::fd::AsRawFd;

        let fd = crate::network::open_raw_socket(libc::AF_NETLINK, libc::SOCK_RAW, protocol)
            .with_context(|| format!("opening {name} socket"))?;
        let mut address: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        address.nl_family = libc::AF_NETLINK as u16;
        // SAFETY: address points to a fully initialized sockaddr_nl.
        let bind_result = unsafe {
            libc::bind(
                fd.as_raw_fd(),
                (&address as *const libc::sockaddr_nl).cast(),
                std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
            )
        };
        if bind_result < 0 {
            return Err(io::Error::last_os_error())
                .with_context(|| format!("binding {name} socket"));
        }
        // Connected netlink sockets can use send(2), avoiding a per-message
        // sockaddr and making every reply come from the kernel peer (pid zero).
        let connect_result = unsafe {
            libc::connect(
                fd.as_raw_fd(),
                (&address as *const libc::sockaddr_nl).cast(),
                std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
            )
        };
        if connect_result < 0 {
            return Err(io::Error::last_os_error())
                .with_context(|| format!("connecting {name} socket"));
        }
        let timeout = libc::timeval {
            tv_sec: 5,
            tv_usec: 0,
        };
        let timeout_result = unsafe {
            libc::setsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                (&timeout as *const libc::timeval).cast(),
                std::mem::size_of::<libc::timeval>() as libc::socklen_t,
            )
        };
        if timeout_result < 0 {
            return Err(io::Error::last_os_error())
                .with_context(|| format!("setting {name} receive timeout"));
        }
        Ok(Self {
            fd,
            buffer: vec![0u8; RECEIVE_BUFFER_BYTES],
            name,
        })
    }
}

impl NetlinkChannel for KernelChannel {
    fn send(&mut self, datagram: &[u8]) -> Result<()> {
        use std::os::fd::AsRawFd;

        let name = self.name;
        let sent = unsafe {
            libc::send(
                self.fd.as_raw_fd(),
                datagram.as_ptr().cast(),
                datagram.len(),
                0,
            )
        };
        if sent < 0 {
            return Err(io::Error::last_os_error())
                .with_context(|| format!("sending {name} request"));
        }
        if sent as usize != datagram.len() {
            bail!(
                "short {name} send: wrote {sent} of {} bytes",
                datagram.len()
            );
        }
        Ok(())
    }

    fn receive(&mut self) -> Result<&[u8]> {
        use std::os::fd::AsRawFd;

        let name = self.name;
        let mut peer: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        let mut iov = libc::iovec {
            iov_base: self.buffer.as_mut_ptr().cast(),
            iov_len: self.buffer.len(),
        };
        let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
        message.msg_name = (&mut peer as *mut libc::sockaddr_nl).cast();
        message.msg_namelen = std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t;
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        let received = unsafe { libc::recvmsg(self.fd.as_raw_fd(), &mut message, 0) };
        if received < 0 {
            return Err(io::Error::last_os_error())
                .with_context(|| format!("receiving {name} response"));
        }
        if received == 0 {
            bail!("{name} returned EOF before completing request");
        }
        if message.msg_flags & libc::MSG_TRUNC != 0 {
            bail!(
                "{name} datagram exceeded {} bytes; refusing a truncated reply",
                self.buffer.len()
            );
        }
        if message.msg_namelen < std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t
            || peer.nl_family != libc::AF_NETLINK as u16
            || peer.nl_pid != 0
        {
            bail!(
                "{name} response did not come from the kernel peer (family={} pid={})",
                peer.nl_family,
                peer.nl_pid
            );
        }
        Ok(&self.buffer[..received as usize])
    }

    fn reserve_receive_buffer(&mut self, bytes: usize) -> Result<usize> {
        use std::os::fd::AsRawFd;

        let name = self.name;
        let requested = libc::c_int::try_from(bytes)
            .with_context(|| format!("{name} receive buffer request is too large"))?;
        let set = |option| {
            // SAFETY: the value points to a c_int of the length passed.
            unsafe {
                libc::setsockopt(
                    self.fd.as_raw_fd(),
                    libc::SOL_SOCKET,
                    option,
                    (&requested as *const libc::c_int).cast(),
                    std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                )
            }
        };
        // SO_RCVBUFFORCE ignores net.core.rmem_default and rmem_max, so a guest
        // that lowered them cannot shrink the buffer a caller needs. It takes
        // CAP_NET_ADMIN; without it SO_RCVBUF still sets the buffer, up to
        // rmem_max.
        if set(libc::SO_RCVBUFFORCE) < 0 && set(libc::SO_RCVBUF) < 0 {
            return Err(io::Error::last_os_error())
                .with_context(|| format!("setting the {name} receive buffer"));
        }
        let mut granted: libc::c_int = 0;
        let mut length = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: granted and length describe a writable c_int.
        let result = unsafe {
            libc::getsockopt(
                self.fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVBUF,
                (&mut granted as *mut libc::c_int).cast(),
                &mut length,
            )
        };
        if result < 0 {
            return Err(io::Error::last_os_error())
                .with_context(|| format!("reading the {name} receive buffer"));
        }
        usize::try_from(granted)
            .with_context(|| format!("the kernel reported a negative {name} receive buffer"))
    }
}

/// What a dump does when the kernel marks it interrupted, which it does when
/// the table changed while the dump was being read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OnInterrupt {
    /// Fail: the caller needs one consistent view of the table.
    Refuse,
    /// Keep what was read, as iproute2 does, and report it to the caller.
    Keep,
}

/// Read the reply to the dump request sent with `sequence`, until NLMSG_DONE.
///
/// Each record of `record_type` goes to `record`, each message that answers
/// another request goes to `other`, and NLMSG_NOOP is skipped. An error
/// acknowledgement, an
/// overrun, a failed completion, a message of any other type, and under
/// `OnInterrupt::Refuse` a dump the kernel marked interrupted all fail the
/// dump. The kernel marks the message it was filling when the table changed,
/// which need not be NLMSG_DONE, so every message is checked. Returns whether
/// the dump was marked interrupted.
pub(crate) fn read_dump<C: NetlinkChannel>(
    channel: &mut C,
    sequence: u32,
    request_type: u16,
    record_type: u16,
    on_interrupt: OnInterrupt,
    mut record: impl FnMut(NetlinkHeader, &[u8]) -> Result<()>,
    mut other: impl FnMut(NetlinkHeader, &[u8]) -> Result<()>,
) -> Result<bool> {
    let mut interrupted = false;
    loop {
        let datagram = channel.receive()?;
        let done = for_each_netlink_message(datagram, |header, payload| {
            if header.sequence != sequence {
                other(header, payload)?;
                return Ok(false);
            }
            interrupted |= header.flags & NLM_F_DUMP_INTR != 0;
            match header.message_type {
                NLMSG_DONE => {
                    validate_dump_completion(payload)?;
                    return Ok(true);
                }
                NLMSG_ERROR => {
                    let (error, request) = decode_netlink_error(payload)?;
                    validate_error_request(request, sequence, request_type)?;
                    if error != 0 {
                        let errno = error
                            .checked_neg()
                            .filter(|errno| *errno > 0)
                            .context("the dump failed with an invalid netlink error")?;
                        bail!(
                            "the dump failed with errno {errno} ({})",
                            io::Error::from_raw_os_error(errno)
                        );
                    }
                }
                NLMSG_NOOP => {}
                NLMSG_OVERRUN => bail!("the dump overran its receive buffer"),
                kind if kind == record_type => record(header, payload)?,
                kind => bail!("unexpected message type {kind} in the dump"),
            }
            Ok(false)
        })?;
        if done {
            break;
        }
    }
    if interrupted && on_interrupt == OnInterrupt::Refuse {
        bail!("the dump was interrupted; refusing an incomplete dump");
    }
    Ok(interrupted)
}

/// Send `request`, which carries `sequence` and `message_type` and asks for an
/// acknowledgement, and wait for that acknowledgement. Replies to other
/// requests are skipped.
pub(crate) fn request_acknowledged<C: NetlinkChannel>(
    channel: &mut C,
    request: &[u8],
    sequence: u32,
    message_type: u16,
) -> Result<()> {
    channel.send(request)?;
    loop {
        let datagram = channel.receive()?;
        let acknowledged = for_each_netlink_message(datagram, |header, payload| {
            if header.sequence != sequence {
                return Ok(false);
            }
            match header.message_type {
                NLMSG_ERROR => {
                    let (error, request) = decode_netlink_error(payload)?;
                    validate_error_request(request, sequence, message_type)?;
                    if error != 0 {
                        let errno = error
                            .checked_neg()
                            .filter(|errno| *errno > 0)
                            .context("the request failed with an invalid netlink error")?;
                        return Err(io::Error::from_raw_os_error(errno).into());
                    }
                    Ok(true)
                }
                NLMSG_NOOP => Ok(false),
                kind => {
                    bail!("unexpected message type {kind} in reply to request type {message_type}")
                }
            }
        })?;
        if acknowledged {
            return Ok(());
        }
    }
}

/// Check the status an NLMSG_DONE carries: a native-endian i32, followed by
/// optional extended-ack attributes. Treating the message type alone as
/// success would pass a partial dump.
pub(crate) fn validate_dump_completion(payload: &[u8]) -> Result<()> {
    if payload.is_empty() {
        return Ok(());
    }
    let error = read_unaligned::<i32>(payload)?;
    if error == 0 {
        return Ok(());
    }
    let errno = error
        .checked_neg()
        .filter(|errno| *errno > 0)
        .context("dump completion contained an invalid netlink error")?;
    bail!(
        "dump completion failed with errno {errno} ({}); refusing an incomplete dump",
        io::Error::from_raw_os_error(errno)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    const RECORD: u16 = 20;

    /// Hands out scripted datagrams; an empty script is a receive timeout.
    #[derive(Default)]
    struct Script {
        datagrams: VecDeque<Vec<u8>>,
        current: Vec<u8>,
    }

    impl NetlinkChannel for Script {
        fn send(&mut self, _datagram: &[u8]) -> Result<()> {
            Ok(())
        }

        fn receive(&mut self) -> Result<&[u8]> {
            self.current = self.datagrams.pop_front().context("receive timed out")?;
            Ok(&self.current)
        }

        fn reserve_receive_buffer(&mut self, bytes: usize) -> Result<usize> {
            Ok(bytes)
        }
    }

    fn message(message_type: u16, flags: u16, sequence: u32, payload: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        append_netlink_header(
            &mut bytes,
            NetlinkHeader {
                length: (std::mem::size_of::<NetlinkHeader>() + payload.len()) as u32,
                message_type,
                flags,
                sequence,
                port_id: 0,
            },
        );
        bytes.extend_from_slice(payload);
        bytes.resize(netlink_align(bytes.len()), 0);
        bytes
    }

    #[test]
    fn an_errored_dump_completion_is_refused() {
        let error = validate_dump_completion(&(-libc::EINTR).to_ne_bytes())
            .expect_err("an errored completion must never pass a partial dump");
        let diagnostic = format!("{error:#}");
        assert!(
            diagnostic.contains("errno 4"),
            "unexpected diagnostic: {diagnostic}"
        );
        assert!(
            diagnostic.contains("incomplete dump"),
            "unexpected diagnostic: {diagnostic}"
        );
    }

    /// The kernel marks the message it was filling when the table changed,
    /// which need not be NLMSG_DONE.
    #[test]
    fn an_interrupted_dump_is_refused_or_kept_as_asked() {
        for on_interrupt in [OnInterrupt::Refuse, OnInterrupt::Keep] {
            let mut script = Script::default();
            script
                .datagrams
                .push_back(message(RECORD, NLM_F_DUMP_INTR, 7, &[0; 8]));
            script
                .datagrams
                .push_back(message(NLMSG_DONE, 0, 7, &0i32.to_ne_bytes()));
            let mut records = 0;
            let result = read_dump(
                &mut script,
                7,
                RECORD,
                RECORD,
                on_interrupt,
                |_, _| {
                    records += 1;
                    Ok(())
                },
                |_, _| Ok(()),
            );
            match on_interrupt {
                OnInterrupt::Refuse => {
                    let error = result.expect_err("an interrupted dump must be refused");
                    assert!(format!("{error:#}").contains("interrupted"), "{error:#}");
                }
                OnInterrupt::Keep => assert!(result.unwrap(), "the interruption was not reported"),
            }
            assert_eq!(records, 1, "{on_interrupt:?}");
        }
    }
}

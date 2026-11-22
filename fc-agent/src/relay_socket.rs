//! TCP_NODELAY on every socket a relay writes to.
//!
//! A relay writes each read, or each frame, as it arrives. A message one end sends in
//! two writes (a short frame, then the payload) therefore leaves the relay as two small
//! writes, and with Nagle on the second waits for the receiver's delayed ACK of the
//! first: 40 ms on loopback, per message. A relay that writes frames sends the small
//! frames already queued when its writer runs in one write (`take_queued_data`).

use std::sync::atomic::{AtomicBool, Ordering};

/// Turn Nagle off on a socket a relay writes to. A failure is logged once per `warned`
/// flag, one per relay: the connection still works, only slower, so a line per connection
/// says nothing new.
pub(crate) fn set_relay_nodelay(
    stream: &tokio::net::TcpStream,
    warned: &AtomicBool,
    relay: std::fmt::Arguments<'_>,
) {
    if let Err(e) = stream.set_nodelay(true) {
        if !warned.swap(true, Ordering::Relaxed) {
            eprintln!("[fc-agent] WARNING: {relay}: TCP_NODELAY: {e} (logged once)");
        }
    }
}

/// `take_queued_data` stops gathering once a write holds this many bytes; the frame that
/// crosses it is still taken, so a write can exceed it by one frame.
pub(crate) const MAX_COALESCED: usize = 64 * 1024;
/// A first frame at least this large is written as it is: gathering pays for itself on
/// small frames, and a backlog of full frames would only add copies.
pub(crate) const GATHER_BELOW: usize = 4 * 1024;

/// Append the DATA frames already queued on `rx` to `first`, without waiting, and hand
/// back the first of `ends` (CLOSE, RST) taken, which the caller acts on after writing.
/// Any other frame type is skipped, as the writer's own loop skips it.
pub(crate) fn take_queued_data(
    mut first: Vec<u8>,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<(u8, Vec<u8>)>,
    data: u8,
    ends: &[u8],
) -> (Vec<u8>, Option<u8>) {
    if first.len() >= GATHER_BELOW {
        return (first, None);
    }
    while first.len() < MAX_COALESCED {
        match rx.try_recv() {
            Ok((t, payload)) if t == data => first.extend_from_slice(&payload),
            Ok((t, _)) if ends.contains(&t) => return (first, Some(t)),
            Ok(_) => {}
            Err(_) => break,
        }
    }
    (first, None)
}

/// Test support for timing a message written in two parts across a relay.
#[cfg(test)]
pub(crate) mod two_part {
    use std::time::Duration;

    pub(crate) const ROUNDS: usize = 30;
    pub(crate) const FIRST: usize = 28;
    pub(crate) const SECOND: usize = 1_300;
    /// A relay that holds the second part waits for the 40 ms delayed ACK.
    pub(crate) const LIMIT: Duration = Duration::from_millis(20);
    /// Longer than the delayed-ACK timer, so a round starts with nothing unacknowledged.
    pub(crate) const BETWEEN_ROUNDS: Duration = Duration::from_millis(50);

    /// Loopback starts in quick-ACK mode, and the kernel turns it back on, so the
    /// receiver's is turned off before every message: then it acknowledges the first
    /// part only when its delayed-ACK timer fires.
    pub(crate) fn quickack_off(s: &tokio::net::TcpStream) {
        use std::os::fd::AsRawFd;
        let off: libc::c_int = 0;
        // SAFETY: a valid socket fd and a c_int option value that outlives the call.
        let rc = unsafe {
            libc::setsockopt(
                s.as_raw_fd(),
                libc::IPPROTO_TCP,
                libc::TCP_QUICKACK,
                &off as *const libc::c_int as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        assert_eq!(rc, 0, "TCP_QUICKACK: {}", std::io::Error::last_os_error());
    }

    pub(crate) fn median(mut took: Vec<Duration>) -> Duration {
        took.sort();
        took[took.len() / 2]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Frames queued while the relay was writing go out in one write, a frame type that is
    /// neither DATA nor an end is skipped, and a CLOSE queued behind them is handed back
    /// after them, not lost and not written.
    #[test]
    fn queued_data_frames_are_gathered_into_one_write() {
        const DATA: u8 = 1;
        const CLOSE: u8 = 2;
        const OTHER: u8 = 9;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        tx.send((DATA, b"bb".to_vec())).unwrap();
        tx.send((OTHER, Vec::new())).unwrap();
        tx.send((DATA, b"cc".to_vec())).unwrap();
        tx.send((CLOSE, Vec::new())).unwrap();
        tx.send((DATA, b"after".to_vec())).unwrap();
        let (bytes, end) = take_queued_data(b"aa".to_vec(), &mut rx, DATA, &[CLOSE]);
        assert_eq!(bytes, b"aabbcc");
        assert_eq!(end, Some(CLOSE));
        let (bytes, end) = take_queued_data(b"x".to_vec(), &mut rx, DATA, &[CLOSE]);
        assert_eq!((bytes, end), (b"xafter".to_vec(), None));
    }
}

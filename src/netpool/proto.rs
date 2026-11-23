//! What a clone and the pool say to each other, over a `SOCK_SEQPACKET` connection.
//!
//! The clone sends one [`Claim`]. The pool answers with one [`Reply`]; an `ok` carries
//! three descriptors, in this order: the namespace's user namespace, its network namespace,
//! and the clone's channel to the namespace's holder. After that the connection carries
//! nothing: it is the lease, and it ends when every process holding it has exited.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::os::fd::{OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use vmm_sys_util::sock_ctrl_msg::ScmSocket;

/// The VMM's TAP in every pooled namespace. A namespace is not made for one VM, so the name
/// cannot carry a VM's id.
pub const TAP: &str = "tap-pool";

/// The longest message either side sends.
pub const MAX_MESSAGE: usize = 4096;

/// Descriptors an `ok` reply carries.
pub const GRANT_FDS: usize = 3;
const _: () = assert!(GRANT_FDS <= crate::network::netns_agent::MAX_PACKET_FDS);

/// The area of the data directory that holds each user's pool directory.
const AREA: &str = "netpool";

/// This user's pool directory, `<data_dir>/netpool/<user>/`, made if it is missing: mode
/// 0700 and refused when anyone else can write it, like the owner records' directory. Root
/// and a user share a data directory and never a pool: their namespaces map different ids.
pub fn dir() -> Result<PathBuf> {
    crate::reclaim::owner_dir_under(&crate::paths::data_dir(), AREA)
}

/// The pool directory if some user's pool has ever run in this data directory, without
/// making anything: a clone on a host with no pool pays one `stat`.
pub fn existing_dir() -> Result<Option<PathBuf>> {
    let data_dir = crate::paths::data_dir();
    match std::fs::symlink_metadata(data_dir.join(AREA)) {
        Ok(_) => dir().map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("looking for {AREA} in the data directory"))
        }
    }
}

pub fn socket_path(dir: &Path) -> PathBuf {
    dir.join("pool.sock")
}

pub fn lock_path(dir: &Path) -> PathBuf {
    dir.join("pool.lock")
}

/// What a client asks of the pool, one request per connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "request", rename_all = "snake_case")]
pub enum Request {
    /// A clone's request for a namespace.
    Claim {
        /// What the clone's namespace must be like
        /// ([`crate::network::PastaNetwork::pool_key`]). The pool lends a namespace only
        /// to a clone whose key is its own.
        key: String,
    },
    /// What the pool holds and has done so far.
    Stats,
}

/// The pool's namespaces by state, and its counters since it started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Stats {
    pub building: usize,
    pub ready: usize,
    pub leased: usize,
    pub resetting: usize,
    pub destroying: usize,
    /// Namespaces built, first builds and replacements.
    pub builds: u64,
    /// Namespaces destroyed.
    pub destroys: u64,
    /// Claims answered with a namespace, and of those with a recycled one (generation 2
    /// or more).
    pub grants: u64,
    pub recycled_grants: u64,
    /// Claims answered with no namespace.
    pub misses: u64,
    /// Resets that ended with a matching fingerprint.
    pub resets: u64,
    /// How long those resets took, from the lease's end to the namespace being ready: the
    /// sum in microseconds, and a histogram whose bucket `i` counts the resets that took
    /// from 2^i to 2^(i+1) microseconds (the last bucket takes everything longer).
    pub reset_us_total: u64,
    pub reset_us_log2: [u64; RESET_BUCKETS],
}

/// Buckets of [`Stats::reset_us_log2`]: up to 2^24 microseconds, 16.8 s.
pub const RESET_BUCKETS: usize = 24;

impl Stats {
    /// Count one reset that took `micros`.
    pub fn record_reset(&mut self, micros: u64) {
        self.resets += 1;
        self.reset_us_total += micros;
        let bucket = (63 - micros.max(1).leading_zeros() as usize).min(RESET_BUCKETS - 1);
        self.reset_us_log2[bucket] += 1;
    }

    /// The upper edge, in microseconds, of the bucket the `quantile` (0 to 1) of the resets
    /// falls in, or `None` before the first reset.
    pub fn reset_us_quantile(&self, quantile: f64) -> Option<u64> {
        let wanted = (self.resets as f64 * quantile).ceil().max(1.0) as u64;
        let mut seen = 0;
        for (bucket, &count) in self.reset_us_log2.iter().enumerate() {
            seen += count;
            if count > 0 && seen >= wanted {
                return Some(1u64 << (bucket + 1));
            }
        }
        None
    }
}

/// The namespace a claim was answered with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    /// The namespace's holder. It is the pool's process, not the clone's.
    pub holder_pid: u32,
    /// The VMM's TAP in the namespace.
    pub tap: String,
    /// How many leases the namespace has had, this one included.
    pub generation: u64,
    /// The pool's number for the namespace.
    pub slot: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum Reply {
    Ok(Grant),
    /// The pool has nothing for this claim; the clone builds its own namespace.
    Miss {
        reason: String,
    },
    /// Boxed: the counters are several times the size of the other replies.
    Stats(Box<Stats>),
}

/// Register `socket` with the runtime, for waiting until it is readable or writable.
pub fn watch(socket: UnixStream) -> std::io::Result<tokio::io::unix::AsyncFd<UnixStream>> {
    // SAFETY: the `AsyncFd` owns the socket, which closes only on drop or when it is taken
    // back out whole (`into_inner`), and nothing replaces it through `get_mut`.
    unsafe { tokio::io::unix::AsyncFd::register(socket) }.map_err(std::io::Error::from)
}

/// Send `message` and `fds` as one packet.
pub fn send(socket: &UnixStream, message: &[u8], fds: &[RawFd]) -> std::io::Result<()> {
    socket
        .send_with_fds(&[message], fds)
        .map(drop)
        .map_err(|error| std::io::Error::from_raw_os_error(error.errno()))
}

/// One packet and the descriptors that came with it, each close-on-exec, or `None` at
/// end-of-file ([`crate::network::netns_agent::receive_packet`]).
pub fn receive(socket: &UnixStream) -> std::io::Result<Option<(Vec<u8>, Vec<OwnedFd>)>> {
    let mut buffer = [0u8; MAX_MESSAGE];
    let (len, fds) = crate::network::netns_agent::receive_packet(socket, &mut buffer)?;
    if len == 0 && fds.is_empty() {
        return Ok(None);
    }
    Ok(Some((buffer[..len].to_vec(), fds)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;

    #[test]
    fn resets_are_counted_in_power_of_two_buckets() {
        let mut stats = Stats::default();
        assert_eq!(stats.reset_us_quantile(0.99), None);
        for micros in [900, 1000, 1100, 3000, 70_000] {
            stats.record_reset(micros);
        }
        assert_eq!((stats.resets, stats.reset_us_total), (5, 76_000));
        assert_eq!(stats.reset_us_log2[9], 2, "512 to 1023");
        assert_eq!(stats.reset_us_log2[10], 1, "1024 to 2047");
        assert_eq!(stats.reset_us_quantile(0.5), Some(2048));
        assert_eq!(stats.reset_us_quantile(0.99), Some(131_072));
        stats.record_reset(u64::MAX);
        assert_eq!(stats.reset_us_log2[RESET_BUCKETS - 1], 1);
    }

    #[test]
    fn replies_round_trip_as_tagged_json() {
        let grant = Reply::Ok(Grant {
            holder_pid: 7,
            tap: TAP.into(),
            generation: 2,
            slot: 3,
        });
        let text = serde_json::to_string(&grant).unwrap();
        assert_eq!(
            text,
            r#"{"reply":"ok","holder_pid":7,"tap":"tap-pool","generation":2,"slot":3}"#
        );
        assert_eq!(serde_json::from_str::<Reply>(&text).unwrap(), grant);
        let miss = Reply::Miss {
            reason: "no free namespace".into(),
        };
        assert_eq!(
            serde_json::from_str::<Reply>(&serde_json::to_string(&miss).unwrap()).unwrap(),
            miss
        );
    }

    /// A packet's descriptors arrive with it, close-on-exec, and a closed peer reads as
    /// end-of-file.
    #[test]
    fn a_packet_carries_its_descriptors() {
        let (a, b) = crate::network::netns_agent::channel().unwrap();
        let files: Vec<std::fs::File> = (0..GRANT_FDS)
            .map(|_| std::fs::File::open("/dev/null").unwrap())
            .collect();
        let raw: Vec<RawFd> = files.iter().map(|f| f.as_raw_fd()).collect();
        send(&a, b"hello", &raw).unwrap();
        let (message, fds) = receive(&b).unwrap().unwrap();
        assert_eq!(message, b"hello");
        assert_eq!(fds.len(), GRANT_FDS);
        for fd in &fds {
            // SAFETY: fcntl with F_GETFD takes no pointers.
            let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
            assert_eq!(flags & libc::FD_CLOEXEC, libc::FD_CLOEXEC);
        }
        drop(a);
        assert!(receive(&b).unwrap().is_none());
    }
}

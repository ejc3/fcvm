//! Pending reply storage for async FUSE pipelining.
//!
//! Stores fuser Reply objects while waiting for host responses.
//! When a response arrives, the matching reply is completed.

use fuser::{
    ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen,
    ReplyStatfs, ReplyWrite,
};
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

/// Global profiling stats
static TOTAL_REQUESTS: AtomicU64 = AtomicU64::new(0);
static TOTAL_RESPONSES: AtomicU64 = AtomicU64::new(0);
static MAX_PENDING: AtomicUsize = AtomicUsize::new(0);
static LAST_STATS_TIME: AtomicU64 = AtomicU64::new(0);

lazy_static::lazy_static! {
    static ref START_TIME: Instant = Instant::now();
}

/// Stored reply objects waiting for host response.
/// Each variant holds a different fuser Reply type.
pub enum PendingReply {
    Entry(ReplyEntry),
    Attr(ReplyAttr),
    Data(ReplyData),
    Directory(ReplyDirectory),
    Write(ReplyWrite),
    Create(ReplyCreate),
    Open(ReplyOpen),
    Empty(ReplyEmpty),
    Statfs(ReplyStatfs),
}

/// Thread-safe storage for pending replies.
/// Maps unique request ID to the reply object waiting for completion.
pub struct PendingReplies {
    inner: Mutex<HashMap<u64, PendingReply>>,
}

impl PendingReplies {
    /// Create a new empty pending replies store.
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Store a reply for later completion.
    pub fn insert(&self, id: u64, reply: PendingReply) {
        let mut guard = self.inner.lock().unwrap();
        guard.insert(id, reply);

        // Update profiling stats
        TOTAL_REQUESTS.fetch_add(1, Ordering::Relaxed);
        let pending = guard.len();
        let mut max = MAX_PENDING.load(Ordering::Relaxed);
        while pending > max {
            match MAX_PENDING.compare_exchange_weak(max, pending, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break,
                Err(current) => max = current,
            }
        }

        // Print stats every 5 seconds
        let now_secs = START_TIME.elapsed().as_secs();
        let last = LAST_STATS_TIME.load(Ordering::Relaxed);
        if now_secs >= last + 5 {
            if LAST_STATS_TIME.compare_exchange(last, now_secs, Ordering::Relaxed, Ordering::Relaxed).is_ok() {
                let reqs = TOTAL_REQUESTS.load(Ordering::Relaxed);
                let resps = TOTAL_RESPONSES.load(Ordering::Relaxed);
                let max_p = MAX_PENDING.load(Ordering::Relaxed);
                eprintln!("[fc-agent STATS] t={}s reqs={} resps={} pending={} max_pending={}",
                    now_secs, reqs, resps, pending, max_p);
            }
        }
    }

    /// Remove and return a pending reply by ID.
    pub fn remove(&self, id: &u64) -> Option<PendingReply> {
        TOTAL_RESPONSES.fetch_add(1, Ordering::Relaxed);
        self.inner.lock().unwrap().remove(id)
    }

    /// Get current count of pending replies (for debugging).
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }
}

impl Default for PendingReplies {
    fn default() -> Self {
        Self::new()
    }
}

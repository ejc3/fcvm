//! Telling the kernel that the host changed a file without changing its size or mtime.
//!
//! With FUSE_AUTO_INVAL_DATA the kernel drops a regular file's cached pages when a refreshed size
//! or mtime differs from the one it holds (`fuse_change_attributes_i`, fs/fuse/inode.c). A host
//! that rewrites a file in place at the same length and puts the mtime back changes neither, so a
//! descriptor or mapping held across the rewrite kept reading the old pages.
//!
//! The host's kernel moves a file's ctime on every change, and nobody can set it back. So the
//! client remembers, per regular file, the size, mtime and ctime the server last reported, and
//! when a refresh (GETATTR, LOOKUP, READDIRPLUS) carries a new ctime with the same size and mtime,
//! it sends FUSE_NOTIFY_INVAL_INODE before the reply. The kernel drops the pages and the
//! attributes in that write, and the reply lets the waiting read go on to read the new bytes. This
//! costs nothing on the read path: the refreshes are the ones FUSE_AUTO_INVAL_DATA already makes
//! once the attribute timeout passes, and only a change adds a notification and one more GETATTR
//! (the notification makes the kernel discard the reply's attributes).
//!
//! A change of size or mtime is left to the kernel. A notification with it would make the kernel
//! discard the reply carrying the new size, and the read that asked would stop at the old one.
//! With the writeback cache, upstream keeps its own size and mtime of a cached file and ignores
//! the server's (#1041); a guest kernel that takes the server's once no guest process has the file
//! open for writing acts on them as without the cache. Either way the case left over is the one
//! handled here.
//!
//! With the writeback cache this mount's own changes can move the host's ctime and leave the size
//! and mtime the client last saw: the kernel writes a file's times back with a SETATTR that sets
//! the mtime it holds. Those changes are counted while they are in flight and until a refresh
//! asked after they finished, and a ctime change in that window is taken as the mount's own.
//! Without the cache every write moves the host's mtime, which the kernel acts on.
//!
//! The record lives in the guest with the kernel's page cache, so a clone restored from a
//! snapshot compares the host's files against what its own kernel holds, whatever server it now
//! talks to.

use crate::protocol::FileAttr;
use dashmap::DashMap;
use fuser::{INodeNo, Notifier};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

/// Where the session's notifier goes once the session exists. The client is made before the
/// session that owns the /dev/fuse descriptor, so the mount sets this after `Session::new` and
/// before the session reads its first request.
pub type NotifierSlot = Arc<OnceLock<Notifier>>;

/// What the client last told the kernel about one regular file.
#[derive(Debug, Clone, Copy)]
struct Known {
    size: u64,
    mtime: (i64, u32),
    ctime: (i64, u32),
    /// The kernel's lookup count on the inode as the client counts it: one per entry reply,
    /// less what FORGET returns. The record goes when it reaches zero.
    lookups: u64,
    /// Writeback cache only: this mount's changes to the file on their way to the server.
    own_in_flight: u32,
    /// Writeback cache only: the clock value when the last of this mount's changes finished.
    own_done: u64,
    /// Writeback cache only: a ctime change seen at the next refresh may be this mount's own.
    own_changed: bool,
}

impl Known {
    fn from_attr(attr: &FileAttr) -> Self {
        Self {
            size: attr.size,
            mtime: (attr.mtime_secs, attr.mtime_nsecs),
            ctime: (attr.ctime_secs, attr.ctime_nsecs),
            lookups: 0,
            own_in_flight: 0,
            own_done: 0,
            own_changed: false,
        }
    }
}

/// A reply to send after the kernel has dropped the listed inodes' caches.
struct Job {
    inodes: Vec<u64>,
    reply: Box<dyn FnOnce() + Send>,
}

/// The client's record of regular files and the thread that sends invalidations.
pub(crate) struct HostChanges {
    writeback: AtomicBool,
    files: DashMap<u64, Known>,
    clock: AtomicU64,
    notifier: NotifierSlot,
    /// To the invalidation thread. A FUSE reader must not wait for an invalidation: dropping a
    /// page waits for its lock, a page under a read is locked until that READ is answered, and
    /// the READ may be queued for the reader that is waiting. The thread sends the reply after
    /// the invalidation, so the request still sees the new data, and the readers keep serving.
    jobs: Option<crossbeam_channel::Sender<Job>>,
}

fn is_regular(ino: u64, attr: &FileAttr) -> bool {
    ino != 0 && attr.is_file()
}

impl HostChanges {
    pub(crate) fn new(notifier: NotifierSlot) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded::<Job>();
        let worker_notifier = Arc::clone(&notifier);
        let jobs = std::thread::Builder::new()
            .name("fuse-pipe-inval".to_string())
            .spawn(move || {
                // Ends when the client, which holds the only sender, is dropped with its session.
                for job in rx {
                    send_invalidations(&worker_notifier, &job.inodes);
                    (job.reply)();
                }
            })
            .map_err(|e| {
                tracing::error!(
                    target: "fuse-pipe::client",
                    error = %e,
                    "cannot start the invalidation thread; invalidating on the reader thread"
                );
            })
            .ok()
            .map(|_| tx);
        Self {
            writeback: AtomicBool::new(false),
            files: DashMap::new(),
            clock: AtomicU64::new(0),
            notifier,
            jobs,
        }
    }

    /// The slot the mount fills with the session's notifier.
    pub(crate) fn notifier_slot(&self) -> NotifierSlot {
        Arc::clone(&self.notifier)
    }

    /// Record whether the kernel accepted FUSE_WRITEBACK_CACHE at INIT.
    pub(crate) fn set_writeback(&self, on: bool) {
        self.writeback.store(on, Ordering::Release);
    }

    /// The clock value to pass to [`refreshed`](Self::refreshed), read before the request is sent.
    pub(crate) fn now(&self) -> u64 {
        self.clock.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// The server answered a GETATTR, LOOKUP or READDIRPLUS asked at `asked` with `attr` for node
    /// `ino`, which the kernel will take. `entry` says whether the reply adds to the kernel's
    /// lookup count. Returns whether the kernel must drop the file's caches before it sees the
    /// reply.
    pub(crate) fn refreshed(&self, ino: u64, attr: &FileAttr, asked: u64, entry: bool) -> bool {
        if !is_regular(ino, attr) {
            return false;
        }
        let writeback = self.writeback.load(Ordering::Acquire);
        let mut known = self
            .files
            .entry(ino)
            .or_insert_with(|| Known::from_attr(attr));
        if entry {
            known.lookups += 1;
        }
        let ctime = (attr.ctime_secs, attr.ctime_nsecs);
        let mtime = (attr.mtime_secs, attr.mtime_nsecs);
        let invalidate = known.ctime != ctime
            && known.size == attr.size
            && known.mtime == mtime
            && !(writeback && known.own_changed);
        if writeback {
            known.own_changed = known.own_in_flight > 0 || known.own_done > asked;
        }
        known.size = attr.size;
        known.mtime = mtime;
        known.ctime = ctime;
        invalidate
    }

    /// This mount made `attr`'s file (CREATE, MKNOD): the reply adds a lookup and the file has
    /// nothing cached.
    pub(crate) fn created(&self, ino: u64, attr: &FileAttr) {
        if !is_regular(ino, attr) {
            return;
        }
        let mut known = self
            .files
            .entry(ino)
            .or_insert_with(|| Known::from_attr(attr));
        known.lookups += 1;
    }

    /// This mount changed `attr`'s file and the reply carries the result (SETATTR, LINK). The
    /// kernel takes the size and mtime from it; the ctime stays as last refreshed, so a host
    /// change made before this one is still found at the next refresh, at the price of one
    /// invalidation the change itself did not need.
    pub(crate) fn changed_here(&self, ino: u64, attr: &FileAttr, entry: bool) {
        if !is_regular(ino, attr) {
            return;
        }
        let mut known = self
            .files
            .entry(ino)
            .or_insert_with(|| Known::from_attr(attr));
        if entry {
            known.lookups += 1;
        }
        known.size = attr.size;
        known.mtime = (attr.mtime_secs, attr.mtime_nsecs);
    }

    /// This mount is about to change inode `ino` on the server. With the writeback cache, the
    /// returned guard keeps a ctime change from notifying until a refresh asked after it is
    /// dropped. Without it the kernel reads its own changes from the size and mtime.
    pub(crate) fn changing(&self, ino: u64) -> OwnChange<'_> {
        let tracked = self.writeback.load(Ordering::Acquire)
            && match self.files.get_mut(&ino) {
                Some(mut known) => {
                    known.own_in_flight += 1;
                    known.own_changed = true;
                    true
                }
                None => false,
            };
        OwnChange {
            changes: self,
            ino,
            tracked,
        }
    }

    /// The kernel returned `nlookup` lookups on `ino`.
    pub(crate) fn forget(&self, ino: u64, nlookup: u64) {
        self.files.remove_if_mut(&ino, |_, known| {
            known.lookups = known.lookups.saturating_sub(nlookup);
            known.lookups == 0
        });
    }

    /// Send `reply` once the kernel has dropped the caches of `inodes`; at once if there are none.
    pub(crate) fn reply_after(&self, inodes: Vec<u64>, reply: impl FnOnce() + Send + 'static) {
        if inodes.is_empty() {
            reply();
            return;
        }
        let job = Job {
            inodes,
            reply: Box::new(reply),
        };
        let job = match &self.jobs {
            Some(jobs) => match jobs.send(job) {
                Ok(()) => return,
                Err(crossbeam_channel::SendError(job)) => job,
            },
            None => job,
        };
        send_invalidations(&self.notifier, &job.inodes);
        (job.reply)();
    }

    #[cfg(test)]
    fn tracked(&self, ino: u64) -> bool {
        self.files.contains_key(&ino)
    }
}

/// Ends one change counted by [`HostChanges::changing`].
pub(crate) struct OwnChange<'a> {
    changes: &'a HostChanges,
    ino: u64,
    tracked: bool,
}

impl Drop for OwnChange<'_> {
    fn drop(&mut self) {
        if !self.tracked {
            return;
        }
        let done = self.changes.now();
        if let Some(mut known) = self.changes.files.get_mut(&self.ino) {
            known.own_in_flight = known.own_in_flight.saturating_sub(1);
            known.own_done = known.own_done.max(done);
        }
    }
}

fn send_invalidations(notifier: &NotifierSlot, inodes: &[u64]) {
    let Some(notifier) = notifier.get() else {
        tracing::error!(
            target: "fuse-pipe::client",
            ?inodes,
            "the host changed these files but the session has no notifier; the kernel keeps their pages"
        );
        return;
    };
    for &ino in inodes {
        // Offset 0 with length 0: every page of the file, and its attributes.
        match notifier.inval_inode(INodeNo(ino), 0, 0) {
            Ok(()) => tracing::debug!(
                target: "fuse-pipe::client",
                ino,
                "host changed the file without changing its size or mtime; dropped its pages"
            ),
            Err(e) => tracing::warn!(
                target: "fuse-pipe::client",
                ino,
                error = %e,
                "the kernel refused to drop the pages of a file the host changed"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attr(ino: u64, size: u64, mtime: i64, ctime: i64) -> FileAttr {
        FileAttr {
            size,
            mtime_secs: mtime,
            ctime_secs: ctime,
            mode: libc::S_IFREG | 0o644,
            ..FileAttr::new(ino)
        }
    }

    fn changes(writeback: bool) -> HostChanges {
        let changes = HostChanges::new(Arc::new(OnceLock::new()));
        changes.set_writeback(writeback);
        changes
    }

    #[test]
    fn without_writeback_only_a_change_that_keeps_size_and_mtime_notifies() {
        let c = changes(false);
        assert!(!c.refreshed(5, &attr(5, 64, 100, 100), c.now(), true));
        assert!(!c.refreshed(5, &attr(5, 64, 100, 100), c.now(), false));
        // Same size and mtime, new ctime: the kernel would keep its pages.
        assert!(c.refreshed(5, &attr(5, 64, 100, 101), c.now(), false));
        // The kernel drops them itself when the size or the mtime moved.
        assert!(!c.refreshed(5, &attr(5, 65, 100, 102), c.now(), false));
        assert!(!c.refreshed(5, &attr(5, 65, 103, 103), c.now(), false));
        // Directories have no data cache to drop.
        let mut dir = attr(6, 4096, 100, 100);
        dir.mode = libc::S_IFDIR | 0o755;
        assert!(!c.refreshed(6, &dir, c.now(), true));
        dir.ctime_secs = 101;
        assert!(!c.refreshed(6, &dir, c.now(), false));
    }

    #[test]
    fn a_change_made_here_without_writeback_still_finds_an_earlier_host_change() {
        let c = changes(false);
        assert!(!c.refreshed(5, &attr(5, 64, 100, 100), c.now(), true));
        // A chmod from the guest: the reply carries the new ctime, which is not taken.
        c.changed_here(5, &attr(5, 64, 100, 105), false);
        assert!(c.refreshed(5, &attr(5, 64, 100, 105), c.now(), false));
        // A truncate from the guest: the kernel takes the new size from the reply.
        c.changed_here(5, &attr(5, 10, 106, 106), false);
        assert!(c.refreshed(5, &attr(5, 10, 106, 107), c.now(), false));
    }

    #[test]
    fn with_writeback_a_ctime_change_notifies_unless_this_mount_made_it() {
        let c = changes(true);
        assert!(!c.refreshed(5, &attr(5, 64, 100, 100), c.now(), true));
        // The host rewrites the file keeping its size and mtime.
        assert!(c.refreshed(5, &attr(5, 64, 100, 101), c.now(), false));
        // The host grows it: the size is the kernel's to act on, as without the cache.
        assert!(!c.refreshed(5, &attr(5, 90, 102, 102), c.now(), false));

        // The kernel writes back the times it holds, which moves only the ctime, while a
        // refresh is asked and answered.
        let setattr = c.changing(5);
        assert!(!c.refreshed(5, &attr(5, 90, 102, 103), c.now(), false));
        // A refresh asked before the change finished may have been answered after it.
        let asked = c.now();
        drop(setattr);
        assert!(!c.refreshed(5, &attr(5, 90, 102, 104), asked, false));
        // One asked after the change finished, with the ctime unmoved since: no notification.
        assert!(!c.refreshed(5, &attr(5, 90, 102, 104), c.now(), false));
        // The next host change is found.
        assert!(c.refreshed(5, &attr(5, 90, 102, 105), c.now(), false));
    }

    #[test]
    fn without_writeback_writes_are_not_counted() {
        let c = changes(false);
        assert!(!c.refreshed(5, &attr(5, 64, 100, 100), c.now(), true));
        let write = c.changing(5);
        assert!(!write.tracked);
        drop(write);
        // The write moved the mtime: the kernel drops its pages itself.
        assert!(!c.refreshed(5, &attr(5, 64, 101, 101), c.now(), false));
    }

    #[test]
    fn the_record_goes_with_the_kernels_last_lookup() {
        let c = changes(false);
        c.refreshed(5, &attr(5, 64, 100, 100), c.now(), true);
        c.refreshed(5, &attr(5, 64, 100, 100), c.now(), true);
        c.created(7, &attr(7, 0, 100, 100));
        c.forget(5, 1);
        assert!(c.tracked(5));
        c.forget(5, 1);
        assert!(!c.tracked(5));
        c.forget(7, 1);
        assert!(!c.tracked(7));
        // A FORGET for an inode never recorded (a directory) is ignored.
        c.forget(9, 3);
        assert!(!c.tracked(9));
    }

    #[test]
    fn a_reply_with_nothing_to_drop_is_sent_on_the_calling_thread() {
        let c = changes(false);
        let caller = std::thread::current().id();
        let (tx, rx) = std::sync::mpsc::channel();
        c.reply_after(Vec::new(), move || {
            tx.send(std::thread::current().id()).unwrap()
        });
        assert_eq!(rx.recv().unwrap(), caller);
    }

    #[test]
    fn a_reply_after_an_invalidation_is_sent_off_the_calling_thread() {
        let c = changes(false);
        let caller = std::thread::current().id();
        let (tx, rx) = std::sync::mpsc::channel();
        c.reply_after(vec![5], move || {
            tx.send(std::thread::current().id()).unwrap()
        });
        let sender = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the reply was sent");
        assert_ne!(sender, caller);
    }
}

//! The restore working set: which pages of a snapshot a clone actually touches.
//!
//! A clone restored over UFFD starts with *nothing* resident and faults its way in one page
//! at a time. Every fault is a vCPU trap, a read from the uffd queue, and an ioctl — measured
//! at ~5.6 us marginal on this host, and a Chromium clone takes ~56,300 of them, so the
//! demand-paging tax is hundreds of milliseconds of pure latency before the guest does any
//! useful work.
//!
//! Clones of the same snapshot fault almost the same pages: across 8 clones of one snapshot
//! the pairwise page-set Jaccard median is 0.927 and 82.2% of the union is faulted by ALL 8.
//! What is *not* reproducible is the ORDER — only 8.6% of faults land on the page after the
//! previous one, which is why readahead and fault-around do nothing here. The set is stable
//! even though the sequence is not, so the thing worth persisting is the SET.
//!
//! This module records that set, keeps it beside the snapshot, and hands it back on the next
//! restore so the server can populate those pages up front instead of trapping for each one.
//!
//! # The set is a hint, never data
//!
//! A working set only ever says WHICH offsets to populate. The bytes always come from the
//! memory file being served right now, through the same code path a demand fault would use.
//! So a wrong, stale, or truncated set cannot corrupt a guest — the worst case is a wasted
//! copy of pages nobody asked for, plus the demand faults that still happen for the pages the
//! set missed. Every validation failure in here therefore degrades to "prefetch nothing and
//! re-record", never to an error that reaches the VM.
//!
//! # Why the image key is an identity tuple and not a content hash
//!
//! The key exists to stop a set recorded against one memory image from being replayed against
//! a different one. Because a mismatch can only waste work (see above), the key is sized to
//! that harm: SHA-256 over the exact snapshot `config.json` digest plus the memory file's
//! `(len, mtime)` with nanosecond mtime, which is one small read and one `stat`.
//! Hashing the image itself was measured at 1.5 GB/s on this host — 1.4 s for a 2 GiB snapshot,
//! paid on every `snapshot serve` startup — which is a far bigger cost than the mis-prefetch it
//! would prevent. Rewriting a snapshot (`snapshot create --tag` over an existing tag) always
//! writes a new file with a new mtime and a new config, so the stale set is dropped.
//!
//! The inode and device number are not part of the key. A store restored from a tar archive
//! (or copied with `cp -p`, `rsync -t`) holds the same bytes under a new inode and possibly a
//! new device, and keying on them made the first restore on every such host skip replay. Length
//! and nanosecond mtime survive that copy and still change on a rewrite. A copy that loses the
//! nanoseconds (tar's default format keeps whole seconds) misses, which is safe: the set is a
//! hint. A false match is also safe: it can only copy pages nobody asked for, since the bytes
//! always come from the served file.

use anyhow::{Context, Result};
use nix::fcntl::{Flock, FlockArg};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use tracing::{debug, info, warn};

use super::pack::{self, Pack};

/// Bitmap granularity, in bytes.
///
/// Fixed at 4 KiB rather than the clone's page size so ONE recorded set serves every clone of
/// a snapshot regardless of how its guest memory is backed: 4 KiB, ARM64 16 KiB, and 2 MiB
/// hugepage granules are all whole multiples of this, so a fault on any of them marks a whole
/// number of bits and a set recorded by a 4 KiB clone is directly usable by a 2 MiB one.
pub const GRANULE: u64 = 4096;

/// `magic || version || granule || mem_len || granules || image_key`, then the bitmap's u64
/// words, then one little-endian [`FirstTouch`] for each granule in the set, in ascending
/// granule order.
const HEADER_LEN: usize = 8 + 8 + 8 + 8 + 8 + 32;
const MAGIC: &[u8; 8] = b"FCVMWSET";
const VERSION: u64 = 2;

/// When a granule was first touched: tenths of a second after the handshake of the clone that
/// touched it, plus one, saturating at `u16::MAX`. 0 means no time is known. Merging two
/// observations keeps the earlier time, so a set converges on the time the guest first needs
/// each page, whichever clone saw it first.
pub type FirstTouch = u16;

/// The [`FirstTouch`] of a fault `elapsed` after its clone's handshake.
pub fn first_touch_stamp(elapsed: std::time::Duration) -> FirstTouch {
    (elapsed.as_millis() / 100 + 1).min(u128::from(FirstTouch::MAX)) as FirstTouch
}

/// Upper bounds, in tenths of a second after the handshake, of the first-touch tiers replay
/// works through in order: the first half second, then each tier twice as long as the one
/// before. A granule with no known time goes in the tier after the last bound. Tiers keep
/// replay an ordered pass over bitmaps rather than a sort of millions of runs.
const TIER_BOUNDS: [FirstTouch; 10] = [5, 10, 20, 40, 80, 160, 320, 640, 1280, 2560];

/// Number of first-touch tiers, the last holding granules touched after the last bound or at
/// an unknown time.
pub const TIERS: usize = TIER_BOUNDS.len() + 1;

/// The tier of a granule first touched at `stamp`.
fn tier_of(stamp: FirstTouch) -> usize {
    if stamp == 0 {
        return TIERS - 1;
    }
    TIER_BOUNDS
        .iter()
        .position(|bound| stamp <= *bound)
        .unwrap_or(TIERS - 1)
}

/// The earlier of two first-touch times, where 0 is unknown and loses to any known time.
fn earlier(a: FirstTouch, b: FirstTouch) -> FirstTouch {
    match (a, b) {
        (0, other) | (other, 0) => other,
        (a, b) => a.min(b),
    }
}

/// Refuse to even allocate a bitmap for an implausible memory file (1 TiB of 4 KiB granules
/// is a 32 MiB bitmap). Guards against a corrupt header turning into a huge allocation.
const MAX_MEM_LEN: u64 = 1 << 40;

/// Identity of the snapshot memory image a working set was recorded against.
///
/// See the module docs for why this combines the exact config digest with a `stat` tuple
/// instead of hashing the multi-gigabyte memory image.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ImageKey([u8; 32]);

impl ImageKey {
    /// Derive the key of the memory image at `path`.
    pub fn of(path: &Path, config_digest: &[u8; 32]) -> Result<Self> {
        let meta = std::fs::metadata(path)
            .with_context(|| format!("stat-ing memory image {}", path.display()))?;
        Ok(Self::of_metadata(&meta, config_digest))
    }

    /// The key of a memory image whose `stat` is `meta`: for an image already open, whose
    /// path may name another image by now.
    pub fn of_metadata(meta: &std::fs::Metadata, config_digest: &[u8; 32]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(config_digest);
        hasher.update(meta.len().to_le_bytes());
        hasher.update(meta.mtime().to_le_bytes());
        hasher.update(meta.mtime_nsec().to_le_bytes());
        Self(hasher.finalize().into())
    }

    /// Derive the key of the snapshot generation whose memory image and config are at these
    /// paths: the key [`WorkingSetStore::open`] binds a store to.
    pub fn of_generation(mem_path: &Path, config_path: &Path) -> Result<Self> {
        let config = std::fs::read(config_path).with_context(|| {
            format!(
                "reading snapshot generation config {}",
                config_path.display()
            )
        })?;
        let config_digest: [u8; 32] = Sha256::digest(&config).into();
        Self::of(mem_path, &config_digest)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl std::fmt::Debug for ImageKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for b in &self.0[..8] {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

/// A contiguous run of snapshot memory, in file offsets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    pub offset: u64,
    pub len: u64,
}

/// A set of snapshot pages, as a bitmap over [`GRANULE`]-sized granules of the memory file,
/// with the time each granule was first touched.
///
/// A bitmap rather than a list of offsets because it dedupes for free (the same page faults
/// more than once across a clone's life), costs a fixed 32 KiB per GiB of guest memory, and
/// makes merging two clones' observations a word-wise OR. The first-touch times cost 512 KiB
/// per GiB, and merge by keeping the earlier time.
#[derive(Clone, PartialEq, Eq)]
pub struct PageSet {
    granules: u64,
    words: Vec<u64>,
    /// One [`FirstTouch`] per granule; meaningful only where the bit is set.
    first: Vec<FirstTouch>,
}

/// Print the runs, not 32 KiB of hex: a failing assertion should say WHICH pages differ.
impl std::fmt::Debug for PageSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PageSet({} of {} granules) ", self.len(), self.granules)?;
        f.debug_list()
            .entries(
                self.runs()
                    .map(|r| format!("{:#x}..{:#x}", r.offset, r.offset + r.len)),
            )
            .finish()
    }
}

impl PageSet {
    /// An empty set covering a memory image of `mem_len` bytes.
    pub fn empty(mem_len: u64) -> Self {
        let granules = mem_len.div_ceil(GRANULE);
        Self {
            granules,
            words: vec![0; (granules as usize).div_ceil(64)],
            first: vec![0; granules as usize],
        }
    }

    /// Number of granules currently in the set.
    pub fn len(&self) -> u64 {
        self.words.iter().map(|w| w.count_ones() as u64).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.words.iter().all(|w| *w == 0)
    }

    /// Bytes covered by the set.
    pub fn bytes(&self) -> u64 {
        self.len() * GRANULE
    }

    /// Mark the whole `[offset, offset + len)` range as touched.
    ///
    /// Ranges outside the image are ignored rather than rejected: this is called from the
    /// fault path, where a mapping that runs past the end of a truncated memory file is
    /// already handled (zero-filled) and must not become an error here.
    pub fn insert_range(&mut self, offset: u64, len: u64) {
        self.insert_range_at(offset, len, 0);
    }

    /// Mark `[offset, offset + len)` as touched at `stamp` (see [`FirstTouch`]). A granule that
    /// already has an earlier time keeps it.
    pub fn insert_range_at(&mut self, offset: u64, len: u64, stamp: FirstTouch) {
        let first = offset / GRANULE;
        let last = offset.saturating_add(len.max(1) - 1) / GRANULE;
        for granule in first..=last.min(self.granules.saturating_sub(1)) {
            if granule >= self.granules {
                break;
            }
            let bit = 1u64 << (granule % 64);
            let word = &mut self.words[(granule / 64) as usize];
            let time = &mut self.first[granule as usize];
            *time = if *word & bit == 0 {
                stamp
            } else {
                earlier(*time, stamp)
            };
            *word |= bit;
        }
    }

    /// The first-touch time of a granule in the set (0 when unknown or absent).
    #[cfg(test)]
    pub(crate) fn first_touch(&self, granule: u64) -> FirstTouch {
        if self.contains(granule) {
            self.first[granule as usize]
        } else {
            0
        }
    }

    /// Membership of a single granule. Serving only ever walks whole [`Run`]s, so this exists
    /// for the tests that pin down `insert_range`'s granule arithmetic.
    #[cfg(test)]
    fn contains(&self, granule: u64) -> bool {
        granule < self.granules
            && self.words[(granule / 64) as usize] & (1u64 << (granule % 64)) != 0
    }

    /// Whether every granule of `self` is in `other`, first-touch times aside.
    pub fn is_subset_of(&self, other: &PageSet) -> bool {
        self.granules == other.granules
            && self
                .words
                .iter()
                .zip(&other.words)
                .all(|(mine, theirs)| mine & !theirs == 0)
    }

    /// OR `other` into `self`, returning how many granules were newly added.
    ///
    /// Sets built for different image sizes merge over their overlap; the caller validates
    /// sizes, this just refuses to read or write out of bounds.
    pub fn union_from(&mut self, other: &PageSet) -> u64 {
        let mut added = 0u64;
        let common = self.words.len().min(other.words.len());
        for i in 0..common {
            let before = self.words[i];
            let theirs = other.words[i];
            if theirs != 0 {
                let mut bits = theirs;
                while bits != 0 {
                    let granule = i * 64 + bits.trailing_zeros() as usize;
                    bits &= bits - 1;
                    let (Some(mine), Some(time)) =
                        (self.first.get(granule), other.first.get(granule))
                    else {
                        continue;
                    };
                    self.first[granule] = if before & (1u64 << (granule % 64)) == 0 {
                        *time
                    } else {
                        earlier(*mine, *time)
                    };
                }
            }
            let after = before | theirs;
            added += (after & !before).count_ones() as u64;
            self.words[i] = after;
        }
        added
    }

    /// A digest of the set's granules and the first-touch tier of each: what a working-set
    /// pack, which stores the granules' bytes in tier order, was built from. A time that moves
    /// within its tier leaves it unchanged. See [`order_digest_of`].
    pub fn order_digest(&self) -> [u8; 32] {
        order_digest_of(&self.by_first_touch())
    }

    /// The set split by first-touch tier: one bitmap per tier, earliest first, each holding the
    /// granules first touched in that tier's interval (the last also those with no known time).
    /// Replay and the page cache warm-up walk the tiers in order, so the pages a clone needs
    /// first are mapped first. Costs one bitmap per tier, which the image size fixes.
    pub fn by_first_touch(&self) -> Vec<PageSet> {
        let mut tiers: Vec<PageSet> = (0..TIERS)
            .map(|_| PageSet {
                granules: self.granules,
                words: vec![0; self.words.len()],
                first: Vec::new(),
            })
            .collect();
        for (i, word) in self.words.iter().enumerate() {
            let mut bits = *word;
            while bits != 0 {
                let granule = i * 64 + bits.trailing_zeros() as usize;
                bits &= bits - 1;
                let tier = tier_of(self.first.get(granule).copied().unwrap_or(0));
                tiers[tier].words[i] |= 1u64 << (granule % 64);
            }
        }
        tiers
    }

    /// Coalesce the set into as few contiguous runs as possible.
    ///
    /// For a Chromium clone this turns 56k scattered fault offsets into a few hundred bulk
    /// copies: the arrival ORDER of faults is scattered, but the SET is dense in runs. A large
    /// warmed guest coalesces far less (see `prefetch::plan`), so replay plans every run and
    /// never a fixed number of them.
    pub fn runs(&self) -> RunIter<'_> {
        RunIter {
            set: self,
            cursor: 0,
        }
    }

    /// First granule at or after `from` that is in the set.
    fn next_set(&self, from: u64) -> Option<u64> {
        let mut granule = from;
        while granule < self.granules {
            let word_idx = (granule / 64) as usize;
            let word = self.words[word_idx] >> (granule % 64);
            if word != 0 {
                let hit = granule + word.trailing_zeros() as u64;
                return (hit < self.granules).then_some(hit);
            }
            granule = (word_idx as u64 + 1) * 64;
        }
        None
    }

    /// First granule at or after `from` that is NOT in the set (capped at `granules`).
    fn next_clear(&self, from: u64) -> u64 {
        let mut granule = from;
        while granule < self.granules {
            let word_idx = (granule / 64) as usize;
            let word = !self.words[word_idx] >> (granule % 64);
            if word != 0 {
                return (granule + word.trailing_zeros() as u64).min(self.granules);
            }
            granule = (word_idx as u64 + 1) * 64;
        }
        self.granules
    }

    /// Serialise to the on-disk representation.
    fn encode(&self, key: &ImageKey, mem_len: u64) -> Vec<u8> {
        let mut buf = Vec::with_capacity(HEADER_LEN + self.words.len() * 8);
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(&VERSION.to_le_bytes());
        buf.extend_from_slice(&GRANULE.to_le_bytes());
        buf.extend_from_slice(&mem_len.to_le_bytes());
        buf.extend_from_slice(&self.granules.to_le_bytes());
        buf.extend_from_slice(&key.0);
        for word in &self.words {
            buf.extend_from_slice(&word.to_le_bytes());
        }
        for (i, word) in self.words.iter().enumerate() {
            let mut bits = *word;
            while bits != 0 {
                let granule = i * 64 + bits.trailing_zeros() as usize;
                bits &= bits - 1;
                buf.extend_from_slice(&self.first[granule].to_le_bytes());
            }
        }
        buf
    }

    /// Parse the on-disk representation, checking it describes `key`'s image.
    ///
    /// Every rejection is a `Ok(None)` with a reason, not an error: a working set that does
    /// not apply is simply absent.
    fn decode(buf: &[u8], key: &ImageKey, mem_len: u64) -> Option<Self> {
        let reject = |why: &str| -> Option<Self> {
            debug!(target: "uffd", reason = why, "ignoring recorded working set");
            None
        };
        if buf.len() < HEADER_LEN {
            return reject("file shorter than header");
        }
        let u64_at = |off: usize| u64::from_le_bytes(buf[off..off + 8].try_into().unwrap());
        if &buf[0..8] != MAGIC {
            return reject("bad magic");
        }
        if u64_at(8) != VERSION {
            return reject("unsupported version");
        }
        if u64_at(16) != GRANULE {
            return reject("different granule");
        }
        if u64_at(24) != mem_len {
            return reject("memory image size changed");
        }
        let granules = u64_at(32);
        if granules != mem_len.div_ceil(GRANULE) {
            return reject("granule count does not match image size");
        }
        if buf[40..72] != key.0 {
            return reject("memory image changed (key mismatch)");
        }
        let words = (granules as usize).div_ceil(64);
        if buf.len() < HEADER_LEN + words * 8 {
            return reject("truncated bitmap");
        }
        let mut set = Self {
            granules,
            words: Vec::with_capacity(words),
            first: vec![0; granules as usize],
        };
        for i in 0..words {
            let off = HEADER_LEN + i * 8;
            set.words
                .push(u64::from_le_bytes(buf[off..off + 8].try_into().unwrap()));
        }
        // Bits past the end of the image would produce runs outside the file.
        if granules % 64 != 0 {
            let tail = &mut set.words[words - 1];
            *tail &= u64::MAX >> (64 - granules % 64);
        }
        let members = set.len() as usize;
        let times_at = HEADER_LEN + words * 8;
        if buf.len() < times_at + members * 2 {
            return reject("truncated first-touch times");
        }
        if buf.len() > times_at + members * 2 {
            return reject("bytes after the first-touch times");
        }
        let mut next = times_at;
        for (i, word) in set.words.iter().enumerate() {
            let mut bits = *word;
            while bits != 0 {
                let granule = i * 64 + bits.trailing_zeros() as usize;
                bits &= bits - 1;
                set.first[granule] = u16::from_le_bytes([buf[next], buf[next + 1]]);
                next += 2;
            }
        }
        Some(set)
    }
}

/// The [`PageSet::order_digest`] of the set `tiers` splits ([`PageSet::by_first_touch`]): the
/// granule count and every tier's bitmap, which together name each granule of the set and its
/// tier. Hashed from the tiers so the store, which keeps the split current, gets the digest
/// with it: the tiers of a 128 GiB guest are 44 MiB of words, hashed in bulk.
pub fn order_digest_of(tiers: &[PageSet]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update((tiers.len() as u64).to_le_bytes());
    if let Some(first) = tiers.first() {
        hasher.update(first.granules.to_le_bytes());
    }
    let mut buf = Vec::with_capacity(64 * 1024);
    for tier in tiers {
        for word in &tier.words {
            buf.extend_from_slice(&word.to_le_bytes());
            if buf.len() >= 64 * 1024 {
                hasher.update(&buf);
                buf.clear();
            }
        }
    }
    hasher.update(&buf);
    hasher.finalize().into()
}

/// Iterator over the maximal contiguous runs of a [`PageSet`].
pub struct RunIter<'a> {
    set: &'a PageSet,
    cursor: u64,
}

impl Iterator for RunIter<'_> {
    type Item = Run;

    fn next(&mut self) -> Option<Run> {
        let start = self.set.next_set(self.cursor)?;
        let end = self.set.next_clear(start);
        self.cursor = end;
        Some(Run {
            offset: start * GRANULE,
            len: (end - start) * GRANULE,
        })
    }
}

/// The runs of a [`PageSet`] in first-touch order: every run of the earliest tier in
/// ascending offset, then every run of the next tier, and so on (see
/// [`PageSet::by_first_touch`]). Runs of different tiers are never joined, so a run is
/// contiguous within its tier.
pub struct OrderedRuns {
    tiers: Arc<Vec<PageSet>>,
    tier: usize,
    cursor: u64,
}

impl OrderedRuns {
    pub fn new(set: &PageSet) -> Self {
        Self::from_tiers(Arc::new(set.by_first_touch()))
    }

    /// The runs of a set already split by [`PageSet::by_first_touch`].
    pub fn from_tiers(tiers: Arc<Vec<PageSet>>) -> Self {
        Self {
            tiers,
            tier: 0,
            cursor: 0,
        }
    }

    /// The tier of the run [`next`](Iterator::next) returned last.
    pub fn tier(&self) -> usize {
        self.tier
    }

    /// How far into its current tier this iterator has read, in bytes.
    #[cfg(test)]
    pub(crate) fn position(&self) -> u64 {
        self.cursor * GRANULE
    }
}

impl Iterator for OrderedRuns {
    type Item = Run;

    fn next(&mut self) -> Option<Run> {
        loop {
            let set = self.tiers.get(self.tier)?;
            let Some(start) = set.next_set(self.cursor) else {
                self.tier += 1;
                self.cursor = 0;
                continue;
            };
            let end = set.next_clear(start);
            self.cursor = end;
            return Some(Run {
                offset: start * GRANULE,
                len: (end - start) * GRANULE,
            });
        }
    }
}

/// What a merge did, for logging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MergeOutcome {
    /// Granules in the union after merging.
    pub total: u64,
    /// Granules this clone contributed that nothing had recorded before.
    pub added: u64,
    /// Whether the union was written back to disk (only when it grew).
    pub persisted: bool,
    /// The image at the memory path is no longer the one we served, so nothing was published.
    /// Happens when a snapshot tag is recreated while this serve process is still running: the
    /// server keeps its open (old) inode, but the working-set path now belongs to the NEW
    /// image, and publishing our bitmap there would clobber the new generation's record.
    pub superseded: bool,
}

/// One clone-completion notification for the persistence worker.
///
/// The bitmap itself is already merged into [`WorkingSetStore::known`] before this is queued;
/// the request is only a coalescible prompt to publish the latest in-memory union.
struct PersistRequest {
    vm_id: String,
    faulted_pages: u64,
    learned_pages: u64,
}

/// A non-blocking front end to the on-disk working-set hint.
///
/// One detached OS thread owns filesystem persistence for a served snapshot. Tokio does not
/// own or join that thread, so a stuck filesystem cannot retain a clone's admission slot or
/// prevent the async runtime from shutting down. The one-entry channel is intentionally
/// bounded: every caller first unions its observations into `known`, so a queued request will
/// publish all later observations too and additional requests can be safely coalesced.
#[derive(Clone)]
pub(crate) struct WorkingSetPersistence {
    store: Arc<WorkingSetStore>,
    requests: SyncSender<PersistRequest>,
}

impl WorkingSetPersistence {
    /// A persistence worker for `store`, which also builds the snapshot's pack after each
    /// publication, with its temporaries recorded in this user's reclaim directory.
    pub(crate) fn new(store: Arc<WorkingSetStore>) -> Result<Self> {
        Self::with_pack_builds(store, PackBuilds::On { reclaim_dir: None })
    }

    /// [`WorkingSetPersistence::new`] with pack builds as `builds` says.
    pub(crate) fn with_pack_builds(
        store: Arc<WorkingSetStore>,
        builds: PackBuilds,
    ) -> Result<Self> {
        let (requests, receiver) = sync_channel(1);
        let worker_store = Arc::clone(&store);
        let builder = match builds {
            PackBuilds::On { reclaim_dir } => {
                PackBuilder::new(Arc::clone(&store), reclaim_dir, None)
            }
            #[cfg(test)]
            PackBuilds::Hooked {
                reclaim_dir,
                frame_hook,
            } => PackBuilder::new(Arc::clone(&store), Some(reclaim_dir), Some(frame_hook)),
            PackBuilds::Off => None,
        };
        let worker = std::thread::Builder::new()
            .name("fcvm-ws-write".to_string())
            .spawn(move || persistence_worker(worker_store, receiver, builder))
            .context("spawning working-set persistence worker")?;
        // Deliberately detach: unlike Tokio's blocking pool, a stuck hint writer must not be
        // joined during runtime/process teardown.
        drop(worker);
        Ok(Self { store, requests })
    }

    /// Merge `observed` into memory and non-blockingly prompt the worker to publish it.
    pub(crate) fn schedule(&self, vm_id: &str, observed: PageSet) {
        let faulted_pages = observed.len();
        let learned_pages = self.store.remember(&observed);
        let request = PersistRequest {
            vm_id: vm_id.to_string(),
            faulted_pages,
            learned_pages,
        };
        match self.requests.try_send(request) {
            Ok(()) => {}
            Err(TrySendError::Full(request)) => debug!(
                target: "uffd",
                vm_id = %request.vm_id,
                faulted_pages = request.faulted_pages,
                learned_pages = request.learned_pages,
                "coalesced working-set persistence behind the queued union"
            ),
            Err(TrySendError::Disconnected(request)) => warn!(
                target: "uffd",
                vm_id = %request.vm_id,
                faulted_pages = request.faulted_pages,
                learned_pages = request.learned_pages,
                "working-set persistence worker is unavailable; restore remains demand-page safe"
            ),
        }
    }
}

fn persistence_worker(
    store: Arc<WorkingSetStore>,
    receiver: Receiver<PersistRequest>,
    builder: Option<PackBuilder>,
) {
    // The first publication pass of a server asks for a pack even when the sidecar did not
    // change, so a snapshot whose pack is missing or stale gets one; after that, only a
    // changed sidecar does. The builder skips a set whose pack is already on disk.
    let mut asked = false;
    while let Ok(request) = receiver.recv() {
        match store.try_persist_known() {
            Ok(Some((outcome, set))) => {
                info!(
                    target: "uffd",
                    vm_id = %request.vm_id,
                    faulted_pages = request.faulted_pages,
                    learned_pages = request.learned_pages,
                    known_pages = outcome.total,
                    persisted = outcome.persisted,
                    superseded = outcome.superseded,
                    "merged this clone's faults into the snapshot's working set"
                );
                if let Some(builder) = &builder {
                    if !outcome.superseded && (outcome.persisted || !asked) {
                        asked = true;
                        builder.request(set);
                    }
                }
            }
            Ok(None) => debug!(
                target: "uffd",
                vm_id = %request.vm_id,
                faulted_pages = request.faulted_pages,
                learned_pages = request.learned_pages,
                "working-set sidecar is busy; retained the union in memory"
            ),
            Err(error) => warn!(
                target: "uffd",
                vm_id = %request.vm_id,
                faulted_pages = request.faulted_pages,
                learned_pages = request.learned_pages,
                error = ?error,
                "could not persist the working set; restore remains demand-page safe"
            ),
        }
    }
}

/// Whether a persistence worker builds the snapshot's pack after it publishes.
pub(crate) enum PackBuilds {
    /// It does, with each temporary recorded in `reclaim_dir` (`None`:
    /// [`crate::reclaim::owner_dir`]).
    On { reclaim_dir: Option<PathBuf> },
    /// It does not: for tests of persistence alone, which must not write reclaim records in
    /// the real data directory or build after they return.
    #[cfg_attr(not(test), allow(dead_code))]
    Off,
    /// Test only: [`PackBuilds::On`] with `frame_hook` called before each frame a build reads.
    #[cfg(test)]
    Hooked {
        reclaim_dir: PathBuf,
        frame_hook: FrameHook,
    },
}

/// Threads a pack build reads and compresses on. A build runs in the background after a clone
/// exits, and nothing waits for it.
pub(crate) const PACK_BUILD_THREADS: usize = 4;

/// The thread that builds a served snapshot's pack after its sidecar is published.
///
/// Detached, like the persistence worker. It holds one pending set: a request made while a
/// build runs replaces the pending one, and the running build finishes and installs (a pack
/// of a smaller set is still used for the runs it holds), after which the latest pending set
/// is built. A build that fails logs a warning, and replay reads the memory file.
struct PackBuilder {
    shared: Arc<PackRequests>,
}

#[derive(Default)]
struct PackRequests {
    pending: Mutex<PendingPack>,
    wake: std::sync::Condvar,
}

#[derive(Default)]
struct PendingPack {
    set: Option<PageSet>,
    /// The persistence worker is gone: the builder finishes what is pending and exits, so it
    /// keeps the store no longer than the serve does.
    closed: bool,
}

impl PackBuilder {
    fn new(
        store: Arc<WorkingSetStore>,
        reclaim_dir: Option<PathBuf>,
        frame_hook: Option<FrameHook>,
    ) -> Option<Self> {
        let shared = Arc::new(PackRequests::default());
        let worker_shared = Arc::clone(&shared);
        let spawned = std::thread::Builder::new()
            .name("fcvm-ws-pack".to_string())
            .spawn(move || pack_builder(store, worker_shared, reclaim_dir, frame_hook));
        match spawned {
            Ok(worker) => {
                drop(worker);
                Some(Self { shared })
            }
            Err(error) => {
                warn!(target: "uffd", %error, "could not start the working-set pack builder; replay reads the memory file");
                None
            }
        }
    }

    fn request(&self, set: PageSet) {
        self.shared.pending.lock().expect("pack request mutex").set = Some(set);
        self.shared.wake.notify_one();
    }
}

impl Drop for PackBuilder {
    fn drop(&mut self) {
        if let Ok(mut pending) = self.shared.pending.lock() {
            pending.closed = true;
        }
        self.shared.wake.notify_one();
    }
}

/// Called before each frame a build reads: a test's way to pause a build. `None` in a serve.
type FrameHook = Arc<dyn Fn() + Send + Sync>;

fn pack_builder(
    store: Arc<WorkingSetStore>,
    requests: Arc<PackRequests>,
    reclaim_dir: Option<PathBuf>,
    frame_hook: Option<FrameHook>,
) {
    loop {
        let set = {
            let mut pending = requests.pending.lock().expect("pack request mutex");
            loop {
                if let Some(set) = pending.set.take() {
                    break set;
                }
                if pending.closed {
                    return;
                }
                pending = requests.wake.wait(pending).expect("pack request mutex");
            }
        };
        let dir = match &reclaim_dir {
            Some(dir) => Ok(dir.clone()),
            None => crate::reclaim::owner_dir(),
        };
        let started = std::time::Instant::now();
        let at_each_frame = || {
            if let Some(hook) = &frame_hook {
                hook();
            }
            false
        };
        let built =
            dir.and_then(|dir| store.build_pack(&set, &dir, PACK_BUILD_THREADS, &at_each_frame));
        log_pack_build(&store.pack_path(), &built, started.elapsed());
        if matches!(built, Ok(PackBuild::Installed(_))) {
            store.load_pack();
        }
    }
}

/// The one line a pack build logs.
pub(crate) fn log_pack_build(path: &Path, built: &Result<PackBuild>, took: std::time::Duration) {
    match built {
        Ok(PackBuild::Installed(stats)) => info!(
            target: "uffd",
            path = %path.display(),
            runs = stats.runs,
            frames = stats.frames,
            mib = stats.raw_bytes / (1024 * 1024),
            compressed_mib = stats.compressed_bytes / (1024 * 1024),
            elapsed_ms = took.as_millis() as u64,
            "built the working-set pack"
        ),
        Ok(outcome) => debug!(
            target: "uffd",
            path = %path.display(),
            outcome = ?outcome,
            "installed no working-set pack"
        ),
        Err(error) => warn!(
            target: "uffd",
            path = %path.display(),
            error = %format!("{error:#}"),
            "could not build the working-set pack; replay reads the memory file"
        ),
    }
}

/// The working set stored beside a snapshot's memory image.
///
/// One store per served snapshot, shared by every clone the server hosts. It holds the union
/// of everything known about the image — what was on disk at startup, plus what each clone has
/// contributed since — so a clone that starts later in the same server benefits from what an
/// earlier clone already discovered without a round trip through the filesystem.
pub struct WorkingSetStore {
    path: PathBuf,
    /// The memory image this set describes. Kept so the identity can be re-checked at publish
    /// time — the path may name a different image by then (see `MergeOutcome::superseded`).
    mem_path: PathBuf,
    /// Exact config bytes bind the hint to one atomically installed snapshot generation,
    /// independently of filesystem inode reuse or timestamp granularity.
    generation_config_path: PathBuf,
    generation_config_digest: [u8; 32],
    /// Stable sibling generation lock used by snapshot create/delete/restore.
    ///
    /// A merge takes this shared before it re-identifies the memory image and keeps it
    /// through the atomic sidecar publication. Without that single critical section, a
    /// creator could replace the snapshot after the identity check but before `rename`, and
    /// the old server would publish its bitmap into the new generation.
    generation_lock_path: PathBuf,
    lock_path: PathBuf,
    key: ImageKey,
    mem_len: u64,
    known: Mutex<PageSet>,
    /// `known` split by first-touch tier, for replay. Splitting a large set scans every
    /// granule (about 144 ms for a dense 128 GiB guest), so it is done when the set is loaded
    /// and on a thread of its own after the set changes, never on a clone's fault path.
    ordered: Arc<Mutex<OrderedTiers>>,
    /// The pack opened last ([`WorkingSetStore::load_pack`]), for clones to replay from. Any
    /// pack of this image serves: replay copies the runs it holds and reads the memory file for
    /// the rest.
    pack: Mutex<Option<Arc<Pack>>>,
    /// Held while a pack is opened, so two loads do not both read its tables.
    pack_load: Mutex<()>,
    /// Whether this store describes the memory image its server maps
    /// ([`WorkingSetStore::bind_served_image`]). A pack is data, so it is replayed only into
    /// clones of the image it was built from, and a store nobody bound replays none.
    pack_allowed: std::sync::atomic::AtomicBool,
}

/// What [`WorkingSetStore::build_pack`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackBuild {
    /// The set is empty, so there is nothing to pack.
    Empty,
    /// The pack on disk already holds this set.
    AlreadyBuilt,
    /// A pack was built and installed.
    Installed(super::pack::BuildStats),
    /// The memory image or snapshot generation is no longer the one this store describes, so
    /// nothing was installed.
    Superseded,
    /// The sidecar no longer holds every run of the set the pack was built from (it was
    /// rewritten with another set), so the pack was not installed.
    SetChanged,
}

/// A cached [`PageSet::by_first_touch`] of the store's union, and which change of the union
/// it was split from.
#[derive(Default)]
struct OrderedTiers {
    /// Bumped by every change of the union.
    version: u64,
    /// The version `tiers` was split from.
    split_from: u64,
    tiers: Option<Ordered>,
}

/// The store's union split by first-touch tier ([`PageSet::by_first_touch`]), with its
/// [`order_digest_of`]: what replay plans from, and what names the pack it may replay from.
#[derive(Clone)]
pub struct Ordered {
    pub tiers: Arc<Vec<PageSet>>,
    pub digest: [u8; 32],
}

impl Ordered {
    fn of(set: &PageSet) -> Self {
        let tiers = set.by_first_touch();
        let digest = order_digest_of(&tiers);
        Self {
            tiers: Arc::new(tiers),
            digest,
        }
    }
}

impl WorkingSetStore {
    /// Path of the working set recorded beside the memory image at `mem_file_path`.
    pub fn path_for(mem_file_path: &Path) -> PathBuf {
        let mut name = mem_file_path.file_name().unwrap_or_default().to_os_string();
        name.push(".working-set");
        mem_file_path.with_file_name(name)
    }

    /// Open (and load, if present and applicable) the working set for a memory image.
    ///
    /// Never fails because of the recorded file: an unreadable, stale or corrupt one leaves an
    /// empty store, which prefetches nothing and re-records from scratch. The exact config,
    /// memory identity, and sidecar are read under the snapshot's shared generation lease so
    /// a creator cannot mix generations during startup.
    pub fn open(
        mem_file_path: &Path,
        mem_len: u64,
        generation_config_path: &Path,
        generation_lock_path: &Path,
    ) -> Result<Self> {
        anyhow::ensure!(
            mem_len <= MAX_MEM_LEN,
            "memory image {} is {mem_len} bytes, past the {MAX_MEM_LEN}-byte working-set limit",
            mem_file_path.display()
        );
        let generation_file = File::options()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(generation_lock_path)
            .with_context(|| {
                format!(
                    "opening snapshot generation lock {}",
                    generation_lock_path.display()
                )
            })?;
        let generation_lock = Flock::lock(generation_file, FlockArg::LockShared)
            .map_err(|(_, err)| err)
            .context("locking snapshot generation while opening its working set")?;
        let generation_config = std::fs::read(generation_config_path).with_context(|| {
            format!(
                "reading snapshot generation config {}",
                generation_config_path.display()
            )
        })?;
        let generation_config_digest: [u8; 32] = Sha256::digest(&generation_config).into();
        let key = ImageKey::of(mem_file_path, &generation_config_digest)?;
        let path = Self::path_for(mem_file_path);
        let lock_path = sidecar_lock_path(&path);

        let known = match read_set(&path, &key, mem_len) {
            Some(set) => {
                info!(
                    target: "uffd",
                    path = %path.display(),
                    pages = set.len(),
                    mib = set.bytes() / (1024 * 1024),
                    key = ?key,
                    "loaded recorded restore working set"
                );
                if hint_covers_most_of_the_image(set.bytes(), mem_len) {
                    warn!(
                        target: "uffd",
                        path = %path.display(),
                        hint_mib = set.bytes() / (1024 * 1024),
                        guest_mib = mem_len / (1024 * 1024),
                        "recorded working set covers more than half the guest, which looks \
                         like a lifetime footprint rather than a restore working set (issue \
                         #858); replay will populate all of it. To re-record: stop the serve \
                         process, delete the sidecar, then restart the serve. Deleting it \
                         while a serve is running does not repair that server; the loaded \
                         hint stays in its in-memory union and the persistence worker can \
                         write the sidecar again."
                    );
                }
                set
            }
            None => PageSet::empty(mem_len),
        };
        generation_lock.unlock().map_err(|(_, err)| err).ok();

        Ok(Self {
            path,
            mem_path: mem_file_path.to_path_buf(),
            generation_config_path: generation_config_path.to_path_buf(),
            generation_config_digest,
            generation_lock_path: generation_lock_path.to_path_buf(),
            lock_path,
            key,
            mem_len,
            ordered: Arc::new(Mutex::new(OrderedTiers {
                version: 0,
                split_from: 0,
                tiers: Some(Ordered::of(&known)),
            })),
            known: Mutex::new(known),
            pack: Mutex::new(None),
            pack_load: Mutex::new(()),
            pack_allowed: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Write `set` as the recorded working set of the snapshot generation whose memory image
    /// and config are at these paths, replacing any, for a caller that holds that generation's
    /// lock exclusively: `snapshot create` calls it after publishing the snapshot and before it
    /// gives the lock back.
    ///
    /// No serve can open the generation's store or publish into it while that lock is held,
    /// so the set is on disk before any serve of the generation loads one. This takes no
    /// generation lock of its own: the caller already holds it, and a second `flock` on it
    /// from this process through another descriptor would wait on the caller. The sidecar's
    /// own lock still orders the write against any other writer.
    pub(crate) fn seed_held_generation(
        mem_file_path: &Path,
        mem_len: u64,
        generation_config_path: &Path,
        set: &PageSet,
    ) -> Result<()> {
        anyhow::ensure!(
            mem_len <= MAX_MEM_LEN,
            "memory image {} is {mem_len} bytes, past the {MAX_MEM_LEN}-byte working-set limit",
            mem_file_path.display()
        );
        let key = ImageKey::of_generation(mem_file_path, generation_config_path)?;
        let path = Self::path_for(mem_file_path);
        let lock_file = File::options()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(sidecar_lock_path(&path))
            .with_context(|| format!("opening the working-set lock for {}", path.display()))?;
        let lock = Flock::lock(lock_file, FlockArg::LockExclusive)
            .map_err(|(_, err)| err)
            .context("locking working set")?;
        let result = write_set(&path, set, &key, mem_len);
        lock.unlock().map_err(|(_, err)| err).ok();
        result
    }

    /// The set to prefetch for a clone starting now.
    pub fn to_prefetch(&self) -> PageSet {
        self.known.lock().expect("working set mutex").clone()
    }

    /// The set to prefetch for a clone starting now, split by first-touch tier. The split is the
    /// cached one when it is current, which is the usual case: it was made when the set loaded
    /// or after its last change. Otherwise it is made here.
    pub fn to_prefetch_ordered(&self) -> Ordered {
        let known = self.known.lock().expect("working set mutex");
        let mut ordered = self.ordered.lock().expect("working set order mutex");
        if ordered.split_from == ordered.version {
            if let Some(tiers) = &ordered.tiers {
                return tiers.clone();
            }
        }
        let tiers = Ordered::of(&known);
        ordered.split_from = ordered.version;
        ordered.tiers = Some(tiers.clone());
        tiers
    }

    /// Split the union again on a thread of its own after it changed. A split that a later
    /// change made stale is dropped.
    fn resplit_after_change(&self, known: &PageSet) {
        let version = {
            let mut ordered = self.ordered.lock().expect("working set order mutex");
            ordered.version += 1;
            ordered.version
        };
        let set = known.clone();
        let ordered = self.ordered.clone();
        let spawned = std::thread::Builder::new()
            .name("fcvm-ws-order".into())
            .spawn(move || {
                let tiers = Ordered::of(&set);
                let mut ordered = ordered.lock().expect("working set order mutex");
                if ordered.version == version {
                    ordered.split_from = version;
                    ordered.tiers = Some(tiers);
                }
            });
        if let Err(error) = spawned {
            debug!(target: "uffd", %error, "the working-set order split runs on the next clone instead");
        }
    }

    /// An empty set sized for this image, for a clone to record into.
    pub fn recorder(&self) -> PageSet {
        PageSet::empty(self.mem_len)
    }

    /// Add one clone's observations to the in-process union without touching the filesystem.
    fn remember(&self, observed: &PageSet) -> u64 {
        let mut known = self.known.lock().expect("working set mutex");
        let added = known.union_from(observed);
        self.resplit_after_change(&known);
        added
    }

    /// Merge one clone's observations into the union and persist it if it grew.
    ///
    /// Two clones of a snapshot fault nearly the same pages, so in the steady state this adds
    /// nothing and writes nothing. It matters in exactly two cases: the first clone of a new
    /// snapshot (which records everything), and a clone that was killed before it finished
    /// faulting in (whose truncated record is completed by the next clone rather than being
    /// baked in forever).
    ///
    /// Race-free against both other servers and snapshot replacement: the whole
    /// read-modify-write runs under an exclusive `flock` on a sidecar lock file, while a
    /// shared snapshot-generation lease spans the final image-identity check and atomic
    /// rename. Creators/deleters take that generation lease exclusively, so they cannot land
    /// in the check/publish gap. Losing the cache to a crash is harmless — the next clone
    /// re-records what is missing.
    ///
    /// This is the synchronous transaction used by direct callers and store tests. Production
    /// clone teardown first updates the in-memory union and schedules
    /// [`WorkingSetPersistence`], whose detached worker uses a non-blocking sidecar lock. The
    /// in-memory mutex is held only long enough to clone or update the bitmap, never while
    /// waiting on `flock` or doing filesystem I/O.
    pub fn merge_and_persist(&self, observed: &PageSet) -> Result<MergeOutcome> {
        self.merge_and_persist_inner(observed, false)?
            .map(|(outcome, _)| outcome)
            .ok_or_else(|| anyhow::anyhow!("blocking working-set lock unexpectedly contended"))
    }

    /// Publish the latest in-memory union without ever waiting for the sidecar lock.
    ///
    /// Another persistence worker already owns that lock when this returns `None`. The
    /// observations remain in `known`; that worker's queued successor or a later clone will
    /// publish the union. Contention can therefore cost a cache update, never liveness.
    ///
    /// With the outcome comes the union the sidecar now holds, or would hold had publication
    /// not been superseded: the set a pack is built for.
    fn try_persist_known(&self) -> Result<Option<(MergeOutcome, PageSet)>> {
        self.merge_and_persist_inner(&PageSet::empty(self.mem_len), true)
    }

    fn merge_and_persist_inner(
        &self,
        observed: &PageSet,
        nonblocking_sidecar: bool,
    ) -> Result<Option<(MergeOutcome, PageSet)>> {
        let mut merged = self.known.lock().expect("working set mutex").clone();

        // Snapshot creators/deleters take this inode exclusively. Keep the shared lease from
        // the image-identity check through the sidecar rename: checking first and locking
        // later leaves a check/use race in which a replacement generation receives the old
        // generation's bitmap.
        let generation_file = File::options()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&self.generation_lock_path)
            .with_context(|| {
                format!(
                    "opening snapshot generation lock {}",
                    self.generation_lock_path.display()
                )
            })?;
        let generation_lock = Flock::lock(generation_file, FlockArg::LockShared)
            .map_err(|(_, err)| err)
            .context("locking snapshot generation for working-set publication")?;

        let lock_file = File::options()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&self.lock_path)
            .with_context(|| format!("opening working-set lock {}", self.lock_path.display()))?;
        let lock_arg = if nonblocking_sidecar {
            FlockArg::LockExclusiveNonblock
        } else {
            FlockArg::LockExclusive
        };
        let lock = match Flock::lock(lock_file, lock_arg) {
            Ok(lock) => lock,
            Err((_file, error))
                if nonblocking_sidecar && error == nix::errno::Errno::EWOULDBLOCK =>
            {
                generation_lock.unlock().map_err(|(_, err)| err).ok();
                return Ok(None);
            }
            Err((_file, error)) => return Err(error).context("locking working set"),
        };

        // Re-read under the lock: another serve process may have recorded since we loaded.
        let on_disk = read_set(&self.path, &self.key, self.mem_len);
        if let Some(disk) = on_disk.as_ref() {
            merged.union_from(disk);
        }
        let added = merged.union_from(observed);
        let total = merged.len();
        // The union holds everything on disk, so it differs from the file only when it gained
        // granules or an earlier first-touch time.
        let changed = on_disk.as_ref() != Some(&merged);

        // Publish ONLY if the path still names the image we served. A snapshot tag may have
        // been recreated before this merge acquired its shared generation lease: we keep
        // serving the old inode we already opened, but `self.path` would now sit beside a NEW
        // image. The lease prevents any further replacement through the rename below; this
        // identity check rejects a replacement that already completed.
        let superseded = !self.image_is_still_ours();
        let persisted = changed && !superseded;
        let result = if persisted {
            write_set(&self.path, &merged, &self.key, self.mem_len)
        } else {
            Ok(())
        };

        // Release explicitly, so both locks demonstrably span the whole identity-check and
        // publication transaction above rather than ending wherever the compiler last uses
        // them. Failed unlocks need no special handling: closing each fd releases its lock.
        lock.unlock().map_err(|(_, err)| err).ok();
        generation_lock.unlock().map_err(|(_, err)| err).ok();

        // Preserve everything learned even when publication fails: the current serve process
        // can still prefetch it, and a later successful merge will retry the disk union.
        {
            let mut known = self.known.lock().expect("working set mutex");
            known.union_from(&merged);
            self.resplit_after_change(&known);
        }

        result?;
        Ok(Some((
            MergeOutcome {
                total,
                added,
                persisted,
                superseded,
            },
            merged,
        )))
    }

    /// Whether the snapshot generation and memory image at our paths are still the ones this
    /// set describes.
    ///
    /// A stat failure counts as "not ours": if the image cannot be identified we must not
    /// publish, because the file we would write next to is not one we can vouch for.
    fn image_is_still_ours(&self) -> bool {
        let current_config = match std::fs::read(&self.generation_config_path) {
            Ok(config) => config,
            Err(error) => {
                warn!(
                    target: "uffd",
                    path = %self.generation_config_path.display(),
                    error = %error,
                    "cannot re-identify the snapshot generation; not publishing its working set"
                );
                return false;
            }
        };
        let current_config_digest: [u8; 32] = Sha256::digest(&current_config).into();
        if current_config_digest != self.generation_config_digest {
            return false;
        }
        match ImageKey::of(&self.mem_path, &current_config_digest) {
            Ok(now) => now == self.key,
            Err(e) => {
                warn!(
                    target: "uffd",
                    path = %self.mem_path.display(),
                    error = ?e,
                    "cannot re-identify the memory image; not publishing a working set for it"
                );
                false
            }
        }
    }

    /// Where this snapshot's pack lives ([`pack::path_for`]).
    pub fn pack_path(&self) -> PathBuf {
        pack::path_for(&self.mem_path)
    }

    /// Check that `served`, the `stat` of the memory file a server mapped, is the image this
    /// store describes. The store reads the snapshot's paths after the server opened the file,
    /// and a snapshot replaced between the two leaves the store describing another image. Its
    /// recorded set is then only a wrong hint, but its pack holds another image's bytes, so no
    /// pack is replayed from it. Until this is called, and when it finds the two differ, the
    /// store loads and replays no pack. Returns whether packs are allowed.
    pub fn bind_served_image(&self, served: &std::fs::Metadata) -> bool {
        let bound = ImageKey::of_metadata(served, &self.generation_config_digest) == self.key;
        if !bound {
            warn!(
                target: "uffd",
                path = %self.mem_path.display(),
                "the snapshot was replaced after its memory file was opened; this server \
                 replays no working-set pack"
            );
        }
        self.pack_allowed
            .store(bound, std::sync::atomic::Ordering::Release);
        bound
    }

    fn pack_allowed(&self) -> bool {
        self.pack_allowed.load(std::sync::atomic::Ordering::Acquire)
    }

    /// The loaded pack, without waiting: what a clone's replay asks on its fault path.
    /// [`WorkingSetStore::load_pack`] loads one.
    pub fn pack_for(&self) -> Option<Arc<Pack>> {
        if !self.pack_allowed() {
            return None;
        }
        self.pack.lock().expect("working set pack mutex").clone()
    }

    /// This image's pack, opened now if it is not loaded or a newer one was installed since:
    /// for the page cache warm-up and a build that just installed one, on threads of their own.
    /// `None` when there is no pack of this image.
    pub fn load_pack(&self) -> Option<Arc<Pack>> {
        if !self.pack_allowed() {
            return None;
        }
        let _loading = self.pack_load.lock().expect("working set pack load mutex");
        // A pack installed since, by this serve's builder or another serve's, is a new inode.
        let on_disk = std::fs::metadata(self.pack_path())
            .ok()
            .map(|meta| (meta.dev(), meta.ino()));
        if let Some(pack) = self.pack_for() {
            let loaded = pack
                .file()
                .metadata()
                .ok()
                .map(|meta| (meta.dev(), meta.ino()));
            if loaded.is_some() && loaded == on_disk {
                return Some(pack);
            }
        }
        let opened = Pack::open(&self.pack_path(), &self.key, self.mem_len);
        match opened {
            Ok(Some(pack)) => {
                let pack = Arc::new(pack);
                *self.pack.lock().expect("working set pack mutex") = Some(Arc::clone(&pack));
                info!(
                    target: "uffd",
                    path = %self.pack_path().display(),
                    frames = pack.frames().len(),
                    "loaded the working-set pack"
                );
                Some(pack)
            }
            Ok(None) => None,
            Err(error) => {
                warn!(
                    target: "uffd",
                    path = %self.pack_path().display(),
                    error = %format!("{error:#}"),
                    "ignoring a damaged working-set pack; replay reads the memory file"
                );
                None
            }
        }
    }

    /// Build the pack of `set` from this store's memory image and install it beside the image,
    /// unless the pack there already holds it. The temporary is recorded in `reclaim_dir`;
    /// `threads` threads read and compress; `cancel` is asked before each frame.
    ///
    /// The build reads an open file whose identity it checks first, without any lock, because
    /// reading a large set takes minutes. Installing it is a rename done under the same locks
    /// and checks as publishing the sidecar ([`WorkingSetStore::install_pack`]), so a pack is
    /// never installed beside an image other than the one it was read from.
    pub fn build_pack(
        &self,
        set: &PageSet,
        reclaim_dir: &Path,
        threads: usize,
        cancel: &(dyn Fn() -> bool + Sync),
    ) -> Result<PackBuild> {
        if set.is_empty() {
            return Ok(PackBuild::Empty);
        }
        let ordered = Ordered::of(set);
        let out = self.pack_path();
        // Already built only if the pack opens, tables and all, and was built from this set:
        // a damaged pack, or one of a smaller set, is rebuilt.
        if let Ok(Some(built)) = Pack::open(&out, &self.key, self.mem_len) {
            if built.digest() == &ordered.digest {
                return Ok(PackBuild::AlreadyBuilt);
            }
        }
        let mem = File::open(&self.mem_path)
            .with_context(|| format!("opening {}", self.mem_path.display()))?;
        let meta = mem
            .metadata()
            .context("reading the memory file's identity")?;
        if ImageKey::of_metadata(&meta, &self.generation_config_digest) != self.key {
            return Ok(PackBuild::Superseded);
        }
        let runs: Vec<Run> =
            pack::runs_of(OrderedRuns::from_tiers(ordered.tiers.clone()), self.mem_len).collect();
        let (temp, stats) = pack::build_temp(
            &mem,
            self.mem_len,
            &self.key,
            &ordered.digest,
            &runs,
            &out,
            threads,
            reclaim_dir,
            cancel,
        )?;
        self.install_pack(temp, stats, set)
    }

    /// Rename `temp`, the pack of `set`, into place, under the snapshot's shared generation
    /// lock and the sidecar's lock, and only if the image and generation are still this
    /// store's and every granule of `set` is in the sidecar's set (it may have grown since). Returns
    /// [`PackBuild::Installed`] with `stats` once the pack is in place; otherwise dropping
    /// `temp` removes it.
    ///
    /// The generation lock is the one snapshot create and delete take exclusively, held from
    /// the identity check through the rename, as for the sidecar
    /// ([`WorkingSetStore::merge_and_persist`]): checking first and locking later would let a
    /// replacement land between the two, and the new image would get the old image's bytes.
    /// The sidecar lock orders the install against a publication of another set, so a pack
    /// is installed only beside a set that holds all of its runs.
    fn install_pack(
        &self,
        temp: pack::PackTemp,
        stats: pack::BuildStats,
        set: &PageSet,
    ) -> Result<PackBuild> {
        let generation_file = File::options()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&self.generation_lock_path)
            .with_context(|| {
                format!(
                    "opening snapshot generation lock {}",
                    self.generation_lock_path.display()
                )
            })?;
        let generation_lock = Flock::lock(generation_file, FlockArg::LockShared)
            .map_err(|(_, err)| err)
            .context("locking snapshot generation to install its working-set pack")?;
        let lock_file = File::options()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&self.lock_path)
            .with_context(|| format!("opening working-set lock {}", self.lock_path.display()))?;
        let lock = Flock::lock(lock_file, FlockArg::LockExclusive)
            .map_err(|(_, err)| err)
            .context("locking working set to install its pack")?;

        let outcome = if !self.image_is_still_ours() {
            PackBuild::Superseded
        } else {
            let on_disk = read_set(&self.path, &self.key, self.mem_len);
            let holds_set = on_disk.as_ref().is_some_and(|disk| set.is_subset_of(disk));
            if holds_set {
                temp.install(&self.pack_path())?;
                PackBuild::Installed(stats)
            } else {
                PackBuild::SetChanged
            }
        };
        lock.unlock().map_err(|(_, err)| err).ok();
        generation_lock.unlock().map_err(|(_, err)| err).ok();
        Ok(outcome)
    }
}

/// Whether a recorded hint is so large relative to the guest that it looks like a
/// lifetime footprint rather than a restore working set.
///
/// Honest restore working sets measured 5 to 11 percent of guest memory (6.5 and 14 GiB
/// of a 128 GiB guest, issue #858); the two poisoned sidecars observed there covered 66
/// and 95 percent. Half the guest separates the two populations with margin on both
/// sides.
pub fn hint_covers_most_of_the_image(hint_bytes: u64, mem_len: u64) -> bool {
    // Division, not saturating_mul(2): doubling saturates at u64::MAX and
    // would call a more-than-half hint safe at the boundary.
    hint_bytes > mem_len / 2
}

/// Read and validate a recorded set. `None` means "nothing usable here" — always a normal
/// outcome, never an error.
fn read_set(path: &Path, key: &ImageKey, mem_len: u64) -> Option<PageSet> {
    let granules = mem_len.div_ceil(GRANULE);
    let bitmap_bytes = granules.div_ceil(64).saturating_mul(8);
    let times_bytes = granules.saturating_mul(std::mem::size_of::<FirstTouch>() as u64);
    let max_len = (HEADER_LEN as u64)
        .saturating_add(bitmap_bytes)
        .saturating_add(times_bytes);
    let mut buf = Vec::new();
    match File::open(path).and_then(|f| f.take(max_len.saturating_add(1)).read_to_end(&mut buf)) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            warn!(
                target: "uffd",
                path = %path.display(),
                error = %e,
                "could not read recorded working set - restore will fault on demand"
            );
            return None;
        }
    }
    if buf.len() as u64 > max_len {
        warn!(
            target: "uffd",
            path = %path.display(),
            bytes = buf.len(),
            max_len,
            "recorded working set is oversized - restore will fault on demand"
        );
        return None;
    }
    PageSet::decode(&buf, key, mem_len)
}

/// The lock file that orders writers of the working set at `sidecar`.
fn sidecar_lock_path(sidecar: &Path) -> PathBuf {
    let mut name = sidecar.file_name().unwrap_or_default().to_os_string();
    name.push(".lock");
    sidecar.with_file_name(name)
}

/// Publish a set atomically: unique temp name in the same directory, then rename.
///
/// The temp name carries a uuid rather than a pid because nested VMs have separate pid
/// namespaces and would collide (see AGENTS.md).
fn write_set(path: &Path, set: &PageSet, key: &ImageKey, mem_len: u64) -> Result<()> {
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let tmp = path.with_file_name(tmp_name);

    let encoded = set.encode(key, mem_len);
    let write = || -> Result<()> {
        let mut f = File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        f.write_all(&encoded)
            .with_context(|| format!("writing {}", tmp.display()))?;
        f.sync_all()
            .with_context(|| format!("syncing {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("publishing {}", path.display()))?;
        Ok(())
    };
    let result = write();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Owns every sidecar a store test creates, so a panic cannot leak state into a later run.
    struct FakeImage {
        _dir: TempDir,
        path: PathBuf,
    }

    /// A fake memory image of `len` bytes, so `ImageKey::of` has something to stat.
    fn fake_image(tag: &str, len: u64) -> FakeImage {
        let dir = tempfile::Builder::new()
            .prefix(&format!("fcvm-ws-test-{tag}-"))
            .tempdir()
            .unwrap();
        let path = dir.path().join("memory.bin");
        let f = File::create(&path).unwrap();
        f.set_len(len).unwrap();
        FakeImage { _dir: dir, path }
    }

    impl std::ops::Deref for FakeImage {
        type Target = Path;

        fn deref(&self) -> &Self::Target {
            &self.path
        }
    }

    impl AsRef<Path> for FakeImage {
        fn as_ref(&self) -> &Path {
            &self.path
        }
    }

    fn generation_lock_for(image: &Path) -> PathBuf {
        let mut name = image.file_name().unwrap_or_default().to_os_string();
        name.push(".generation.lock");
        image.with_file_name(name)
    }

    fn generation_config_for(image: &Path) -> PathBuf {
        let mut name = image.file_name().unwrap_or_default().to_os_string();
        name.push(".config.json");
        image.with_file_name(name)
    }

    fn image_key(image: &Path) -> ImageKey {
        ImageKey::of(image, &[0x42; 32]).unwrap()
    }

    fn open_store(image: &Path, mem_len: u64) -> WorkingSetStore {
        let config = generation_config_for(image);
        if !config.exists() {
            std::fs::write(&config, b"generation-1").unwrap();
        }
        WorkingSetStore::open(image, mem_len, &config, &generation_lock_for(image)).unwrap()
    }

    #[test]
    fn insert_range_marks_every_granule_it_spans() {
        let mut set = PageSet::empty(16 * GRANULE);
        // A 2 MiB-granule fault marks all the 4 KiB granules under it.
        set.insert_range(4 * GRANULE, 3 * GRANULE);
        assert_eq!(set.len(), 3);
        assert!(!set.contains(3));
        assert!(set.contains(4) && set.contains(5) && set.contains(6));
        assert!(!set.contains(7));

        // A sub-granule fault still marks exactly one granule.
        set.insert_range(9 * GRANULE + 17, 1);
        assert!(set.contains(9));
        assert_eq!(set.len(), 4);
    }

    #[test]
    fn insert_range_ignores_offsets_past_the_image() {
        let mut set = PageSet::empty(4 * GRANULE);
        set.insert_range(100 * GRANULE, GRANULE);
        assert!(set.is_empty(), "out-of-range insert must be dropped");
        // A range that straddles the end keeps the part that is inside.
        set.insert_range(3 * GRANULE, 8 * GRANULE);
        assert_eq!(set.len(), 1);
        assert!(set.contains(3));

        // A malformed mapping must not wrap the inclusive end back into the bitmap.
        set.insert_range(u64::MAX - GRANULE, 4 * GRANULE);
        assert_eq!(set.len(), 1, "overflowing ranges must remain out of bounds");
    }

    #[test]
    fn runs_coalesce_contiguous_granules() {
        let mut set = PageSet::empty(300 * GRANULE);
        for granule in [0u64, 1, 2, 5, 130, 131] {
            set.insert_range(granule * GRANULE, GRANULE);
        }
        let runs: Vec<Run> = set.runs().collect();
        assert_eq!(
            runs,
            vec![
                Run {
                    offset: 0,
                    len: 3 * GRANULE
                },
                Run {
                    offset: 5 * GRANULE,
                    len: GRANULE
                },
                Run {
                    offset: 130 * GRANULE,
                    len: 2 * GRANULE
                },
            ]
        );
        // Runs cover exactly the set, so bulk copies never touch an unrecorded page.
        assert_eq!(runs.iter().map(|r| r.len).sum::<u64>(), set.bytes());
    }

    #[test]
    fn runs_handle_a_full_set_and_an_empty_set() {
        let empty = PageSet::empty(64 * GRANULE);
        assert_eq!(empty.runs().count(), 0);

        let mut full = PageSet::empty(200 * GRANULE);
        full.insert_range(0, 200 * GRANULE);
        let runs: Vec<Run> = full.runs().collect();
        assert_eq!(
            runs,
            vec![Run {
                offset: 0,
                len: 200 * GRANULE
            }],
            "a fully-faulted image must be ONE run, not 200"
        );
    }

    #[test]
    fn union_reports_only_newly_added_granules() {
        let mut a = PageSet::empty(64 * GRANULE);
        a.insert_range(0, 2 * GRANULE);
        let mut b = PageSet::empty(64 * GRANULE);
        b.insert_range(GRANULE, 3 * GRANULE); // granules 1,2,3 - overlaps a on 1

        assert_eq!(a.union_from(&b), 2, "only granules 2 and 3 are new");
        assert_eq!(a.len(), 4);
        assert_eq!(a.union_from(&b), 0, "re-merging adds nothing");
    }

    #[test]
    fn encode_decode_round_trips() {
        let image = fake_image("roundtrip", 1024 * GRANULE);
        let key = image_key(&image);
        let mem_len = 1024 * GRANULE;

        let mut set = PageSet::empty(mem_len);
        set.insert_range(0, GRANULE);
        set.insert_range(1000 * GRANULE, 5 * GRANULE);

        let encoded = set.encode(&key, mem_len);
        let decoded = PageSet::decode(&encoded, &key, mem_len).expect("must decode");
        assert_eq!(decoded, set);
        assert_eq!(decoded.len(), 6);
    }

    #[test]
    fn first_touch_times_merge_to_the_earlier_known_time() {
        let mem_len = 64 * GRANULE;
        let mut a = PageSet::empty(mem_len);
        a.insert_range_at(0, GRANULE, 30);
        a.insert_range_at(GRANULE, GRANULE, 0);
        a.insert_range_at(2 * GRANULE, GRANULE, 7);
        a.insert_range_at(2 * GRANULE, GRANULE, 12);
        let mut b = PageSet::empty(mem_len);
        b.insert_range_at(0, GRANULE, 5);
        b.insert_range_at(GRANULE, GRANULE, 40);
        b.insert_range_at(3 * GRANULE, GRANULE, 9);
        assert_eq!(a.union_from(&b), 1);
        assert_eq!(
            (0..4).map(|g| a.first_touch(g)).collect::<Vec<_>>(),
            vec![5, 40, 7, 9],
            "a merge keeps the earlier known time, and a known time beats an unknown one"
        );
    }

    #[test]
    fn first_touch_times_round_trip_through_the_file() {
        let image = fake_image("times", 1024 * GRANULE);
        let key = image_key(&image);
        let mem_len = 1024 * GRANULE;
        let mut set = PageSet::empty(mem_len);
        set.insert_range_at(0, GRANULE, 3);
        set.insert_range_at(700 * GRANULE, 2 * GRANULE, 900);
        set.insert_range_at(1000 * GRANULE, GRANULE, 0);
        let encoded = set.encode(&key, mem_len);
        assert_eq!(encoded.len(), HEADER_LEN + 16 * 8 + 4 * 2);
        let decoded = PageSet::decode(&encoded, &key, mem_len).expect("must decode");
        assert_eq!(decoded, set);
        assert_eq!(decoded.first_touch(701), 900);
        let mut longer = encoded.clone();
        longer.push(0);
        assert!(PageSet::decode(&longer, &key, mem_len).is_none());
        assert!(PageSet::decode(&encoded[..encoded.len() - 1], &key, mem_len).is_none());
    }

    #[test]
    fn ordered_runs_come_earliest_tier_first() {
        let mem_len = 256 * GRANULE;
        let mut set = PageSet::empty(mem_len);
        // High offsets touched first, low offsets late or at an unknown time.
        set.insert_range_at(0, 4 * GRANULE, 0);
        set.insert_range_at(10 * GRANULE, 2 * GRANULE, 600);
        set.insert_range_at(200 * GRANULE, 3 * GRANULE, 2);
        set.insert_range_at(203 * GRANULE, GRANULE, 15);
        let runs: Vec<(u64, u64)> = OrderedRuns::new(&set)
            .map(|r| (r.offset / GRANULE, r.len / GRANULE))
            .collect();
        assert_eq!(
            runs,
            vec![(200, 3), (203, 1), (10, 2), (0, 4)],
            "runs must come first-touch tier by tier, and a tier boundary splits a run"
        );
        assert_eq!(
            runs.iter().map(|(_, n)| n).sum::<u64>(),
            set.len(),
            "every granule once"
        );
    }

    #[test]
    fn decode_rejects_a_set_recorded_for_a_different_image() {
        let mem_len = 64 * GRANULE;
        let image = fake_image("key-a", mem_len);
        let other = fake_image("key-b", mem_len);
        // Same length and, on a coarse clock, possibly the same tick: the key tells images apart
        // by mtime, not by inode, so make the difference explicit.
        let mtime = std::fs::metadata(&image).unwrap().modified().unwrap();
        File::options()
            .write(true)
            .open(&other)
            .unwrap()
            .set_modified(mtime + std::time::Duration::from_nanos(1))
            .unwrap();
        let key = image_key(&image);
        let other_key = image_key(&other);
        assert_ne!(
            key.0, other_key.0,
            "two distinct images must not share a key"
        );

        let mut set = PageSet::empty(mem_len);
        set.insert_range(0, GRANULE);
        let encoded = set.encode(&key, mem_len);

        assert!(
            PageSet::decode(&encoded, &other_key, mem_len).is_none(),
            "a set from another image must be rejected"
        );
        assert!(
            PageSet::decode(&encoded, &key, mem_len * 2).is_none(),
            "a set from another image SIZE must be rejected"
        );
    }

    #[test]
    fn decode_rejects_corrupt_and_truncated_files() {
        let mem_len = 64 * GRANULE;
        let image = fake_image("corrupt", mem_len);
        let key = image_key(&image);
        let mut set = PageSet::empty(mem_len);
        set.insert_range(0, GRANULE);
        let good = set.encode(&key, mem_len);

        assert!(PageSet::decode(&[], &key, mem_len).is_none(), "empty file");
        assert!(
            PageSet::decode(&good[..HEADER_LEN - 1], &key, mem_len).is_none(),
            "short header"
        );
        assert!(
            PageSet::decode(&good[..good.len() - 1], &key, mem_len).is_none(),
            "truncated bitmap"
        );

        let mut bad_magic = good.clone();
        bad_magic[0] = b'X';
        assert!(
            PageSet::decode(&bad_magic, &key, mem_len).is_none(),
            "magic"
        );

        let mut bad_version = good.clone();
        bad_version[8] = 99;
        assert!(
            PageSet::decode(&bad_version, &key, mem_len).is_none(),
            "version"
        );
    }

    #[test]
    fn decode_masks_bits_past_the_end_of_the_image() {
        // 100 granules is not a multiple of 64, so the last word has 36 unused bits. A file
        // with those bits set (corruption, or a hand-edited file) must not produce runs that
        // point past the image.
        let mem_len = 100 * GRANULE;
        let image = fake_image("tail", mem_len);
        let key = image_key(&image);
        let mut encoded = PageSet::empty(mem_len).encode(&key, mem_len);
        let last_word = encoded.len() - 8;
        encoded[last_word..].copy_from_slice(&u64::MAX.to_le_bytes());
        // The 36 granules left after masking each carry an (unknown) first-touch time.
        encoded.extend(std::iter::repeat_n(0u8, (100 - 64) * 2));

        let decoded = PageSet::decode(&encoded, &key, mem_len).unwrap();
        let end = decoded.runs().map(|r| r.offset + r.len).max().unwrap_or(0);
        assert!(
            end <= mem_len,
            "runs must stay inside the image (got end {end} > {mem_len})"
        );
    }

    #[test]
    fn store_persists_a_union_that_only_gained_an_earlier_time() {
        let mem_len = 512 * GRANULE;
        let image = fake_image("earlier", mem_len);
        let store = open_store(&image, mem_len);
        let mut clone1 = store.recorder();
        clone1.insert_range_at(0, GRANULE, 300);
        assert!(store.merge_and_persist(&clone1).unwrap().persisted);
        let mut clone2 = store.recorder();
        clone2.insert_range_at(0, GRANULE, 4);
        let outcome = store.merge_and_persist(&clone2).unwrap();
        assert_eq!((outcome.total, outcome.added), (1, 0));
        assert!(
            outcome.persisted,
            "an earlier first-touch time with no new granule was not written"
        );
        let reopened = open_store(&image, mem_len);
        assert_eq!(reopened.to_prefetch().first_touch(0), 4);
        let mut clone3 = store.recorder();
        clone3.insert_range_at(0, GRANULE, 50);
        assert!(
            !store.merge_and_persist(&clone3).unwrap().persisted,
            "a later time changes nothing, so nothing is written"
        );
    }

    /// A clone takes the tier split the store made when it loaded the set, not one made on its
    /// fault path, and a change of the union is split again on another thread.
    #[test]
    fn the_store_keeps_its_tier_split_off_the_fault_path() {
        let mem_len = 512 * GRANULE;
        let image = fake_image("tiers", mem_len);
        let store = open_store(&image, mem_len);
        let mut clone1 = store.recorder();
        clone1.insert_range_at(0, GRANULE, 600);
        store.merge_and_persist(&clone1).unwrap();
        let reopened = open_store(&image, mem_len);
        let first = reopened.to_prefetch_ordered();
        assert!(
            Arc::ptr_eq(&first.tiers, &reopened.to_prefetch_ordered().tiers),
            "a clone split the set itself instead of taking the store's split"
        );
        let mut clone2 = reopened.recorder();
        clone2.insert_range_at(100 * GRANULE, GRANULE, 1);
        reopened.remember(&clone2);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let ordered = reopened.ordered.lock().unwrap();
            if ordered.split_from == ordered.version {
                break;
            }
            drop(ordered);
            assert!(
                std::time::Instant::now() < deadline,
                "the union was not split again"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let runs: Vec<u64> = OrderedRuns::from_tiers(reopened.to_prefetch_ordered().tiers)
            .map(|run| run.offset / GRANULE)
            .collect();
        assert_eq!(
            runs,
            vec![100, 0],
            "the new split holds the change, earliest first"
        );
    }

    #[test]
    fn store_persists_the_union_and_reloads_it() {
        let mem_len = 512 * GRANULE;
        let image = fake_image("store", mem_len);

        let store = open_store(&image, mem_len);
        assert!(
            store.to_prefetch().is_empty(),
            "a snapshot with no recording prefetches nothing"
        );

        // Clone 1 faults 3 granules.
        let mut clone1 = store.recorder();
        clone1.insert_range(0, 2 * GRANULE);
        clone1.insert_range(10 * GRANULE, GRANULE);
        let outcome = store.merge_and_persist(&clone1).unwrap();
        assert_eq!(
            outcome,
            MergeOutcome {
                total: 3,
                added: 3,
                persisted: true,
                superseded: false
            }
        );

        // Clone 2 faults an overlapping set: only the new granule is added, and because the
        // union grew it is written again.
        let mut clone2 = store.recorder();
        clone2.insert_range(0, GRANULE);
        clone2.insert_range(11 * GRANULE, GRANULE);
        let outcome = store.merge_and_persist(&clone2).unwrap();
        assert_eq!(
            outcome,
            MergeOutcome {
                total: 4,
                added: 1,
                persisted: true,
                superseded: false
            }
        );

        // Clone 3 adds nothing: the steady state writes nothing at all.
        let outcome = store.merge_and_persist(&clone2).unwrap();
        assert_eq!(
            outcome,
            MergeOutcome {
                total: 4,
                added: 0,
                persisted: false,
                superseded: false
            }
        );

        // A fresh server for the same image sees the union.
        let reopened = open_store(&image, mem_len);
        let loaded = reopened.to_prefetch();
        assert_eq!(loaded.len(), 4);
        assert_eq!(
            loaded.runs().collect::<Vec<_>>(),
            vec![
                Run {
                    offset: 0,
                    len: 2 * GRANULE
                },
                Run {
                    offset: 10 * GRANULE,
                    len: 2 * GRANULE
                },
            ]
        );
    }

    #[test]
    fn store_drops_a_set_recorded_before_the_snapshot_was_rewritten() {
        let mem_len = 64 * GRANULE;
        let image = fake_image("rewrite", mem_len);

        let store = open_store(&image, mem_len);
        let mut observed = store.recorder();
        observed.insert_range(0, 4 * GRANULE);
        store.merge_and_persist(&observed).unwrap();
        assert_eq!(open_store(&image, mem_len).to_prefetch().len(), 4);

        // Rewriting the snapshot replaces the memory image: new mtime, new config, new key.
        std::fs::remove_file(&image).unwrap();
        let f = File::create(&image).unwrap();
        f.set_len(mem_len).unwrap();
        drop(f);
        std::fs::write(generation_config_for(&image), b"generation-2").unwrap();

        let after = open_store(&image, mem_len);
        assert!(
            after.to_prefetch().is_empty(),
            "a working set from the previous snapshot must not be replayed"
        );
    }

    /// Copy a snapshot directory the way an extracted tar archive lands: every file is a new
    /// inode with the same bytes, length and nanosecond mtime.
    fn copy_dir_like_tar(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let dest = to.join(entry.file_name());
            std::fs::copy(entry.path(), &dest).unwrap();
            let mtime = std::fs::metadata(entry.path()).unwrap().modified().unwrap();
            File::options()
                .write(true)
                .open(&dest)
                .unwrap()
                .set_modified(mtime)
                .unwrap();
        }
    }

    #[test]
    fn store_survives_a_copy_that_keeps_content_length_and_mtime() {
        let mem_len = 64 * GRANULE;
        let image = fake_image("tar-src", mem_len);
        let store = open_store(&image, mem_len);
        let mut observed = store.recorder();
        observed.insert_range(0, 4 * GRANULE);
        assert!(store.merge_and_persist(&observed).unwrap().persisted);

        let dest = tempfile::Builder::new()
            .prefix("fcvm-ws-test-tar-dst-")
            .tempdir()
            .unwrap();
        copy_dir_like_tar(image.parent().unwrap(), dest.path());
        let copied = dest.path().join("memory.bin");
        let (a, b) = (
            std::fs::metadata(&image).unwrap(),
            std::fs::metadata(&copied).unwrap(),
        );
        assert_ne!(a.ino(), b.ino(), "the copy must be a new inode");
        assert_eq!(a.mtime_nsec(), b.mtime_nsec());

        assert_eq!(
            open_store(&copied, mem_len).to_prefetch().len(),
            4,
            "a tar-restored snapshot must keep its recorded working set"
        );
    }

    #[test]
    fn store_drops_a_set_when_the_image_length_or_mtime_changed() {
        let mem_len = 64 * GRANULE;
        let image = fake_image("rewrite-in-place", mem_len);
        let store = open_store(&image, mem_len);
        let mut observed = store.recorder();
        observed.insert_range(0, 4 * GRANULE);
        assert!(store.merge_and_persist(&observed).unwrap().persisted);
        assert_eq!(open_store(&image, mem_len).to_prefetch().len(), 4);

        // Same config, same path: only the image's own stat differs.
        let original = std::fs::metadata(&image).unwrap().modified().unwrap();
        let f = File::options().write(true).open(&image).unwrap();
        f.set_modified(original + std::time::Duration::from_nanos(1))
            .unwrap();
        assert!(
            open_store(&image, mem_len).to_prefetch().is_empty(),
            "a changed mtime means a rewritten image"
        );
        f.set_modified(original).unwrap();
        assert_eq!(open_store(&image, mem_len).to_prefetch().len(), 4);

        let longer = mem_len + GRANULE;
        f.set_len(longer).unwrap();
        f.set_modified(original).unwrap();
        assert!(
            open_store(&image, longer).to_prefetch().is_empty(),
            "a changed length means a rewritten image"
        );
    }

    #[test]
    fn store_binds_to_the_exact_snapshot_config_generation() {
        let mem_len = 64 * GRANULE;
        let image = fake_image("config-generation", mem_len);
        let store = open_store(&image, mem_len);
        let mut observed = store.recorder();
        observed.insert_range(0, 2 * GRANULE);
        assert!(store.merge_and_persist(&observed).unwrap().persisted);

        // Leave memory.bin completely untouched. A different config.json alone represents a
        // different atomically installed generation and must invalidate the old hint.
        std::fs::write(generation_config_for(&image), b"generation-2").unwrap();
        assert!(
            open_store(&image, mem_len).to_prefetch().is_empty(),
            "a new config generation must not replay the prior generation's bitmap"
        );
        let outcome = store.merge_and_persist(&observed).unwrap();
        assert!(outcome.superseded && !outcome.persisted);
    }

    /// A server keeps serving the inode it opened, but its working-set PATH resolves through
    /// the snapshot tag. If the tag was recreated before its merge acquired the generation
    /// lease, the finishing old clone must NOT publish over the new generation's record.
    #[test]
    fn an_old_generation_clone_does_not_clobber_the_new_generations_record() {
        let mem_len = 64 * GRANULE;
        let image = fake_image("supersede", mem_len);

        // Generation 1: the serve process that is still running.
        let old = open_store(&image, mem_len);

        // The tag is recreated in place: same path, new image.
        std::fs::remove_file(&image).unwrap();
        let f = File::create(&image).unwrap();
        f.set_len(mem_len).unwrap();
        drop(f);
        std::fs::write(generation_config_for(&image), b"generation-2").unwrap();

        // Generation 2 records its own working set at the same path.
        let new = open_store(&image, mem_len);
        assert_ne!(
            old.key, new.key,
            "test is vacuous unless rewriting the image changed its identity"
        );
        let mut new_observed = new.recorder();
        new_observed.insert_range(10 * GRANULE, 3 * GRANULE);
        assert!(new.merge_and_persist(&new_observed).unwrap().persisted);

        // Now generation 1's clone finishes and tries to record a DIFFERENT set.
        let mut old_observed = old.recorder();
        old_observed.insert_range(0, 5 * GRANULE);
        let outcome = old.merge_and_persist(&old_observed).unwrap();
        assert!(
            outcome.superseded,
            "publishing must be recognised as superseded once the image was replaced"
        );
        assert!(
            !outcome.persisted,
            "an old-generation set must never be published over the new one"
        );

        // The new generation's record must be intact and still usable.
        let reloaded = open_store(&image, mem_len).to_prefetch();
        assert_eq!(reloaded.len(), 3, "the new generation's set must survive");
        assert!(reloaded.contains(10) && reloaded.contains(12));
        assert!(
            !reloaded.contains(0),
            "the old generation's pages must not have leaked in"
        );
    }

    /// A creator must not be able to replace the snapshot after the old image is checked but
    /// before its working set is atomically published. Hold the sidecar lock to park a real
    /// merge after it acquires the generation lease, then prove a separate open file
    /// description cannot acquire the creator's exclusive lease until that merge completes.
    #[test]
    fn working_set_publication_holds_the_snapshot_generation_lease() {
        let mem_len = 64 * GRANULE;
        let image = fake_image("generation-lease", mem_len);
        let store = open_store(&image, mem_len);
        let sidecar_lock_path = store.lock_path.clone();
        let generation_lock_path = store.generation_lock_path.clone();

        let sidecar_file = File::options()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&sidecar_lock_path)
            .unwrap();
        let sidecar_guard = Flock::lock(sidecar_file, FlockArg::LockExclusive).unwrap();

        let mut observed = store.recorder();
        observed.insert_range(0, GRANULE);
        let merge = std::thread::spawn(move || store.merge_and_persist(&observed));

        let generation_probe = File::options()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&generation_lock_path)
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            match fs2::FileExt::try_lock_exclusive(&generation_probe) {
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Ok(()) => {
                    fs2::FileExt::unlock(&generation_probe).unwrap();
                    assert!(
                        std::time::Instant::now() < deadline,
                        "merge never acquired its shared snapshot-generation lease"
                    );
                    std::thread::yield_now();
                }
                Err(error) => panic!("probing snapshot-generation lease: {error}"),
            }
        }

        sidecar_guard.unlock().unwrap();
        let outcome = merge.join().unwrap().unwrap();
        assert!(
            outcome.persisted,
            "the parked merge must publish after release"
        );
        fs2::FileExt::try_lock_exclusive(&generation_probe)
            .expect("creator's exclusive lease must become available after publication");
        fs2::FileExt::unlock(&generation_probe).unwrap();
    }

    /// Direct callers can still wait synchronously on another process's `flock`, while clone
    /// startup calls `to_prefetch()` on an async worker. A blocked transaction therefore must
    /// not hold the in-memory set mutex that clone startup needs. Production finalization uses
    /// the detached non-blocking worker tested below.
    #[test]
    fn persistence_waiting_on_file_lock_does_not_block_prefetch_snapshot() {
        let mem_len = 64 * GRANULE;
        let image = fake_image("nonblocking-prefetch-snapshot", mem_len);
        let store = std::sync::Arc::new(open_store(&image, mem_len));

        // Park the real persistence path on the sidecar flock.
        let sidecar_file = File::options()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&store.lock_path)
            .unwrap();
        let sidecar_guard = Flock::lock(sidecar_file, FlockArg::LockExclusive).unwrap();

        let mut observed = store.recorder();
        observed.insert_range(0, GRANULE);
        let merge_store = std::sync::Arc::clone(&store);
        let merge = std::thread::spawn(move || merge_store.merge_and_persist(&observed));

        // Seeing the merge's shared generation lease proves it reached the blocking sidecar
        // lock. This is an exact interleaving, not a sleep or a scheduler guess.
        let generation_probe = File::options()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&store.generation_lock_path)
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            match fs2::FileExt::try_lock_exclusive(&generation_probe) {
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Ok(()) => {
                    fs2::FileExt::unlock(&generation_probe).unwrap();
                    assert!(
                        std::time::Instant::now() < deadline,
                        "merge never reached the sidecar-lock boundary"
                    );
                    std::thread::yield_now();
                }
                Err(error) => panic!("probing snapshot-generation lease: {error}"),
            }
        }

        let prefetch_snapshot_is_available = store.known.try_lock().is_ok();
        sidecar_guard.unlock().unwrap();
        let outcome = merge.join().unwrap().unwrap();
        assert!(outcome.persisted, "the released merge must publish");
        assert!(
            prefetch_snapshot_is_available,
            "a merger blocked on filesystem I/O held the in-memory set mutex and would block \
             clone startup on a Tokio worker"
        );
    }

    #[test]
    fn background_persistence_skips_sidecar_contention_without_losing_the_union() {
        let mem_len = 64 * GRANULE;
        let image = fake_image("fail-soft-sidecar-contention", mem_len);
        let store = open_store(&image, mem_len);

        let sidecar_file = File::options()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&store.lock_path)
            .unwrap();
        let sidecar_guard = Flock::lock(sidecar_file, FlockArg::LockExclusive).unwrap();

        let mut observed = store.recorder();
        observed.insert_range(3 * GRANULE, 2 * GRANULE);
        assert_eq!(store.remember(&observed), 2);
        assert!(
            store.try_persist_known().unwrap().is_none(),
            "the hint path must skip a busy sidecar instead of waiting"
        );
        let generation_probe = File::options()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&store.generation_lock_path)
            .unwrap();
        fs2::FileExt::try_lock_exclusive(&generation_probe)
            .expect("skipping a busy sidecar must release the snapshot-generation lease");
        fs2::FileExt::unlock(&generation_probe).unwrap();
        assert_eq!(
            store.to_prefetch().len(),
            2,
            "skipping disk persistence must retain the union for this server's next clone"
        );

        sidecar_guard.unlock().unwrap();
        let outcome = store
            .try_persist_known()
            .unwrap()
            .expect("the retry must acquire the released sidecar")
            .0;
        assert!(outcome.persisted);
        assert_eq!(open_store(&image, mem_len).to_prefetch().len(), 2);
    }

    /// Issue #858's poisoned-sidecar tell: a hint covering most of the guest cannot be a
    /// restore working set. The thresholds pin the measured populations: honest sets were
    /// 6.5 and 14 GiB of a 128 GiB guest, poisoned ones 84 and 122 GiB.
    #[test]
    fn a_hint_covering_most_of_the_guest_is_flagged_as_suspect() {
        const GIB: u64 = 1024 * 1024 * 1024;
        let guest = 128 * GIB;

        // Honest restore working sets must not be flagged.
        assert!(!hint_covers_most_of_the_image(13 * GIB / 2, guest));
        assert!(!hint_covers_most_of_the_image(14 * GIB, guest));
        // Saturation boundary: doubling u64::MAX / 2 + 1 saturates to u64::MAX,
        // which is not strictly greater than a u64::MAX-byte image, so the
        // multiply form called a more-than-half hint safe.
        assert!(hint_covers_most_of_the_image(u64::MAX / 2 + 1, u64::MAX));

        // The two poisoned sidecars from the issue must be.
        assert!(hint_covers_most_of_the_image(84 * GIB, guest));
        assert!(hint_covers_most_of_the_image(122 * GIB, guest));

        // Exactly half is the boundary: not suspect at half, suspect one granule past it.
        assert!(!hint_covers_most_of_the_image(64 * GIB, guest));
        assert!(hint_covers_most_of_the_image(64 * GIB + GRANULE, guest));

        // An empty hint on an empty image is not suspect.
        assert!(!hint_covers_most_of_the_image(0, 0));
    }

    /// The suspect-hint warning must order the repair as stop, delete, restart. Deleting
    /// the sidecar while the serve is still running does not repair that server: the
    /// suspect set stays in `WorkingSetStore::known`, later clones still replay it, and
    /// the persistence worker can rewrite the sidecar, racing the deletion.
    #[test]
    fn suspect_hint_warning_orders_stop_before_delete_before_restart() {
        #[derive(Clone, Default)]
        struct Captured(Arc<Mutex<Vec<u8>>>);

        impl Write for Captured {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
            type Writer = Captured;

            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        // Persist a hint covering more than half the image (33 of 64 granules).
        let mem_len = 64 * GRANULE;
        let image = fake_image("suspect-warning", mem_len);
        {
            let store = open_store(&image, mem_len);
            let mut set = store.recorder();
            set.insert_range(0, 33 * GRANULE);
            store.merge_and_persist(&set).unwrap();
        }

        let captured = Captured::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(captured.clone())
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let _ = open_store(&image, mem_len);
        });

        let log = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
        assert!(
            log.contains("covers more than half the guest"),
            "the suspect warning must fire for this hint; captured: {log}"
        );
        let stop = log
            .find("stop the serve")
            .unwrap_or_else(|| panic!("guidance must start with stopping the serve: {log}"));
        let delete = log
            .find("delete the sidecar")
            .unwrap_or_else(|| panic!("guidance must include deleting the sidecar: {log}"));
        let restart = log
            .find("restart the serve")
            .unwrap_or_else(|| panic!("guidance must end with restarting the serve: {log}"));
        assert!(
            stop < delete && delete < restart,
            "repair steps must read stop, then delete, then restart: {log}"
        );
    }

    #[test]
    fn store_survives_a_corrupt_recorded_file() {
        let mem_len = 64 * GRANULE;
        let image = fake_image("garbage", mem_len);
        std::fs::write(WorkingSetStore::path_for(&image), b"not a working set").unwrap();

        let store = open_store(&image, mem_len);
        assert!(store.to_prefetch().is_empty());

        // ...and recording over it repairs the file.
        let mut observed = store.recorder();
        observed.insert_range(0, GRANULE);
        assert!(store.merge_and_persist(&observed).unwrap().persisted);
        assert_eq!(open_store(&image, mem_len).to_prefetch().len(), 1);
    }

    #[test]
    fn store_rejects_a_record_larger_than_the_exact_bitmap_shape() {
        let mem_len = 64 * GRANULE;
        let image = fake_image("oversized", mem_len);
        let store = open_store(&image, mem_len);
        let mut recorded = store.recorder();
        recorded.insert_range(0, GRANULE);
        let mut encoded = recorded.encode(&store.key, mem_len);
        encoded.push(0);
        std::fs::write(WorkingSetStore::path_for(&image), encoded).unwrap();

        assert!(
            open_store(&image, mem_len).to_prefetch().is_empty(),
            "trailing bytes past the exact bitmap shape must invalidate the hint"
        );
    }

    // -------------------------------------------------------------------------
    // The working-set pack: when it is built, and when it is not used.
    // -------------------------------------------------------------------------

    /// A memory image with distinct bytes in every granule, so a pack of the wrong bytes
    /// cannot pass for the right ones.
    fn patterned_image(tag: &str, len: u64) -> FakeImage {
        let image = fake_image(tag, len);
        let bytes: Vec<u8> = (0..len)
            .map(|i| (i / GRANULE) as u8 ^ (i % 13) as u8)
            .collect();
        std::fs::write(&image.path, bytes).unwrap();
        image
    }

    /// Bind `store` to the image now at `image`, as a server binds the file it opened.
    fn bind(store: &WorkingSetStore, image: &Path) {
        assert!(store.bind_served_image(&File::open(image).unwrap().metadata().unwrap()));
    }

    fn reclaim_dir() -> (TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let dir = crate::reclaim::owner_dir_in(root.path()).unwrap();
        (root, dir)
    }

    /// Temporaries left beside the image.
    fn temporaries(image: &Path) -> Vec<String> {
        std::fs::read_dir(image.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect()
    }

    /// Replace the snapshot the way `snapshot create --tag` over an existing tag does: a new
    /// memory file and a new config, of the same length.
    fn replace_snapshot(image: &Path, len: u64) {
        let tmp = image.with_extension("new");
        std::fs::write(&tmp, vec![0x5Au8; len as usize]).unwrap();
        std::fs::rename(&tmp, image).unwrap();
        std::fs::write(generation_config_for(image), b"generation-2").unwrap();
    }

    fn recorded_set(store: &WorkingSetStore) -> PageSet {
        let mut observed = store.recorder();
        observed.insert_range_at(0, 600 * GRANULE, 3);
        observed.insert_range_at(1000 * GRANULE, 300 * GRANULE, 40);
        observed
    }

    /// Publishing a clone's working set is followed, in the background, by a pack of exactly
    /// the published set, which a later serve of the snapshot opens.
    #[test]
    fn a_pack_is_built_after_the_working_set_is_published() {
        let mem_len = 2048 * GRANULE;
        let image = patterned_image("pack-after-publish", mem_len);
        let store = Arc::new(open_store(&image, mem_len));
        let (_root, dir) = reclaim_dir();
        bind(&store, &image);
        let persistence = WorkingSetPersistence::with_pack_builds(
            Arc::clone(&store),
            PackBuilds::On {
                reclaim_dir: Some(dir),
            },
        )
        .unwrap();
        persistence.schedule("vm-pack", recorded_set(&store));

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while store.pack_for().is_none() {
            assert!(
                std::time::Instant::now() < deadline,
                "no pack of the published set was built within 30 s"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let reopened = open_store(&image, mem_len);
        bind(&reopened, &image);
        assert!(
            reopened.load_pack().is_some(),
            "a serve of the snapshot does not open the pack beside the sidecar"
        );
        assert_eq!(temporaries(&image), Vec::<String>::new());
    }

    /// A pack holds one image's bytes. Neither a store of the image that replaced it nor a
    /// server that mapped the old image while its store read the new one replays from it.
    #[test]
    fn a_pack_keyed_to_a_replaced_image_is_not_used() {
        let mem_len = 2048 * GRANULE;
        let image = patterned_image("pack-replaced", mem_len);
        let (_root, dir) = reclaim_dir();
        let store = open_store(&image, mem_len);
        let observed = recorded_set(&store);
        store.merge_and_persist(&observed).unwrap();
        assert!(matches!(
            store.build_pack(&observed, &dir, 2, &|| false).unwrap(),
            PackBuild::Installed(_)
        ));
        let control = open_store(&image, mem_len);
        bind(&control, &image);
        assert!(control.load_pack().is_some(), "control");
        let old_image = File::open(&image).unwrap().metadata().unwrap();

        // The snapshot is replaced and the new generation gets the same set: the old pack's
        // run table is the new set's, but its bytes are the old image's.
        replace_snapshot(&image, mem_len);
        WorkingSetStore::seed_held_generation(
            &image,
            mem_len,
            &generation_config_for(&image),
            &observed,
        )
        .unwrap();
        let replaced = open_store(&image, mem_len);
        bind(&replaced, &image);
        assert_eq!(replaced.to_prefetch(), observed);
        assert!(
            replaced.load_pack().is_none(),
            "a store of the new image opened the old image's pack"
        );

        // The new image gets its own pack. A server that opened the old image before its
        // store read the new snapshot must not replay it.
        assert!(matches!(
            replaced.build_pack(&observed, &dir, 2, &|| false).unwrap(),
            PackBuild::Installed(_)
        ));
        let served_old = open_store(&image, mem_len);
        assert!(!served_old.bind_served_image(&old_image));
        assert!(
            served_old.load_pack().is_none(),
            "a server of the old image replays the new image's pack"
        );
        let served_new = open_store(&image, mem_len);
        assert!(served_new.bind_served_image(&File::open(&image).unwrap().metadata().unwrap()));
        assert!(
            served_new.load_pack().is_some(),
            "control: the new image's own server"
        );
    }

    /// Packs fail closed: a store that no server bound to the image it maps loads none, so a
    /// caller that forgets the binding replays from the memory file, never another image's
    /// bytes.
    #[test]
    fn a_store_nobody_bound_replays_no_pack() {
        let mem_len = 2048 * GRANULE;
        let image = patterned_image("pack-unbound", mem_len);
        let (_root, dir) = reclaim_dir();
        let store = open_store(&image, mem_len);
        let observed = recorded_set(&store);
        store.merge_and_persist(&observed).unwrap();
        assert!(matches!(
            store.build_pack(&observed, &dir, 2, &|| false).unwrap(),
            PackBuild::Installed(_)
        ));
        let unbound = open_store(&image, mem_len);
        assert!(
            unbound.load_pack().is_none(),
            "a store nobody bound loaded a pack"
        );
        bind(&unbound, &image);
        assert!(unbound.load_pack().is_some(), "control: once bound it does");
    }

    /// A build reads the memory file without any lock, for minutes on a large set. The image
    /// can be replaced meanwhile; the install, which takes the generation lock that snapshot
    /// create holds exclusively, waits for the creator and then finds the image replaced, and
    /// installs nothing.
    #[test]
    fn pack_installation_cannot_race_image_replacement() {
        let mem_len = 2048 * GRANULE;
        let image = patterned_image("pack-race", mem_len);
        let (_root, dir) = reclaim_dir();
        let store = Arc::new(open_store(&image, mem_len));
        let observed = recorded_set(&store);
        store.merge_and_persist(&observed).unwrap();
        let generation_lock_path = store.generation_lock_path.clone();

        // The build signals when it reads its first frame, and waits there for the creator.
        let (reading_tx, reading_rx) = std::sync::mpsc::sync_channel(1);
        let (go_tx, go_rx) = std::sync::mpsc::sync_channel::<()>(1);
        let hook = Mutex::new(Some((reading_tx, go_rx)));
        let builder_store = Arc::clone(&store);
        let builder = std::thread::spawn(move || {
            let cancel = || {
                if let Some((reading, go)) = hook.lock().unwrap().take() {
                    reading.send(()).unwrap();
                    go.recv().unwrap();
                }
                false
            };
            builder_store.build_pack(&observed, &dir, 1, &cancel)
        });
        reading_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the build never started reading");

        // The creator: exclusive generation lock, replace the snapshot, hold on.
        let creator_file = File::options()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&generation_lock_path)
            .unwrap();
        let creator = Flock::lock(creator_file, FlockArg::LockExclusive).unwrap();
        replace_snapshot(&image, mem_len);
        go_tx.send(()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(
            !builder.is_finished(),
            "the install did not wait for the creator's generation lock"
        );
        assert!(!pack::path_for(&image).exists());
        creator.unlock().unwrap();

        let outcome = builder.join().unwrap().unwrap();
        assert_eq!(outcome, PackBuild::Superseded);
        assert!(
            !pack::path_for(&image).exists(),
            "a pack of the old image was installed beside the new one"
        );
        assert_eq!(temporaries(&image), Vec::<String>::new());
    }

    /// A pack is installed only beside a set that holds all its runs: a sidecar rewritten
    /// with another set while the pack was built gets no pack of the old one.
    #[test]
    fn a_pack_is_not_installed_beside_another_set() {
        let mem_len = 2048 * GRANULE;
        let image = patterned_image("pack-set-changed", mem_len);
        let (_root, dir) = reclaim_dir();
        let store = open_store(&image, mem_len);
        let observed = recorded_set(&store);
        store.merge_and_persist(&observed).unwrap();
        let other = open_store(&image, mem_len);
        let published = std::sync::atomic::AtomicBool::new(false);
        let cancel = || {
            if !published.swap(true, std::sync::atomic::Ordering::Relaxed) {
                let mut another = other.recorder();
                another.insert_range_at(1800 * GRANULE, 10 * GRANULE, 1);
                WorkingSetStore::seed_held_generation(
                    &image,
                    mem_len,
                    &generation_config_for(&image),
                    &another,
                )
                .unwrap();
            }
            false
        };
        assert_eq!(
            store.build_pack(&observed, &dir, 1, &cancel).unwrap(),
            PackBuild::SetChanged
        );
        assert!(!pack::path_for(&image).exists());
        assert_eq!(temporaries(&image), Vec::<String>::new());
    }

    /// The set the pack on disk beside `image` was built from, by its order digest.
    fn pack_built_from(image: &Path) -> Option<[u8; 32]> {
        let bytes = std::fs::read(pack::path_for(image)).ok()?;
        bytes.get(56..88)?.try_into().ok()
    }

    /// A set that grew since its pack was built still replays from that pack: the pack holds
    /// the bytes of the runs it was built from, and replay reads the memory file for the rest.
    #[test]
    fn a_pack_of_a_smaller_set_is_used_after_the_set_grows() {
        let mem_len = 2048 * GRANULE;
        let image = patterned_image("pack-grown", mem_len);
        let (_root, dir) = reclaim_dir();
        let store = open_store(&image, mem_len);
        let observed = recorded_set(&store);
        store.merge_and_persist(&observed).unwrap();
        assert!(matches!(
            store.build_pack(&observed, &dir, 2, &|| false).unwrap(),
            PackBuild::Installed(_)
        ));
        let mut more = store.recorder();
        more.insert_range_at(1800 * GRANULE, 10 * GRANULE, 1);
        assert!(store.merge_and_persist(&more).unwrap().persisted);

        let grown = open_store(&image, mem_len);
        bind(&grown, &image);
        assert_ne!(grown.to_prefetch(), observed, "the set grew");
        assert!(
            grown.load_pack().is_some(),
            "the pack of the smaller set is not used once the set grew"
        );
    }

    /// A pack is installed beside a sidecar that grew while it was built: every run it holds
    /// is still a recorded run.
    #[test]
    fn a_pack_is_installed_beside_a_set_that_grew_while_it_was_built() {
        let mem_len = 2048 * GRANULE;
        let image = patterned_image("pack-set-grew", mem_len);
        let (_root, dir) = reclaim_dir();
        let store = open_store(&image, mem_len);
        let observed = recorded_set(&store);
        store.merge_and_persist(&observed).unwrap();
        let other = open_store(&image, mem_len);
        let published = std::sync::atomic::AtomicBool::new(false);
        let cancel = || {
            if !published.swap(true, std::sync::atomic::Ordering::Relaxed) {
                let mut more = other.recorder();
                more.insert_range_at(1800 * GRANULE, 10 * GRANULE, 1);
                assert!(other.merge_and_persist(&more).unwrap().persisted);
            }
            false
        };
        assert!(matches!(
            store.build_pack(&observed, &dir, 1, &cancel).unwrap(),
            PackBuild::Installed(_)
        ));
        assert_eq!(pack_built_from(&image), Some(observed.order_digest()));
    }

    /// A serve of a snapshot that has a recorded set and no pack builds one on its first
    /// publication, although that publication changes nothing: this is how an inherited set,
    /// or one recorded before packs existed, gets its pack.
    #[test]
    fn a_serve_of_a_set_with_no_pack_builds_one() {
        let mem_len = 2048 * GRANULE;
        let image = patterned_image("pack-first-serve", mem_len);
        let recorded = {
            let store = open_store(&image, mem_len);
            let observed = recorded_set(&store);
            store.merge_and_persist(&observed).unwrap();
            observed
        };
        assert!(!pack::path_for(&image).exists());
        let store = Arc::new(open_store(&image, mem_len));
        let (_root, dir) = reclaim_dir();
        let persistence = WorkingSetPersistence::with_pack_builds(
            Arc::clone(&store),
            PackBuilds::On {
                reclaim_dir: Some(dir),
            },
        )
        .unwrap();
        // A clone that faulted only pages already recorded: the sidecar does not change.
        let mut clone = store.recorder();
        clone.insert_range_at(0, GRANULE, 3);
        persistence.schedule("vm-first", clone);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while pack_built_from(&image) != Some(recorded.order_digest()) {
            assert!(
                std::time::Instant::now() < deadline,
                "the first serve of a set with no pack built none within 30 s"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// A set that grows while its pack is built does not cancel that build: the build
    /// finishes and installs, and the grown set is built after it.
    #[test]
    fn a_grown_set_is_built_after_the_running_build_and_does_not_cancel_it() {
        let mem_len = 2048 * GRANULE;
        let image = patterned_image("pack-coalesce", mem_len);
        let store = Arc::new(open_store(&image, mem_len));
        let (_root, dir) = reclaim_dir();
        // The first build pauses at its first frame, the second at its first frame too.
        let (paused_tx, paused_rx) = std::sync::mpsc::channel::<()>();
        let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
        let go_rx = Mutex::new(go_rx);
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let hook: FrameHook = Arc::new(move || {
            if calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 2 {
                paused_tx.send(()).ok();
                go_rx.lock().unwrap().recv().ok();
            }
        });
        let persistence = WorkingSetPersistence::with_pack_builds(
            Arc::clone(&store),
            PackBuilds::Hooked {
                reclaim_dir: dir,
                frame_hook: hook,
            },
        )
        .unwrap();
        let first = recorded_set(&store);
        persistence.schedule("vm-1", first.clone());
        paused_rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("the first build never started");

        let mut more = store.recorder();
        more.insert_range_at(1800 * GRANULE, 10 * GRANULE, 1);
        persistence.schedule("vm-2", more);
        let grown = store.to_prefetch();
        assert_ne!(grown, first);
        // Let the sidecar of the grown set be published while the first build waits.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while open_store(&image, mem_len).to_prefetch() != grown {
            assert!(
                std::time::Instant::now() < deadline,
                "the grown set was not published"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        go_tx.send(()).unwrap();

        paused_rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("the grown set was never built");
        assert_eq!(
            pack_built_from(&image),
            Some(first.order_digest()),
            "the first build was not installed before the grown set's build began"
        );
        go_tx.send(()).unwrap();
        while pack_built_from(&image) != Some(grown.order_digest()) {
            assert!(
                std::time::Instant::now() < deadline,
                "the grown set's pack was not installed"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}

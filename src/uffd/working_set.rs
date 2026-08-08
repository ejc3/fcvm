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
//! that harm: SHA-256 over the memory file's `(len, mtime, ino, dev)`, which is one `stat`.
//! Hashing the image itself was measured at 1.5 GB/s on this host — 1.4 s for a 2 GiB snapshot,
//! paid on every `snapshot serve` startup — which is a far bigger cost than the mis-prefetch it
//! would prevent. Rewriting a snapshot (`snapshot create --tag` over an existing tag) always
//! writes a new file: new inode, new mtime, new key, and the stale set is dropped.

use anyhow::{Context, Result};
use nix::fcntl::{Flock, FlockArg};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tracing::{debug, info, warn};

/// Bitmap granularity, in bytes.
///
/// Fixed at 4 KiB rather than the clone's page size so ONE recorded set serves every clone of
/// a snapshot regardless of how its guest memory is backed: 4 KiB, ARM64 16 KiB, and 2 MiB
/// hugepage granules are all whole multiples of this, so a fault on any of them marks a whole
/// number of bits and a set recorded by a 4 KiB clone is directly usable by a 2 MiB one.
pub const GRANULE: u64 = 4096;

/// `magic || version || granule || mem_len || granules || image_key`.
const HEADER_LEN: usize = 8 + 8 + 8 + 8 + 8 + 32;
const MAGIC: &[u8; 8] = b"FCVMWSET";
const VERSION: u64 = 1;

/// Refuse to even allocate a bitmap for an implausible memory file (1 TiB of 4 KiB granules
/// is a 32 MiB bitmap). Guards against a corrupt header turning into a huge allocation.
const MAX_MEM_LEN: u64 = 1 << 40;

/// Identity of the snapshot memory image a working set was recorded against.
///
/// See the module docs for why this is a `stat` tuple rather than a digest of the image.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ImageKey([u8; 32]);

impl ImageKey {
    /// Derive the key of the memory image at `path`.
    pub fn of(path: &Path) -> Result<Self> {
        let meta = std::fs::metadata(path)
            .with_context(|| format!("stat-ing memory image {}", path.display()))?;
        let mut hasher = Sha256::new();
        hasher.update(meta.len().to_le_bytes());
        hasher.update(meta.mtime().to_le_bytes());
        hasher.update(meta.mtime_nsec().to_le_bytes());
        hasher.update(meta.ino().to_le_bytes());
        hasher.update(meta.dev().to_le_bytes());
        Ok(Self(hasher.finalize().into()))
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

/// A set of snapshot pages, as a bitmap over [`GRANULE`]-sized granules of the memory file.
///
/// A bitmap rather than a list of offsets because it dedupes for free (the same page faults
/// more than once across a clone's life), costs a fixed 32 KiB per GiB of guest memory, and
/// makes merging two clones' observations a word-wise OR.
#[derive(Clone, PartialEq, Eq)]
pub struct PageSet {
    granules: u64,
    words: Vec<u64>,
}

impl std::fmt::Debug for PageSet {
    /// Summary, not the bitmap: a 1 GiB image is 4096 words, which no assertion message
    /// wants to print.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "PageSet {{ {}/{} granules, {} runs }}",
            self.len(),
            self.granules,
            self.runs().count()
        )
    }
}

impl PageSet {
    /// An empty set covering a memory image of `mem_len` bytes.
    pub fn empty(mem_len: u64) -> Self {
        let granules = mem_len.div_ceil(GRANULE);
        Self {
            granules,
            words: vec![0; (granules as usize).div_ceil(64)],
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
        let first = offset / GRANULE;
        let last = (offset + len.max(1) - 1) / GRANULE;
        for granule in first..=last.min(self.granules.saturating_sub(1)) {
            if granule >= self.granules {
                break;
            }
            self.words[(granule / 64) as usize] |= 1u64 << (granule % 64);
        }
    }

    /// Membership of a single granule. Serving only ever walks whole [`Run`]s, so this
    /// exists for the tests that pin down `insert_range`'s exact granule arithmetic.
    #[cfg(test)]
    fn contains(&self, granule: u64) -> bool {
        granule < self.granules
            && self.words[(granule / 64) as usize] & (1u64 << (granule % 64)) != 0
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
            let after = before | other.words[i];
            added += (after & !before).count_ones() as u64;
            self.words[i] = after;
        }
        added
    }

    /// Coalesce the set into as few contiguous runs as possible.
    ///
    /// This is what turns 56k scattered fault offsets into a few hundred bulk copies: the
    /// arrival ORDER of faults is scattered, but the SET is dense in runs.
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
        Some(set)
    }
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

/// What a merge did, for logging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MergeOutcome {
    /// Granules in the union after merging.
    pub total: u64,
    /// Granules this clone contributed that nothing had recorded before.
    pub added: u64,
    /// Whether the union was written back to disk (only when it grew).
    pub persisted: bool,
}

/// The working set stored beside a snapshot's memory image.
///
/// One store per served snapshot, shared by every clone the server hosts. It holds the union
/// of everything known about the image — what was on disk at startup, plus what each clone has
/// contributed since — so a clone that starts later in the same server benefits from what an
/// earlier clone already discovered without a round trip through the filesystem.
pub struct WorkingSetStore {
    path: PathBuf,
    lock_path: PathBuf,
    key: ImageKey,
    mem_len: u64,
    known: Mutex<PageSet>,
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
    /// empty store, which prefetches nothing and re-records from scratch.
    pub fn open(mem_file_path: &Path, mem_len: u64) -> Result<Self> {
        anyhow::ensure!(
            mem_len <= MAX_MEM_LEN,
            "memory image {} is {mem_len} bytes, past the {MAX_MEM_LEN}-byte working-set limit",
            mem_file_path.display()
        );
        let key = ImageKey::of(mem_file_path)?;
        let path = Self::path_for(mem_file_path);
        let mut lock_name = path.file_name().unwrap_or_default().to_os_string();
        lock_name.push(".lock");
        let lock_path = path.with_file_name(lock_name);

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
                set
            }
            None => PageSet::empty(mem_len),
        };

        Ok(Self {
            path,
            lock_path,
            key,
            mem_len,
            known: Mutex::new(known),
        })
    }

    /// The set to prefetch for a clone starting now.
    pub fn to_prefetch(&self) -> PageSet {
        self.known.lock().expect("working set mutex").clone()
    }

    /// An empty set sized for this image, for a clone to record into.
    pub fn recorder(&self) -> PageSet {
        PageSet::empty(self.mem_len)
    }

    /// Merge one clone's observations into the union and persist it if it grew.
    ///
    /// Two clones of a snapshot fault nearly the same pages, so in the steady state this adds
    /// nothing and writes nothing. It matters in exactly two cases: the first clone of a new
    /// snapshot (which records everything), and a clone that was killed before it finished
    /// faulting in (whose truncated record is completed by the next clone rather than being
    /// baked in forever).
    ///
    /// Race-free against other serve processes on the same snapshot: the whole
    /// read-modify-write runs under an exclusive `flock` on a sidecar lock file, and the
    /// result is published by atomic rename. Losing this to a crash is harmless — the file is
    /// a cache, and the next clone re-records what is missing.
    pub fn merge_and_persist(&self, observed: &PageSet) -> Result<MergeOutcome> {
        let mut known = self.known.lock().expect("working set mutex");

        let lock_file = File::options()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&self.lock_path)
            .with_context(|| format!("opening working-set lock {}", self.lock_path.display()))?;
        let lock = Flock::lock(lock_file, FlockArg::LockExclusive)
            .map_err(|(_, err)| err)
            .context("locking working set")?;

        // Re-read under the lock: another serve process may have recorded since we loaded.
        let on_disk = read_set(&self.path, &self.key, self.mem_len);
        let disk_len = on_disk.as_ref().map_or(0, PageSet::len);
        if let Some(disk) = on_disk {
            known.union_from(&disk);
        }
        let added = known.union_from(observed);
        let total = known.len();

        let persisted = total > disk_len;
        let result = if persisted {
            write_set(&self.path, &known, &self.key, self.mem_len)
        } else {
            Ok(())
        };

        // Release explicitly so the lock outlives the rename above, and so a failure to
        // unlock is reported rather than swallowed by a drop.
        lock.unlock().map_err(|(_, err)| err).ok();

        result?;
        Ok(MergeOutcome {
            total,
            added,
            persisted,
        })
    }
}

/// Read and validate a recorded set. `None` means "nothing usable here" — always a normal
/// outcome, never an error.
fn read_set(path: &Path, key: &ImageKey, mem_len: u64) -> Option<PageSet> {
    let mut buf = Vec::new();
    match File::open(path).and_then(|mut f| f.read_to_end(&mut buf)) {
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
    PageSet::decode(&buf, key, mem_len)
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

    fn tmp_path(tag: &str) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        std::env::temp_dir().join(format!(
            "fcvm-ws-test-{}-{}-{}",
            tag,
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ))
    }

    /// A fake memory image of `len` bytes, so `ImageKey::of` has something to stat.
    fn fake_image(tag: &str, len: u64) -> PathBuf {
        let path = tmp_path(tag);
        let f = File::create(&path).unwrap();
        f.set_len(len).unwrap();
        path
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
        let key = ImageKey::of(&image).unwrap();
        let mem_len = 1024 * GRANULE;

        let mut set = PageSet::empty(mem_len);
        set.insert_range(0, GRANULE);
        set.insert_range(1000 * GRANULE, 5 * GRANULE);

        let encoded = set.encode(&key, mem_len);
        let decoded = PageSet::decode(&encoded, &key, mem_len).expect("must decode");
        assert_eq!(decoded, set);
        assert_eq!(decoded.len(), 6);

        std::fs::remove_file(&image).unwrap();
    }

    #[test]
    fn decode_rejects_a_set_recorded_for_a_different_image() {
        let mem_len = 64 * GRANULE;
        let image = fake_image("key-a", mem_len);
        let other = fake_image("key-b", mem_len);
        let key = ImageKey::of(&image).unwrap();
        let other_key = ImageKey::of(&other).unwrap();
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

        std::fs::remove_file(&image).unwrap();
        std::fs::remove_file(&other).unwrap();
    }

    #[test]
    fn decode_rejects_corrupt_and_truncated_files() {
        let mem_len = 64 * GRANULE;
        let image = fake_image("corrupt", mem_len);
        let key = ImageKey::of(&image).unwrap();
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

        std::fs::remove_file(&image).unwrap();
    }

    #[test]
    fn decode_masks_bits_past_the_end_of_the_image() {
        // 100 granules is not a multiple of 64, so the last word has 36 unused bits. A file
        // with those bits set (corruption, or a hand-edited file) must not produce runs that
        // point past the image.
        let mem_len = 100 * GRANULE;
        let image = fake_image("tail", mem_len);
        let key = ImageKey::of(&image).unwrap();
        let mut encoded = PageSet::empty(mem_len).encode(&key, mem_len);
        let last_word = encoded.len() - 8;
        encoded[last_word..].copy_from_slice(&u64::MAX.to_le_bytes());

        let decoded = PageSet::decode(&encoded, &key, mem_len).unwrap();
        let end = decoded.runs().map(|r| r.offset + r.len).max().unwrap_or(0);
        assert!(
            end <= mem_len,
            "runs must stay inside the image (got end {end} > {mem_len})"
        );

        std::fs::remove_file(&image).unwrap();
    }

    #[test]
    fn store_persists_the_union_and_reloads_it() {
        let mem_len = 512 * GRANULE;
        let image = fake_image("store", mem_len);

        let store = WorkingSetStore::open(&image, mem_len).unwrap();
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
                persisted: true
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
                persisted: true
            }
        );

        // Clone 3 adds nothing: the steady state writes nothing at all.
        let outcome = store.merge_and_persist(&clone2).unwrap();
        assert_eq!(
            outcome,
            MergeOutcome {
                total: 4,
                added: 0,
                persisted: false
            }
        );

        // A fresh server for the same image sees the union.
        let reopened = WorkingSetStore::open(&image, mem_len).unwrap();
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

        std::fs::remove_file(WorkingSetStore::path_for(&image)).unwrap();
        let _ = std::fs::remove_file(store.lock_path);
        std::fs::remove_file(&image).unwrap();
    }

    #[test]
    fn store_drops_a_set_recorded_before_the_snapshot_was_rewritten() {
        let mem_len = 64 * GRANULE;
        let image = fake_image("rewrite", mem_len);

        let store = WorkingSetStore::open(&image, mem_len).unwrap();
        let mut observed = store.recorder();
        observed.insert_range(0, 4 * GRANULE);
        store.merge_and_persist(&observed).unwrap();
        assert_eq!(
            WorkingSetStore::open(&image, mem_len)
                .unwrap()
                .to_prefetch()
                .len(),
            4
        );

        // Rewriting the snapshot replaces the memory image: new inode, new mtime, new key.
        std::fs::remove_file(&image).unwrap();
        let f = File::create(&image).unwrap();
        f.set_len(mem_len).unwrap();
        drop(f);

        let after = WorkingSetStore::open(&image, mem_len).unwrap();
        assert!(
            after.to_prefetch().is_empty(),
            "a working set from the previous snapshot must not be replayed"
        );

        std::fs::remove_file(WorkingSetStore::path_for(&image)).unwrap();
        let _ = std::fs::remove_file(store.lock_path);
        std::fs::remove_file(&image).unwrap();
    }

    #[test]
    fn store_survives_a_corrupt_recorded_file() {
        let mem_len = 64 * GRANULE;
        let image = fake_image("garbage", mem_len);
        std::fs::write(WorkingSetStore::path_for(&image), b"not a working set").unwrap();

        let store = WorkingSetStore::open(&image, mem_len).unwrap();
        assert!(store.to_prefetch().is_empty());

        // ...and recording over it repairs the file.
        let mut observed = store.recorder();
        observed.insert_range(0, GRANULE);
        assert!(store.merge_and_persist(&observed).unwrap().persisted);
        assert_eq!(
            WorkingSetStore::open(&image, mem_len)
                .unwrap()
                .to_prefetch()
                .len(),
            1
        );

        std::fs::remove_file(WorkingSetStore::path_for(&image)).unwrap();
        let _ = std::fs::remove_file(store.lock_path);
        std::fs::remove_file(&image).unwrap();
    }
}

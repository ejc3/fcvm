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
//! `(len, mtime, ino, dev)`, which is one small read and one `stat`.
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
/// See the module docs for why this combines the exact config digest with a `stat` tuple
/// instead of hashing the multi-gigabyte memory image.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ImageKey([u8; 32]);

impl ImageKey {
    /// Derive the key of the memory image at `path`.
    pub fn of(path: &Path, config_digest: &[u8; 32]) -> Result<Self> {
        let meta = std::fs::metadata(path)
            .with_context(|| format!("stat-ing memory image {}", path.display()))?;
        let mut hasher = Sha256::new();
        hasher.update(config_digest);
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
        let last = offset.saturating_add(len.max(1) - 1) / GRANULE;
        for granule in first..=last.min(self.granules.saturating_sub(1)) {
            if granule >= self.granules {
                break;
            }
            self.words[(granule / 64) as usize] |= 1u64 << (granule % 64);
        }
    }

    /// Membership of a single granule. Serving only ever walks whole [`Run`]s, so this exists
    /// for the tests that pin down `insert_range`'s granule arithmetic.
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
    /// The image at the memory path is no longer the one we served, so nothing was published.
    /// Happens when a snapshot tag is recreated while this serve process is still running: the
    /// server keeps its open (old) inode, but the working-set path now belongs to the NEW
    /// image, and publishing our bitmap there would clobber the new generation's record.
    pub superseded: bool,
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
    /// Race-free against both other servers and snapshot replacement: the whole
    /// read-modify-write runs under an exclusive `flock` on a sidecar lock file, while a
    /// shared snapshot-generation lease spans the final image-identity check and atomic
    /// rename. Creators/deleters take that generation lease exclusively, so they cannot land
    /// in the check/publish gap. Losing the cache to a crash is harmless — the next clone
    /// re-records what is missing.
    ///
    /// The lock is taken blocking, on the clone's own teardown path: the critical section is
    /// one read and one write of a bitmap (32 KiB per GiB of guest RAM), and it is only ever
    /// contended by another serve process of the SAME snapshot finishing a clone at the same
    /// moment.
    pub fn merge_and_persist(&self, observed: &PageSet) -> Result<MergeOutcome> {
        let mut known = self.known.lock().expect("working set mutex");

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

        // Publish ONLY if the path still names the image we served. A snapshot tag may have
        // been recreated before this merge acquired its shared generation lease: we keep
        // serving the old inode we already opened, but `self.path` would now sit beside a NEW
        // image. The lease prevents any further replacement through the rename below; this
        // identity check rejects a replacement that already completed.
        let superseded = !self.image_is_still_ours();
        let persisted = total > disk_len && !superseded;
        let result = if persisted {
            write_set(&self.path, &known, &self.key, self.mem_len)
        } else {
            Ok(())
        };

        // Release explicitly, so both locks demonstrably span the whole identity-check and
        // publication transaction above rather than ending wherever the compiler last uses
        // them. Failed unlocks need no special handling: closing each fd releases its lock.
        lock.unlock().map_err(|(_, err)| err).ok();
        generation_lock.unlock().map_err(|(_, err)| err).ok();

        result?;
        Ok(MergeOutcome {
            total,
            added,
            persisted,
            superseded,
        })
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
}

/// Read and validate a recorded set. `None` means "nothing usable here" — always a normal
/// outcome, never an error.
fn read_set(path: &Path, key: &ImageKey, mem_len: u64) -> Option<PageSet> {
    let granules = mem_len.div_ceil(GRANULE);
    let bitmap_bytes = granules.div_ceil(64).saturating_mul(8);
    let max_len = (HEADER_LEN as u64).saturating_add(bitmap_bytes);
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

        std::fs::remove_file(&image).unwrap();
    }

    #[test]
    fn decode_rejects_a_set_recorded_for_a_different_image() {
        let mem_len = 64 * GRANULE;
        let image = fake_image("key-a", mem_len);
        let other = fake_image("key-b", mem_len);
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

        std::fs::remove_file(&image).unwrap();
        std::fs::remove_file(&other).unwrap();
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

        std::fs::remove_file(&image).unwrap();
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

        std::fs::remove_file(WorkingSetStore::path_for(&image)).unwrap();
        let _ = std::fs::remove_file(&store.lock_path);
        let _ = std::fs::remove_file(&store.generation_lock_path);
        let _ = std::fs::remove_file(generation_config_for(&image));
        std::fs::remove_file(&image).unwrap();
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

        // Rewriting the snapshot replaces the memory image: new inode, new mtime, new key.
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

        std::fs::remove_file(WorkingSetStore::path_for(&image)).unwrap();
        let _ = std::fs::remove_file(&store.lock_path);
        let _ = std::fs::remove_file(&store.generation_lock_path);
        let _ = std::fs::remove_file(generation_config_for(&image));
        std::fs::remove_file(&image).unwrap();
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

        std::fs::remove_file(WorkingSetStore::path_for(&image)).unwrap();
        let _ = std::fs::remove_file(&store.lock_path);
        let _ = std::fs::remove_file(&store.generation_lock_path);
        let _ = std::fs::remove_file(generation_config_for(&image));
        std::fs::remove_file(&image).unwrap();
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

        std::fs::remove_file(WorkingSetStore::path_for(&image)).unwrap();
        let _ = std::fs::remove_file(&old.lock_path);
        let _ = std::fs::remove_file(&old.generation_lock_path);
        let _ = std::fs::remove_file(generation_config_for(&image));
        std::fs::remove_file(&image).unwrap();
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
        let working_set_path = WorkingSetStore::path_for(&image);
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

        std::fs::remove_file(working_set_path).unwrap();
        std::fs::remove_file(sidecar_lock_path).unwrap();
        std::fs::remove_file(generation_lock_path).unwrap();
        std::fs::remove_file(generation_config_for(&image)).unwrap();
        std::fs::remove_file(image).unwrap();
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

        std::fs::remove_file(WorkingSetStore::path_for(&image)).unwrap();
        let _ = std::fs::remove_file(&store.lock_path);
        let _ = std::fs::remove_file(&store.generation_lock_path);
        let _ = std::fs::remove_file(generation_config_for(&image));
        std::fs::remove_file(&image).unwrap();
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

        std::fs::remove_file(WorkingSetStore::path_for(&image)).unwrap();
        let _ = std::fs::remove_file(&store.lock_path);
        let _ = std::fs::remove_file(&store.generation_lock_path);
        let _ = std::fs::remove_file(generation_config_for(&image));
        std::fs::remove_file(&image).unwrap();
    }
}

//! Rewriting a snapshot's memory file once the snapshot it shared extents with is deleted
//! (#1810).
//!
//! A diff snapshot's memory file shares its parent's extents: it is a subvolume snapshot or a
//! clone of the parent's file, and the diff is written over it. A shared block is written
//! copy-on-write, so the diff's pages land in new extents, and the parent's extents stay
//! referenced by the blocks the diff did not replace. btrfs frees an extent only when none of
//! its blocks is referenced, so once the parent is deleted the diff snapshot keeps the whole
//! of every parent extent it still references, the replaced blocks included. A nodatacow
//! memory file's extents are up to 128 MiB: one diff snapshot of a 128 GiB guest took 189 GiB
//! on disk for a file that references 126 GiB.
//!
//! `snapshots delete` and `snapshots prune` therefore look, after the delete, at the
//! snapshots linked to the deleted one by `parent_snapshot` ([`Compaction`]). One sequential
//! copy of such a survivor's file into a new file, renamed over the old one, gives it extents
//! of its own and frees every block of the old ones. It is done only where it cannot cost
//! space and pays for its write ([`Holding::refusal`]): the file must share no extent with any
//! other file (another snapshot staged from it, a sibling with the same parent), because a
//! copy of shared data takes its size again, and it must keep at least one byte in eight more
//! than it references. The copy keeps the file's bytes, holes, length, mode, owner, nodatacow
//! flag and modification time, so the working set recorded for the snapshot (keyed on the
//! config digest, the length and the nanosecond mtime) stays valid. A process that has the
//! old file open or mapped keeps reading the old inode, whose bytes are the same; its blocks
//! are freed when the last such process closes it.

use anyhow::{Context, Result};
use std::fs::File;
use std::ops::ControlFlow;
use std::os::unix::fs::{FileExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use tracing::{debug, info};

use crate::storage::snapshot::SNAPSHOT_MEMORY_FILE;

/// A survivor is rewritten when it keeps at least one byte in eight of what it references
/// on disk without referencing it: the rewrite writes every referenced byte once.
const REWRITE_MIN_HELD_FRACTION: u64 = 8;

/// The largest extent btrfs writes for uncompressed data (`BTRFS_MAX_EXTENT_SIZE`).
const MAX_EXTENT: u64 = 128 << 20;

/// How far apart in the file two pieces of one extent can be: a gap between them can hold at
/// most the ends of two extents, since an extent wholly inside it is freed.
const MAX_GAP: u64 = 2 * MAX_EXTENT;

/// What a memory file references and what it keeps on disk without referencing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Holding {
    /// Whether any of its extents is shared with another file.
    pub shared: bool,
    /// The bytes its extents reference.
    pub referenced: u64,
    /// An estimate of the bytes of its extents it does not reference ([`held_bytes`]).
    pub held: u64,
    /// Its extent count, as FIEMAP reports them.
    pub extents: u64,
}

/// The bytes a file whose extents FIEMAP reports as `extents` (in file order) keeps on disk
/// without referencing them, estimated from its own layout.
///
/// FIEMAP reports the part of an extent a file references, not the whole extent. A file
/// written in one stream (a Full snapshot, a copy) has each extent's pieces at one distance
/// between file offset and device address, so two pieces at the same distance with a gap
/// between them in the file are two pieces of one extent, and the gap's device bytes are the
/// part of it another write replaced, which the file keeps without referencing. Each gap is
/// counted up to [`MAX_GAP`]. A compressed extent's pieces all report the extent's start, so a
/// gap between two of them is counted at its length in the file, more than the compressed
/// bytes it costs. This is an estimate: pieces of two extents that happen to sit at the same
/// distance count too, and nothing here sees a part of an extent no piece of the file follows.
pub fn held_bytes(extents: &[crate::storage::extents::Extent]) -> u64 {
    let mut estimate = HeldEstimate::default();
    for extent in extents {
        estimate.push(extent);
    }
    estimate.held
}

/// [`held_bytes`] fed one extent at a time, in file order, so a file of millions of extents
/// is not held in memory: only the pieces within [`MAX_GAP`] of the current one are kept.
#[derive(Default)]
struct HeldEstimate {
    /// The end in the file and on the device of the last piece seen at each distance.
    recent: std::collections::HashMap<i128, (u64, u64)>,
    /// The pieces in `recent`, oldest first, to forget them once nothing can follow them.
    order: std::collections::VecDeque<(u64, i128)>,
    held: u64,
}

impl HeldEstimate {
    fn push(&mut self, extent: &crate::storage::extents::Extent) {
        while let Some(&(end, key)) = self.order.front() {
            if end.saturating_add(MAX_GAP) >= extent.start {
                break;
            }
            self.order.pop_front();
            if self
                .recent
                .get(&key)
                .is_some_and(|&(last_end, _)| last_end == end)
            {
                self.recent.remove(&key);
            }
        }
        // Compressed pieces are keyed by the extent they come from, the others by distance.
        let key = if extent.encoded {
            (1i128 << 100) + extent.physical as i128
        } else {
            extent.physical as i128 - extent.start as i128
        };
        if let Some(&(last_end, last_physical_end)) = self.recent.get(&key) {
            if extent.start > last_end {
                let gap = if extent.encoded {
                    extent.start - last_end
                } else {
                    extent.physical.saturating_sub(last_physical_end)
                };
                self.held += gap.min(MAX_GAP);
            }
        }
        let end = extent.start + extent.len;
        self.recent.insert(key, (end, extent.physical + extent.len));
        self.order.push_back((end, key));
    }
}

impl Holding {
    /// Read `file`'s extents.
    pub fn of(file: &File) -> std::io::Result<Self> {
        let mut holding = Holding {
            shared: false,
            referenced: 0,
            held: 0,
            extents: 0,
        };
        let mut estimate = HeldEstimate::default();
        crate::storage::extents::for_each_extent(file, |extent| {
            holding.shared |= extent.shared;
            holding.referenced += extent.len;
            holding.extents += 1;
            estimate.push(&extent);
            ControlFlow::Continue(())
        })?;
        holding.held = estimate.held;
        Ok(holding)
    }

    /// Why a rewrite would not pay, or `None` when it would: one that shares an extent would
    /// copy what it shares, and one that holds little would write far more than it frees.
    pub fn refusal(&self) -> Option<String> {
        if self.shared {
            Some(
                "it shares extents with another file, so a copy would take their space again"
                    .into(),
            )
        } else if self.held == 0 {
            Some("it keeps no unreferenced part of its extents".into())
        } else if self.held.saturating_mul(REWRITE_MIN_HELD_FRACTION) < self.referenced {
            Some(format!(
                "it keeps about {} bytes it does not reference, less than one in {REWRITE_MIN_HELD_FRACTION} of the {} it references",
                self.held, self.referenced
            ))
        } else {
            None
        }
    }
}

/// The snapshots one delete or prune may leave keeping parts of a deleted snapshot's extents.
/// Fill it with [`Compaction::before_removal`] for each snapshot deleted, then
/// [`Compaction::rewrite_survivors`] once they are all gone.
#[derive(Debug, Default)]
pub struct Compaction {
    candidates: std::collections::BTreeSet<String>,
}

impl Compaction {
    /// Call with the generation lock of snapshot `name` in the store `store` held
    /// exclusively, right before its directory is removed. When its memory file shares
    /// extents and the store holds snapshots of its lineage (linked by `parent_snapshot`,
    /// either way), records the lineage's survivors and unlinks the memory
    /// file: a subvolume delete leaves the file's references to the btrfs cleaner thread, and
    /// until they are dropped every survivor reads as sharing them. Unlinking drops one
    /// reference per extent; that costs time only for a file of millions of extents, which
    /// only a snapshot whose lineage survives it reaches here with.
    pub async fn before_removal(&mut self, store: &Path, name: &str) {
        let dir = store.join(name);
        // A link to another snapshot's directory is removed as the link alone.
        if !std::fs::symlink_metadata(&dir).is_ok_and(|meta| meta.file_type().is_dir()) {
            return;
        }
        let memory = dir.join(SNAPSHOT_MEMORY_FILE);
        match std::fs::symlink_metadata(&memory) {
            Ok(meta) if meta.file_type().is_file() => {}
            _ => return,
        }
        let shared = {
            let memory = memory.clone();
            tokio::task::spawn_blocking(move || {
                File::open(&memory)
                    .and_then(|file| crate::storage::extents::reports_shared_extents(&file))
            })
            .await
        };
        match shared {
            Ok(Ok(true)) => {}
            Ok(Ok(false)) => return,
            Ok(Err(e)) => {
                debug!(snapshot = name, error = %e, "cannot read the memory file's extents");
                return;
            }
            Err(e) => {
                debug!(snapshot = name, error = %e, "the extent read panicked");
                return;
            }
        }
        let lineage = read_lineage(store).await;
        let mut family = family_of(name, &lineage);
        family.remove(name);
        family.retain(|member| lineage.iter().any(|(survivor, _)| survivor == member));
        if family.is_empty() {
            return;
        }
        if let Err(e) = tokio::fs::remove_file(&memory).await {
            debug!(snapshot = name, error = %e, "could not unlink the memory file before the snapshot's removal");
        }
        self.candidates.extend(family);
    }

    /// Rewrite the memory file of every recorded survivor that still exists, shares nothing
    /// and keeps enough it does not reference ([`rewrite_memory_file`]). Logs each outcome; never fails.
    pub async fn rewrite_survivors(self, store: &Path) {
        if self.candidates.is_empty() {
            return;
        }
        let lineage = read_lineage(store).await;
        for name in self.candidates {
            if !lineage.iter().any(|(survivor, _)| *survivor == name) {
                continue;
            }
            let outcome = rewrite_memory_file(&store.join(&name)).await;
            log_rewrite(&name, &outcome);
        }
    }
}

/// Every snapshot of `store` with the parent its config records: directories whose
/// `config.json` names them (a create's `.creating` and `.old` directories do not). Only
/// `name` and `parent_snapshot` are read, so a config another fcvm wrote still counts.
async fn read_lineage(store: &Path) -> Vec<(String, Option<String>)> {
    let store = store.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let Ok(entries) = std::fs::read_dir(&store) else {
            return Vec::new();
        };
        let mut lineage = Vec::new();
        for entry in entries.flatten() {
            let Some(dir_name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let Ok(bytes) = std::fs::read(entry.path().join("config.json")) else {
                continue;
            };
            let Ok(config) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                continue;
            };
            if config.get("name").and_then(|n| n.as_str()) != Some(dir_name.as_str()) {
                continue;
            }
            let parent = config
                .get("parent_snapshot")
                .and_then(|p| p.as_str())
                .map(str::to_string);
            lineage.push((dir_name, parent));
        }
        lineage
    })
    .await
    .unwrap_or_default()
}

/// The names linked to `start` through recorded parents, either way and through names no
/// snapshot holds any more, `start` included.
fn family_of(
    start: &str,
    lineage: &[(String, Option<String>)],
) -> std::collections::BTreeSet<String> {
    let mut family = std::collections::BTreeSet::from([start.to_string()]);
    let mut queue = vec![start.to_string()];
    while let Some(name) = queue.pop() {
        for (child, parent) in lineage {
            let linked = match parent {
                Some(parent) if *parent == name => Some(child),
                Some(parent) if *child == name => Some(parent),
                _ => None,
            };
            if let Some(linked) = linked {
                if family.insert(linked.clone()) {
                    queue.push(linked.clone());
                }
            }
        }
    }
    family
}

/// What [`rewrite_memory_file`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rewrite {
    /// The memory file was replaced by a copy of itself.
    Rewrote {
        /// What the old file referenced and held.
        before: Holding,
        /// The data bytes copied.
        copied: u64,
    },
    /// The memory file was left as it is, for this reason.
    Kept(String),
}

/// The identity of a memory file the rewrite copied: a replacement or an edit found before the
/// rename keeps the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Identity {
    dev: u64,
    ino: u64,
    len: u64,
    mtime: i64,
    mtime_nsec: i64,
}

impl Identity {
    fn of(meta: &std::fs::Metadata) -> Self {
        Self {
            dev: meta.dev(),
            ino: meta.ino(),
            len: meta.len(),
            mtime: meta.mtime(),
            mtime_nsec: meta.mtime_nsec(),
        }
    }
}

/// A copy of a memory file next to it, ready to be renamed over it.
struct Copy {
    path: PathBuf,
    source: Identity,
    before: Holding,
    copied: u64,
}

/// Rewrite the memory file of the snapshot in `snapshot_dir` as a copy of itself when it keeps
/// enough of its extents it does not reference and shares no extent with another file ([`Holding::refusal`]).
///
/// The copy and the rename run under the snapshot's generation lock shared (so the snapshot is
/// not replaced or deleted under them) and its stage lock exclusively (so no snapshot create
/// stages from it meanwhile, which would also carry the copy into the new snapshot's
/// directory); the snapshot can still be restored and served. The rename runs only after the
/// file is found to be the one copied and to share nothing still. A process killed during the copy leaves the copy recorded with the
/// reclaim, which removes it; the rename is atomic and the copy is synced before it, so the
/// path names the whole old file or the whole new one.
pub async fn rewrite_memory_file(snapshot_dir: &Path) -> Result<Rewrite> {
    let memory = snapshot_dir.join(SNAPSHOT_MEMORY_FILE);
    match std::fs::symlink_metadata(&memory) {
        Ok(meta) if meta.file_type().is_file() => {}
        Ok(_) => {
            return Ok(Rewrite::Kept(
                "its memory file is not a regular file".into(),
            ))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Rewrite::Kept("it has no memory file".into()))
        }
        Err(e) => return Err(e).with_context(|| format!("reading {}", memory.display())),
    }
    // Both locks are held across the copy as well as the rename: a create that staged from the
    // snapshot, or replaced it, while the copy was written would carry the copy into its own
    // directory or into `<tag>.old`, where the reclaim record no longer names it. A create
    // that stages from this snapshot waits for the copy.
    let _generation = crate::commands::common::acquire_snapshot_dir_lock(snapshot_dir, false)
        .await
        .context("locking the snapshot's generation for the rewrite")?;
    let _stage = crate::commands::common::acquire_snapshot_stage_lock(snapshot_dir, true)
        .await
        .context("locking the snapshot against staging for the rewrite")?;
    // Absolute, so the reclaim finds it from any working directory.
    let snapshot_dir = std::path::absolute(snapshot_dir)
        .with_context(|| format!("resolving {}", snapshot_dir.display()))?;
    let memory = snapshot_dir.join(SNAPSHOT_MEMORY_FILE);
    let temp = snapshot_dir.join(format!(
        "{SNAPSHOT_MEMORY_FILE}.rewrite-{}",
        uuid::Uuid::new_v4().simple()
    ));
    let owner_id = temp
        .file_name()
        .and_then(|name| name.to_str())
        .context("a rewrite's temporary file has a name")?
        .to_string();
    let owner = crate::reclaim::Owner::begin(&owner_id)?;
    owner.record(crate::reclaim::Resource::Temp { path: temp.clone() })?;

    let copied = {
        let (memory, temp) = (memory.clone(), temp.clone());
        tokio::task::spawn_blocking(move || copy_if_worth_it(&memory, &temp))
            .await
            .context("the memory file copy task panicked")?
    };
    let copy = match copied {
        Ok(Ok(copy)) => copy,
        Ok(Err(reason)) => {
            discard_copy(&temp, owner);
            return Ok(Rewrite::Kept(reason));
        }
        Err(e) => {
            discard_copy(&temp, owner);
            return Err(e);
        }
    };

    let swapped = {
        let memory = memory.clone();
        let copy_path = copy.path.clone();
        let source = copy.source;
        tokio::task::spawn_blocking(move || swap_in(&memory, &copy_path, source))
            .await
            .context("the memory file rename task panicked")?
    };
    match swapped {
        Ok(Ok(())) => {
            owner.discard()?;
            Ok(Rewrite::Rewrote {
                before: copy.before,
                copied: copy.copied,
            })
        }
        Ok(Err(reason)) => {
            discard_copy(&temp, owner);
            Ok(Rewrite::Kept(reason))
        }
        Err(e) => {
            discard_copy(&temp, owner);
            Err(e)
        }
    }
}

/// Remove an unused copy and its reclaim record. A copy that cannot be removed stays recorded,
/// so the next launch's reclaim removes it.
fn discard_copy(temp: &Path, owner: crate::reclaim::Owner) {
    match std::fs::remove_file(temp) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            debug!(path = %temp.display(), error = %e, "could not remove an unused memory file copy");
            return;
        }
    }
    if let Err(e) = owner.discard() {
        debug!(path = %temp.display(), error = %format!("{e:#}"), "could not discard the copy's reclaim record");
    }
}

/// Decide whether `memory` is worth rewriting and, if it is, write the copy to `temp`.
/// `Ok(Err(reason))` when it is not, or when the store has no room for the copy.
fn copy_if_worth_it(memory: &Path, temp: &Path) -> Result<std::result::Result<Copy, String>> {
    let source = File::open(memory).with_context(|| format!("opening {}", memory.display()))?;
    let meta = source
        .metadata()
        .context("reading the memory file's metadata")?;
    let before = match Holding::of(&source) {
        Ok(holding) => holding,
        Err(e) => return Ok(Err(format!("its filesystem cannot report extents: {e}"))),
    };
    if let Some(reason) = before.refusal() {
        return Ok(Err(reason));
    }
    // SAFETY: geteuid has no preconditions.
    let euid = unsafe { libc::geteuid() };
    if euid != 0 && euid != meta.uid() {
        return Ok(Err(format!(
            "it belongs to uid {}, and this process runs as uid {euid}",
            meta.uid()
        )));
    }
    let len = meta.len();
    let data = crate::commands::common::data_run_bytes(&source)?;
    let available = available_bytes(memory.parent().unwrap_or(memory))?;
    let needed = data + data / 16;
    if available < needed {
        return Ok(Err(format!(
            "the copy needs {needed} bytes free and the store has {available}"
        )));
    }

    let copy = match create_copy_like(&source, &meta, temp, euid)? {
        Ok(copy) => copy,
        Err(reason) => return Ok(Err(reason)),
    };
    copy.set_len(len).context("sizing the copy")?;
    let copied = copy_data_runs(&source, &copy, len)?;
    copy.sync_all().context("syncing the copy")?;
    copy.set_times(
        std::fs::FileTimes::new()
            .set_accessed(meta.accessed().context("reading the access time")?)
            .set_modified(meta.modified().context("reading the modification time")?),
    )
    .context("giving the copy the memory file's times")?;
    copy.sync_all().context("syncing the copy's times")?;
    Ok(Ok(Copy {
        path: temp.to_path_buf(),
        source: Identity::of(&meta),
        before,
        copied,
    }))
}

/// Create the empty file `temp` with `source`'s nodatacow flag, mode, owner and group. A rootless
/// owner can give it any group it belongs to, root any owner; a copy that cannot keep both is
/// removed and refused (`Ok(Err(reason))`), so a rewrite never changes who may read the file.
fn create_copy_like(
    source: &File,
    meta: &std::fs::Metadata,
    temp: &Path,
    euid: u32,
) -> Result<std::result::Result<File, String>> {
    let copy = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temp)
        .with_context(|| format!("creating {}", temp.display()))?;
    // The flag can only change while the file is empty: match it before writing.
    crate::storage::inode_flags::match_nodatacow(source, &copy);
    let made = copy.metadata().context("reading the copy's metadata")?;
    if made.uid() != meta.uid() || made.gid() != meta.gid() {
        let uid = (euid == 0 && made.uid() != meta.uid()).then_some(meta.uid());
        let _ = std::os::unix::fs::fchown(&copy, uid, Some(meta.gid()));
        let given = copy.metadata().context("reading the copy's metadata")?;
        if given.uid() != meta.uid() || given.gid() != meta.gid() {
            drop(copy);
            let _ = std::fs::remove_file(temp);
            return Ok(Err(format!(
                "a copy cannot keep its owner and group ({}:{}); this process made it {}:{}",
                meta.uid(),
                meta.gid(),
                given.uid(),
                given.gid()
            )));
        }
    }
    // After any fchown, which clears the set-id bits.
    copy.set_permissions(std::fs::Permissions::from_mode(meta.mode() & 0o7777))
        .context("setting the copy's mode")?;
    Ok(Ok(copy))
}

/// Copy every data run of the first `len` bytes of `source` to the same offset of `dest`,
/// leaving its holes holes. The bytes copied.
fn copy_data_runs(source: &File, dest: &File, len: u64) -> Result<u64> {
    const CHUNK: usize = 8 << 20;
    let mut buffer = vec![0u8; CHUNK];
    let mut copied = 0u64;
    let mut failed: Option<anyhow::Error> = None;
    crate::uffd::for_each_data_run(source, len, |start, end| {
        let mut at = start;
        while failed.is_none() && at < end {
            let n = ((end - at) as usize).min(CHUNK);
            let piece = &mut buffer[..n];
            let result = source
                .read_exact_at(piece, at)
                .with_context(|| format!("reading the memory file at {at}"))
                .and_then(|()| {
                    dest.write_all_at(piece, at)
                        .with_context(|| format!("writing the copy at {at}"))
                });
            match result {
                Ok(()) => {
                    copied += n as u64;
                    at += n as u64;
                }
                Err(e) => failed = Some(e),
            }
        }
    })?;
    match failed {
        Some(e) => Err(e),
        None => Ok(copied),
    }
}

/// Free bytes an unprivileged writer can use on the filesystem holding `path`.
fn available_bytes(path: &Path) -> Result<u64> {
    let stat = nix::sys::statvfs::statvfs(path)
        .with_context(|| format!("reading the free space of {}", path.display()))?;
    Ok((stat.blocks_available() as u64).saturating_mul(stat.fragment_size() as u64))
}

/// Rename the copy over `memory` if `memory` is still the file copied and shares no extent;
/// `Ok(Err(reason))` when it is not. Runs with the snapshot's generation lock shared and its
/// stage lock exclusive.
fn swap_in(
    memory: &Path,
    copy: &Path,
    source: Identity,
) -> Result<std::result::Result<(), String>> {
    let file = match File::open(memory) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Err("its memory file was removed during the copy".into()))
        }
        Err(e) => return Err(e).with_context(|| format!("opening {}", memory.display())),
    };
    let meta = file
        .metadata()
        .context("reading the memory file's metadata")?;
    if Identity::of(&meta) != source {
        return Ok(Err("its memory file changed during the copy".into()));
    }
    let shared = crate::storage::extents::reports_shared_extents(&file)
        .context("reading the memory file's extents")?;
    if shared {
        return Ok(Err(
            "its memory file came to share extents during the copy (a snapshot was staged from it)"
                .into(),
        ));
    }
    std::fs::rename(copy, memory)
        .with_context(|| format!("renaming {} to {}", copy.display(), memory.display()))?;
    if let Some(dir) = memory.parent() {
        File::open(dir)
            .and_then(|dir| dir.sync_all())
            .with_context(|| format!("syncing {}", dir.display()))?;
    }
    Ok(Ok(()))
}

/// Log what [`rewrite_memory_file`] did for `snapshot`.
pub fn log_rewrite(snapshot: &str, outcome: &Result<Rewrite>) {
    match outcome {
        Ok(Rewrite::Rewrote { before, copied }) => info!(
            snapshot,
            held_bytes = before.held,
            referenced_bytes = before.referenced,
            extents_before = before.extents,
            copied_bytes = copied,
            "rewrote the snapshot's memory file to free the parts of its extents it kept without referencing them"
        ),
        Ok(Rewrite::Kept(reason)) => info!(
            snapshot,
            %reason,
            "kept the snapshot's memory file as it is after deleting a snapshot it shared extents with"
        ),
        Err(e) => tracing::warn!(
            snapshot,
            error = %format!("{e:#}"),
            "could not rewrite the snapshot's memory file; it keeps the parts of its extents it does not reference"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A lineage is followed both ways and through a deleted name.
    #[test]
    fn a_family_joins_parents_children_and_siblings() {
        let lineage = vec![
            ("b".to_string(), Some("a".to_string())),
            ("b2".to_string(), Some("a".to_string())),
            ("c".to_string(), Some("b".to_string())),
            ("other".to_string(), None),
            ("other-child".to_string(), Some("other".to_string())),
        ];
        let names = |set: std::collections::BTreeSet<String>| set.into_iter().collect::<Vec<_>>();
        // "a" was deleted: its children are still one family
        assert_eq!(names(family_of("b2", &lineage)), ["a", "b", "b2", "c"]);
        assert_eq!(names(family_of("c", &lineage)), ["a", "b", "b2", "c"]);
        assert_eq!(
            names(family_of("other", &lineage)),
            ["other", "other-child"]
        );
        assert_eq!(names(family_of("lone", &lineage)), ["lone"]);
    }

    fn extent(start: u64, physical: u64, len: u64) -> crate::storage::extents::Extent {
        crate::storage::extents::Extent {
            start,
            physical,
            len,
            shared: false,
            encoded: false,
        }
    }

    /// A file written in one stream and then rewritten at every other MiB keeps the
    /// replaced MiBs of the first stream's extent: the gaps between the pieces at one
    /// distance.
    #[test]
    fn gaps_between_pieces_of_one_extent_are_held() {
        const M: u64 = 1 << 20;
        // the first stream at device 1000 M, the rewrites packed at device 5000 M
        let mut extents = Vec::new();
        for i in 0..8u64 {
            if i % 2 == 0 {
                extents.push(extent(i * M, 5000 * M + (i / 2) * M, M));
            } else {
                extents.push(extent(i * M, 1000 * M + i * M, M));
            }
        }
        // pieces of the first stream at 1, 3, 5, 7: three gaps of 1 MiB
        assert_eq!(held_bytes(&extents), 3 * M);
        // a file written in one stream holds nothing
        let whole: Vec<_> = (0..8u64)
            .map(|i| extent(i * M, 1000 * M + i * M, M))
            .collect();
        assert_eq!(held_bytes(&whole), 0);
        // pieces too far apart in the file count at most MAX_GAP
        let far = [extent(0, 0, M), extent(1000 * M, 1000 * M, M)];
        assert_eq!(held_bytes(&far), 0, "a piece past MAX_GAP is forgotten");
        let near = [extent(0, 0, M), extent(MAX_GAP, MAX_GAP, M)];
        assert_eq!(held_bytes(&near), MAX_GAP - M);
        // compressed pieces of one extent report its start: their gap in the file counts
        let mut a = extent(0, 77 * M, 64 << 10);
        a.encoded = true;
        let mut b = extent(96 << 10, 77 * M, 32 << 10);
        b.encoded = true;
        assert_eq!(held_bytes(&[a, b]), 32 << 10);
    }

    #[test]
    fn a_rewrite_pays_only_for_an_unshared_file_that_holds_enough() {
        let holding = |shared, referenced, held| Holding {
            shared,
            referenced,
            held,
            extents: 1,
        };
        assert!(holding(false, 800, 100).refusal().is_none());
        assert!(holding(false, 800, 99).refusal().is_some());
        assert!(holding(true, 800, 800).refusal().is_some());
        assert!(holding(false, 800, 0).refusal().is_some());
    }

    /// The copy keeps the bytes, the holes, the length and the nanosecond modification time,
    /// so the working-set key of the snapshot is the same before and after.
    /// The reclaim record names the copy by an absolute path: a launch from another working
    /// directory would otherwise resolve a relative one elsewhere, find nothing, and drop the
    /// record while the copy stays (#1810 review). And the copy is written under both locks.
    #[test]
    fn the_rewrite_records_an_absolute_copy_path_and_copies_under_its_locks() {
        let source = include_str!("compact.rs");
        let body = &source[source.find("pub async fn rewrite_memory_file(").unwrap()..];
        let at = |needle: &str| {
            body.find(needle)
                .unwrap_or_else(|| panic!("rewrite_memory_file does not call {needle}"))
        };
        assert!(at("acquire_snapshot_stage_lock(") < at("std::path::absolute(snapshot_dir)"));
        assert!(at("std::path::absolute(snapshot_dir)") < at("Resource::Temp"));
        assert!(at("Resource::Temp") < at("copy_if_worth_it("));
    }

    /// A copy keeps its source's owner and group, or is refused and removed: a rewrite never
    /// changes who may read a memory file (#1810 review). Unprivileged, a root-owned source
    /// cannot be matched, so its copy is refused; a source in one of this process's
    /// supplementary groups keeps that group.
    ///
    /// RED BEFORE THE FIX: an unprivileged copy took the process's own group and was kept.
    #[test]
    fn a_copy_keeps_owner_and_group_or_is_refused() {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: geteuid has no preconditions.
        let euid = unsafe { libc::geteuid() };
        let dir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let root_owned = std::path::Path::new("/etc/hostname");
        let source = File::open(root_owned).unwrap();
        let meta = source.metadata().unwrap();
        let temp = dir.path().join("copy");
        match create_copy_like(&source, &meta, &temp, euid).unwrap() {
            Ok(copy) => {
                let made = copy.metadata().unwrap();
                assert_eq!((made.uid(), made.gid()), (meta.uid(), meta.gid()));
                assert_eq!(euid, 0, "an unprivileged process matched a root-owned file");
            }
            Err(_) => {
                assert_ne!(euid, 0, "root could not match a root-owned file");
                assert!(!temp.exists(), "the refused copy was left behind");
            }
        }

        let egid = unsafe { libc::getegid() };
        let other = nix::unistd::getgroups()
            .unwrap()
            .into_iter()
            .map(|gid| gid.as_raw())
            .find(|gid| *gid != egid);
        if let Some(gid) = other {
            let own = dir.path().join("own");
            std::fs::write(&own, b"x").unwrap();
            std::os::unix::fs::chown(&own, None, Some(gid)).unwrap();
            let source = File::open(&own).unwrap();
            let meta = source.metadata().unwrap();
            let temp = dir.path().join("own-copy");
            let copy = create_copy_like(&source, &meta, &temp, euid)
                .unwrap()
                .expect("a group this process belongs to is kept");
            assert_eq!(copy.metadata().unwrap().gid(), gid);
        }
    }

    #[test]
    fn a_copy_keeps_bytes_holes_length_and_mtime() {
        let dir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let memory = dir.path().join(SNAPSHOT_MEMORY_FILE);
        {
            let file = File::create(&memory).unwrap();
            file.set_len(64 << 20).unwrap();
            file.write_all_at(&[0x5a; 3 << 20], 1 << 20).unwrap();
            file.write_all_at(&[0xa5; 4096], 40 << 20).unwrap();
            file.sync_all().unwrap();
        }
        let config = dir.path().join("config.json");
        std::fs::write(&config, b"{}").unwrap();
        let key = crate::uffd::working_set::ImageKey::of_generation(&memory, &config).unwrap();
        let runs = |path: &Path| {
            let file = File::open(path).unwrap();
            let mut runs = Vec::new();
            crate::uffd::for_each_data_run(&file, 64 << 20, |s, e| runs.push(s..e)).unwrap();
            runs
        };
        let runs_before = runs(&memory);
        let bytes_before = std::fs::read(&memory).unwrap();
        let mtime_before = std::fs::metadata(&memory).unwrap().mtime_nsec();

        let temp = dir.path().join("copy");
        let source = File::open(&memory).unwrap();
        let copy = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .unwrap();
        copy.set_len(64 << 20).unwrap();
        let copied = copy_data_runs(&source, &copy, 64 << 20).unwrap();
        let meta = source.metadata().unwrap();
        copy.set_times(
            std::fs::FileTimes::new()
                .set_accessed(meta.accessed().unwrap())
                .set_modified(meta.modified().unwrap()),
        )
        .unwrap();
        drop(copy);
        assert_eq!(
            swap_in(&memory, &temp, Identity::of(&meta)).unwrap(),
            Ok(())
        );

        assert_eq!(
            copied,
            runs_before.iter().map(|r| r.end - r.start).sum::<u64>()
        );
        assert_eq!(std::fs::read(&memory).unwrap(), bytes_before);
        assert_eq!(runs(&memory), runs_before);
        assert_eq!(
            std::fs::metadata(&memory).unwrap().mtime_nsec(),
            mtime_before
        );
        assert!(
            crate::uffd::working_set::ImageKey::of_generation(&memory, &config).unwrap() == key,
            "the rewrite changed the snapshot's working-set key"
        );
    }

    /// A memory file replaced after the copy began is kept, and so is the copy's target left
    /// alone.
    #[test]
    fn a_file_replaced_during_the_copy_is_kept() {
        let dir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let memory = dir.path().join(SNAPSHOT_MEMORY_FILE);
        std::fs::write(&memory, b"old").unwrap();
        let identity = Identity::of(&std::fs::metadata(&memory).unwrap());
        std::fs::remove_file(&memory).unwrap();
        std::fs::write(&memory, b"new generation").unwrap();
        let temp = dir.path().join("copy");
        std::fs::write(&temp, b"old").unwrap();
        assert!(swap_in(&memory, &temp, identity).unwrap().is_err());
        assert_eq!(std::fs::read(&memory).unwrap(), b"new generation");
    }
}

//! Deleting a snapshot rewrites the memory file of the diff snapshot that kept parts of its
//! extents (#1810).
//!
//! On a private btrfs loopback: a nodatacow memory file `A` of 256 MiB, a subvolume snapshot
//! `B` of it with every other MiB rewritten (a diff written in place), then `A` deleted. Before
//! the fix `B` kept the replaced halves of `A`'s extents, so the filesystem held about 384 MiB
//! for a file of 256 MiB. The space is read with statvfs on a filesystem nothing else uses,
//! after `btrfs subvolume sync` and `btrfs filesystem sync`.

#![cfg(feature = "privileged-tests")]

use anyhow::{bail, Context, Result};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use fcvm::storage::compact::Compaction;
use fcvm::storage::snapshot::SnapshotManager;

const MIB: u64 = 1 << 20;
const MEMORY_BYTES: u64 = 256 * MIB;

fn run(program: &str, args: &[&str]) -> Result<()> {
    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("running {program}"))?;
    if !output.status.success() {
        bail!(
            "{program} {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// A btrfs loopback mounted for the test, unmounted and removed on drop.
struct Loopback {
    _dir: tempfile::TempDir,
    mount: PathBuf,
}

impl Loopback {
    fn new() -> Result<Self> {
        let dir = tempfile::tempdir()?;
        let image = dir.path().join("store.img");
        let mount = dir.path().join("mnt");
        std::fs::create_dir(&mount)?;
        std::fs::File::create(&image)?.set_len(4 << 30)?;
        run("mkfs.btrfs", &["-q", "-f", image.to_str().unwrap()])?;
        run(
            "mount",
            &[
                "-o",
                "loop",
                image.to_str().unwrap(),
                mount.to_str().unwrap(),
            ],
        )?;
        Ok(Self { _dir: dir, mount })
    }

    /// Bytes the filesystem has allocated, once deleted subvolumes are cleaned and the
    /// transaction is committed.
    fn used(&self) -> Result<u64> {
        let mount = self.mount.to_str().unwrap();
        run("btrfs", &["subvolume", "sync", mount])?;
        run("btrfs", &["filesystem", "sync", mount])?;
        let stat = nix::sys::statvfs::statvfs(&self.mount)?;
        Ok((stat.blocks() - stat.blocks_free()) as u64 * stat.fragment_size() as u64)
    }
}

impl Drop for Loopback {
    fn drop(&mut self) {
        let _ = Command::new("umount").arg(&self.mount).status();
    }
}

fn write_config(dir: &Path, name: &str, parent: Option<&str>) -> Result<()> {
    let config = serde_json::json!({ "name": name, "parent_snapshot": parent });
    std::fs::write(dir.join("config.json"), serde_json::to_vec(&config)?)?;
    Ok(())
}

/// A snapshot `name` whose memory file is `MEMORY_BYTES` of a pattern, nodatacow, written
/// as one sequential stream (large extents, as Firecracker's Full snapshot writes it).
fn full_snapshot(store: &Path, name: &str) -> Result<PathBuf> {
    let dir = store.join(name);
    run("btrfs", &["subvolume", "create", dir.to_str().unwrap()])?;
    let memory = dir.join("memory.bin");
    std::fs::File::create(&memory)?;
    run("chattr", &["+C", memory.to_str().unwrap()])?;
    let file = std::fs::OpenOptions::new().write(true).open(&memory)?;
    let chunk: Vec<u8> = (0..MIB).map(|i| (i % 251) as u8 + 1).collect();
    for at in (0..MEMORY_BYTES).step_by(MIB as usize) {
        file.write_all_at(&chunk, at)?;
    }
    file.sync_all()?;
    write_config(&dir, name, None)?;
    Ok(dir)
}

/// A diff snapshot `name` of `parent`: a subvolume snapshot of its directory with every
/// other MiB of the memory file written again with `fill`.
fn diff_snapshot(store: &Path, parent: &str, name: &str, fill: u8) -> Result<PathBuf> {
    let dir = store.join(name);
    run(
        "btrfs",
        &[
            "subvolume",
            "snapshot",
            store.join(parent).to_str().unwrap(),
            dir.to_str().unwrap(),
        ],
    )?;
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(dir.join("memory.bin"))?;
    let chunk = vec![fill; MIB as usize];
    for at in (0..MEMORY_BYTES).step_by(2 * MIB as usize) {
        file.write_all_at(&chunk, at)?;
    }
    file.sync_all()?;
    write_config(&dir, name, Some(parent))?;
    Ok(dir)
}

async fn delete(store: &Path, name: &str) -> Result<()> {
    let manager = SnapshotManager::new(store.to_path_buf());
    let mut compaction = Compaction::default();
    manager
        .delete_snapshot_compacting(name, Some(&mut compaction))
        .await?;
    compaction.rewrite_survivors(store).await;
    Ok(())
}

/// The space a memory file of `MEMORY_BYTES` may take once nothing else holds its extents:
/// the file and the filesystem's metadata, well under the 128 MiB a kept half of the
/// parent's extents adds.
fn assert_holds_only_its_own_bytes(fs: &Loopback, base: u64, what: &str) -> Result<()> {
    let used = fs.used()? - base;
    println!(
        "{what}: {} MiB on disk for a {} MiB memory file",
        used / MIB,
        MEMORY_BYTES / MIB
    );
    assert!(
        used < MEMORY_BYTES + 32 * MIB,
        "{what}: the filesystem holds {} MiB for a {} MiB memory file",
        used / MIB,
        MEMORY_BYTES / MIB
    );
    Ok(())
}

/// Deleting the parent of an in-place diff snapshot gives the diff extents of its own: the
/// filesystem holds the diff's memory file and nothing of the parent. The bytes and the
/// nanosecond mtime (the working set's key) are the same as before.
#[tokio::test]
async fn deleting_the_parent_frees_the_extents_its_diff_replaced() -> Result<()> {
    let fs = Loopback::new()?;
    let store = fs.mount.join("snapshots");
    std::fs::create_dir(&store)?;
    let base = fs.used()?;

    full_snapshot(&store, "a")?;
    let b = diff_snapshot(&store, "a", "b", 0xee)?;
    let memory = b.join("memory.bin");
    let bytes = std::fs::read(&memory)?;
    let mtime = std::fs::metadata(&memory)?;
    println!(
        "before the delete: {} MiB on disk",
        (fs.used()? - base) / MIB
    );

    delete(&store, "a").await?;
    assert!(!store.join("a").exists());
    assert_holds_only_its_own_bytes(&fs, base, "after deleting a")?;

    let after = std::fs::metadata(&memory)?;
    assert_eq!(
        std::fs::read(&memory)?,
        bytes,
        "the rewrite changed the memory file"
    );
    assert_eq!(
        (after.mtime(), after.mtime_nsec(), after.len()),
        (mtime.mtime(), mtime.mtime_nsec(), mtime.len())
    );
    let leftovers: Vec<_> = std::fs::read_dir(&b)?
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| name.to_string_lossy().contains(".rewrite-"))
        .collect();
    assert!(leftovers.is_empty(), "the rewrite left {leftovers:?}");
    Ok(())
}

/// Two diffs of one parent share the parent's pages, so deleting the parent rewrites neither
/// (a copy of either would take the shared pages' space again). Deleting one of them then
/// leaves the other alone with the parent's extents, and that one is rewritten.
#[tokio::test]
async fn siblings_are_rewritten_only_once_one_is_left() -> Result<()> {
    let fs = Loopback::new()?;
    let store = fs.mount.join("snapshots");
    std::fs::create_dir(&store)?;
    let base = fs.used()?;

    full_snapshot(&store, "a")?;
    let b = diff_snapshot(&store, "a", "b", 0xee)?;
    diff_snapshot(&store, "a", "b2", 0xdd)?;
    let ino = |dir: &Path| std::fs::metadata(dir.join("memory.bin")).map(|m| m.ino());
    let b_ino = ino(&b)?;

    delete(&store, "a").await?;
    assert_eq!(
        ino(&b)?,
        b_ino,
        "a diff that shares with its sibling was rewritten"
    );

    let bytes = std::fs::read(b.join("memory.bin"))?;
    delete(&store, "b2").await?;
    assert_ne!(
        ino(&b)?,
        b_ino,
        "the last diff of the deleted parent was not rewritten"
    );
    assert_eq!(std::fs::read(b.join("memory.bin"))?, bytes);
    assert_holds_only_its_own_bytes(&fs, base, "after deleting a and b2")?;
    Ok(())
}

/// The rewrite waits for a create that stages from the snapshot before it writes its copy: a
/// subvolume snapshot taken during the copy would carry the copy into the new snapshot's
/// directory, where its reclaim record does not follow it (#1810 review).
///
/// RED BEFORE THE FIX: the copy was written without the stage lock, so it appeared beside the
/// memory file while a create held the lock.
#[tokio::test]
async fn the_rewrite_waits_for_a_create_staging_from_the_snapshot() -> Result<()> {
    let fs = Loopback::new()?;
    let store = fs.mount.join("snapshots");
    std::fs::create_dir(&store)?;
    full_snapshot(&store, "a")?;
    let b = diff_snapshot(&store, "a", "b", 0xee)?;
    let manager = SnapshotManager::new(store.clone());
    let mut compaction = Compaction::default();
    manager
        .delete_snapshot_compacting("a", Some(&mut compaction))
        .await?;
    fs.used()?;

    // A create staging from b holds b's stage lock shared.
    let staging = fcvm::commands::common::acquire_snapshot_stage_lock(&b, false).await?;
    let rewrite = tokio::spawn({
        let b = b.clone();
        async move { fcvm::storage::compact::rewrite_memory_file(&b).await }
    });
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    let copies = |dir: &Path| -> Vec<std::ffi::OsString> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name.to_string_lossy().contains(".rewrite-"))
            .collect()
    };
    assert!(
        copies(&b).is_empty(),
        "the rewrite wrote {:?} while a create held the snapshot's stage lock",
        copies(&b)
    );
    assert!(
        !rewrite.is_finished(),
        "the rewrite did not wait for the stage lock"
    );
    drop(staging);
    let outcome = rewrite.await??;
    assert!(
        matches!(outcome, fcvm::storage::compact::Rewrite::Rewrote { .. }),
        "after the create, the rewrite ran: {outcome:?}"
    );
    assert!(copies(&b).is_empty());
    Ok(())
}

//! `fcvm snapshot export <tag>`: one snapshot written as an archive
//! (`storage::snapshot_archive`) to a file or to stdout.
//!
//! The archive carries the snapshot's directory: every regular file under it by its path
//! relative to the directory, `config.json` byte for byte. Lock files and temporaries stay
//! behind (names ending in `.lock` or `.tmp`, which covers the working-set sidecar's
//! `.lock` and `.<uuid>.tmp`). The memory image is read from `config.memory_path`, which
//! `snapshot create --memory-dir` makes a link to a file outside the directory, and is
//! always the entry `snapshot/memory.bin`; the export fails when that path and the
//! directory's `memory.bin` are not the same file.
//!
//! What a restore reads outside the directory is not carried. `manifest.not_shipped` names
//! what the snapshot's config records a path for (the image disk, each pmem image, each
//! mapped directory, each NFS share, the firmware), so an importer can refuse or warn. It
//! cannot name a read-only `--disk` file outside the VM's directory or a kernel profile's
//! NVIDIA driver image: a snapshot's config does not record them
//! (`commands::common::extra_disks_to_snapshot` keeps only the disks inside the directory),
//! and Firecracker reopens them from the paths in its own VM state.
//!
//! `manifest.firecracker` describes the binary that is at the recorded path when the export
//! runs: its `--version` output and its SHA-256 are those of the file as it is then. A
//! snapshot records the path only, so a binary replaced at that path after the snapshot was
//! taken is the one the manifest describes.
//!
//! The export holds the tag's lock shared from before it reads the config until the last
//! byte is written, as a memory server does, so a create or a delete of the tag waits for
//! the stream to end and the archive is one generation of the snapshot. At the end it reads
//! `config.json` again and fails if its digest is no longer the one in the manifest. A tag
//! that ends in `.creating` or `.old` is refused: a directory of that name is also where a
//! create of the tag without the suffix works, under that other tag's lock.
//!
//! An export that fails while it streams writes no trailer, and no reader accepts an
//! archive without one. An export to a file writes a temporary beside it and renames it
//! over the name only after the trailer is written and the file is synced; a failure in
//! that sync, in the second check of the output or in the rename removes the temporary,
//! which by then is a whole archive. The one failure that leaves a whole
//! archive under the name is the sync of the directory after the rename, and its error says
//! so. The temporary has a reclaim record (`crate::reclaim`), so one a killed export leaves
//! is removed by the next VM launch or the next export to a file by the same user.
//!
//! While the memory image is streamed the export holds the in-use mark on it
//! (`uffd::release`), so a memory server that starts for another snapshot does not drop the
//! pages the export has read ahead. When the image's entry has been written, or has failed
//! after data was read, the export gives the file back the way a release pass would: it
//! asks the kernel to drop it from the page cache unless another reader holds the mark.
//! Every other file is dropped from the page cache once its entry is written, with nothing
//! to ask: no reader keeps a snapshot's disk, VM state or sidecar in the page cache (a clone
//! snapshots or reflinks the disk into a file of its own), and the request drops clean pages
//! only. The disks sort before `memory.bin`, so without this a disk stays in the page
//! cache while the memory image is read.

use std::fs::File;
use std::io::{BufWriter, IsTerminal, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{bail, ensure, Context, Result};
use nix::fcntl::{posix_fadvise, PosixFadviseAdvice};
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use crate::cli::SnapshotExportArgs;
use crate::hypervisor::Backend;
use crate::paths;
use crate::reclaim::{Owner, Resource};
use crate::storage::snapshot::SNAPSHOT_MEMORY_FILE;
use crate::storage::snapshot_archive::{
    ArchiveManifest, ArchiveWriter, EntryRecord, ManifestFirecracker, ManifestMemory,
    ARCHIVE_VERSION,
};
use crate::storage::{
    validate_snapshot_name, SnapshotConfig, SnapshotGeneration, SnapshotKind, SnapshotManager,
};

/// The buffer between the archive writer and its output. The writer hands over a file's
/// data in pieces of this size, which pass straight through; the headers, the maps and the
/// two JSON entries are what it gathers.
const OUTPUT_BUFFER: usize = 1 << 20;

pub(crate) const CONFIG_FILE: &str = "config.json";

/// Where the exporting host keeps its snapshots and its data.
pub(crate) struct ExportHost {
    pub snapshots_dir: PathBuf,
    /// Goes into the manifest: the VM state embeds paths under it.
    pub data_dir: PathBuf,
}

impl ExportHost {
    fn configured() -> Self {
        Self {
            snapshots_dir: paths::snapshot_dir(),
            data_dir: paths::data_dir(),
        }
    }
}

/// Export a snapshot (`fcvm snapshot export`).
pub async fn cmd_snapshot_export(args: SnapshotExportArgs) -> Result<()> {
    let host = ExportHost::configured();
    let started = Instant::now();
    let entries = match &args.output {
        Some(output) => export_to_file(&host, &args.tag, output).await?,
        None => {
            let stdout = archive_output(&std::io::stdout())?;
            export_snapshot(&host, &args.tag, stdout).await?.1
        }
    };
    info!(
        snapshot = %args.tag,
        files = entries.len(),
        bytes = entries.iter().map(|entry| entry.len).sum::<u64>(),
        stored_bytes = entries.iter().map(|entry| entry.data_len).sum::<u64>(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "exported snapshot"
    );
    Ok(())
}

/// The file an archive is written to when it goes to `stdout`: the same open file, without
/// the line buffering of `std::io::Stdout`. A terminal is refused.
fn archive_output(stdout: &(impl IsTerminal + AsFd)) -> Result<File> {
    ensure!(
        !stdout.is_terminal(),
        "refusing to write a snapshot archive to a terminal: redirect stdout or pass --output \
         <FILE>"
    );
    let fd = stdout
        .as_fd()
        .try_clone_to_owned()
        .context("duplicating stdout")?;
    Ok(File::from(fd))
}

/// Export snapshot `tag` to the file `output`. The archive is written under a temporary
/// name beside `output` and renamed over it once it is whole and synced, so `output` never
/// names part of an archive. The temporary is recorded with a reclaim owner before it is
/// created: an export that fails removes it, and one that is killed leaves it to the next
/// VM launch or the next export to a file by the same user, both of which reclaim first.
pub(crate) async fn export_to_file(
    host: &ExportHost,
    tag: &str,
    output: &Path,
) -> Result<Vec<EntryRecord>> {
    let name = output
        .file_name()
        .with_context(|| format!("--output {} does not name a file", output.display()))?;
    let parent = match output.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let parent = std::fs::canonicalize(parent)
        .with_context(|| format!("resolving the directory of --output {}", output.display()))?;
    let output = parent.join(name);
    // Before anything is written, and again right before the rename.
    ensure_replaceable(&output)?;

    let owner_dir = crate::reclaim::owner_dir_in(&host.data_dir)?;
    // A killed export's temporary can be as large as the archive, and the export that is
    // run again in its place needs the room.
    crate::reclaim::reclaim_abandoned_in(&owner_dir).await;
    let id = uuid::Uuid::new_v4().simple().to_string();
    let temp = parent.join(format!(".{}.{id}.tmp", name.to_string_lossy()));
    let owner = Owner::begin_in(&owner_dir, &format!("export-{id}"))?;
    owner.record(Resource::Temp { path: temp.clone() })?;

    match write_output_file(host, tag, &temp, &output).await {
        Ok(entries) => {
            // The temporary has its final name. A record that stays names a path that no
            // longer exists, which the next launch's reclaim finds gone.
            if let Err(error) = owner.discard() {
                warn!(error = %format!("{error:#}"), "could not delete the export's reclaim record");
            }
            Ok(entries)
        }
        Err(error) => {
            if let Err(cleanup) = owner.complete().await {
                warn!(
                    temporary = %temp.display(),
                    error = %format!("{cleanup:#}"),
                    "could not remove the failed export's temporary file"
                );
            }
            Err(error)
        }
    }
}

/// The rename that publishes an archive replaces whatever has the name `output`. Anything
/// but a regular file there (a device, a pipe, a link) is what the caller meant to write
/// into, not to replace.
fn ensure_replaceable(output: &Path) -> Result<()> {
    match std::fs::symlink_metadata(output) {
        Ok(existing) => ensure!(
            existing.is_file(),
            "--output {} exists and is not a regular file; to write into it, export to stdout \
             and redirect",
            output.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("reading --output {}", output.display()))
        }
    }
    Ok(())
}

/// Give the whole, synced archive at `temp` the name `output`, then sync the directory with
/// `sync_dir`.
///
/// `output` is checked again first: the check before the stream is as old as the snapshot
/// is large, and something else can have taken the name since. A check and a rename are two
/// calls, so this narrows that window and does not close it.
///
/// When `sync_dir` fails the archive is already under its name, whole and synced, and only
/// the directory entry may be lost in a crash. The error says that, because the caller
/// reports a failure and `output` no longer holds what it held before.
fn publish_archive(
    temp: &Path,
    output: &Path,
    sync_dir: impl FnOnce(&Path) -> std::io::Result<()>,
) -> Result<()> {
    ensure_replaceable(output)?;
    std::fs::rename(temp, output)
        .with_context(|| format!("renaming the archive to {}", output.display()))?;
    if let Some(parent) = output.parent() {
        sync_dir(parent).with_context(|| {
            format!(
                "the archive is in place at {}, whole and synced, but syncing its directory \
                 {} failed, so the name may not survive a crash",
                output.display(),
                parent.display()
            )
        })?;
    }
    Ok(())
}

/// Write the archive of `tag` into the new file `temp`, sync it, and rename it to `output`.
async fn write_output_file(
    host: &ExportHost,
    tag: &str,
    temp: &Path,
    output: &Path,
) -> Result<Vec<EntryRecord>> {
    // A named temporary and a rename, not `O_TMPFILE` and `linkat`: `linkat` refuses a name
    // that exists, and `--output` replaces a file, so the unnamed file would still need a
    // second name to be renamed from. The reclaim record covers what a named temporary
    // costs, which is the file a killed export leaves.
    //
    // Owner only: the archive holds the guest's memory.
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(temp)
        .with_context(|| format!("creating {}", temp.display()))?;
    let (file, entries) = export_snapshot(host, tag, file).await?;
    let (temp, output) = (temp.to_path_buf(), output.to_path_buf());
    tokio::task::spawn_blocking(move || -> Result<()> {
        file.sync_all()
            .with_context(|| format!("syncing {}", temp.display()))?;
        drop(file);
        failpoint::hit("snapshot.export.pre_rename");
        publish_archive(&temp, &output, |dir| {
            File::open(dir).and_then(|dir| dir.sync_all())
        })
    })
    .await
    .context("the task that publishes the archive panicked")??;
    Ok(entries)
}

/// Write snapshot `tag` to `out` as one archive, and give `out` back with one record per
/// file written. `out` is written through a buffer of [`OUTPUT_BUFFER`] bytes, flushed
/// before this returns.
///
/// On an error `out` holds an archive without a trailer, or nothing.
pub(crate) async fn export_snapshot<W>(
    host: &ExportHost,
    tag: &str,
    out: W,
) -> Result<(W, Vec<EntryRecord>)>
where
    W: Write + Send + 'static,
{
    validate_snapshot_name(tag)?;
    // `<x>.creating` and `<x>.old` are the directories a create of `<x>` works in, and it
    // holds the lock of `<x>` for them. The lock taken below is this tag's own, so it would
    // not keep that create out, and the export could archive a generation that is still
    // being written or is about to be removed. The name cannot tell such a directory from a
    // snapshot that was given the tag, so both are refused.
    if let Some(other) = crate::uffd::staging_tag(tag) {
        bail!(
            "snapshot export refuses the tag '{tag}': a directory of that name is where a \
             create of snapshot '{other}' works, under the lock of '{other}', which an export \
             of '{tag}' does not hold"
        );
    }
    tokio::fs::create_dir_all(&host.snapshots_dir)
        .await
        .context("creating snapshot directory")?;
    let dir = host.snapshots_dir.join(tag);

    // Shared, for the whole stream: a create or a delete of this tag takes the lock
    // exclusively, so it waits until the archive is written, and the files read below are
    // one generation's.
    let generation_lock = super::common::acquire_snapshot_dir_lock(&dir, false).await?;
    let (config, generation) = SnapshotManager::new(host.snapshots_dir.clone())
        .load_snapshot_with_generation(tag)
        .await
        .context("loading snapshot configuration")?;
    ensure!(
        config.metadata.hypervisor == Backend::Firecracker,
        "snapshot '{tag}' was taken with Cloud Hypervisor; snapshot export carries Firecracker \
         snapshots only"
    );

    let memory = match config.kind {
        SnapshotKind::Full => {
            crate::storage::disk::ensure_memory_file_present(&config.memory_path, tag)?;
            let file = File::open(&config.memory_path).with_context(|| {
                format!("opening the memory image {}", config.memory_path.display())
            })?;
            ensure_memory_is_the_directorys(tag, &config.memory_path, &file, &dir)?;
            crate::uffd::mark_in_use(&file)
                .await
                .context("marking the memory image in use")?;
            Some(MarkedMemory { file, read: false })
        }
        SnapshotKind::DiskOnly => None,
    };

    let data_dir = host.data_dir.clone();
    let tag = tag.to_string();
    tokio::task::spawn_blocking(move || {
        // Held until the archive is written, or the write has failed.
        let _generation_lock = generation_lock;
        write_archive(&data_dir, &tag, &dir, &config, &generation, memory, out)
    })
    .await
    .context("the task that writes the archive panicked")?
}

/// The memory image is read from the path the config records, and every other file from
/// the walk of `dir`, which stands `memory.bin` in for it. Both have to be one file, or the
/// archive would pair this directory's disk and VM state with another memory image: `file`
/// (the config's path, opened) and `<dir>/memory.bin` (the image, or `--memory-dir`'s link
/// to it) must be the same inode on the same device.
fn ensure_memory_is_the_directorys(
    tag: &str,
    recorded: &Path,
    file: &File,
    dir: &Path,
) -> Result<()> {
    let in_dir = dir.join(SNAPSHOT_MEMORY_FILE);
    let listed = std::fs::metadata(&in_dir).with_context(|| {
        format!(
            "reading {}, the memory image of snapshot '{tag}'",
            in_dir.display()
        )
    })?;
    let opened = file
        .metadata()
        .with_context(|| format!("stat-ing the memory image {}", recorded.display()))?;
    ensure!(
        (opened.dev(), opened.ino()) == (listed.dev(), listed.ino()),
        "snapshot '{tag}' records its memory image at {}, which is not the file at {}; the \
         archive would pair the directory's files with another memory image",
        recorded.display(),
        in_dir.display()
    );
    Ok(())
}

/// The memory image, marked in use (`uffd::release`) from before its first byte is read.
/// Dropping it ends the mark. If the export has read any of the image's data by then, the
/// drop also gives the file back as a release pass would: the kernel is asked to drop it
/// from the page cache unless another reader holds the mark. An export that fails before it
/// reads any of the image's data leaves the image's pages as it found them.
struct MarkedMemory {
    file: File,
    /// Whether the export has read any of the image's data.
    read: bool,
}

impl Drop for MarkedMemory {
    fn drop(&mut self) {
        if !self.read {
            return;
        }
        match crate::uffd::release_unless_in_use(&self.file) {
            Ok(true) => info!("released the exported memory image from the page cache"),
            Ok(false) => info!(
                "left the exported memory image in the page cache: another reader holds it in use"
            ),
            Err(error) => warn!(
                error = %error,
                "the kernel refused to drop the exported memory image from the page cache"
            ),
        }
    }
}

/// Write the archive of the snapshot in `dir`, under the tag's shared lock.
fn write_archive<W: Write>(
    data_dir: &Path,
    tag: &str,
    dir: &Path,
    config: &SnapshotConfig,
    generation: &SnapshotGeneration,
    mut memory: Option<MarkedMemory>,
    out: W,
) -> Result<(W, Vec<EntryRecord>)> {
    let manifest = build_manifest(
        data_dir,
        tag,
        config,
        generation,
        memory.as_ref().map(|memory| &memory.file),
    )?;
    let files = snapshot_files(dir, memory.is_some())?;

    let out = BufWriter::with_capacity(OUTPUT_BUFFER, out);
    let mut writer = ArchiveWriter::new(out, &manifest)?;
    let mut entries = Vec::with_capacity(files.len());
    for relative in &files {
        let is_memory = relative == Path::new(SNAPSHOT_MEMORY_FILE);
        // What the writer has read of this file once the append returns, failed or not: an
        // append that fails in the file's headers has read none of it.
        let read_before = writer.data_bytes_read();
        let record = if let Some(mut memory) = memory.take_if(|_| is_memory) {
            let appended = writer.append_snapshot_file(relative, &memory.file);
            memory.read = writer.data_bytes_read() > read_before;
            // `memory` is dropped at the end of this block, which ends the mark and gives
            // the file back now, not when the files that sort after it are written.
            appended?
        } else {
            let path = dir.join(relative);
            let file = File::open(&path).with_context(|| format!("opening {}", path.display()))?;
            let appended = writer.append_snapshot_file(relative, &file);
            if writer.data_bytes_read() > read_before {
                drop_from_page_cache(&file, &path);
            }
            appended?
        };
        entries.push(record);
    }

    // The files were read under the shared lock, which keeps creates and deletes out. This
    // catches a config rewritten in place by something that does not take the lock.
    let config_path = dir.join(CONFIG_FILE);
    let now = sha256_hex(&config_path)?;
    ensure!(
        now == manifest.config_sha256,
        "{} changed while snapshot '{tag}' was exported, so the archive would not be one \
         generation of the snapshot",
        config_path.display()
    );

    let out = writer.finish()?;
    let out = out
        .into_inner()
        .map_err(|error| error.into_error())
        .context("flushing the archive")?;
    Ok((out, entries))
}

/// Ask the kernel to drop a file the export has read, other than the memory image, from the
/// page cache. These files have no in-use mark to ask: no reader keeps a snapshot's disk, VM
/// state or sidecar cached (a clone works on its own snapshot or reflink of the disk), and
/// `POSIX_FADV_DONTNEED` drops clean pages that no process has mapped, so a page with a
/// write still to reach the disk stays. The disks sort before `memory.bin`: left cached, a
/// disk stays in the page cache while the memory image is read.
fn drop_from_page_cache(file: &File, path: &Path) {
    if let Err(error) = posix_fadvise(file, 0, 0, PosixFadviseAdvice::POSIX_FADV_DONTNEED) {
        warn!(
            file = %path.display(),
            error = %error,
            "the kernel refused to drop an exported file from the page cache"
        );
    }
}

/// The regular files of the snapshot in `dir` as paths relative to it: `config.json`, then
/// the rest in path order. Lock files and temporaries are left out. With `memory_by_config`
/// the entry `memory.bin` at the top stands for the memory image the config names, whatever
/// is at that name in the directory (the image, or `--memory-dir`'s link to it).
///
/// Anything else that is neither a regular file nor a directory is refused: an archive holds
/// regular files, and leaving such an entry out would export a snapshot that is not the one
/// on this host.
fn snapshot_files(dir: &Path, memory_by_config: bool) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut pending = vec![PathBuf::new()];
    while let Some(relative_dir) = pending.pop() {
        let absolute = dir.join(&relative_dir);
        let listing = std::fs::read_dir(&absolute)
            .with_context(|| format!("listing {}", absolute.display()))?;
        for entry in listing {
            let entry = entry.with_context(|| format!("listing {}", absolute.display()))?;
            let relative = relative_dir.join(entry.file_name());
            if memory_by_config && relative == Path::new(SNAPSHOT_MEMORY_FILE) {
                continue;
            }
            if is_lock_or_temporary(&entry.file_name()) {
                continue;
            }
            let kind = entry
                .file_type()
                .with_context(|| format!("reading the type of {}", entry.path().display()))?;
            if kind.is_dir() {
                pending.push(relative);
            } else if kind.is_file() {
                files.push(relative);
            } else {
                bail!(
                    "{} is neither a regular file nor a directory, and an archive carries only \
                     regular files",
                    entry.path().display()
                );
            }
        }
    }
    if memory_by_config {
        files.push(PathBuf::from(SNAPSHOT_MEMORY_FILE));
    }
    files.sort();
    let config = files
        .iter()
        .position(|file| file == Path::new(CONFIG_FILE))
        .with_context(|| format!("{} has no {CONFIG_FILE}", dir.display()))?;
    files[..=config].rotate_right(1);
    Ok(files)
}

/// Whether a file of this name is a lock file or a writer's temporary, which belong to the
/// host and not to the snapshot.
fn is_lock_or_temporary(name: &std::ffi::OsStr) -> bool {
    let name = name.as_encoded_bytes();
    name.ends_with(b".lock") || name.ends_with(b".tmp")
}

fn build_manifest(
    data_dir: &Path,
    tag: &str,
    config: &SnapshotConfig,
    generation: &SnapshotGeneration,
    memory: Option<&File>,
) -> Result<ArchiveManifest> {
    let memory = memory
        .map(|file| {
            let meta = file.metadata().context("stat-ing the memory image")?;
            Ok::<_, anyhow::Error>(ManifestMemory {
                len: meta.len(),
                mtime_sec: meta.mtime(),
                mtime_nsec: meta.mtime_nsec(),
            })
        })
        .transpose()?;
    Ok(ArchiveManifest {
        archive_version: ARCHIVE_VERSION,
        arch: std::env::consts::ARCH.to_string(),
        network_boundary_version: config.network_boundary_version,
        container_start_version: config.container_start_version,
        working_set_version: crate::uffd::WORKING_SET_VERSION,
        tag: tag.to_string(),
        kind: config.kind,
        hypervisor: config.metadata.hypervisor,
        data_dir: data_dir.to_path_buf(),
        config_sha256: generation.config_digest_hex(),
        memory,
        firecracker: firecracker_identity(tag, config)?,
        externals: Vec::new(),
        pmem_owner_uid: pmem_owner_uid(config)?,
        not_shipped: not_shipped(config),
    })
}

/// The Firecracker binary at the path the snapshot recorded, as it is now, at export time:
/// its path, what `--version` prints (memoised by `version_cache` per path, modification
/// time and size) and the SHA-256 of the file. A snapshot records the path only, so a binary
/// replaced at that path after the snapshot was taken is the one this describes.
fn firecracker_identity(tag: &str, config: &SnapshotConfig) -> Result<ManifestFirecracker> {
    let path = config.metadata.firecracker_bin.clone().with_context(|| {
        format!(
            "snapshot '{tag}' records no Firecracker binary, so an importing host could not \
             be checked against the one that wrote it; recreate the snapshot to export it"
        )
    })?;
    let sha256 = sha256_hex(&path)?;
    let version_output = crate::version_cache::version_output(&path)?;
    Ok(ManifestFirecracker {
        path,
        version_output,
        sha256,
    })
}

/// The uid that owns the snapshot's pmem store entries, which is part of each entry's name
/// (`storage::pmem_store`). `None` for a snapshot without pmem devices.
fn pmem_owner_uid(config: &SnapshotConfig) -> Result<Option<u32>> {
    let mut owner = None;
    for device in &config.metadata.pmem_devices {
        let uid = std::fs::metadata(&device.path)
            .with_context(|| format!("reading the owner of the pmem image {}", device.path))?
            .uid();
        match owner {
            None => owner = Some(uid),
            Some(first) => ensure!(
                first == uid,
                "the snapshot's pmem images belong to uid {first} and uid {uid}, and a \
                 manifest names one owner"
            ),
        }
    }
    Ok(owner)
}

/// What a restore of the snapshot reads that the archive does not carry, one sentence each,
/// for everything the config records a path for: the image disk, the pmem images, the
/// mapped directories, the NFS shares and the firmware.
///
/// Not listed, because no field of the config names them: a read-only `--disk` file outside
/// the VM's directory (`commands::common::extra_disks_to_snapshot` records only the disks a
/// snapshot keeps in its own directory) and a kernel profile's NVIDIA driver image. A
/// restore reopens both from the paths in Firecracker's VM state.
pub(crate) fn not_shipped(config: &SnapshotConfig) -> Vec<String> {
    let metadata = &config.metadata;
    let mut list = Vec::new();
    if let Some(path) = &metadata.image_disk_path {
        list.push(format!(
            "The image disk {} holds the container image and is attached read-only at this path.",
            path.display()
        ));
    }
    for device in &metadata.pmem_devices {
        list.push(format!(
            "The pmem image {} (a copy of {}) is mapped at {} and reopened at this path.",
            device.path, device.source, device.mount_path
        ));
    }
    for volume in &metadata.volumes {
        list.push(format!(
            "The directory {} is mapped into the guest at {}.",
            volume.host_path.display(),
            volume.guest_path
        ));
    }
    for share in &metadata.nfs_shares {
        list.push(format!(
            "The directory {} is exported over NFS and mounted in the guest at {}.",
            share.host_path, share.mount_path
        ));
    }
    if let Some(path) = &metadata.firmware {
        list.push(format!(
            "The UEFI firmware {} is booted again when the guest restarts.",
            path.display()
        ));
    }
    list
}

/// SHA-256 of the file at `path`, in hex.
pub(crate) fn sha256_hex(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = Sha256::new();
    std::io::copy(
        &mut file,
        &mut crate::storage::disk_dir::HashWriter(&mut hasher),
    )
    .with_context(|| format!("reading {}", path.display()))?;
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::storage::snapshot::{
        SNAPSHOT_CONTAINER_START_VERSION, SNAPSHOT_NETWORK_BOUNDARY_VERSION,
    };
    use crate::storage::snapshot_archive::{ArchiveReader, EntryName, CHUNK};
    use crate::storage::SnapshotType;
    use crate::uffd::page_cache_testing::resident_pages;
    use memmap2::MmapOptions;
    use std::os::unix::fs::{FileExt, PermissionsExt};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    pub(crate) const TAG: &str = "example-tag";
    pub(crate) const PAGE: u64 = 4096;
    pub(crate) const MIB: u64 = 1 << 20;
    pub(crate) const MEMORY_LEN: u64 = 8 * MIB;
    const VERSION_OUTPUT: &str = "Firecracker v0.0.0-example\n";

    /// A directory on the filesystem the snapshot store is on, which keeps a 4 KiB hole and
    /// lets a file be dropped from the page cache. `sparse` and the page cache tests check
    /// that it does both.
    pub(crate) fn test_dir() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("snapshot-export-test-")
            .tempdir_in("/mnt/fcvm-btrfs")
            .expect("a test directory under /mnt/fcvm-btrfs")
    }

    /// `len` bytes that are never zero and differ from offset to offset.
    pub(crate) fn pattern(seed: u64, len: u64) -> Vec<u8> {
        let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state as u8) | 1
            })
            .collect()
    }

    /// The file `path` of `len` bytes holding `runs` and nothing else, synced so its extents
    /// are settled.
    pub(crate) fn sparse(path: &Path, len: u64, runs: &[(u64, Vec<u8>)]) {
        let file = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)
            .unwrap();
        assert_eq!(
            file.metadata().unwrap().blksize(),
            PAGE,
            "control: {} is on a filesystem that cannot keep a 4 KiB hole",
            path.display()
        );
        file.set_len(len).unwrap();
        for (offset, bytes) in runs {
            file.write_all_at(bytes, *offset).unwrap();
        }
        file.sync_all().unwrap();
    }

    /// The data runs of the file at `path` as `(start, end)`, the way an archive stores
    /// them: whole 512-byte blocks, clipped to the file's length, runs that touch merged.
    pub(crate) fn data_runs(path: &Path) -> Vec<(u64, u64)> {
        let file = File::open(path).unwrap();
        let len = file.metadata().unwrap().len();
        let mut runs: Vec<(u64, u64)> = Vec::new();
        crate::uffd::for_each_data_run(&file, len, |start, end| {
            let (start, end) = (start / 512 * 512, (end.div_ceil(512) * 512).min(len));
            match runs.last_mut() {
                Some(last) if last.1 >= start => last.1 = end,
                _ => runs.push((start, end)),
            }
        })
        .unwrap();
        runs
    }

    /// A snapshot directory as `snapshot create` leaves one, under a snapshot store of its
    /// own, with the files a serve adds beside the memory image.
    pub(crate) struct Fake {
        pub(crate) root: tempfile::TempDir,
        pub(crate) host: ExportHost,
        tag: String,
    }

    impl Fake {
        pub(crate) fn dir(&self) -> PathBuf {
            self.host.snapshots_dir.join(&self.tag)
        }

        pub(crate) fn memory(&self) -> PathBuf {
            self.dir().join(SNAPSHOT_MEMORY_FILE)
        }

        pub(crate) fn firecracker(&self) -> PathBuf {
            self.root.path().join("firecracker")
        }

        fn host(&self) -> ExportHost {
            ExportHost {
                snapshots_dir: self.host.snapshots_dir.clone(),
                data_dir: self.host.data_dir.clone(),
            }
        }

        /// The names in the reclaim owner directory that belong to an export.
        fn export_records(&self) -> Vec<String> {
            let dir = crate::reclaim::owner_dir_in(&self.host.data_dir).unwrap();
            names_in(&dir)
                .into_iter()
                .filter(|name| name.contains("export-"))
                .collect()
        }
    }

    pub(crate) fn names_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    /// The config a create writes for the snapshot `TAG` in `dir`, naming `firecracker` as
    /// its binary.
    fn fake_config(dir: &Path, firecracker: &Path) -> SnapshotConfig {
        let mut vm = crate::state::VmState::new(
            "vm-export".to_string(),
            "example-image".to_string(),
            1,
            (MEMORY_LEN / MIB) as u32,
        );
        vm.config.source_vsock_socket_path = Some(PathBuf::from("/run/example-vsock/vsock.sock"));
        let mut config = crate::commands::common::build_snapshot_config(
            &vm,
            TAG,
            SnapshotType::User,
            dir,
            Vec::new(),
            Vec::new(),
            None,
        )
        .unwrap();
        config.metadata.firecracker_bin = Some(firecracker.to_path_buf());
        config
    }

    pub(crate) fn fake_snapshot() -> Fake {
        fake_snapshot_with(|_| {})
    }

    /// [`fake_snapshot`] with `edit` applied to the config before it is written.
    pub(crate) fn fake_snapshot_with(edit: impl FnOnce(&mut SnapshotConfig)) -> Fake {
        fake_snapshot_tagged(TAG, edit)
    }

    /// [`fake_snapshot_with`] in the directory of `tag`.
    fn fake_snapshot_tagged(tag: &str, edit: impl FnOnce(&mut SnapshotConfig)) -> Fake {
        let root = test_dir();
        let host = ExportHost {
            snapshots_dir: root.path().join("snapshots"),
            data_dir: root.path().join("data"),
        };
        let fake = Fake {
            root,
            host,
            tag: tag.to_string(),
        };
        let dir = fake.dir();
        std::fs::create_dir_all(dir.join("disk")).unwrap();

        std::fs::write(
            fake.firecracker(),
            format!("#!/bin/sh\nprintf '%s' '{VERSION_OUTPUT}'\n"),
        )
        .unwrap();
        std::fs::set_permissions(fake.firecracker(), std::fs::Permissions::from_mode(0o755))
            .unwrap();

        let mut config = fake_config(&dir, &fake.firecracker());
        edit(&mut config);
        std::fs::write(
            dir.join(CONFIG_FILE),
            serde_json::to_vec_pretty(&config).unwrap(),
        )
        .unwrap();

        // Data at the start, a run longer than one copy buffer, and a hole at the end.
        sparse(
            &fake.memory(),
            MEMORY_LEN,
            &[(0, pattern(1, PAGE)), (MIB, pattern(2, 2 * MIB + PAGE))],
        );
        // A hole at the start and data up to the last byte.
        sparse(
            &dir.join("disk/disk.raw"),
            4 * MIB,
            &[
                (MIB, pattern(3, 2 * PAGE)),
                (4 * MIB - PAGE, pattern(4, PAGE)),
            ],
        );
        std::fs::write(dir.join("vmstate.bin"), pattern(5, 5000)).unwrap();
        std::fs::write(dir.join("volume-5000-inode-table.json"), b"{}").unwrap();
        // What a serve keeps beside the memory image: the working set, the lock that orders
        // its writers, and a writer's temporary.
        std::fs::write(dir.join("memory.bin.working-set"), pattern(6, 300)).unwrap();
        std::fs::write(dir.join("memory.bin.working-set.lock"), b"").unwrap();
        std::fs::write(
            dir.join(format!(
                "memory.bin.working-set.{}.tmp",
                uuid::Uuid::new_v4()
            )),
            pattern(7, 100),
        )
        .unwrap();
        fake
    }

    /// An output a test keeps a handle on, so it can read what an export that failed wrote.
    #[derive(Clone, Default)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl SharedBuf {
        fn bytes(&self) -> Vec<u8> {
            self.0.lock().unwrap().clone()
        }
    }

    impl Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// An output that runs `before` ahead of every write, with the bytes about to be
    /// written.
    struct Hooked<F> {
        out: SharedBuf,
        before: F,
    }

    impl<F: FnMut(&[u8])> Write for Hooked<F> {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            (self.before)(buf);
            self.out.write(buf)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// An output that refuses the first write `refuse` picks, and every write after it. It
    /// keeps the bytes of the write it refused.
    struct Refusing<F> {
        out: SharedBuf,
        refuse: F,
        refused: Arc<Mutex<Option<Vec<u8>>>>,
    }

    impl<F: FnMut(&[u8]) -> bool> Write for Refusing<F> {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let mut refused = self.refused.lock().unwrap();
            if refused.is_none() && (self.refuse)(buf) {
                *refused = Some(buf.to_vec());
            }
            if refused.is_some() {
                return Err(std::io::Error::other("the reader of the archive is gone"));
            }
            self.out.write(buf)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Where `needle` starts in `haystack`.
    fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    /// The message of the failure of an export of `fake` into `out`.
    async fn export_failure<W: Write + Send + 'static>(fake: &Fake, out: W) -> String {
        match export_snapshot(&fake.host, &fake.tag, out).await {
            Ok(_) => panic!("the export succeeded"),
            Err(error) => format!("{error:#}"),
        }
    }

    /// One file read back from an archive.
    #[derive(Debug)]
    struct ReadBack {
        name: PathBuf,
        /// Its data runs as `(start, end)`, pieces that touch merged.
        runs: Vec<(u64, u64)>,
        /// The whole file: the runs' bytes at their offsets, zeros everywhere else.
        bytes: Vec<u8>,
    }

    /// Read `archive` to its verified end.
    fn read_archive(archive: &[u8]) -> Result<(ArchiveManifest, Vec<ReadBack>)> {
        let mut reader = ArchiveReader::new(archive)?;
        let manifest = reader.manifest().clone();
        let mut files = Vec::new();
        while let Some(entry) = reader.next_entry()? {
            let EntryName::Snapshot(name) = entry.name else {
                bail!("the archive holds an external file");
            };
            let mut bytes = vec![0u8; usize::try_from(entry.len)?];
            let mut runs: Vec<(u64, u64)> = Vec::new();
            while let Some((offset, piece)) = reader.next_run()? {
                let end = offset + piece.len() as u64;
                bytes[offset as usize..end as usize].copy_from_slice(piece);
                match runs.last_mut() {
                    Some(last) if last.1 == offset => last.1 = end,
                    _ => runs.push((offset, end)),
                }
            }
            files.push(ReadBack { name, runs, bytes });
        }
        Ok((manifest, files))
    }

    /// `read` holds the file at `source`: its length, its data runs and its bytes.
    fn assert_reads_back(read: &ReadBack, source: &Path) {
        let expected = std::fs::read(source).unwrap();
        assert_eq!(
            read.bytes.len(),
            expected.len(),
            "length of {}",
            read.name.display()
        );
        assert_eq!(
            read.runs,
            data_runs(source),
            "data runs of {}",
            read.name.display()
        );
        assert!(read.bytes == expected, "bytes of {}", read.name.display());
    }

    fn sha256_of(bytes: &[u8]) -> String {
        hex::encode(Sha256::digest(bytes))
    }

    /// The export the command runs, read back with the archive reader: the snapshot's files
    /// by relative path with their lengths, hole maps and bytes, `config.json` first and
    /// byte for byte, no lock file and no temporary, and a manifest with every field filled
    /// from this host and this snapshot.
    #[tokio::test]
    async fn an_export_reads_back_as_the_snapshot_directory() {
        let fake = fake_snapshot();
        let dir = fake.dir();
        assert_eq!(
            data_runs(&fake.memory()),
            [(0, PAGE), (MIB, 3 * MIB + PAGE)],
            "control: the fake memory image keeps its holes"
        );
        assert_eq!(
            data_runs(&dir.join("disk/disk.raw")),
            [(MIB, MIB + 2 * PAGE), (4 * MIB - PAGE, 4 * MIB)],
            "control: the fake disk keeps its holes"
        );
        assert_eq!(
            names_in(&dir)
                .iter()
                .filter(|name| name.ends_with(".lock") || name.ends_with(".tmp"))
                .count(),
            2,
            "control: the directory holds a lock file and a temporary"
        );

        let (archive, records) = export_snapshot(&fake.host, TAG, Vec::new()).await.unwrap();
        let (manifest, files) = read_archive(&archive).unwrap();

        let names: Vec<&str> = files
            .iter()
            .map(|file| file.name.to_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "config.json",
                "disk/disk.raw",
                "memory.bin",
                "memory.bin.working-set",
                "vmstate.bin",
                "volume-5000-inode-table.json",
            ]
        );
        for file in &files {
            assert_reads_back(file, &dir.join(&file.name));
        }
        assert_eq!(
            records
                .iter()
                .map(|record| record.name.as_str())
                .collect::<Vec<_>>(),
            names
                .iter()
                .map(|name| format!("snapshot/{name}"))
                .collect::<Vec<_>>(),
            "the records the export returns name the files it wrote"
        );

        let memory = std::fs::metadata(fake.memory()).unwrap();
        assert_eq!(
            manifest,
            ArchiveManifest {
                archive_version: ARCHIVE_VERSION,
                arch: std::env::consts::ARCH.to_string(),
                network_boundary_version: SNAPSHOT_NETWORK_BOUNDARY_VERSION,
                container_start_version: SNAPSHOT_CONTAINER_START_VERSION,
                working_set_version: crate::uffd::WORKING_SET_VERSION,
                tag: TAG.to_string(),
                kind: SnapshotKind::Full,
                hypervisor: Backend::Firecracker,
                data_dir: fake.host.data_dir.clone(),
                config_sha256: sha256_of(&std::fs::read(dir.join(CONFIG_FILE)).unwrap()),
                memory: Some(ManifestMemory {
                    len: MEMORY_LEN,
                    mtime_sec: memory.mtime(),
                    mtime_nsec: memory.mtime_nsec(),
                }),
                firecracker: ManifestFirecracker {
                    path: fake.firecracker(),
                    version_output: VERSION_OUTPUT.to_string(),
                    sha256: sha256_of(&std::fs::read(fake.firecracker()).unwrap()),
                },
                externals: Vec::new(),
                pmem_owner_uid: None,
                not_shipped: Vec::new(),
            }
        );
    }

    /// `snapshot create --memory-dir` leaves `memory.bin` as a link to a file outside the
    /// directory. The export reads the file the config names through the link and archives
    /// it as `memory.bin`.
    #[tokio::test]
    async fn a_memory_image_outside_the_directory_is_archived_as_memory_bin() {
        let fake = fake_snapshot();
        let outside = fake.root.path().join("memory-dir");
        std::fs::create_dir(&outside).unwrap();
        let target = outside.join("fcvm-memory-example.bin");
        std::fs::rename(fake.memory(), &target).unwrap();
        std::os::unix::fs::symlink(&target, fake.memory()).unwrap();

        let (archive, _) = export_snapshot(&fake.host, TAG, Vec::new()).await.unwrap();
        let (manifest, files) = read_archive(&archive).unwrap();

        let memory = files
            .iter()
            .find(|file| file.name == Path::new("memory.bin"))
            .expect("the archive has no memory.bin");
        assert_reads_back(memory, &target);
        assert_eq!(manifest.memory.map(|memory| memory.len), Some(MEMORY_LEN));
    }

    /// A disk-only snapshot has no memory image: the manifest says so and the archive has
    /// no `memory.bin`.
    #[tokio::test]
    async fn a_disk_only_snapshot_exports_without_a_memory_image() {
        let fake = fake_snapshot_with(|config| config.kind = SnapshotKind::DiskOnly);
        std::fs::remove_file(fake.memory()).unwrap();
        std::fs::remove_file(fake.dir().join("vmstate.bin")).unwrap();

        let (archive, _) = export_snapshot(&fake.host, TAG, Vec::new()).await.unwrap();
        let (manifest, files) = read_archive(&archive).unwrap();

        assert_eq!(manifest.kind, SnapshotKind::DiskOnly);
        assert_eq!(manifest.memory, None);
        let names: Vec<&str> = files
            .iter()
            .map(|file| file.name.to_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "config.json",
                "disk/disk.raw",
                "memory.bin.working-set",
                "volume-5000-inode-table.json",
            ]
        );
    }

    #[tokio::test]
    async fn a_cloud_hypervisor_snapshot_is_refused() {
        let fake = fake_snapshot_with(|config| {
            config.metadata.hypervisor = Backend::CloudHypervisor;
        });
        let out = SharedBuf::default();

        let message = export_failure(&fake, out.clone()).await;

        assert!(
            message.contains("Cloud Hypervisor") && message.contains("Firecracker snapshots only"),
            "the refusal does not say why: {message}"
        );
        assert!(
            out.bytes().is_empty(),
            "the export wrote {} bytes before it refused",
            out.bytes().len()
        );
    }

    /// A snapshot can record no Firecracker binary (the one its VM ran was replaced before
    /// the snapshot was taken), and its manifest could not name one. The refusal comes after
    /// the memory image is marked in use and before it is read, so the image keeps the pages
    /// it had in the page cache.
    #[tokio::test]
    async fn a_snapshot_that_records_no_firecracker_is_refused() {
        let fake = fake_snapshot_with(|config| config.metadata.firecracker_bin = None);
        let out = SharedBuf::default();
        let memory = File::open(fake.memory()).unwrap();
        // SAFETY: the file is private to this test and is not written again.
        let mapped = unsafe { MmapOptions::new().len(MEMORY_LEN as usize).map(&memory) }.unwrap();
        std::fs::read(fake.memory()).unwrap();
        let cached = resident_pages(&mapped, 0, MEMORY_LEN);
        assert!(cached > 0, "control: a file that was just read is cached");

        let message = export_failure(&fake, out.clone()).await;

        assert!(
            message.contains("records no Firecracker binary"),
            "{message}"
        );
        assert!(out.bytes().is_empty());
        assert_eq!(
            resident_pages(&mapped, 0, MEMORY_LEN),
            cached,
            "an export that read nothing of the memory image dropped it from the page cache"
        );
        assert!(!in_use(&fake.memory()), "the refused export kept its mark");
    }

    /// A link in the directory that is not the memory image is refused, not left out.
    #[tokio::test]
    async fn an_entry_an_archive_cannot_carry_is_refused() {
        let fake = fake_snapshot();
        let stray = fake.dir().join("disk/stray-link");
        std::os::unix::fs::symlink("/example/elsewhere", &stray).unwrap();
        let out = SharedBuf::default();

        let message = export_failure(&fake, out.clone()).await;

        assert!(
            message.contains("stray-link")
                && message.contains("neither a regular file nor a directory"),
            "{message}"
        );
        assert!(out.bytes().is_empty());
    }

    /// The function that decides whether stdout can take an archive, on a real terminal (the
    /// slave side of a pty) and on a file.
    #[test]
    fn a_terminal_is_refused_as_the_archive_output() {
        let pty = nix::pty::openpty(None, None).unwrap();
        let error = archive_output(&pty.slave).unwrap_err();
        assert!(
            format!("{error:#}").contains("terminal"),
            "the refusal does not say why: {error:#}"
        );

        // Control: a file is accepted, and the result writes into that same file.
        let file = tempfile::tempfile().unwrap();
        let mut out = archive_output(&file).unwrap();
        out.write_all(b"archive").unwrap();
        assert_eq!(file.metadata().unwrap().len(), 7);
    }

    /// `config.json` is rewritten in place while the export is writing: the export fails,
    /// before `finish()`, so what it wrote has no trailer and no reader takes it for an archive.
    #[tokio::test]
    async fn a_config_that_changes_during_the_export_fails_it_and_leaves_no_archive() {
        let fake = fake_snapshot();
        let config_path = fake.dir().join(CONFIG_FILE);
        let out = SharedBuf::default();
        let mut changed = false;
        let hooked = Hooked {
            out: out.clone(),
            before: move |_: &[u8]| {
                if !changed {
                    changed = true;
                    // Still a config the loader accepts: only its bytes differ.
                    let mut config = std::fs::read(&config_path).unwrap();
                    config.push(b'\n');
                    std::fs::write(&config_path, config).unwrap();
                }
            },
        };

        let message = export_failure(&fake, hooked).await;

        assert!(
            message.contains("config.json changed while snapshot 'example-tag' was exported"),
            "the failure is not the export's own check of the config: {message}"
        );
        assert!(
            !out.bytes().is_empty(),
            "control: the export had written part of the archive when it failed"
        );
        let read = read_archive(&out.bytes());
        assert!(
            read.is_err(),
            "a reader accepted what the failed export wrote"
        );

        // Control: the same snapshot, left alone, exports and reads back.
        let (archive, _) = export_snapshot(&fake.host, TAG, Vec::new()).await.unwrap();
        read_archive(&archive).unwrap();
    }

    /// A create of the tag takes the tag's lock exclusively. While an export is writing it
    /// waits, and it gets the lock once the export has ended.
    #[tokio::test]
    async fn a_create_of_the_tag_waits_until_the_export_ends() {
        let fake = fake_snapshot();
        let out = SharedBuf::default();
        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let mut stopped = false;
        let hooked = Hooked {
            out: out.clone(),
            before: move |_: &[u8]| {
                if !stopped {
                    stopped = true;
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                }
            },
        };
        let host = fake.host();
        let export = tokio::spawn(async move { export_snapshot(&host, TAG, hooked).await });
        tokio::time::timeout(Duration::from_secs(60), entered_rx.recv())
            .await
            .expect("the export never wrote")
            .unwrap();

        let dir = fake.dir();
        let mut create = tokio::spawn(async move {
            crate::commands::common::acquire_snapshot_create_generation_locks(&dir, None).await
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(500), &mut create)
                .await
                .is_err(),
            "a create took the tag's lock while its export was still writing"
        );

        release_tx.send(()).unwrap();
        export.await.unwrap().unwrap();
        tokio::time::timeout(Duration::from_secs(30), create)
            .await
            .expect("the create did not get the lock after the export ended")
            .unwrap()
            .unwrap();
        read_archive(&out.bytes()).unwrap();
    }

    /// Whether a release pass would find the file at `path` in use: a reader's mark refuses
    /// the exclusive lock a pass takes.
    fn in_use(path: &Path) -> bool {
        let file = File::open(path).unwrap();
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => false,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => true,
            Err(error) => panic!("locking {}: {error}", path.display()),
        }
    }

    /// The memory image carries the in-use mark while its data is written, loses it once it
    /// is written, and is then gone from the page cache.
    #[tokio::test]
    async fn the_memory_image_is_in_use_while_it_is_written_and_leaves_the_page_cache_after() {
        let fake = fake_snapshot();
        let memory = fake.memory();
        // For mincore only: no page is touched through the mapping, so none is mapped.
        let file = File::open(&memory).unwrap();
        // SAFETY: the file is private to this test and is not written again.
        let mapped = unsafe { MmapOptions::new().len(MEMORY_LEN as usize).map(&file) }.unwrap();
        std::fs::read(&memory).unwrap();
        assert!(
            resident_pages(&mapped, 0, MEMORY_LEN) > 0,
            "control: a file that was just read is in the page cache"
        );
        assert!(!in_use(&memory), "control: nothing holds the memory image");

        // Each write of a full copy buffer is memory data: no other file here is that long.
        let probes = Arc::new(Mutex::new(Vec::new()));
        let hooked = Hooked {
            out: SharedBuf::default(),
            before: {
                let (probes, memory) = (probes.clone(), memory.clone());
                move |buf: &[u8]| probes.lock().unwrap().push((buf.len(), in_use(&memory)))
            },
        };
        export_snapshot(&fake.host, TAG, hooked).await.unwrap();

        let probes = probes.lock().unwrap().clone();
        let during: Vec<bool> = probes
            .iter()
            .filter(|(len, _)| *len == CHUNK)
            .map(|(_, held)| *held)
            .collect();
        assert_eq!(
            during,
            [true, true],
            "the memory image was not marked in use while its two full buffers were written"
        );
        assert_eq!(
            probes.last().map(|(_, held)| *held),
            Some(false),
            "the mark outlived the memory image's entry"
        );
        assert_eq!(
            resident_pages(&mapped, 0, MEMORY_LEN),
            0,
            "the exported memory image is still in the page cache"
        );
    }

    /// Another reader of the memory image (a memory server, a VM restored through the File
    /// backend) holds the mark: the export leaves the image in the page cache and the
    /// reader's mark where it was.
    #[tokio::test]
    async fn a_memory_image_another_reader_holds_stays_in_the_page_cache() {
        let fake = fake_snapshot();
        let memory = fake.memory();
        let reader = File::open(&memory).unwrap();
        crate::uffd::mark_in_use(&reader).await.unwrap();
        // SAFETY: the file is private to this test and is not written again.
        let mapped = unsafe { MmapOptions::new().len(MEMORY_LEN as usize).map(&reader) }.unwrap();

        export_snapshot(&fake.host, TAG, Vec::new()).await.unwrap();

        assert!(
            resident_pages(&mapped, 0, MEMORY_LEN) > 0,
            "the export dropped a memory image another reader holds in use"
        );
        assert!(in_use(&memory), "the other reader lost its mark");
    }

    /// The archive lands under the name it was asked for, whole, readable by its owner only,
    /// and replaces what had the name. Nothing else is left beside it or in the reclaim
    /// records.
    #[tokio::test]
    async fn an_export_to_a_file_installs_the_whole_archive_and_nothing_else() {
        let fake = fake_snapshot();
        let out_dir = fake.root.path().join("out");
        std::fs::create_dir(&out_dir).unwrap();
        let output = out_dir.join("snapshot.archive");
        std::fs::write(&output, b"an older archive").unwrap();

        export_to_file(&fake.host, TAG, &output).await.unwrap();

        let (_, files) = read_archive(&std::fs::read(&output).unwrap()).unwrap();
        assert_eq!(files.len(), 6);
        assert_eq!(
            std::fs::metadata(&output).unwrap().mode() & 0o777,
            0o600,
            "the archive holds the guest's memory"
        );
        assert_eq!(names_in(&out_dir), ["snapshot.archive"]);
        assert_eq!(fake.export_records(), Vec::<String>::new());
    }

    /// An export that fails after it created its temporary removes it, keeps the file that
    /// had the name, and leaves no reclaim record.
    #[tokio::test]
    async fn a_failed_export_to_a_file_keeps_the_old_file_and_no_temporary() {
        let fake = fake_snapshot_with(|config| {
            config.metadata.hypervisor = Backend::CloudHypervisor;
        });
        let out_dir = fake.root.path().join("out");
        std::fs::create_dir(&out_dir).unwrap();
        let output = out_dir.join("snapshot.archive");
        std::fs::write(&output, b"an older archive").unwrap();

        export_to_file(&fake.host, TAG, &output).await.unwrap_err();

        assert_eq!(std::fs::read(&output).unwrap(), b"an older archive");
        assert_eq!(names_in(&out_dir), ["snapshot.archive"]);
        assert_eq!(fake.export_records(), Vec::<String>::new());
    }

    /// The rename would replace a directory entry that is not a file: a link stays a link
    /// and its target is not written.
    #[tokio::test]
    async fn an_output_that_is_not_a_regular_file_is_refused() {
        let fake = fake_snapshot();
        let out_dir = fake.root.path().join("out");
        std::fs::create_dir(&out_dir).unwrap();
        let target = out_dir.join("target");
        std::fs::write(&target, b"kept").unwrap();
        let link = out_dir.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let error = export_to_file(&fake.host, TAG, &link).await.unwrap_err();

        assert!(
            format!("{error:#}").contains("not a regular file"),
            "{error:#}"
        );
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(std::fs::read(&target).unwrap(), b"kept");
        assert_eq!(names_in(&out_dir), ["link", "target"]);
    }

    /// `<x>.creating` and `<x>.old` are the directories a create of `<x>` works in, and it
    /// holds the lock of `<x>` for them, not theirs. With that create in flight (its lock
    /// held here) an export of either name is refused before it writes anything.
    #[tokio::test]
    async fn a_tag_that_names_a_creates_working_directory_is_refused() {
        for suffix in ["creating", "old"] {
            let tag = format!("{TAG}.{suffix}");
            let fake = fake_snapshot_tagged(&tag, |_| {});
            let _create = crate::commands::common::acquire_snapshot_create_generation_locks(
                &fake.host.snapshots_dir.join(TAG),
                None,
            )
            .await
            .unwrap();
            let out = SharedBuf::default();

            let message = export_failure(&fake, out.clone()).await;

            assert!(
                message.contains(&format!("refuses the tag '{tag}'"))
                    && message.contains(&format!("create of snapshot '{TAG}'")),
                "{suffix}: the refusal does not say why: {message}"
            );
            assert!(
                out.bytes().is_empty(),
                "{suffix}: the export wrote {} bytes before it refused",
                out.bytes().len()
            );
        }
    }

    /// The memory image is read from the path the config records and every other file from
    /// the directory. A config whose path is another file than the directory's `memory.bin`
    /// fails the export before it writes anything.
    #[tokio::test]
    async fn a_recorded_memory_path_that_is_not_the_directorys_memory_bin_fails_the_export() {
        let fake = fake_snapshot();
        let elsewhere = fake.root.path().join("another-memory.bin");
        sparse(&elsewhere, MEMORY_LEN, &[(0, pattern(9, PAGE))]);
        let config_path = fake.dir().join(CONFIG_FILE);
        let mut config: SnapshotConfig =
            serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
        config.memory_path = elsewhere.clone();
        std::fs::write(&config_path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();
        let out = SharedBuf::default();

        let message = export_failure(&fake, out.clone()).await;

        assert!(
            message.contains("another-memory.bin")
                && message.contains("which is not the file at")
                && message.contains("example-tag/memory.bin"),
            "the failure does not name the two files: {message}"
        );
        assert!(out.bytes().is_empty());
        assert!(!in_use(&elsewhere), "the failed export kept its mark");
    }

    /// The output fails while the memory image's headers are written, before any of its data
    /// is read: the image keeps the pages it had in the page cache.
    #[tokio::test]
    async fn a_failure_in_the_memory_images_headers_leaves_the_image_in_the_page_cache() {
        const MEMORY_NAME: &[u8] = b"GNU.sparse.name=snapshot/memory.bin\n";
        let fake = fake_snapshot();
        // One run that leaves 1024 bytes of the export's buffer free. The memory image's two
        // pax blocks fill them, and its file header is the write that empties the buffer
        // into the output.
        let disk = fake.dir().join("disk/disk.raw");
        std::fs::remove_file(&disk).unwrap();
        let disk_len = OUTPUT_BUFFER as u64 - 1024;
        sparse(&disk, disk_len, &[(0, pattern(8, disk_len))]);
        let memory = File::open(fake.memory()).unwrap();
        // SAFETY: the file is private to this test and is not written again.
        let mapped = unsafe { MmapOptions::new().len(MEMORY_LEN as usize).map(&memory) }.unwrap();
        std::fs::read(fake.memory()).unwrap();
        let cached = resident_pages(&mapped, 0, MEMORY_LEN);
        assert!(cached > 0, "control: a file that was just read is cached");

        let refused = Arc::new(Mutex::new(None));
        let out = Refusing {
            out: SharedBuf::default(),
            refuse: |buf: &[u8]| find(buf, MEMORY_NAME).is_some(),
            refused: refused.clone(),
        };
        let message = export_failure(&fake, out).await;

        assert!(message.contains("writing the archive"), "{message}");
        let refused = refused.lock().unwrap().take().expect(
            "control: the output refused no write, so the memory image's entry was not reached",
        );
        let name_at = find(&refused, MEMORY_NAME).unwrap();
        assert!(
            refused.len() - name_at <= 512,
            "control: the refused write holds {} bytes after the memory image's pax records, \
             so it came after the entry's file header and the image's data may have been read",
            refused.len() - name_at
        );
        assert_eq!(
            resident_pages(&mapped, 0, MEMORY_LEN),
            cached,
            "an export that read none of the memory image's data dropped it from the page cache"
        );
        assert!(!in_use(&fake.memory()), "the failed export kept its mark");
    }

    /// Every file but the memory image leaves the page cache once its entry is written: the
    /// disk, which sorts before the memory image, is out of it while the image's data is
    /// written, and the VM state after the export.
    #[tokio::test]
    async fn the_other_files_leave_the_page_cache_once_their_entries_are_written() {
        const DISK_LEN: u64 = 4 * MIB;
        const STATE_LEN: u64 = 5000;
        let fake = fake_snapshot();
        let disk_path = fake.dir().join("disk/disk.raw");
        let state_path = fake.dir().join("vmstate.bin");
        let (disk, state) = (
            File::open(&disk_path).unwrap(),
            File::open(&state_path).unwrap(),
        );
        // SAFETY: the files are private to this test and are not written again. The mappings
        // are for mincore only: no page is touched through them, so none is mapped.
        let disk = unsafe { MmapOptions::new().len(DISK_LEN as usize).map(&disk) }.unwrap();
        let state = unsafe { MmapOptions::new().len(STATE_LEN as usize).map(&state) }.unwrap();
        File::open(&state_path).unwrap().sync_all().unwrap();
        std::fs::read(&disk_path).unwrap();
        std::fs::read(&state_path).unwrap();
        assert!(
            resident_pages(&disk, 0, DISK_LEN) > 0 && resident_pages(&state, 0, STATE_LEN) > 0,
            "control: files that were just read are in the page cache"
        );

        // Each write of a full copy buffer is memory data: no other file here is that long.
        let during = Arc::new(Mutex::new(Vec::new()));
        let hooked = Hooked {
            out: SharedBuf::default(),
            before: {
                let during = during.clone();
                move |buf: &[u8]| {
                    if buf.len() == CHUNK {
                        during
                            .lock()
                            .unwrap()
                            .push(resident_pages(&disk, 0, DISK_LEN));
                    }
                }
            },
        };
        export_snapshot(&fake.host, TAG, hooked).await.unwrap();

        assert_eq!(
            *during.lock().unwrap(),
            [0, 0],
            "pages of the disk in the page cache while the memory image's two full buffers \
             were written"
        );
        assert_eq!(
            resident_pages(&state, 0, STATE_LEN),
            0,
            "the exported VM state is still in the page cache"
        );
    }

    /// The name was a regular file, or free, when the export began, and is a link by the
    /// time the archive is whole: the rename is refused, the link stays a link and the
    /// archive keeps its temporary name for the caller to remove.
    #[test]
    fn an_output_that_stops_being_a_regular_file_during_the_export_is_not_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let temp = dir.path().join(".snapshot.archive.example.tmp");
        std::fs::write(&temp, b"a whole archive").unwrap();
        let target = dir.path().join("target");
        std::fs::write(&target, b"kept").unwrap();
        let output = dir.path().join("snapshot.archive");
        std::os::unix::fs::symlink(&target, &output).unwrap();

        let error = publish_archive(&temp, &output, |_| Ok(())).unwrap_err();

        assert!(
            format!("{error:#}").contains("not a regular file"),
            "{error:#}"
        );
        assert!(std::fs::symlink_metadata(&output)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(std::fs::read(&target).unwrap(), b"kept");
        assert_eq!(std::fs::read(&temp).unwrap(), b"a whole archive");

        // Control: with a regular file under the name, the same call replaces it.
        std::fs::remove_file(&output).unwrap();
        std::fs::write(&output, b"an older archive").unwrap();
        publish_archive(&temp, &output, |_| Ok(())).unwrap();
        assert_eq!(std::fs::read(&output).unwrap(), b"a whole archive");
        assert!(!temp.exists());
    }

    /// The directory's sync fails after the rename: the export fails, and its error says
    /// that the archive is under its name, because the file that had the name is gone.
    #[test]
    fn a_failed_directory_sync_says_the_archive_is_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let temp = dir.path().join(".snapshot.archive.example.tmp");
        std::fs::write(&temp, b"a whole archive").unwrap();
        let output = dir.path().join("snapshot.archive");
        std::fs::write(&output, b"an older archive").unwrap();
        let mut synced = None;

        let error = publish_archive(&temp, &output, |parent| {
            synced = Some(parent.to_path_buf());
            Err(std::io::Error::from_raw_os_error(libc::EIO))
        })
        .unwrap_err();

        assert_eq!(synced.as_deref(), Some(dir.path()));
        let message = format!("{error:#}");
        assert!(
            message.contains(&format!("the archive is in place at {}", output.display()))
                && message.contains("may not survive a crash"),
            "the failure does not say where the archive is: {message}"
        );
        assert_eq!(std::fs::read(&output).unwrap(), b"a whole archive");
        assert!(!temp.exists());
    }

    /// One sentence, with the path, for each thing a restore reads outside the archive.
    #[test]
    fn the_manifest_names_what_a_restore_reads_outside_the_archive() {
        let mut config = fake_config(
            Path::new("/example/snapshots/example-tag"),
            Path::new("/example/firecracker"),
        );
        assert_eq!(not_shipped(&config), Vec::<String>::new());

        let metadata = &mut config.metadata;
        metadata.image_disk_path = Some(PathBuf::from("/example/image-cache/layers.img"));
        metadata.pmem_devices = vec![crate::state::types::PmemDevice {
            path: "/example/data/pmem/0123.img".to_string(),
            source: "/example/models/model.img".to_string(),
            mount_path: "/models".to_string(),
            identity: "1:2:3".to_string(),
        }];
        metadata.volumes = vec![crate::storage::SnapshotVolumeConfig {
            host_path: PathBuf::from("/example/host/data"),
            guest_path: "/data".to_string(),
            read_only: false,
            vsock_port: 5000,
            portable: false,
        }];
        metadata.nfs_shares = vec![crate::state::types::NfsShare {
            host_path: "/example/host/share".to_string(),
            mount_path: "/share".to_string(),
            read_only: true,
        }];
        metadata.firmware = Some(PathBuf::from("/example/firmware/OVMF.fd"));

        assert_eq!(
            not_shipped(&config),
            [
                "The image disk /example/image-cache/layers.img holds the container image and \
                 is attached read-only at this path.",
                "The pmem image /example/data/pmem/0123.img (a copy of \
                 /example/models/model.img) is mapped at /models and reopened at this path.",
                "The directory /example/host/data is mapped into the guest at /data.",
                "The directory /example/host/share is exported over NFS and mounted in the \
                 guest at /share.",
                "The UEFI firmware /example/firmware/OVMF.fd is booted again when the guest \
                 restarts.",
            ]
        );
    }

    /// The manifest's pmem owner is the uid that owns the snapshot's pmem entries, and an
    /// entry that is gone fails the export instead of leaving the owner unsaid.
    #[test]
    fn the_pmem_owner_is_the_owner_of_the_snapshots_entries() {
        let dir = tempfile::tempdir().unwrap();
        let entry = dir.path().join("entry.img");
        std::fs::write(&entry, b"image").unwrap();
        let device = |path: &Path| crate::state::types::PmemDevice {
            path: path.to_string_lossy().into_owned(),
            source: "/example/models/model.img".to_string(),
            mount_path: "/models".to_string(),
            identity: "1:2:3".to_string(),
        };
        let mut config = fake_config(
            Path::new("/example/snapshots/example-tag"),
            Path::new("/example/firecracker"),
        );
        assert_eq!(pmem_owner_uid(&config).unwrap(), None);

        config.metadata.pmem_devices = vec![device(&entry)];
        assert_eq!(
            pmem_owner_uid(&config).unwrap(),
            Some(nix::unistd::geteuid().as_raw())
        );

        config.metadata.pmem_devices = vec![device(&dir.path().join("gone.img"))];
        let error = pmem_owner_uid(&config).unwrap_err();
        assert!(format!("{error:#}").contains("gone.img"), "{error:#}");
    }
}

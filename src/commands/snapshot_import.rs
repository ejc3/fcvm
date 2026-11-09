//! `fcvm snapshot import`: install the snapshot that an archive from `snapshot export`
//! carries (`storage::snapshot_archive`), read from a file or from stdin.
//!
//! The archive holds the snapshot's directory, and the import installs it under the tag it
//! was exported from. It also holds the image disk the snapshot attaches and the pmem
//! store entries it maps (`manifest.externals`), and the import installs each one this host
//! lacks at the path the snapshot recorded, where a restore opens it.
//!
//! # What is refused before anything is written
//!
//! The import reads the manifest and the archived `config.json`, which is the first file of
//! every archive, and refuses the archive unless all of this holds:
//!
//! - the archive is this fcvm's archive version, and the snapshot in it has the versions
//!   this fcvm restores (the config goes through the loader's own parser);
//! - it was taken on this architecture, with Firecracker;
//! - it was taken under this host's data directory. The VM state embeds paths under it,
//!   and the config's own paths have to name the directory the import installs;
//! - the Firecracker binary it was taken on is at the recorded path here, with the same
//!   SHA-256. A restore would otherwise fall back to another binary;
//! - its kernel profile is configured here, without which a restore is refused;
//! - `config.json` has the SHA-256 the manifest gives, and the two agree on the tag, the
//!   kind, the snapshot versions, the hypervisor and the Firecracker binary;
//! - the manifest lists the external files the config names: the image disk and each pmem
//!   store entry, with the digest the config binds each one's bytes to;
//! - the image disk's recorded path is one this host's image cache gives the disk of an
//!   image, whether this host has the file or not: directly under the cache, with the name
//!   a launch gives such a disk. A launch reuses a file of that name by its existence, so
//!   the name an import installs under decides which image gets the disk. A storage image
//!   comes with the identity the snapshot recorded, `sha256:` and 64 lowercase hex digits;
//! - an image disk this host has at that path is a regular file and, where the snapshot
//!   recorded an identity, the build it recorded (`verify_image_disk_identity`, the check
//!   a restore makes);
//! - a pmem entry is named in this host's store. One this host has is an entry this run
//!   can use; for one it lacks, the entries belong to the uid this import publishes for.
//!   An entry's name is a digest of that uid and of the bytes, so another uid's entries
//!   would not get the paths the snapshot recorded, and Firecracker reopens those paths;
//! - every extra disk is named by one path component;
//! - the tag does not exist here.
//!
//! None of this is verified yet: the reader verifies nothing until it has read the trailer.
//! The checks decide whether to start, and the trailer decides whether to publish.
//!
//! The checks do not make an archive from an untrusted source safe: an archive is trusted
//! as a snapshot made on this host is (DESIGN.md, `fcvm snapshot import`).
//!
//! # How it installs
//!
//! As `snapshot create` does. The import takes the tag's lock exclusively, records the
//! staging directory `<tag>.creating` for reclaim before it makes it, and makes it and its
//! `disk/` as btrfs subvolumes where the store can. Each file is written at the offsets of
//! its data runs, so its holes stay holes. The memory image is a file in the directory
//! whatever it was on the exporting host: `snapshot create --memory-dir` leaves
//! `memory.bin` as a link to a file elsewhere, an export archives the file, and an import
//! never makes the link again.
//!
//! The working-set sidecar (`memory.bin.working-set`) is dropped. Its key is the digest of
//! the exporting host's `config.json` and the memory image's modification time there, and
//! the imported snapshot has neither, so a memory server would read it as another
//! snapshot's and ignore it.
//!
//! Once the reader has verified the trailer, the staged directory is checked for the files
//! a restore opens (`check_generation_files`, as `podman prepare` checks a generation; the
//! config is not among them and is not written yet), the external files are published,
//! `config.json` is written, and the directory is synced and renamed to `<tag>`. The config
//! is the archived one with a new generation id, no parent snapshot and each pmem device's
//! identity as its entry has it on this host, and every other field as it was. That rename is
//! the only step that makes the tag exist, and it replaces nothing: it is made with
//! `RENAME_NOREPLACE`, which the kernel refuses when anything has the name. A cut stream, a
//! digest that does not match, or any other failure removes the staging directory. A killed
//! import leaves it to the next import or the next VM launch, both of which reclaim first.
//!
//! # External files
//!
//! A file this host has is never written: the archived copy is read, verified with the rest
//! of the archive, and dropped, because the stream cannot skip it. A file this host lacks
//! is staged where its store stages its own, and published only after the trailer has
//! verified and the staged directory has every file a restore opens:
//!
//! - The image disk goes into a temporary beside its path in the image cache
//!   (`setup::StoreTemp`), recorded for reclaim before the file exists. Where the snapshot
//!   recorded an identity, the SHA-256 computed from the bytes as they are written has to
//!   be that identity. A Docker archive disk has no recorded identity, and is installed as
//!   the archive carries it, checked by the archive's trailer only. The disk is synced,
//!   and then published with a hard link, which fails where a file has the name, under the
//!   lock of its image in the image cache: a launch holds that lock from its look for the
//!   image's disk through the rename that publishes the one it builds, so the import waits
//!   for a build in flight on this kernel and then finds the build's disk under the name
//!   (the lock is a flock, which a FUSE map does not carry to another VM's kernel). A disk
//!   this host had all along is checked again under the same lock. The digest computed
//!   while writing is recorded for the published file (`remember_image_disk_digest`), so
//!   the first restore does not read the disk to check it.
//! - A pmem entry goes through the store's stream ingest
//!   (`PmemStore::begin_stream_ingest`): a temp under the store's `tmp/` with the store's
//!   lock held, the store's digest computed as the bytes arrive, and the store's own
//!   publication. The name those bytes get for this run's uid has to be the recorded path.
//!
//! A failure before publication removes every temporary and its reclaim record; the
//! directories the import made (the image cache, the pmem store) and lock files stay. A
//! killed import leaves no temporary for good: the next import or VM launch removes them
//! (`reclaim_abandoned`, which also collects the pmem store's temps). A file that another
//! process published under the same name meanwhile is kept, and has to be what the
//! snapshot recorded. An external that is published stays if a later step fails or the
//! import is killed: it is a whole, verified entry of its store, as one a `podman run`
//! leaves.

use std::fs::File;
use std::io::{BufReader, IsTerminal, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{bail, ensure, Context, Result};
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use super::common::{
    acquire_snapshot_dir_lock, create_memory_file_nodatacow, record_snapshot_staging_in,
    snapshot_sibling,
};
use super::snapshot_export::{not_shipped, recorded_externals, sha256_hex, CONFIG_FILE};
use crate::cli::SnapshotImportArgs;
use crate::hypervisor::Backend;
use crate::paths;
use crate::setup::StoreTemp;
use crate::storage::disk::{clear_staging, create_dir_or_subvolume, remove_snapshot_dir};
use crate::storage::pmem_store::{PmemStore, StagedEntry};
use crate::storage::snapshot::{
    check_extra_disk_names, check_generation_files, parse_snapshot_config, snapshot_disk_dir,
    SNAPSHOT_DISK_FILE, SNAPSHOT_MEMORY_FILE, SNAPSHOT_VMSTATE_FILE,
};
use crate::storage::snapshot_archive::{
    ArchiveEntry, ArchiveManifest, ArchiveReader, ContentSha256, EntryName, ExternalRole,
    ManifestExternal,
};
use crate::storage::{validate_snapshot_name, SnapshotConfig, SnapshotKind};

/// The buffer between the input and the archive reader, for the headers, the maps and the
/// two JSON entries, which the reader takes 512 bytes or less at a time. It is much smaller
/// than the pieces the reader takes a file's data in (`snapshot_archive::CHUNK`): a
/// `BufReader` hands a read straight to its input only when the read is at least its own
/// size and it holds nothing, and a piece that follows a header finds a remainder in the
/// buffer. With a buffer of the piece's size every piece would be copied through it.
const INPUT_BUFFER: usize = 64 << 10;

/// The largest `config.json` an import reads. It is held in memory, and a real one is a few
/// KiB.
const MAX_CONFIG_BYTES: u64 = 16 << 20;

/// What the importing host is, for the checks an archive has to pass.
#[derive(Clone)]
pub(crate) struct ImportHost {
    pub snapshots_dir: PathBuf,
    /// Compared with the manifest's: the VM state embeds paths under it.
    pub data_dir: PathBuf,
    /// Where this host keeps image disks, and the one directory an import installs an image
    /// disk into.
    pub image_cache_dir: PathBuf,
    /// Whether the kernel profile of this name is configured for this architecture.
    pub kernel_profile_configured: fn(&str) -> Result<bool>,
}

impl ImportHost {
    fn configured() -> Self {
        Self {
            snapshots_dir: paths::snapshot_dir(),
            data_dir: paths::data_dir(),
            image_cache_dir: paths::image_cache_dir(),
            kernel_profile_configured: |name| Ok(crate::setup::get_kernel_profile(name)?.is_some()),
        }
    }
}

/// What an import installed.
#[derive(Debug)]
pub(crate) struct Imported {
    pub tag: String,
    /// One per file installed, `config.json` first, as the archive describes it.
    pub files: Vec<ArchiveEntry>,
    /// One per external file of the archive, in its order.
    pub externals: Vec<ImportedExternal>,
    /// What a restore of the installed snapshot reads that the archive did not carry, one
    /// sentence each, from the installed config.
    pub not_shipped: Vec<String>,
}

/// What an import did with one external file of its archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImportedExternal {
    pub role: ExternalRole,
    pub path: PathBuf,
    pub len: u64,
    /// Whether this import put the file there. `false` for a file the host already had,
    /// which is kept as it was.
    pub installed: bool,
}

/// Import a snapshot (`fcvm snapshot import`).
pub async fn cmd_snapshot_import(args: SnapshotImportArgs) -> Result<()> {
    // A killed import's staging directory is as large as its snapshot, and this import
    // needs the room.
    crate::reclaim::reclaim_abandoned().await;
    let host = ImportHost::configured();
    let started = Instant::now();
    let input = match &args.input {
        Some(path) => {
            File::open(path).with_context(|| format!("opening --input {}", path.display()))?
        }
        None => archive_input(&std::io::stdin())?,
    };
    let imported = import_snapshot(&host, input).await?;
    info!(
        snapshot = %imported.tag,
        files = imported.files.len(),
        bytes = imported.files.iter().map(|file| file.len).sum::<u64>(),
        stored_bytes = imported.files.iter().map(|file| file.data_len).sum::<u64>(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "imported snapshot"
    );
    for external in &imported.externals {
        if external.installed {
            info!(
                path = %external.path.display(),
                bytes = external.len,
                "installed the snapshot's {}",
                external.role
            );
        } else {
            info!(
                path = %external.path.display(),
                "this host already had the snapshot's {}; it is kept as it was",
                external.role
            );
        }
    }
    for sentence in &imported.not_shipped {
        warn!(
            snapshot = %imported.tag,
            "the archive does not carry something a restore of the snapshot reads on this \
             host: {sentence}"
        );
    }
    Ok(())
}

/// The file an archive is read from when it comes on `stdin`: the same open file, without
/// the lock and the small buffer of `std::io::Stdin`. A terminal is refused.
fn archive_input(stdin: &(impl IsTerminal + AsFd)) -> Result<File> {
    ensure!(
        !stdin.is_terminal(),
        "refusing to read a snapshot archive from a terminal: redirect stdin or pass --input \
         <FILE>"
    );
    let fd = stdin
        .as_fd()
        .try_clone_to_owned()
        .context("duplicating stdin")?;
    Ok(File::from(fd))
}

/// Install the snapshot in the archive read from `input`, under the tag it was exported
/// from. `input` is read through a buffer of [`INPUT_BUFFER`] bytes.
///
/// On an error the tag is not installed and the staging directory is removed, unless the
/// error says the snapshot is installed. External files the import published before the
/// error stay: each is a whole, verified entry of its store.
pub(crate) async fn import_snapshot<R>(host: &ImportHost, input: R) -> Result<Imported>
where
    R: Read + Send + 'static,
{
    let (reader, archive) = check_archive(host, input).await?;
    install(host, reader, archive).await
}

/// Everything that can refuse the archive before anything is written ([`read_and_check`]):
/// the manifest and the archived config against this host, and the tag, which must not
/// exist. The reader comes back positioned after the config.
async fn check_archive<R>(
    host: &ImportHost,
    input: R,
) -> Result<(ArchiveReader<BufReader<R>>, CheckedArchive)>
where
    R: Read + Send + 'static,
{
    let host = host.clone();
    tokio::task::spawn_blocking(move || read_and_check(&host, input))
        .await
        .context("the task that checks the archive panicked")?
}

/// Install a checked archive: take the tag's lock, stage the files, and publish them once
/// the archive has verified.
async fn install<R>(
    host: &ImportHost,
    reader: ArchiveReader<BufReader<R>>,
    archive: CheckedArchive,
) -> Result<Imported>
where
    R: Read + Send + 'static,
{
    let tag = archive.manifest.tag.clone();
    let dir = host.snapshots_dir.join(&tag);
    tokio::fs::create_dir_all(&host.snapshots_dir)
        .await
        .context("creating snapshot directory")?;
    // Exclusive, as a create holds it: a restore takes the lock shared before it reads the
    // tag, and a reclaim pass takes it before it removes a staging directory. The task that
    // writes the files keeps a reference, so the lock outlives a caller that stops waiting.
    let generation_lock = Arc::new(acquire_snapshot_dir_lock(&dir, true).await?);
    // Again, under the lock, which is the check that counts: a create or an import that held
    // the lock has installed the tag by now.
    ensure_tag_is_free(&dir, &tag)?;

    let owners = crate::reclaim::owner_dir_in(&host.data_dir)?;
    let staging = snapshot_sibling(&dir, "creating");
    // Dropped before the lock, and after the staging directory is renamed or removed.
    let _staging = record_snapshot_staging_in(&owners, &dir)?;
    clear_staging(&staging).await?;

    let installed = stage_and_publish(
        host,
        &dir,
        &staging,
        reader,
        archive,
        generation_lock.clone(),
    )
    .await;
    if installed.is_err() {
        if let Err(error) = remove_snapshot_dir(&staging).await {
            warn!(
                staging = %staging.display(),
                %error,
                "could not remove the failed import's staging directory; the next VM launch \
                 removes it"
            );
        }
    }
    installed
}

/// Fail if anything has the name of the tag's directory.
fn ensure_tag_is_free(dir: &Path, tag: &str) -> Result<()> {
    match std::fs::symlink_metadata(dir) {
        Ok(_) => bail!(
            "snapshot '{tag}' exists on this host ({}); delete it to import the archive",
            dir.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("reading {}", dir.display())),
    }
}

/// Stage the archive's files into `staging`, and rename it to `dir` once the archive has
/// verified and the files are checked and synced.
async fn stage_and_publish<R>(
    host: &ImportHost,
    dir: &Path,
    staging: &Path,
    reader: ArchiveReader<BufReader<R>>,
    archive: CheckedArchive,
    generation_lock: Arc<File>,
) -> Result<Imported>
where
    R: Read + Send + 'static,
{
    // What a create makes: the directory and its disk directory are subvolumes where the
    // store can make them, so the snapshot is deleted and its disks are cloned as a
    // created one's are.
    create_dir_or_subvolume(staging)
        .await
        .context("creating the staging directory")?;
    create_dir_or_subvolume(&snapshot_disk_dir(staging))
        .await
        .context("creating the staged snapshot's disk directory")?;

    let tag = archive.manifest.tag.clone();
    let config = archive.config.clone();
    let Staged { files, externals } = {
        let (staging, generation_lock) = (staging.to_path_buf(), generation_lock.clone());
        let host = host.clone();
        tokio::task::spawn_blocking(move || {
            // Held until the last file is written, or the write has failed.
            let _generation_lock = generation_lock;
            stage_files(reader, &staging, &archive, &host)
        })
        .await
        .context("the task that stages the snapshot panicked")??
    };

    failpoint::hit_async("snapshot.import.pre_publish").await;
    let (root, staging, dir) = (
        host.snapshots_dir.clone(),
        staging.to_path_buf(),
        dir.to_path_buf(),
    );
    {
        let tag = tag.clone();
        tokio::task::spawn_blocking(move || {
            let _generation_lock = generation_lock;
            publish(&root, &staging, &dir, &tag)
        })
        .await
        .context("the task that publishes the snapshot panicked")??;
    }
    Ok(Imported {
        tag,
        files,
        externals,
        not_shipped: not_shipped(&config),
    })
}

/// Rename the staged directory `staging` to `dir`, both directly under `root`, and sync
/// `root`. The rename replaces nothing: the tag did not exist when its lock was taken, and
/// whatever has the name since was made without the lock and is not this import's to
/// remove. `atomic_replace_dir`, which a create publishes with, would move it aside and
/// delete it.
fn publish(root: &Path, staging: &Path, dir: &Path, tag: &str) -> Result<()> {
    let root_dir = File::open(root).with_context(|| format!("opening {}", root.display()))?;
    let name = |path: &Path| {
        path.file_name()
            .map(std::ffi::OsStr::to_os_string)
            .with_context(|| format!("{} has no name", path.display()))
    };
    match nix::fcntl::renameat2(
        &root_dir,
        name(staging)?.as_os_str(),
        &root_dir,
        name(dir)?.as_os_str(),
        nix::fcntl::RenameFlags::RENAME_NOREPLACE,
    ) {
        Ok(()) => {}
        Err(nix::errno::Errno::EEXIST) => bail!(
            "snapshot '{tag}' appeared on this host while its archive was imported ({}); it \
             is kept as it is, and the import is dropped",
            dir.display()
        ),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "renaming {} to {} without replacing (RENAME_NOREPLACE)",
                    staging.display(),
                    dir.display()
                )
            })
        }
    }
    root_dir.sync_all().with_context(|| {
        format!(
            "the snapshot is installed, but syncing {} failed",
            root.display()
        )
    })
}

/// An archive whose manifest and config passed every check an import makes before it
/// writes. Nothing in it is verified against the trailer yet.
struct CheckedArchive {
    manifest: ArchiveManifest,
    /// The archived `config.json`, parsed.
    config: SnapshotConfig,
    /// Its bytes.
    config_json: Vec<u8>,
    /// The entry the config was read from.
    config_entry: ArchiveEntry,
    /// What to do with each external file, in the manifest's order.
    externals: Vec<ExternalPlan>,
}

/// What an import does with one external file of its archive.
struct ExternalPlan {
    described: ManifestExternal,
    /// Whether this host lacked the file when the archive was checked, so the archived copy
    /// is installed. A file the host has is kept as it is, and the archived copy is read and
    /// not written.
    install: bool,
}

/// Read the manifest and the archived `config.json` from `input` and check them against
/// this host. The reader comes back positioned after the config.
fn read_and_check<R: Read>(
    host: &ImportHost,
    input: R,
) -> Result<(ArchiveReader<BufReader<R>>, CheckedArchive)> {
    // Another archive version is refused here, by its manifest.
    let mut reader = ArchiveReader::new(BufReader::with_capacity(INPUT_BUFFER, input))?;
    let manifest = reader.manifest().clone();
    check_manifest(host, &manifest)?;
    // Before the lock is asked for: a tag that exists is refused without a wait behind
    // whatever holds its lock, and without so much as its lock file being opened.
    ensure_tag_is_free(&host.snapshots_dir.join(&manifest.tag), &manifest.tag)?;
    let (config_entry, config_json) = read_config_entry(&mut reader)?;
    let config = check_config(host, &manifest, &config_json)?;
    let externals = plan_externals(host, &manifest, &config)?;
    Ok((
        reader,
        CheckedArchive {
            manifest,
            config,
            config_json,
            config_entry,
            externals,
        },
    ))
}

/// What the manifest alone decides: the tag is a name, and the snapshot was taken on this
/// architecture, with Firecracker, under this data directory, on a Firecracker binary this
/// host has.
fn check_manifest(host: &ImportHost, manifest: &ArchiveManifest) -> Result<()> {
    // The tag becomes a path under the snapshot directory.
    validate_snapshot_name(&manifest.tag).context("the archive names its snapshot")?;
    ensure!(
        manifest.arch == std::env::consts::ARCH,
        "the archive holds a snapshot taken on {} and this host is {}; a snapshot restores \
         only on the architecture it was taken on",
        manifest.arch,
        std::env::consts::ARCH
    );
    ensure!(
        manifest.hypervisor == Backend::Firecracker,
        "the archive holds a snapshot taken with {}; snapshot import installs Firecracker \
         snapshots only",
        manifest.hypervisor
    );
    ensure!(
        manifest.data_dir == host.data_dir,
        "the snapshot was taken under the data directory {} and this host's is {}; its VM \
         state embeds paths under the first, so it restores only where the data directory \
         has the same path",
        manifest.data_dir.display(),
        host.data_dir.display()
    );

    let firecracker = &manifest.firecracker;
    let version = firecracker.version_output.trim();
    ensure!(
        firecracker.path.is_file(),
        "the snapshot was taken on the Firecracker binary {} ({version}), which this host \
         does not have; a restore on another binary can refuse the snapshot or misread it",
        firecracker.path.display()
    );
    let found = sha256_hex(&firecracker.path)?;
    ensure!(
        found == firecracker.sha256,
        "the Firecracker binary {} on this host is not the one the snapshot was taken on: \
         its SHA-256 is {found} and the archive records {} ({version})",
        firecracker.path.display(),
        firecracker.sha256
    );
    Ok(())
}

/// Read the archive's first file, which has to be `config.json`, whole.
fn read_config_entry<R: Read>(reader: &mut ArchiveReader<R>) -> Result<(ArchiveEntry, Vec<u8>)> {
    let entry = reader.next_entry()?.context("the archive holds no files")?;
    ensure!(
        entry.name == EntryName::Snapshot(PathBuf::from(CONFIG_FILE)),
        "the archive's first file is {:?} and an export writes {CONFIG_FILE} first, so the \
         snapshot cannot be checked before it is written",
        entry.name
    );
    ensure!(
        entry.len <= MAX_CONFIG_BYTES,
        "the archive's {CONFIG_FILE} is {} bytes long (the limit is {MAX_CONFIG_BYTES})",
        entry.len
    );
    let mut json = vec![0u8; usize::try_from(entry.len)?];
    while let Some((offset, bytes)) = reader.next_run()? {
        let start = usize::try_from(offset)?;
        json[start..start + bytes.len()].copy_from_slice(bytes);
    }
    Ok((entry, json))
}

/// Check the archived `config.json` against the manifest and this host, and parse it.
fn check_config(
    host: &ImportHost,
    manifest: &ArchiveManifest,
    config_json: &[u8],
) -> Result<SnapshotConfig> {
    let found = hex::encode(Sha256::digest(config_json));
    ensure!(
        found == manifest.config_sha256,
        "the archive's {CONFIG_FILE} is not the one its manifest describes: its SHA-256 is \
         {found} and the manifest records {}",
        manifest.config_sha256
    );
    // The loader's parser: the snapshot versions this fcvm restores, and nothing else.
    let config = parse_snapshot_config(&manifest.tag, config_json)?;

    let disagreements = [
        ("the tag", config.name != manifest.tag),
        ("the snapshot kind", config.kind != manifest.kind),
        (
            "the snapshot versions",
            config.network_boundary_version != manifest.network_boundary_version
                || config.container_start_version != manifest.container_start_version,
        ),
        (
            "the hypervisor",
            config.metadata.hypervisor != manifest.hypervisor,
        ),
        (
            "the Firecracker binary",
            config.metadata.firecracker_bin.as_deref() != Some(manifest.firecracker.path.as_path()),
        ),
        (
            "the memory image",
            manifest.memory.is_some() != (config.kind == SnapshotKind::Full),
        ),
        (
            "the image disk and the pmem images the archive carries",
            !manifest_lists_recorded_externals(manifest, &config)?,
        ),
    ];
    if let Some((what, _)) = disagreements.iter().find(|(_, differs)| *differs) {
        bail!(
            "the archive's manifest and its {CONFIG_FILE} disagree about {what}, so the \
             checks made on the manifest say nothing about the snapshot"
        );
    }

    // The config keeps its paths, so they have to name the directory this import installs.
    let dir = host.snapshots_dir.join(&manifest.tag);
    for (field, recorded, here) in [
        (
            "memory_path",
            &config.memory_path,
            dir.join(SNAPSHOT_MEMORY_FILE),
        ),
        (
            "vmstate_path",
            &config.vmstate_path,
            dir.join(SNAPSHOT_VMSTATE_FILE),
        ),
        (
            "disk_path",
            &config.disk_path,
            snapshot_disk_dir(&dir).join(SNAPSHOT_DISK_FILE),
        ),
    ] {
        ensure!(
            *recorded == here,
            "the archived {CONFIG_FILE} records {field} {} and this host keeps that file at \
             {}, so a restore here would read another path",
            recorded.display(),
            here.display()
        );
    }

    // An extra disk is opened by its recorded name under the snapshot's disk directory.
    check_extra_disk_names(&config, "the archived snapshot")?;

    let profile = config
        .metadata
        .kernel_profile
        .as_deref()
        .unwrap_or("default");
    ensure!(
        (host.kernel_profile_configured)(profile)?,
        "the snapshot's kernel profile '{profile}' is not configured on this host, and a \
         restore of the snapshot would be refused for that"
    );

    Ok(config)
}

/// Whether the manifest lists exactly the external files the config names
/// ([`recorded_externals`]), in that order, each with the digest the config binds its bytes
/// to, and a pmem entry with the length the config records for it. The length of an image
/// disk is recorded nowhere but in the manifest.
fn manifest_lists_recorded_externals(
    manifest: &ArchiveManifest,
    config: &SnapshotConfig,
) -> Result<bool> {
    let recorded = recorded_externals(&manifest.tag, config)
        .context("reading what the archived snapshot names outside its directory")?;
    if recorded.len() != manifest.externals.len() {
        return Ok(false);
    }
    for (listed, (role, path, sha256)) in manifest.externals.iter().zip(recorded) {
        if listed.role != role || listed.path != path || listed.sha256 != sha256 {
            return Ok(false);
        }
        if role == ExternalRole::Pmem {
            // A device's identity is inode:length:mtime (`storage::pmem::entry_identity`).
            let lengths_agree = config
                .metadata
                .pmem_devices
                .iter()
                .filter(|device| Path::new(&device.path) == path)
                .all(|device| {
                    device.identity.split(':').nth(1) == Some(listed.len.to_string().as_str())
                });
            if !lengths_agree {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

/// Decide, for each external file the manifest lists, whether this host has it or the
/// import installs it, and refuse the archive where neither can be done. The manifest
/// agrees with the config by now ([`manifest_lists_recorded_externals`]). Nothing is
/// written here, and the pmem store is not created.
///
/// Last of the checks, because an image disk this host has is read whole (once per build of
/// the file; `image_disk_digest` remembers it).
fn plan_externals(
    host: &ImportHost,
    manifest: &ArchiveManifest,
    config: &SnapshotConfig,
) -> Result<Vec<ExternalPlan>> {
    let mut plans = Vec::with_capacity(manifest.externals.len());
    for described in &manifest.externals {
        let install = match described.role {
            ExternalRole::ImageDisk => plan_image_disk(host, described)?,
            ExternalRole::Pmem => plan_pmem_entry(host, manifest, config, described)?,
        };
        plans.push(ExternalPlan {
            described: described.clone(),
            install,
        });
    }
    Ok(plans)
}

/// The key of the image in the recorded path of an image disk, which names the image's
/// lock in the image cache. The path comes from the archive, so it is held against this
/// host's image cache by the rules an export holds it against its own
/// (`snapshot_export::image_of_disk`): directly under the cache, with the name a launch
/// gives such a disk, and a storage image with the identity the snapshot recorded.
fn image_of_disk<'a>(host: &ImportHost, described: &'a ManifestExternal) -> Result<&'a str> {
    super::snapshot_export::image_of_disk(
        &host.image_cache_dir,
        &described.path,
        described.sha256.is_some(),
    )
}

/// Whether the image disk has to be installed. Its path has to be one this host's image
/// cache gives an image disk ([`image_of_disk`]). One this host has there is kept, and has
/// to be a regular file and, where the snapshot recorded an identity, the build it
/// recorded, which is the check a restore makes. One it lacks is installed.
///
/// This decides whether to start. The check the installed snapshot relies on is made
/// again when the disk is published, under the image's lock ([`settle_image_disk`]).
fn plan_image_disk(host: &ImportHost, described: &ManifestExternal) -> Result<bool> {
    let disk = &described.path;
    image_of_disk(host, described)?;
    match std::fs::symlink_metadata(disk) {
        Ok(found) => {
            check_present_image_disk(
                described,
                &found,
                "this host has another build of the image disk than the one the snapshot was \
                 taken against",
            )?;
            Ok(false)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error)
            .with_context(|| format!("reading the snapshot's image disk {}", disk.display())),
    }
}

/// An image disk that is at its recorded path, with the metadata `found` (not followed
/// through a link), has to be a regular file and, where the snapshot recorded an identity,
/// the build it recorded. `another_build` says how that other build came to be there.
fn check_present_image_disk(
    described: &ManifestExternal,
    found: &std::fs::Metadata,
    another_build: &str,
) -> Result<()> {
    let disk = &described.path;
    ensure!(
        found.is_file(),
        "the snapshot reads the image disk {}, which on this host is not a regular file",
        disk.display()
    );
    if let Some(sha256) = &described.sha256 {
        crate::utils::verify_image_disk_identity(disk, &format!("sha256:{sha256}")).with_context(
            || {
                format!(
                    "{another_build} ({}), and an import does not replace a file",
                    disk.display()
                )
            },
        )?;
    }
    Ok(())
}

/// Whether a pmem store entry has to be installed. The recorded path has to name an entry
/// of this host's store. One this host has is kept, and has to be an entry this run can use
/// (`PmemStore::check_entry`). One it lacks is installed, which gives the bytes the name
/// they have for the uid this import publishes for: unless that is the uid the entries
/// belong to on the exporting host, the name is not the recorded one.
fn plan_pmem_entry(
    host: &ImportHost,
    manifest: &ArchiveManifest,
    config: &SnapshotConfig,
    described: &ManifestExternal,
) -> Result<bool> {
    let entry = &described.path;
    let mapped = config
        .metadata
        .pmem_devices
        .iter()
        .find(|device| Path::new(&device.path) == entry)
        .map(|device| {
            format!(
                " (a copy of {}, mapped at {})",
                device.source, device.mount_path
            )
        })
        .unwrap_or_default();
    let store_dir = crate::storage::pmem_store::store_dir_under(&host.data_dir)?;
    ensure!(
        crate::storage::pmem_store::is_entry_of(&store_dir, entry),
        "the snapshot maps the pmem image {}{mapped}, which is not an entry of this host's \
         pmem image store {}",
        entry.display(),
        store_dir.display()
    );
    match std::fs::symlink_metadata(entry) {
        Ok(_) => {
            crate::storage::pmem_store::check_existing_entry(&store_dir, entry, described.len)
                .with_context(|| {
                    format!(
                        "the snapshot maps the pmem image {}{mapped}, and what this host has \
                         at that path is not an entry this import can use",
                        entry.display()
                    )
                })?;
            Ok(false)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let here = crate::storage::pmem_store::run_owner();
            ensure!(
                manifest.pmem_owner_uid == Some(here),
                "the snapshot maps the pmem image {}{mapped}, which this host does not have. \
                 The snapshot's pmem images belong to {} on the exporting host and this \
                 import publishes store entries for uid {here}. A store entry's name is a \
                 digest of the uid it belongs to and of its bytes, so the entry this uid \
                 publishes would not have the path the snapshot recorded, and Firecracker \
                 reopens that path. Run the import as the uid the entries belong to",
                entry.display(),
                manifest.pmem_owner_uid.map_or_else(
                    || "no uid the manifest names".to_string(),
                    |uid| format!("uid {uid}")
                )
            );
            Ok(true)
        }
        Err(error) => Err(error)
            .with_context(|| format!("reading the snapshot's pmem image {}", entry.display())),
    }
}

/// What [`stage_files`] wrote.
struct Staged {
    /// One per file written into the staging directory, the config's first.
    files: Vec<ArchiveEntry>,
    externals: Vec<ImportedExternal>,
}

/// An external file in its temporary, whole and not yet published. Dropping it removes the
/// temporary.
enum StagedExternal<'a> {
    ImageDisk { temp: StoreTemp, file: File },
    Pmem(StagedEntry<'a>),
}

/// Write the external file `entry`, which is `plan`'s, into a temporary of the store it
/// belongs to, or read past it when this host has the file.
fn stage_external<'a, R: Read>(
    reader: &mut ArchiveReader<R>,
    entry: &ArchiveEntry,
    plan: &ExternalPlan,
    owners: &Path,
    store: Option<&'a PmemStore>,
) -> Result<Option<StagedExternal<'a>>> {
    let described = &plan.described;
    let (role, path) = (described.role, &described.path);
    ensure!(
        entry.len == described.len,
        "the archive carries {} bytes for the {role} {} and its manifest says {}",
        entry.len,
        path.display(),
        described.len
    );
    if !plan.install {
        // Not written: this host has the file. The reader still reads and verifies the
        // archived copy, on the way to the next entry.
        return Ok(None);
    }
    match role {
        ExternalRole::ImageDisk => {
            // A host that never ran a local image has no image cache yet.
            if let Some(cache) = path.parent().filter(|cache| !cache.exists()) {
                std::fs::create_dir_all(cache)
                    .with_context(|| format!("creating the image cache {}", cache.display()))?;
                crate::setup::give_store_entry_to_invoker(cache);
            }
            // Recorded before the file exists, so a killed import leaves it to the next
            // import or VM launch.
            let temp = StoreTemp::new_in(owners, path)?;
            // Readable too: its digest is recorded from this descriptor once it is published.
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(temp.path())
                .with_context(|| format!("creating {}", temp.path().display()))?;
            file.set_len(entry.len)
                .with_context(|| format!("setting the length of {}", temp.path().display()))?;
            let mut content = described.sha256.as_ref().map(|_| ContentSha256::new());
            while let Some((offset, bytes)) = reader.next_run()? {
                file.write_all_at(bytes, offset).with_context(|| {
                    format!("writing {} at offset {offset}", temp.path().display())
                })?;
                if let Some(content) = content.as_mut() {
                    content.piece(offset, bytes)?;
                }
            }
            if let (Some(recorded), Some(content)) = (&described.sha256, content) {
                let found = content.finish(entry.len)?;
                ensure!(
                    found == *recorded,
                    "the image disk the archive carries for {} is not the build the snapshot \
                     was taken against: its SHA-256 is {found} and the snapshot records \
                     {recorded}",
                    path.display()
                );
            }
            Ok(Some(StagedExternal::ImageDisk { temp, file }))
        }
        ExternalRole::Pmem => {
            let store =
                store.context("the pmem image store was not opened for an entry to install")?;
            let mut ingest = store.begin_stream_ingest(entry.len)?;
            while let Some((offset, bytes)) = reader.next_run()? {
                ingest.write_at(offset, bytes)?;
            }
            Ok(Some(StagedExternal::Pmem(ingest.finish_as(path)?)))
        }
    }
}

/// Publish the staged external files and settle the image disk, and return the paths of
/// the files this import staged and did not publish because another process had published
/// them meanwhile, which are kept. The archive has verified by now.
///
/// A pmem entry is published by its store, which keeps an entry another run published
/// first.
fn publish_externals(
    host: &ImportHost,
    plans: &[ExternalPlan],
    staged: Vec<StagedExternal<'_>>,
    store: Option<&PmemStore>,
) -> Result<Vec<PathBuf>> {
    let mut image_disk = None;
    let mut entries = Vec::new();
    for external in staged {
        match external {
            StagedExternal::ImageDisk { temp, file } => image_disk = Some((temp, file)),
            StagedExternal::Pmem(entry) => entries.push(entry),
        }
    }
    let mut kept = Vec::new();
    for plan in plans {
        if plan.described.role == ExternalRole::ImageDisk {
            let published = settle_image_disk(host, plan, image_disk.take())?;
            if plan.install && !published {
                kept.push(plan.described.path.clone());
            }
        }
    }
    for entry in entries {
        let store = store.context("the pmem image store was not opened to publish an entry")?;
        let published = store.publish_staged(entry)?;
        if !published.linked {
            kept.push(published.path);
        }
    }
    Ok(kept)
}

/// Give the image disk its name, or check the one that has it, under the lock of its image
/// in the image cache. Returns whether the staged copy got the name.
///
/// A launch of the image holds that lock from its look for the image's disk through the
/// rename that publishes the one it builds, and that rename replaces. Under the lock no
/// build is in flight on this kernel, so the disk this function leaves at the path is the
/// one a later launch finds and reuses, and a build that finished first is met here as a
/// file that has the name. The lock is a flock, and VMs that share the store through a
/// FUSE map each get it from their own kernel: a build in another VM is not kept out, and
/// one that replaces the linked disk leaves a snapshot whose restore is refused.
///
/// The staged copy is private until it is linked, so it is handed to the sudo invoker
/// through its descriptor and synced before the lock is taken: a launch of the image does
/// not wait behind the flush of a disk. Under the lock it is linked into place, which
/// fails where anything has the name. A disk that has the name, from before the import or
/// since, is kept: it has to be a regular file and, where the snapshot recorded an
/// identity, the build it recorded. The installed config relies on this check, not on the
/// one made before the archive was read.
fn settle_image_disk(
    host: &ImportHost,
    plan: &ExternalPlan,
    staged: Option<(StoreTemp, File)>,
) -> Result<bool> {
    let described = &plan.described;
    let dest = &described.path;
    ensure!(
        plan.install == staged.is_some(),
        "the image disk {} was {} and the archive was read as if it was not",
        dest.display(),
        if plan.install {
            "to be installed"
        } else {
            "on this host"
        }
    );
    let digest = image_of_disk(host, described)?;
    if let Some((temp, file)) = &staged {
        crate::setup::give_store_fd_to(file, temp.path(), crate::setup::sudo_invoker());
        file.sync_all()
            .with_context(|| format!("syncing {}", temp.path().display()))?;
    }
    let _lock = crate::commands::podman::lock_image_cache_entries(&host.image_cache_dir, digest)
        .with_context(|| format!("locking the image of the image disk {}", dest.display()))?;
    let Some((temp, file)) = staged else {
        let found = match std::fs::symlink_metadata(dest) {
            Ok(found) => found,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => bail!(
                "the snapshot's image disk {} is gone from this host: it was there when the \
                 import began, so the archive's copy was not kept",
                dest.display()
            ),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("reading the image disk {}", dest.display()))
            }
        };
        check_present_image_disk(
            described,
            &found,
            "another build of the image disk took its place on this host while the archive \
             was imported",
        )?;
        return Ok(false);
    };

    let published = match std::fs::hard_link(temp.path(), dest) {
        Ok(()) => {
            if let Some(parent) = dest.parent() {
                File::open(parent)
                    .and_then(|dir| dir.sync_all())
                    .with_context(|| format!("syncing {}", parent.display()))?;
            }
            // The digest of the bytes as they were written is the recorded identity
            // (`stage_external`), so the first restore need not read the disk to learn it.
            if let Some(sha256) = &described.sha256 {
                if let Err(error) = crate::utils::remember_image_disk_digest(
                    dest,
                    &file,
                    &format!("sha256:{sha256}"),
                ) {
                    warn!(
                        path = %dest.display(),
                        error = %format!("{error:#}"),
                        "could not record the installed image disk's digest; the first \
                         restore reads the disk to compute it"
                    );
                }
            }
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            info!(
                path = %dest.display(),
                "the image disk appeared on this host while its archive was imported; it is \
                 kept as it is"
            );
            let found = std::fs::symlink_metadata(dest)
                .with_context(|| format!("reading the image disk {}", dest.display()))?;
            check_present_image_disk(
                described,
                &found,
                "another build of the image disk appeared on this host while the archive \
                 was imported",
            )?;
            false
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!("linking {} to {}", temp.path().display(), dest.display())
            })
        }
    };
    // Removes the temporary's name, which is a second link when the copy was published,
    // and its reclaim record.
    drop(temp);
    Ok(published)
}

/// The identity each pmem device of `config` has on this host now, as a snapshot records it
/// (`storage::pmem::entry_identity`), by the device's path. The devices with these
/// identities have to pass the check a restore makes of them.
fn pmem_identities(host: &ImportHost, config: &SnapshotConfig) -> Result<Vec<(String, String)>> {
    let mut devices = config.metadata.pmem_devices.clone();
    for device in &mut devices {
        let found = std::fs::symlink_metadata(&device.path)
            .with_context(|| format!("reading the installed pmem image {}", device.path))?;
        device.identity = crate::storage::pmem::entry_identity(&found);
    }
    crate::storage::pmem::check_snapshot_pmem_images(&host.data_dir, &devices)
        .context("checking the imported snapshot's pmem images as a restore checks them")?;
    Ok(devices
        .into_iter()
        .map(|device| (device.path, device.identity))
        .collect())
}

/// Write every file left in the archive: the snapshot's into `staging`, and the external
/// ones this host lacks into temporaries of their stores. Read the archive to its verified
/// end, check that the directory holds every file a restore opens, publish the external
/// files, write the imported `config.json`, and sync the directory.
/// Runs on a blocking thread of the runtime.
fn stage_files<R: Read>(
    mut reader: ArchiveReader<R>,
    staging: &Path,
    archive: &CheckedArchive,
    host: &ImportHost,
) -> Result<Staged> {
    let working_set = crate::uffd::WorkingSetStore::path_for(Path::new(SNAPSHOT_MEMORY_FILE));
    let memory = Path::new(SNAPSHOT_MEMORY_FILE);
    let owners = crate::reclaim::owner_dir_in(&host.data_dir)?;
    // Opened, and so created where this host has none, only for an entry to install.
    let store = archive
        .externals
        .iter()
        .any(|plan| plan.install && plan.described.role == ExternalRole::Pmem)
        .then(|| PmemStore::open(&host.data_dir))
        .transpose()
        .context("opening the pmem image store")?;
    // Dropped with their temporaries if anything below fails before they are published.
    let mut staged_externals = Vec::new();
    let mut files = vec![archive.config_entry.clone()];
    while let Some(entry) = reader.next_entry()? {
        let relative = match &entry.name {
            EntryName::Snapshot(relative) => relative,
            EntryName::External(index) => {
                // The reader gives the external files in the manifest's order and no more
                // of them than it lists.
                let plan = archive
                    .externals
                    .get(*index)
                    .with_context(|| format!("the archive's manifest lists no external/{index}"))?;
                staged_externals.extend(stage_external(
                    &mut reader,
                    &entry,
                    plan,
                    &owners,
                    store.as_ref(),
                )?);
                continue;
            }
        };
        if *relative == working_set {
            // Not written: its key is the exporting host's config digest and the memory
            // image's modification time there, and the imported snapshot has neither. The
            // reader still reads and verifies it.
            info!(
                bytes = entry.len,
                "dropped the archive's working set; the first restores record a new one"
            );
            continue;
        }
        let path = staging.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let mut options = std::fs::OpenOptions::new();
        options.write(true);
        if relative == memory {
            // The empty file a create hands to Firecracker: nodatacow where the store has
            // the flag, so the image is not compressed.
            create_memory_file_nodatacow(&path, &archive.manifest.tag)?;
        } else {
            options.create_new(true);
        }
        let file = options
            .open(&path)
            .with_context(|| format!("creating {}", path.display()))?;
        // Each data run at its offset, so a hole is never written.
        reader.copy_entry_to(&file)?;
        files.push(entry);
    }

    // The reader has verified the trailer: every file above is the one the export wrote.
    // An archive can verify and still lack a file: the trailer says the files are the ones
    // the export wrote, not that they are all a restore opens.
    tokio::runtime::Handle::current().block_on(check_generation_files(
        staging,
        &archive.config,
        &format!("imported snapshot {}", archive.manifest.tag),
    ))?;

    // Only now does anything of this import get a name outside the staging directory.
    failpoint::hit("snapshot.import.externals_staged");
    let kept = publish_externals(host, &archive.externals, staged_externals, store.as_ref())?;
    let pmem_identities = pmem_identities(host, &archive.config)?;

    // The config is written last, as a create writes it. `create_new`, so the archive
    // cannot have supplied it.
    let config_path = staging.join(CONFIG_FILE);
    let config_json = imported_config_json(&archive.config_json, &pmem_identities)?;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&config_path)
        .and_then(|mut file| file.write_all(&config_json))
        .with_context(|| format!("writing {}", config_path.display()))?;

    super::podman::sync_generation(staging).context("syncing the staged snapshot")?;
    let externals = archive
        .externals
        .iter()
        .map(|plan| ImportedExternal {
            role: plan.described.role,
            path: plan.described.path.clone(),
            len: plan.described.len,
            installed: plan.install && !kept.contains(&plan.described.path),
        })
        .collect();
    Ok(Staged { files, externals })
}

/// The `config.json` an import writes: the archived one as a generation of its own, with
/// each pmem device's identity as `pmem_identities` gives it by the device's path. Only
/// those fields are set, on the parsed JSON, so every other field is installed as the
/// export wrote it, also one this build does not know.
fn imported_config_json(archived: &[u8], pmem_identities: &[(String, String)]) -> Result<Vec<u8>> {
    let mut config: serde_json::Value =
        serde_json::from_slice(archived).context("parsing the archived snapshot config")?;
    let fields = config
        .as_object_mut()
        .context("the archived snapshot config is not a JSON object")?;
    // An invalidation that carries the exporting host's generation id, or the id of an
    // earlier import of the same archive, must not match this directory
    // (`delete_snapshot_if_generation`).
    fields.insert(
        "generation_id".to_string(),
        serde_json::to_value(uuid::Uuid::new_v4()).context("serializing a generation id")?,
    );
    // The parent is a tag on the exporting host. Here it is absent, or names another
    // snapshot. The memory image is whole without it.
    fields.insert("parent_snapshot".to_string(), serde_json::Value::Null);
    // A snapshot records each pmem store entry by inode, length and modification time, and
    // a restore refuses an entry that has another identity. An entry this import installed
    // is a new file, so the exporting host's identity is not its own.
    if !pmem_identities.is_empty() {
        let devices = fields
            .get_mut("metadata")
            .and_then(|metadata| metadata.get_mut("pmem_devices"))
            .and_then(serde_json::Value::as_array_mut)
            .context("the archived snapshot config lists no pmem devices")?;
        for device in devices {
            let path = device
                .get("path")
                .and_then(serde_json::Value::as_str)
                .context("a pmem device of the archived snapshot config has no path")?;
            let (_, identity) = pmem_identities
                .iter()
                .find(|(entry, _)| entry == path)
                .with_context(|| format!("no identity was read for the pmem image {path}"))?;
            device["identity"] = serde_json::Value::String(identity.clone());
        }
    }
    serde_json::to_vec_pretty(&config).context("serializing snapshot config")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::snapshot_export::export_snapshot;
    use crate::commands::snapshot_export::tests::{
        data_runs, fake_snapshot, fake_snapshot_with, fake_snapshot_with_externals, names_in,
        pattern, Externals, Fake, IMAGE_DISK_LEN, MIB, PMEM_LEN, TAG,
    };
    use crate::storage::snapshot_archive::ArchiveWriter;
    use crate::storage::SnapshotManager;
    use std::io::Cursor;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::time::Duration;

    /// The importing host a fake snapshot was exported on: the same directories, and every
    /// kernel profile configured.
    fn host_of(fake: &Fake) -> ImportHost {
        ImportHost {
            snapshots_dir: fake.host.snapshots_dir.clone(),
            data_dir: fake.host.data_dir.clone(),
            image_cache_dir: fake.image_cache(),
            kernel_profile_configured: |_| Ok(true),
        }
    }

    fn staging_of(fake: &Fake) -> PathBuf {
        snapshot_sibling(&fake.dir(), "creating")
    }

    /// The reclaim records of snapshot staging directories under the fake's data directory.
    fn staging_records(fake: &Fake) -> Vec<String> {
        let owners = crate::reclaim::owner_dir_in(&fake.host.data_dir).unwrap();
        names_in(&owners)
            .into_iter()
            .filter(|name| name.contains("snapshot-staging-"))
            .collect()
    }

    /// Every name under `root`, relative to it, files and directories alike.
    fn tree(root: &Path) -> Vec<PathBuf> {
        let mut names = Vec::new();
        let mut pending = vec![PathBuf::new()];
        while let Some(relative) = pending.pop() {
            for entry in std::fs::read_dir(root.join(&relative)).unwrap() {
                let entry = entry.unwrap();
                let name = relative.join(entry.file_name());
                if entry.file_type().unwrap().is_dir() {
                    pending.push(name.clone());
                }
                names.push(name);
            }
        }
        names.sort();
        names
    }

    /// The regular files under `dir`, relative to it.
    fn files_under(dir: &Path) -> Vec<PathBuf> {
        tree(dir)
            .into_iter()
            .filter(|name| dir.join(name).is_file())
            .collect()
    }

    /// Export the fake's snapshot with the command's own function.
    async fn export_of(fake: &Fake) -> Vec<u8> {
        export_snapshot(&fake.host, TAG, Vec::new())
            .await
            .unwrap()
            .0
    }

    /// A fake snapshot, exported and then deleted: the host an archive of it arrives on.
    async fn exported() -> (Fake, Vec<u8>) {
        exported_with(|_| {}).await
    }

    /// [`exported`] with `edit` applied to the snapshot's config before it is written.
    async fn exported_with(edit: impl FnOnce(&mut SnapshotConfig)) -> (Fake, Vec<u8>) {
        let fake = fake_snapshot_with(edit);
        let archive = export_of(&fake).await;
        std::fs::remove_dir_all(fake.dir()).unwrap();
        (fake, archive)
    }

    /// The message of the refusal of `archive` on `host`. The refused import left every
    /// name under the fake's root as it was: no directory, no lock file, no reclaim record.
    async fn refusal(fake: &Fake, host: &ImportHost, archive: Vec<u8>) -> String {
        let before = tree(fake.root.path());
        let message = match import_snapshot(host, Cursor::new(archive)).await {
            Ok(imported) => panic!("the import installed {}", imported.tag),
            Err(error) => format!("{error:#}"),
        };
        assert_eq!(
            tree(fake.root.path()),
            before,
            "the refused import wrote something ({message})"
        );
        message
    }

    /// The message of the failure of an import of `archive` that got as far as staging.
    /// Nothing of it is left: no tag, no staging directory, no reclaim record.
    async fn failure_that_leaves_nothing(fake: &Fake, archive: Vec<u8>) -> String {
        let message = match import_snapshot(&host_of(fake), Cursor::new(archive)).await {
            Ok(imported) => panic!("the import installed {}", imported.tag),
            Err(error) => format!("{error:#}"),
        };
        assert!(
            !fake.dir().exists(),
            "the failed import installed the tag ({message})"
        );
        assert!(
            !staging_of(fake).exists(),
            "the failed import left its staging directory ({message})"
        );
        assert_eq!(
            staging_records(fake),
            Vec::<String>::new(),
            "the failed import left a reclaim record ({message})"
        );
        message
    }

    /// The files of an archive, whole, for a test to change before it writes them again.
    struct Unpacked {
        manifest: ArchiveManifest,
        files: Vec<(PathBuf, Vec<u8>)>,
        /// The external files, in the archive's order.
        externals: Vec<Vec<u8>>,
    }

    impl Unpacked {
        fn of(archive: &[u8]) -> Self {
            let mut reader = ArchiveReader::new(archive).unwrap();
            let manifest = reader.manifest().clone();
            let mut files = Vec::new();
            let mut externals = Vec::new();
            while let Some(entry) = reader.next_entry().unwrap() {
                let mut bytes = vec![0u8; entry.len as usize];
                while let Some((offset, piece)) = reader.next_run().unwrap() {
                    let start = offset as usize;
                    bytes[start..start + piece.len()].copy_from_slice(piece);
                }
                match entry.name {
                    EntryName::Snapshot(name) => files.push((name, bytes)),
                    EntryName::External(_) => externals.push(bytes),
                }
            }
            Self {
                manifest,
                files,
                externals,
            }
        }

        /// Change the archived config, and the digest the manifest gives for it.
        fn edit_config(&mut self, edit: impl FnOnce(&mut SnapshotConfig)) {
            let json = &mut self.files[0].1;
            let mut config: SnapshotConfig = serde_json::from_slice(json).unwrap();
            edit(&mut config);
            *json = serde_json::to_vec_pretty(&config).unwrap();
            self.manifest.config_sha256 = hex::encode(Sha256::digest(&*json));
        }

        /// Change the archived config as JSON, and the digest the manifest gives for it.
        fn edit_config_json(&mut self, edit: impl FnOnce(&mut serde_json::Value)) {
            let json = &mut self.files[0].1;
            let mut config: serde_json::Value = serde_json::from_slice(json).unwrap();
            edit(&mut config);
            *json = serde_json::to_vec_pretty(&config).unwrap();
            self.manifest.config_sha256 = hex::encode(Sha256::digest(&*json));
        }

        /// The archive of these files under this manifest, with a trailer that verifies.
        fn pack(&self) -> Vec<u8> {
            let file_of = |bytes: &[u8]| {
                let mut file = tempfile::tempfile().unwrap();
                file.write_all(bytes).unwrap();
                file
            };
            let mut writer = ArchiveWriter::new(Vec::new(), &self.manifest).unwrap();
            for (name, bytes) in &self.files {
                writer.append_snapshot_file(name, &file_of(bytes)).unwrap();
            }
            // One per external file the manifest lists: the file's bytes, or a few bytes
            // for one a test added to the manifest.
            for index in 0..self.manifest.externals.len() {
                let bytes = self
                    .externals
                    .get(index)
                    .map_or(&b"external"[..], Vec::as_slice);
                writer.append_external(&file_of(bytes)).unwrap();
            }
            writer.finish().unwrap()
        }
    }

    /// Where `bytes` are in `archive`, which holds them once.
    fn position_of(archive: &[u8], bytes: &[u8]) -> usize {
        let at: Vec<usize> = archive
            .windows(bytes.len())
            .enumerate()
            .filter(|(_, window)| *window == bytes)
            .map(|(at, _)| at)
            .collect();
        assert_eq!(at.len(), 1, "the archive holds the bytes once");
        at[0]
    }

    /// One file of a snapshot directory.
    #[derive(PartialEq)]
    struct FileImage {
        /// Its path relative to the directory.
        name: PathBuf,
        len: u64,
        /// Its data runs, as `(start, end)`.
        runs: Vec<(u64, u64)>,
        bytes: Vec<u8>,
    }

    /// What a snapshot directory holds.
    fn image_of(dir: &Path) -> Vec<FileImage> {
        files_under(dir)
            .into_iter()
            .map(|name| {
                let path = dir.join(&name);
                let bytes = std::fs::read(&path).unwrap();
                FileImage {
                    len: bytes.len() as u64,
                    runs: data_runs(&path),
                    bytes,
                    name,
                }
            })
            .collect()
    }

    /// Export, delete, import: the directory comes back with the same names, lengths, hole
    /// maps and bytes, and without the working set. Nothing else is left of the import.
    #[tokio::test]
    async fn an_import_installs_what_the_export_read() {
        let fake = fake_snapshot();
        let dir = fake.dir();
        let before = image_of(&dir);
        let archive = export_of(&fake).await;
        assert!(
            Unpacked::of(&archive)
                .files
                .iter()
                .any(|(name, _)| name == Path::new("memory.bin.working-set")),
            "control: the archive carries the working set"
        );
        std::fs::remove_dir_all(&dir).unwrap();

        let imported = import_snapshot(&host_of(&fake), Cursor::new(archive))
            .await
            .unwrap();

        assert_eq!(imported.tag, TAG);
        let names = [
            "config.json",
            "disk/disk.raw",
            "memory.bin",
            "vmstate.bin",
            "volume-5000-inode-table.json",
        ];
        let after = image_of(&dir);
        assert_eq!(
            after
                .iter()
                .map(|file| file.name.to_str().unwrap())
                .collect::<Vec<_>>(),
            names,
            "the installed files (the working set, its lock and its temporary stay out)"
        );
        assert_eq!(
            imported
                .files
                .iter()
                .map(|entry| entry.name.clone())
                .collect::<Vec<_>>(),
            names.map(|name| EntryName::Snapshot(PathBuf::from(name))),
            "the files the import reports"
        );
        for file in &after {
            if file.name == Path::new(CONFIG_FILE) {
                continue;
            }
            let name = file.name.display();
            let exported = before
                .iter()
                .find(|exported| exported.name == file.name)
                .unwrap_or_else(|| panic!("{name} was not in the exported directory"));
            assert_eq!(file.len, exported.len, "length of {name}");
            assert_eq!(file.runs, exported.runs, "data runs of {name}");
            assert!(file.bytes == exported.bytes, "bytes of {name}");
        }
        assert!(
            runs_of(&after, "memory.bin").len() > 1 && runs_of(&after, "disk/disk.raw").len() > 1,
            "control: the memory image and the disk have holes to keep"
        );
        assert!(
            std::fs::symlink_metadata(dir.join("memory.bin"))
                .unwrap()
                .is_file(),
            "the memory image is a file of the directory"
        );
        // The loader takes it.
        SnapshotManager::new(fake.host.snapshots_dir.clone())
            .load_snapshot(TAG)
            .await
            .unwrap();

        assert!(
            !staging_of(&fake).exists(),
            "the staging directory outlived the import"
        );
        assert_eq!(staging_records(&fake), Vec::<String>::new());
        remove_snapshot_dir(&dir).await.unwrap();
    }

    /// The imported config is the archived one as a generation of its own: a new generation
    /// id, no parent, and every other field as it was, also a field this build does not
    /// know.
    #[tokio::test]
    async fn the_imported_config_differs_in_its_generation_and_its_parent_only() {
        let (fake, archive) = exported_with(|config| {
            config.parent_snapshot = Some("example-parent".to_string());
        })
        .await;
        let mut unpacked = Unpacked::of(&archive);
        unpacked.edit_config_json(|config| {
            config["example_field_of_another_build"] = serde_json::json!({"kept": true});
            config["metadata"]["example_field_of_another_build"] = serde_json::json!(7);
        });
        let mut config_before: serde_json::Value =
            serde_json::from_slice(&unpacked.files[0].1).unwrap();
        assert_eq!(
            config_before["parent_snapshot"], "example-parent",
            "control: the exported snapshot names a parent"
        );

        import_snapshot(&host_of(&fake), Cursor::new(unpacked.pack()))
            .await
            .unwrap();

        let mut config_after: serde_json::Value =
            serde_json::from_slice(&std::fs::read(fake.dir().join(CONFIG_FILE)).unwrap()).unwrap();
        assert_eq!(
            config_after["parent_snapshot"],
            serde_json::Value::Null,
            "the parent is a tag of the exporting host"
        );
        let generation = |config: &serde_json::Value| {
            uuid::Uuid::parse_str(config["generation_id"].as_str().unwrap()).unwrap()
        };
        assert_ne!(
            generation(&config_after),
            generation(&config_before),
            "the import is a generation of its own"
        );
        for config in [&mut config_before, &mut config_after] {
            let fields = config.as_object_mut().unwrap();
            fields.remove("generation_id").unwrap();
            fields.remove("parent_snapshot").unwrap();
        }
        assert_eq!(
            config_after, config_before,
            "the config changed outside the two rewritten fields"
        );
        remove_snapshot_dir(&fake.dir()).await.unwrap();
    }

    /// Staged with a create's primitives: where this store makes a subvolume, the imported
    /// directory and its disk directory are subvolumes.
    #[tokio::test]
    async fn an_import_is_staged_in_subvolumes_where_the_store_makes_them() {
        let (fake, archive) = exported().await;
        let probe = fake.root.path().join("probe");
        create_dir_or_subvolume(&probe).await.unwrap();
        let makes_subvolumes = crate::storage::disk::is_btrfs_subvolume(&probe);
        remove_snapshot_dir(&probe).await.unwrap();

        import_snapshot(&host_of(&fake), Cursor::new(archive))
            .await
            .unwrap();

        let dir = fake.dir();
        assert_eq!(
            crate::storage::disk::is_btrfs_subvolume(&dir),
            makes_subvolumes,
            "the imported directory"
        );
        assert_eq!(
            crate::storage::disk::is_btrfs_subvolume(&snapshot_disk_dir(&dir)),
            makes_subvolumes,
            "the imported disk directory"
        );
        remove_snapshot_dir(&dir).await.unwrap();
    }

    fn runs_of(image: &[FileImage], name: &str) -> Vec<(u64, u64)> {
        image
            .iter()
            .find(|file| file.name == Path::new(name))
            .unwrap()
            .runs
            .clone()
    }

    /// A disk-only snapshot has no memory image, no VM state and no inode table for a
    /// volume: a create writes none of them for it, and its clones boot from the disk. It
    /// imports without them.
    #[tokio::test]
    async fn a_disk_only_snapshot_imports_without_a_memory_image() {
        let fake = fake_snapshot_with(|config| {
            config.kind = SnapshotKind::DiskOnly;
            config.metadata.volumes = vec![crate::storage::snapshot::SnapshotVolumeConfig {
                host_path: PathBuf::from("/example/volume"),
                guest_path: "/volume".to_string(),
                read_only: false,
                vsock_port: 5000,
                portable: true,
            }];
        });
        std::fs::remove_file(fake.memory()).unwrap();
        for name in ["vmstate.bin", "volume-5000-inode-table.json"] {
            std::fs::remove_file(fake.dir().join(name)).unwrap();
        }
        let archive = export_of(&fake).await;
        std::fs::remove_dir_all(fake.dir()).unwrap();

        import_snapshot(&host_of(&fake), Cursor::new(archive))
            .await
            .unwrap();

        assert_eq!(
            files_under(&fake.dir()),
            ["config.json", "disk/disk.raw"].map(PathBuf::from)
        );
        let config = SnapshotManager::new(fake.host.snapshots_dir.clone())
            .load_snapshot(TAG)
            .await
            .unwrap();
        assert_eq!(config.kind, SnapshotKind::DiskOnly);
        assert!(config.metadata.volumes[0].portable);
        remove_snapshot_dir(&fake.dir()).await.unwrap();
    }

    /// What the archive does not carry comes back from the import, for the command to warn
    /// about. It is read from the config that is installed, whatever the manifest lists.
    #[tokio::test]
    async fn an_import_reports_what_the_archive_does_not_carry() {
        let (fake, archive) = exported_with(|config| {
            config.metadata.volumes = vec![crate::storage::SnapshotVolumeConfig {
                host_path: PathBuf::from("/example/host/data"),
                guest_path: "/data".to_string(),
                read_only: false,
                vsock_port: 5000,
                portable: false,
            }];
        })
        .await;
        let mut unpacked = Unpacked::of(&archive);
        assert_eq!(
            unpacked.manifest.not_shipped.len(),
            1,
            "control: the export listed the mapped directory"
        );
        unpacked.manifest.not_shipped.clear();

        let imported = import_snapshot(&host_of(&fake), Cursor::new(unpacked.pack()))
            .await
            .unwrap();

        assert_eq!(
            imported.not_shipped,
            ["The directory /example/host/data is mapped into the guest at /data."]
        );
        remove_snapshot_dir(&fake.dir()).await.unwrap();
    }

    #[tokio::test]
    async fn an_archive_of_another_version_is_refused() {
        let (fake, mut archive) = exported().await;
        // The manifest's version, which is the first thing in the archive's first entry.
        let version = b"\"archive_version\": 1,";
        let at = archive
            .windows(version.len())
            .position(|window| window == version)
            .unwrap();
        archive[at + version.len() - 2] = b'2';

        let message = refusal(&fake, &host_of(&fake), archive).await;

        assert!(
            message.contains("the archive is version 2 and this fcvm reads version 1"),
            "{message}"
        );
    }

    #[tokio::test]
    async fn a_snapshot_of_another_architecture_is_refused() {
        let (fake, archive) = exported().await;
        let mut unpacked = Unpacked::of(&archive);
        unpacked.manifest.arch = "example-arch".to_string();

        let message = refusal(&fake, &host_of(&fake), unpacked.pack()).await;

        assert!(
            message.contains("taken on example-arch") && message.contains("architecture"),
            "{message}"
        );
    }

    #[tokio::test]
    async fn a_cloud_hypervisor_snapshot_is_refused() {
        let (fake, archive) = exported().await;
        let mut unpacked = Unpacked::of(&archive);
        unpacked.manifest.hypervisor = Backend::CloudHypervisor;

        let message = refusal(&fake, &host_of(&fake), unpacked.pack()).await;

        assert!(
            message.contains("cloud-hypervisor") && message.contains("Firecracker snapshots only"),
            "{message}"
        );
    }

    /// The checks made of the external files count only if the manifest lists the ones the
    /// config names, each with the digest and the length the config records.
    #[tokio::test]
    async fn a_manifest_that_does_not_list_the_configs_external_files_is_refused() {
        const DISAGREE: &str =
            "disagree about the image disk and the pmem images the archive carries,";
        let (fake, _externals, archive) = exported_with_externals().await;
        type Edit = fn(&mut Unpacked);
        let cases: [(&str, Edit); 6] = [
            ("no external files", |unpacked| {
                unpacked.manifest.externals.clear()
            }),
            ("one external file", |unpacked| {
                unpacked.manifest.externals.truncate(1)
            }),
            ("another image disk digest", |unpacked| {
                unpacked.manifest.externals[0].sha256 = Some("00".repeat(32))
            }),
            ("another image disk path", |unpacked| {
                unpacked.manifest.externals[0].path = PathBuf::from("/example/elsewhere.img")
            }),
            ("the roles exchanged", |unpacked| {
                unpacked.manifest.externals[0].role = ExternalRole::Pmem;
                unpacked.manifest.externals[1].role = ExternalRole::ImageDisk;
            }),
            ("another pmem length", |unpacked| {
                unpacked.manifest.externals[1].len += PMEM_LEN
            }),
        ];
        for (what, edit) in cases {
            let mut unpacked = Unpacked::of(&archive);
            edit(&mut unpacked);

            let message = refusal(&fake, &host_of(&fake), unpacked.pack()).await;

            assert!(message.contains(DISAGREE), "{what}: {message}");
        }

        // A manifest that lists a file the config does not name.
        let (fake, archive) = exported().await;
        let mut unpacked = Unpacked::of(&archive);
        unpacked.manifest.externals = vec![ManifestExternal {
            role: ExternalRole::ImageDisk,
            path: fake.image_cache().join("layers.img"),
            len: 8,
            sha256: None,
        }];
        let message = refusal(&fake, &host_of(&fake), unpacked.pack()).await;
        assert!(message.contains(DISAGREE), "{message}");
    }

    #[tokio::test]
    async fn a_snapshot_taken_under_another_data_directory_is_refused() {
        let (fake, archive) = exported().await;
        let mut host = host_of(&fake);
        host.data_dir = fake.root.path().join("another-data");

        let message = refusal(&fake, &host, archive).await;

        assert!(
            message.contains("taken under the data directory") && message.contains("another-data"),
            "{message}"
        );
    }

    #[tokio::test]
    async fn a_host_without_the_snapshots_firecracker_is_refused() {
        let (fake, archive) = exported().await;
        std::fs::remove_file(fake.firecracker()).unwrap();

        let message = refusal(&fake, &host_of(&fake), archive).await;

        assert!(
            message.contains("which this host does not have")
                && message.contains("Firecracker v0.0.0-example"),
            "{message}"
        );
    }

    /// The recorded path holds another build of Firecracker.
    #[tokio::test]
    async fn a_host_with_another_firecracker_at_the_path_is_refused() {
        let (fake, archive) = exported().await;
        std::fs::write(fake.firecracker(), "#!/bin/sh\necho 'another build'\n").unwrap();

        let message = refusal(&fake, &host_of(&fake), archive).await;

        assert!(
            message.contains("is not the one the snapshot was taken on"),
            "{message}"
        );
    }

    /// The archived config is not the file the manifest was written for.
    #[tokio::test]
    async fn a_config_the_manifest_does_not_describe_is_refused() {
        let (fake, archive) = exported().await;
        let mut unpacked = Unpacked::of(&archive);
        // Still a config the loader accepts: only its bytes differ.
        unpacked.files[0].1.push(b'\n');

        let message = refusal(&fake, &host_of(&fake), unpacked.pack()).await;

        assert!(
            message.contains("config.json is not the one its manifest describes"),
            "{message}"
        );
    }

    /// The archived config goes through the loader's parser, which refuses a snapshot of
    /// another version.
    #[tokio::test]
    async fn a_snapshot_of_another_version_is_refused() {
        let (fake, archive) = exported().await;
        let mut unpacked = Unpacked::of(&archive);
        unpacked.edit_config(|config| config.container_start_version = 2);

        let message = refusal(&fake, &host_of(&fake), unpacked.pack()).await;

        assert!(
            message.contains("uses container start notification version 2"),
            "{message}"
        );
    }

    #[tokio::test]
    async fn a_snapshot_whose_kernel_profile_is_not_configured_is_refused() {
        let (fake, archive) = exported_with(|config| {
            config.metadata.kernel_profile = Some("example-profile".to_string());
        })
        .await;
        let mut host = host_of(&fake);
        host.kernel_profile_configured = |name| Ok(name != "example-profile");

        let message = refusal(&fake, &host, archive.clone()).await;

        assert!(
            message.contains("kernel profile 'example-profile' is not configured"),
            "{message}"
        );

        // A snapshot that names no profile is checked against the default one.
        let (fake, archive) = exported().await;
        let mut host = host_of(&fake);
        host.kernel_profile_configured = |name| Ok(name != "default");
        let message = refusal(&fake, &host, archive).await;
        assert!(
            message.contains("kernel profile 'default' is not configured"),
            "{message}"
        );
    }

    /// The tag becomes a path, so a manifest cannot name anything but a tag.
    #[tokio::test]
    async fn a_tag_that_is_not_a_snapshot_name_is_refused() {
        let (fake, archive) = exported().await;
        let mut unpacked = Unpacked::of(&archive);
        unpacked.manifest.tag = "../example-tag".to_string();

        let message = refusal(&fake, &host_of(&fake), unpacked.pack()).await;

        assert!(message.contains("invalid snapshot name"), "{message}");
    }

    /// The checks made on the manifest count only if the config says the same.
    #[tokio::test]
    async fn a_manifest_that_disagrees_with_the_config_is_refused() {
        let (fake, archive) = exported().await;
        let copy = fake.root.path().join("firecracker-copy");
        std::fs::copy(fake.firecracker(), &copy).unwrap();
        type Edit = Box<dyn FnOnce(&mut Unpacked)>;
        let cases: Vec<(&str, Edit)> = vec![
            (
                "the tag",
                Box::new(|unpacked| unpacked.manifest.tag = "another-tag".to_string()),
            ),
            (
                "the snapshot kind",
                Box::new(|unpacked| unpacked.manifest.kind = SnapshotKind::DiskOnly),
            ),
            (
                "the snapshot versions",
                Box::new(|unpacked| unpacked.manifest.container_start_version = 2),
            ),
            (
                "the hypervisor",
                Box::new(|unpacked| {
                    unpacked
                        .edit_config(|config| config.metadata.hypervisor = Backend::CloudHypervisor)
                }),
            ),
            (
                // The same binary at another path passes the manifest's own check.
                "the Firecracker binary",
                Box::new(move |unpacked| unpacked.manifest.firecracker.path = copy),
            ),
            (
                "the memory image",
                Box::new(|unpacked| unpacked.manifest.memory = None),
            ),
        ];
        for (what, edit) in cases {
            let mut unpacked = Unpacked::of(&archive);
            edit(&mut unpacked);

            let message = refusal(&fake, &host_of(&fake), unpacked.pack()).await;

            assert!(
                message.contains(&format!("disagree about {what},")),
                "{what}: {message}"
            );
        }
    }

    /// The config keeps its paths, so each has to be the file's path on this host.
    #[tokio::test]
    async fn a_config_whose_paths_are_not_this_hosts_is_refused() {
        let (fake, archive) = exported().await;
        type Edit = fn(&mut SnapshotConfig);
        let cases: [(&str, Edit); 3] = [
            ("memory_path", |config| {
                config.memory_path = PathBuf::from("/example/elsewhere/memory.bin")
            }),
            ("vmstate_path", |config| {
                config.vmstate_path = PathBuf::from("/example/elsewhere/vmstate.bin")
            }),
            ("disk_path", |config| {
                config.disk_path = PathBuf::from("/example/elsewhere/disk/disk.raw")
            }),
        ];
        for (field, edit) in cases {
            let mut unpacked = Unpacked::of(&archive);
            unpacked.edit_config(edit);

            let message = refusal(&fake, &host_of(&fake), unpacked.pack()).await;

            assert!(
                message.contains(&format!("records {field} /example/elsewhere/")),
                "{field}: {message}"
            );
        }
    }

    /// An archive whose first file is not the config cannot be checked before it is
    /// written.
    #[tokio::test]
    async fn an_archive_that_does_not_start_with_its_config_is_refused() {
        let (fake, archive) = exported().await;
        let mut unpacked = Unpacked::of(&archive);
        unpacked.files.rotate_left(1);

        let message = refusal(&fake, &host_of(&fake), unpacked.pack()).await;

        assert!(
            message.contains("an export writes config.json first"),
            "{message}"
        );
    }

    /// The tag exists: the import is refused, and the snapshot that has the name is as it
    /// was. The refusal does not wait for the tag's lock, which a memory server of the
    /// snapshot holds for as long as it runs.
    #[tokio::test]
    async fn an_import_over_an_existing_tag_is_refused_and_changes_nothing() {
        let fake = fake_snapshot();
        let archive = export_of(&fake).await;
        let before = image_of(&fake.dir());
        let _server = acquire_snapshot_dir_lock(&fake.dir(), false).await.unwrap();

        let host = host_of(&fake);
        let message = tokio::time::timeout(Duration::from_secs(30), refusal(&fake, &host, archive))
            .await
            .expect("the import waited for the lock of a tag that exists");

        assert!(
            message.contains("snapshot 'example-tag' exists on this host"),
            "{message}"
        );
        assert!(
            image_of(&fake.dir()) == before,
            "the refused import changed the existing snapshot"
        );
    }

    /// A create installs the tag after the import has checked its archive and before the
    /// import holds the tag's lock: the import looks again under the lock, refuses, and
    /// leaves the created snapshot alone.
    #[tokio::test]
    async fn a_tag_created_while_the_import_waits_for_its_lock_is_refused_and_kept() {
        let (fake, archive) = exported().await;
        let host = host_of(&fake);
        let (reader, checked) = check_archive(&host, Cursor::new(archive)).await.unwrap();
        let dir = fake.dir();
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join(CONFIG_FILE), b"another create's snapshot").unwrap();

        let error = install(&host, reader, checked).await.unwrap_err();

        assert!(
            format!("{error:#}").contains("snapshot 'example-tag' exists on this host"),
            "{error:#}"
        );
        assert_eq!(
            std::fs::read(dir.join(CONFIG_FILE)).unwrap(),
            b"another create's snapshot",
            "the import replaced a snapshot that was created while it waited"
        );
        assert_eq!(files_under(&dir), [PathBuf::from(CONFIG_FILE)]);
        assert!(!staging_of(&fake).exists());
        assert_eq!(staging_records(&fake), Vec::<String>::new());
    }

    /// The function that decides whether stdin can give an archive, on a real terminal (the
    /// slave side of a pty) and on a file.
    #[test]
    fn a_terminal_is_refused_as_the_archive_input() {
        let pty = nix::pty::openpty(None, None).unwrap();
        let error = archive_input(&pty.slave).unwrap_err();
        assert!(
            format!("{error:#}").contains("terminal"),
            "the refusal does not say why: {error:#}"
        );

        // Control: a file is accepted, and the result reads that same file.
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"archive").unwrap();
        let mut input = archive_input(&file).unwrap();
        std::io::Seek::rewind(&mut input).unwrap();
        let mut read = String::new();
        input.read_to_string(&mut read).unwrap();
        assert_eq!(read, "archive");
    }

    /// A stream cut in a file's data, and one cut after the trailer with only the end of
    /// the archive missing, both fail and leave nothing.
    #[tokio::test]
    async fn a_truncated_archive_installs_nothing_and_leaves_nothing() {
        let (fake, archive) = exported().await;
        for (cut, expected) in [
            (
                archive.len() / 2,
                "ended inside the data of snapshot/memory.bin",
            ),
            (archive.len() - 1024, "ended inside the end of the archive"),
        ] {
            let message = failure_that_leaves_nothing(&fake, archive[..cut].to_vec()).await;
            assert!(message.contains(expected), "cut at {cut}: {message}");
        }

        // Control: the whole archive imports on the same host.
        import_snapshot(&host_of(&fake), Cursor::new(archive))
            .await
            .unwrap();
        remove_snapshot_dir(&fake.dir()).await.unwrap();
    }

    /// One byte of the memory image's data differs from what the export hashed: every file
    /// is staged, the trailer does not verify, and nothing is installed.
    #[tokio::test]
    async fn an_archive_with_a_flipped_data_byte_installs_nothing_and_leaves_nothing() {
        let (fake, mut archive) = exported().await;
        // The start of the memory image's second data run.
        let at = position_of(&archive, &pattern(2, 64));
        archive[at + 10] ^= 0x01;

        let message = failure_that_leaves_nothing(&fake, archive).await;

        assert!(
            message.contains("snapshot/memory.bin is corrupt"),
            "{message}"
        );
    }

    /// An archive that verifies and lacks a file a restore opens is not installed: the VM
    /// state, an extra disk, and the inode table of a full snapshot's volume (a plain map's
    /// too: a clone without it answers EBADF for every inode the guest held, #1684).
    #[tokio::test]
    async fn an_archive_without_a_file_a_restore_reads_installs_nothing() {
        let (fake, archive) = exported().await;

        let mut unpacked = Unpacked::of(&archive);
        unpacked
            .files
            .retain(|(name, _)| name != Path::new("vmstate.bin"));
        let message = failure_that_leaves_nothing(&fake, unpacked.pack()).await;
        assert!(
            message.contains("reading imported snapshot example-tag VM state artifact"),
            "{message}"
        );

        let mut unpacked = Unpacked::of(&archive);
        unpacked.edit_config(|config| {
            config.metadata.extra_disks = vec![extra_disk("extra.raw")];
        });
        let message = failure_that_leaves_nothing(&fake, unpacked.pack()).await;
        assert!(
            message.contains("reading imported snapshot example-tag extra disk disk0 artifact"),
            "{message}"
        );

        let mut unpacked = Unpacked::of(&archive);
        unpacked.edit_config(|config| {
            config.metadata.volumes = vec![crate::storage::snapshot::SnapshotVolumeConfig {
                host_path: PathBuf::from("/example/volume"),
                guest_path: "/volume".to_string(),
                read_only: false,
                vsock_port: 5001,
                portable: false,
            }];
        });
        let message = failure_that_leaves_nothing(&fake, unpacked.pack()).await;
        assert!(
            message.contains("reading imported snapshot example-tag volume /volume inode table"),
            "{message}"
        );
    }

    fn extra_disk(filename: &str) -> crate::storage::SnapshotExtraDisk {
        crate::storage::SnapshotExtraDisk {
            filename: filename.to_string(),
            mount_path: "/extra".to_string(),
            read_only: true,
            drive_id: "disk0".to_string(),
        }
    }

    /// A restore opens an extra disk by its recorded name under the snapshot's disk
    /// directory. A name that leaves the directory, here for a file this host has, is
    /// refused before anything is written.
    #[tokio::test]
    async fn a_snapshot_whose_extra_disk_is_named_outside_it_is_refused() {
        let (fake, archive) = exported().await;
        let mut unpacked = Unpacked::of(&archive);
        unpacked.edit_config(|config| {
            // From `<tag>/disk/`, the fake's Firecracker: a non-empty file.
            config.metadata.extra_disks = vec![extra_disk("../../../firecracker")];
        });
        assert!(
            fake.host.snapshots_dir.parent() == fake.firecracker().parent()
                && fake.firecracker().is_file(),
            "control: from the snapshot's disk directory the recorded name is a file of this host"
        );

        let message = refusal(&fake, &host_of(&fake), unpacked.pack()).await;

        assert!(
            message.contains(
                "the archived snapshot extra disk disk0 names a path outside its generation"
            ),
            "{message}"
        );
    }

    /// A fake snapshot with an image disk and a pmem entry, exported, and its directory
    /// deleted. The external files are still on the host.
    async fn exported_with_externals() -> (Fake, Externals, Vec<u8>) {
        let (fake, externals) = fake_snapshot_with_externals();
        let archive = export_of(&fake).await;
        std::fs::remove_dir_all(fake.dir()).unwrap();
        (fake, externals, archive)
    }

    /// The pmem store directory of the fake host.
    fn pmem_store_dir(fake: &Fake) -> PathBuf {
        crate::storage::pmem_store::store_dir_under(&fake.host.data_dir).unwrap()
    }

    /// Remove the image disk and the pmem entry from the fake host, and the link a run
    /// leaves from an image to its entry: a host that never had the files.
    fn remove_externals(fake: &Fake, externals: &Externals) {
        std::fs::remove_file(&externals.image_disk).unwrap();
        std::fs::remove_file(&externals.pmem.path).unwrap();
        let store = pmem_store_dir(fake);
        for name in names_in(&store) {
            if name.ends_with(".source") {
                std::fs::remove_file(store.join(name)).unwrap();
            }
        }
    }

    /// What an import in flight keeps outside its staging directory: the temporaries beside
    /// the image disk, the temps of the pmem store, and the reclaim records of store
    /// temporaries.
    fn external_temporaries(fake: &Fake) -> Vec<String> {
        let mut found = Vec::new();
        for name in names_in(&fake.image_cache()) {
            if name.ends_with(".tmp") {
                found.push(format!("image-cache/{name}"));
            }
        }
        let tmp = pmem_store_dir(fake).join("tmp");
        if tmp.is_dir() {
            for name in names_in(&tmp) {
                found.push(format!("pmem/tmp/{name}"));
            }
        }
        let owners = crate::reclaim::owner_dir_in(&fake.host.data_dir).unwrap();
        for name in names_in(&owners) {
            if name.contains("store-temp-") {
                found.push(format!("reclaim/{name}"));
            }
        }
        found
    }

    /// The length, the data runs and the bytes of the file at `path`.
    fn file_image(path: &Path) -> FileImage {
        let bytes = std::fs::read(path).unwrap();
        FileImage {
            name: path.to_path_buf(),
            len: bytes.len() as u64,
            runs: data_runs(path),
            bytes,
        }
    }

    /// The inode and the modification time of the file at `path`.
    fn inode_and_mtime(path: &Path) -> (u64, i64, i64) {
        let found = std::fs::metadata(path).unwrap();
        (found.ino(), found.mtime(), found.mtime_nsec())
    }

    /// The config of the snapshot the fake host has under the test's tag.
    async fn installed_config(fake: &Fake) -> SnapshotConfig {
        SnapshotManager::new(fake.host.snapshots_dir.clone())
            .load_snapshot(TAG)
            .await
            .unwrap()
    }

    /// A host that has neither the image disk nor the pmem entry gets both at the paths the
    /// snapshot recorded, with the exported lengths, hole maps and bytes. The image disk is
    /// the build the config records, the entry is read-only like any the store publishes,
    /// and the config records the entry as it is on this host, so the checks a restore
    /// makes pass. Nothing else is left: no temporary, no reclaim record, and no link from
    /// an image to the entry, which a run records for an image this host does not have.
    #[tokio::test]
    async fn an_import_installs_the_external_files_this_host_lacks() {
        let (fake, externals, archive) = exported_with_externals().await;
        let entry = PathBuf::from(&externals.pmem.path);
        let (disk_before, entry_before) = (file_image(&externals.image_disk), file_image(&entry));
        remove_externals(&fake, &externals);

        let imported = import_snapshot(&host_of(&fake), Cursor::new(archive))
            .await
            .unwrap();

        assert_eq!(
            imported.externals,
            [
                ImportedExternal {
                    role: ExternalRole::ImageDisk,
                    path: externals.image_disk.clone(),
                    len: IMAGE_DISK_LEN,
                    installed: true,
                },
                ImportedExternal {
                    role: ExternalRole::Pmem,
                    path: entry.clone(),
                    len: PMEM_LEN,
                    installed: true,
                },
            ]
        );
        let (disk_after, entry_after) = (file_image(&externals.image_disk), file_image(&entry));
        assert!(
            disk_after.runs.len() > 1 && entry_after.runs.len() > 1,
            "control: the image disk and the pmem entry have holes to keep"
        );
        assert!(
            disk_after == disk_before,
            "the installed image disk is not the exported one"
        );
        assert!(
            entry_after == entry_before,
            "the installed pmem entry is not the exported one"
        );
        assert_eq!(
            std::fs::metadata(&entry).unwrap().permissions().mode() & 0o7777,
            0o400,
            "a store entry is read-only for its owner"
        );

        let config = installed_config(&fake).await;
        let metadata = &config.metadata;
        crate::utils::verify_image_disk_identity(
            &externals.image_disk,
            metadata.image_disk_identity.as_deref().unwrap(),
        )
        .unwrap();
        let device = &metadata.pmem_devices[0];
        assert_eq!(
            device.identity,
            crate::storage::pmem::entry_identity(&std::fs::metadata(&entry).unwrap()),
            "the config records the entry as it is on this host"
        );
        assert_ne!(
            device.identity, externals.pmem.identity,
            "control: the installed entry is another file than the exported one"
        );
        assert_eq!(
            (&device.path, &device.source, &device.mount_path),
            (
                &externals.pmem.path,
                &externals.pmem.source,
                &externals.pmem.mount_path
            ),
            "the import changed more of the device than its identity"
        );
        crate::storage::pmem::check_snapshot_pmem_images(
            &fake.host.data_dir,
            &metadata.pmem_devices,
        )
        .unwrap();

        assert_eq!(
            names_in(&fake.image_cache()),
            [
                "0123456789abcdef.lock",
                externals.image_disk.file_name().unwrap().to_str().unwrap()
            ],
            "the image cache holds the disk and the lock of its image"
        );
        assert_eq!(
            names_in(&pmem_store_dir(&fake)),
            [".lock", entry.file_name().unwrap().to_str().unwrap(), "tmp"],
            "the store holds the entry, its lock and its temp directory"
        );
        assert_eq!(external_temporaries(&fake), Vec::<String>::new());
        remove_snapshot_dir(&fake.dir()).await.unwrap();
    }

    /// External files this host has are kept as they are: the same inodes with the same
    /// modification times, and the config records the pmem entry as it was. The archived
    /// copies are read and written nowhere.
    #[tokio::test]
    async fn external_files_this_host_has_are_kept_and_not_written() {
        let (fake, externals, archive) = exported_with_externals().await;
        let entry = PathBuf::from(&externals.pmem.path);
        let before = (
            inode_and_mtime(&externals.image_disk),
            inode_and_mtime(&entry),
        );
        let store_before = names_in(&pmem_store_dir(&fake));

        let imported = import_snapshot(&host_of(&fake), Cursor::new(archive))
            .await
            .unwrap();

        assert_eq!(
            imported
                .externals
                .iter()
                .map(|external| (external.role, external.installed))
                .collect::<Vec<_>>(),
            [
                (ExternalRole::ImageDisk, false),
                (ExternalRole::Pmem, false)
            ]
        );
        assert_eq!(
            (
                inode_and_mtime(&externals.image_disk),
                inode_and_mtime(&entry)
            ),
            before,
            "the import wrote a file this host already had"
        );
        assert_eq!(
            installed_config(&fake).await.metadata.pmem_devices,
            std::slice::from_ref(&externals.pmem)
        );
        assert_eq!(names_in(&pmem_store_dir(&fake)), store_before);
        assert_eq!(external_temporaries(&fake), Vec::<String>::new());
        remove_snapshot_dir(&fake.dir()).await.unwrap();
    }

    /// This host has another build of the image disk at the recorded path. The import is
    /// refused before it writes anything and the disk is kept. A Docker archive disk, for
    /// which a snapshot records no identity, is kept whatever it holds, which is what a
    /// restore accepts.
    #[tokio::test]
    async fn an_image_disk_of_another_build_on_this_host_is_not_replaced() {
        let (fake, externals, archive) = exported_with_externals().await;
        let mut other = std::fs::read(&externals.image_disk).unwrap();
        other[MIB as usize + 5] ^= 0x01;
        std::fs::write(&externals.image_disk, &other).unwrap();

        let message = refusal(&fake, &host_of(&fake), archive).await;

        assert!(
            message.contains("this host has another build of the image disk")
                && message.contains("image disk build changed"),
            "{message}"
        );
        assert!(
            std::fs::read(&externals.image_disk).unwrap() == other,
            "the refused import changed the image disk"
        );

        let (fake, externals) = fake_snapshot_with_externals();
        let disk = fake.image_cache().join("0123456789abcdef.docker.tar");
        std::fs::rename(&externals.image_disk, &disk).unwrap();
        fake.edit_config(|config| {
            config.metadata.image_disk_path = Some(disk.clone());
            config.metadata.image_disk_identity = None;
        });
        let archive = export_of(&fake).await;
        std::fs::remove_dir_all(fake.dir()).unwrap();
        std::fs::write(&disk, &other).unwrap();
        let before = inode_and_mtime(&disk);

        let imported = import_snapshot(&host_of(&fake), Cursor::new(archive))
            .await
            .unwrap();

        assert!(!imported.externals[0].installed);
        assert_eq!(inode_and_mtime(&disk), before);
        remove_snapshot_dir(&fake.dir()).await.unwrap();
    }

    /// The recorded path of the image disk comes from the archive, and a launch reuses a
    /// file of the image cache by its name. So the path has to be one this host's image
    /// cache gives the disk of an image, whether this host has the file or not: directly
    /// under the cache, with the name a launch gives such a disk. A storage image comes
    /// with the identity its snapshot recorded.
    #[tokio::test]
    async fn an_image_disk_has_a_name_of_this_hosts_image_cache() {
        let (fake, externals, archive) = exported_with_externals().await;

        // Another image cache than the path's, without and with a file at the path.
        let mut elsewhere = host_of(&fake);
        elsewhere.image_cache_dir = fake.root.path().join("another-image-cache");
        let aside = fake.root.path().join("image-disk-aside");
        std::fs::rename(&externals.image_disk, &aside).unwrap();
        let message = refusal(&fake, &elsewhere, archive.clone()).await;
        assert!(
            message.contains("is not a file of this host's image cache")
                && message.contains("another-image-cache"),
            "a disk this host lacks: {message}"
        );
        std::fs::rename(&aside, &externals.image_disk).unwrap();
        let message = refusal(&fake, &elsewhere, archive.clone()).await;
        assert!(
            message.contains("is not a file of this host's image cache")
                && message.contains("another-image-cache"),
            "a disk this host has: {message}"
        );

        // A name the image cache gives no disk, with the recorded build under it.
        let misnamed = fake.image_cache().join("layers.img");
        std::fs::copy(&externals.image_disk, &misnamed).unwrap();
        let mut unpacked = Unpacked::of(&archive);
        unpacked.manifest.externals[0].path = misnamed.clone();
        unpacked.edit_config(|config| config.metadata.image_disk_path = Some(misnamed.clone()));
        let message = refusal(&fake, &host_of(&fake), unpacked.pack()).await;
        assert!(
            message.contains("is not a name this host's image cache gives an image disk"),
            "a file of the cache under another name: {message}"
        );
        std::fs::remove_file(&misnamed).unwrap();

        // A storage image the snapshot recorded no identity for.
        let mut unpacked = Unpacked::of(&archive);
        unpacked.manifest.externals[0].sha256 = None;
        unpacked.edit_config(|config| config.metadata.image_disk_identity = None);
        let message = refusal(&fake, &host_of(&fake), unpacked.pack()).await;
        assert!(
            message.contains("records no identity for it"),
            "a storage image without an identity: {message}"
        );

        // Control: the archive as it was exported imports on the same host.
        let imported = import_snapshot(&host_of(&fake), Cursor::new(archive))
            .await
            .unwrap();
        assert!(!imported.externals[0].installed);
        remove_snapshot_dir(&fake.dir()).await.unwrap();
    }

    /// A store entry's name is a digest of the uid it belongs to and of its bytes. An entry
    /// this host lacks would be published for this import's uid, so pmem images that belong
    /// to another uid on the exporting host are refused before anything is written, with a
    /// message that names both uids. So is an entry that is not named in this host's store.
    #[tokio::test]
    async fn pmem_entries_of_another_uid_or_another_store_are_refused() {
        let (fake, externals, archive) = exported_with_externals().await;
        std::fs::remove_file(&externals.pmem.path).unwrap();
        let here = nix::unistd::getuid().as_raw();

        let mut unpacked = Unpacked::of(&archive);
        assert_eq!(
            unpacked.manifest.pmem_owner_uid,
            Some(here),
            "control: the export recorded this uid as the entries' owner"
        );
        unpacked.manifest.pmem_owner_uid = Some(here + 1);
        let message = refusal(&fake, &host_of(&fake), unpacked.pack()).await;
        assert!(
            message.contains(&format!("belong to uid {} on the exporting host", here + 1))
                && message.contains(&format!("publishes store entries for uid {here}"))
                && message.contains("Run the import as the uid the entries belong to"),
            "{message}"
        );

        let mut unpacked = Unpacked::of(&archive);
        let elsewhere = PathBuf::from("/example/another-data/pmem")
            .join(Path::new(&externals.pmem.path).file_name().unwrap());
        unpacked.manifest.externals[1].path = elsewhere.clone();
        unpacked.edit_config(|config| {
            config.metadata.pmem_devices[0].path = elsewhere.to_string_lossy().into_owned();
        });
        let message = refusal(&fake, &host_of(&fake), unpacked.pack()).await;
        assert!(
            message.contains("is not an entry of this host's pmem image store"),
            "{message}"
        );
    }

    /// The archive verifies, and the bytes it carries for an external file are not the ones
    /// the snapshot recorded: an image disk of another build, and pmem bytes that are not
    /// the ones the entry is named for. Each fails the import after the file is staged, and
    /// nothing is left: no file at the recorded path, no temporary, no tag.
    #[tokio::test]
    async fn external_bytes_that_are_not_the_recorded_ones_install_nothing() {
        let (fake, externals, archive) = exported_with_externals().await;
        let entry = PathBuf::from(&externals.pmem.path);
        remove_externals(&fake, &externals);
        let changed = |index: usize| {
            let mut unpacked = Unpacked::of(&archive);
            unpacked.externals[index][MIB as usize + 5] ^= 0x01;
            unpacked.pack()
        };

        let message = failure_that_leaves_nothing(&fake, changed(0)).await;
        assert!(
            message.contains("is not the build the snapshot was taken against"),
            "another image disk: {message}"
        );
        assert_eq!(external_temporaries(&fake), Vec::<String>::new());

        let message = failure_that_leaves_nothing(&fake, changed(1)).await;
        assert!(
            message.contains("the pmem image the archive carries for")
                && message.contains("belongs to another uid"),
            "other pmem bytes: {message}"
        );
        assert_eq!(external_temporaries(&fake), Vec::<String>::new());

        assert!(
            !externals.image_disk.exists() && !entry.exists(),
            "a failed import published an external file"
        );

        // Control: the archive as it was exported installs both.
        import_snapshot(&host_of(&fake), Cursor::new(archive))
            .await
            .unwrap();
        assert!(externals.image_disk.is_file() && entry.is_file());
        remove_snapshot_dir(&fake.dir()).await.unwrap();
    }

    /// Nothing is published before the trailer verifies. A stream cut inside an external
    /// file, and an archive whose memory image has a flipped byte, which is known only
    /// when the trailer is read after both external files are staged whole, both leave no
    /// external file, no temporary and no tag.
    #[tokio::test]
    async fn a_cut_or_corrupt_archive_publishes_no_external_file() {
        let (fake, externals, archive) = exported_with_externals().await;
        let entry = PathBuf::from(&externals.pmem.path);
        remove_externals(&fake, &externals);

        // The pmem entry's second data run, which is the last data of the archive.
        let cut = position_of(&archive, &pattern(22, 64)) + 100;
        let message = failure_that_leaves_nothing(&fake, archive[..cut].to_vec()).await;
        assert!(
            message.contains("ended inside the data of external/1"),
            "{message}"
        );
        assert_eq!(external_temporaries(&fake), Vec::<String>::new());
        assert!(!externals.image_disk.exists() && !entry.exists());

        let mut corrupt = archive.clone();
        let at = position_of(&corrupt, &pattern(2, 64));
        corrupt[at + 10] ^= 0x01;
        let message = failure_that_leaves_nothing(&fake, corrupt).await;
        assert!(
            message.contains("snapshot/memory.bin is corrupt"),
            "{message}"
        );
        assert_eq!(external_temporaries(&fake), Vec::<String>::new());
        assert!(
            !externals.image_disk.exists() && !entry.exists(),
            "an archive that did not verify published an external file"
        );
    }

    /// An import parked once every external file is staged and before the archive has
    /// verified, with what it has staged by then.
    struct StagedImport {
        import: tokio::task::JoinHandle<Result<Imported>>,
        release: std::sync::mpsc::Sender<()>,
    }

    /// Start an import of `archive` and wait until it has read past the last external
    /// file's data: the input stops at the read that looks for the end of the archive.
    async fn import_parked_with_externals_staged(fake: &Fake, archive: Vec<u8>) -> StagedImport {
        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let input = Parked {
            park_after: position_of(&archive, &pattern(22, 64)) as u64,
            archive: Cursor::new(archive),
            entered: entered_tx,
            release: release_rx,
            parked: false,
        };
        let host = host_of(fake);
        let import = tokio::spawn(async move { import_snapshot(&host, input).await });
        tokio::time::timeout(Duration::from_secs(60), entered_rx.recv())
            .await
            .expect("the import never read past the last external file")
            .unwrap();
        StagedImport {
            import,
            release: release_tx,
        }
    }

    /// While the archive has not verified, an external file this host lacks is in a
    /// temporary of its store and has no name of its own: the image disk beside its path
    /// with a reclaim record, the pmem entry under the store's `tmp/` with the store's
    /// lock held, so that a collection of abandoned temps removes nothing. Once the import
    /// ends, both are at their paths and no temporary is left.
    #[tokio::test]
    async fn external_files_are_staged_in_their_stores_until_the_archive_has_verified() {
        let (fake, externals, archive) = exported_with_externals().await;
        let entry = PathBuf::from(&externals.pmem.path);
        remove_externals(&fake, &externals);

        let staged = import_parked_with_externals_staged(&fake, archive).await;

        let temporaries = external_temporaries(&fake);
        // A reclaim record is a `.json` file and its lock.
        let count = |prefix: &str| {
            temporaries
                .iter()
                .filter(|name| name.starts_with(prefix) && !name.ends_with(".lock"))
                .count()
        };
        assert_eq!(
            (count("image-cache/"), count("pmem/tmp/"), count("reclaim/")),
            (1, 1, 1),
            "the staged image disk, the staged pmem entry and the image disk's reclaim \
             record: {temporaries:?}"
        );
        assert!(
            !externals.image_disk.exists() && !entry.exists() && !fake.dir().exists(),
            "an external file or the tag has its name before the archive has verified"
        );
        assert_eq!(
            crate::storage::pmem_store::collect_abandoned_temps(&fake.host.data_dir).unwrap(),
            0,
            "a collection of abandoned temps removed the temp of an import that is running"
        );
        assert_eq!(count("pmem/tmp/"), 1);

        staged.release.send(()).unwrap();
        staged.import.await.unwrap().unwrap();

        assert!(externals.image_disk.is_file() && entry.is_file());
        assert_eq!(external_temporaries(&fake), Vec::<String>::new());
        remove_snapshot_dir(&fake.dir()).await.unwrap();
    }

    /// The lines of `/proc/locks` that are requests waiting for a `flock` of the file at
    /// `path`: the kernel lists a blocked request under the lock it waits behind, marked
    /// `->`, with the file's inode number.
    fn flock_waiters(path: &Path) -> usize {
        use std::os::unix::fs::MetadataExt;
        let inode = std::fs::metadata(path).unwrap().ino();
        std::fs::read_to_string("/proc/locks")
            .unwrap()
            .lines()
            .filter(|line| {
                line.contains("-> FLOCK") && line.trim_end().ends_with(&format!(":{inode} 0 EOF"))
            })
            .count()
    }

    /// A launch holds the lock of an image's cache entries from its look for the image's
    /// disk through the rename that publishes the one it built, and that rename replaces. An
    /// import publishes the disk under the same lock: while a build holds it the import
    /// waits, with the disk still in its temporary, and it then meets the build's file as
    /// one that appeared meanwhile, which it keeps and checks.
    #[tokio::test]
    async fn the_image_disk_is_published_under_the_lock_of_its_image() {
        let (fake, externals, archive) = exported_with_externals().await;
        let mut other = std::fs::read(&externals.image_disk).unwrap();
        other[MIB as usize + 5] ^= 0x01;
        std::fs::remove_file(&externals.image_disk).unwrap();

        // What a launch of the image holds while it builds the disk.
        let digest = externals
            .image_disk
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .split('.')
            .next()
            .unwrap()
            .to_string();
        let lock_path = fake.image_cache().join(format!("{digest}.lock"));
        let build = File::create(&lock_path).unwrap();
        fs2::FileExt::lock_exclusive(&build).unwrap();

        let host = host_of(&fake);
        let import =
            tokio::spawn(async move { import_snapshot(&host, Cursor::new(archive)).await });
        let deadline = Instant::now() + Duration::from_secs(60);
        while flock_waiters(&lock_path) == 0 {
            assert!(
                !import.is_finished(),
                "the import ended while a build held the lock of the image: it published \
                 the image disk (present: {}) without the lock",
                externals.image_disk.exists()
            );
            assert!(
                Instant::now() < deadline,
                "the import neither ended nor asked for the lock of the image"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            !externals.image_disk.exists() && !fake.dir().exists(),
            "the import published before it had the lock of the image"
        );

        // The build publishes its disk, another build than the snapshot's, and lets go.
        std::fs::write(&externals.image_disk, &other).unwrap();
        drop(build);
        let error = match import.await.unwrap() {
            Ok(imported) => panic!("the import installed {}", imported.tag),
            Err(error) => format!("{error:#}"),
        };

        assert!(
            error.contains("another build of the image disk")
                && error.contains("appeared on this host while the archive was imported"),
            "{error}"
        );
        assert!(
            std::fs::read(&externals.image_disk).unwrap() == other,
            "the import replaced the disk the build published"
        );
        assert!(!fake.dir().exists() && !staging_of(&fake).exists());
    }

    /// The staged disk is private until it is linked, and the larger the disk, the longer
    /// its flush. It is synced before the lock of its image is taken, so a launch of the
    /// image never waits behind that flush, and it is linked under the lock.
    #[test]
    fn the_staged_image_disk_is_synced_before_the_lock_of_its_image_is_taken() {
        let source = include_str!("snapshot_import.rs");
        let from = source
            .find("\nfn settle_image_disk(")
            .expect("no settle_image_disk");
        let body = &source[from..];
        let body = &body[..body.find("\n}\n").expect("unterminated fn")];
        let at = |what: &str| {
            let found: Vec<usize> = body.match_indices(what).map(|(at, _)| at).collect();
            assert_eq!(found.len(), 1, "{what:?} in settle_image_disk: {found:?}");
            found[0]
        };
        let (sync, lock, link) = (
            at("file.sync_all()"),
            at("lock_image_cache_entries("),
            at("hard_link("),
        );
        assert!(
            sync < lock,
            "the staged image disk is synced with the lock of its image held"
        );
        assert!(
            lock < link,
            "the image disk is linked before the lock of its image is taken"
        );
    }

    /// A recorded image disk identity is `sha256:` and 64 lowercase hex digits. An archive
    /// whose config records anything else is refused before anything is written, also when
    /// its manifest repeats it.
    #[tokio::test]
    async fn an_archive_whose_image_disk_identity_is_not_a_digest_is_refused() {
        let (fake, externals, archive) = exported_with_externals().await;
        std::fs::remove_file(&externals.image_disk).unwrap();
        let upper = "0123456789ABCDEF".repeat(4);

        for digest in ["abc", upper.as_str()] {
            let mut unpacked = Unpacked::of(&archive);
            unpacked.manifest.externals[0].sha256 = Some(digest.to_string());
            unpacked.edit_config(|config| {
                config.metadata.image_disk_identity = Some(format!("sha256:{digest}"));
            });
            let message = refusal(&fake, &host_of(&fake), unpacked.pack()).await;
            assert!(
                message.contains("which is not the SHA-256 of its bytes"),
                "{digest}: {message}"
            );
        }
    }

    /// A Docker archive disk (`<digest>.docker.tar`) has no recorded identity: a launch
    /// records one for a storage image only. An import installs one this host lacks as the
    /// archive carries it, with no check of its bytes beyond the archive's own.
    #[tokio::test]
    async fn an_image_disk_without_a_recorded_identity_is_installed_as_the_archive_carries_it() {
        let (fake, externals, archive) = exported_with_externals().await;
        let disk = fake.image_cache().join("0123456789abcdef.docker.tar");
        let before = std::fs::read(&externals.image_disk).unwrap();
        std::fs::remove_file(&externals.image_disk).unwrap();
        let mut unpacked = Unpacked::of(&archive);
        unpacked.manifest.externals[0].path = disk.clone();
        unpacked.manifest.externals[0].sha256 = None;
        unpacked.edit_config(|config| {
            config.metadata.image_disk_path = Some(disk.clone());
            config.metadata.image_disk_identity = None;
        });

        let imported = import_snapshot(&host_of(&fake), Cursor::new(unpacked.pack()))
            .await
            .unwrap();

        assert_eq!(
            (
                imported.externals[0].role,
                &imported.externals[0].path,
                imported.externals[0].installed
            ),
            (ExternalRole::ImageDisk, &disk, true)
        );
        assert!(
            std::fs::read(&disk).unwrap() == before,
            "the installed image disk does not hold the archived bytes"
        );
        assert_eq!(
            installed_config(&fake).await.metadata.image_disk_identity,
            None
        );
        assert_eq!(external_temporaries(&fake), Vec::<String>::new());
        remove_snapshot_dir(&fake.dir()).await.unwrap();
    }

    /// What has the image disk's name on this host has to be a regular file: a directory
    /// there is refused, and so is a link to the disk, which a launch would follow to a
    /// file that is not the image cache's.
    #[tokio::test]
    async fn an_image_disk_that_is_not_a_regular_file_is_refused() {
        let (fake, externals, archive) = exported_with_externals().await;
        let aside = fake.root.path().join("image-disk-aside");
        std::fs::rename(&externals.image_disk, &aside).unwrap();

        std::fs::create_dir(&externals.image_disk).unwrap();
        let message = refusal(&fake, &host_of(&fake), archive.clone()).await;
        assert!(
            message.contains("which on this host is not a regular file"),
            "a directory: {message}"
        );
        std::fs::remove_dir(&externals.image_disk).unwrap();

        std::os::unix::fs::symlink(&aside, &externals.image_disk).unwrap();
        let message = refusal(&fake, &host_of(&fake), archive.clone()).await;
        assert!(
            message.contains("which on this host is not a regular file"),
            "a link to the recorded build: {message}"
        );
        std::fs::remove_file(&externals.image_disk).unwrap();

        // Control: with the file itself at the path, the archive imports.
        std::fs::rename(&aside, &externals.image_disk).unwrap();
        import_snapshot(&host_of(&fake), Cursor::new(archive))
            .await
            .unwrap();
        remove_snapshot_dir(&fake.dir()).await.unwrap();
    }

    /// A pmem entry this host has at the recorded path has to pass the store's own check of
    /// an entry: one that is not the read-only file the store publishes is refused before
    /// anything is written, and is left as it is.
    #[tokio::test]
    async fn a_pmem_entry_this_run_cannot_use_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let (fake, externals, archive) = exported_with_externals().await;
        let entry = PathBuf::from(&externals.pmem.path);
        std::fs::set_permissions(&entry, std::fs::Permissions::from_mode(0o644)).unwrap();

        let message = refusal(&fake, &host_of(&fake), archive.clone()).await;

        assert!(
            message.contains("is not an entry this import can use")
                && message.contains("is not a mode 0400 regular file"),
            "{message}"
        );

        // Control: the entry as the store published it is accepted.
        std::fs::set_permissions(&entry, std::fs::Permissions::from_mode(0o400)).unwrap();
        import_snapshot(&host_of(&fake), Cursor::new(archive))
            .await
            .unwrap();
        remove_snapshot_dir(&fake.dir()).await.unwrap();
    }

    /// An image disk this host had when the import began is not written, so the archive's
    /// copy is gone by the time the import publishes. A disk that was removed meanwhile
    /// fails the import, which leaves nothing.
    #[tokio::test]
    async fn an_image_disk_that_is_gone_when_the_import_publishes_fails_it() {
        let (fake, externals, archive) = exported_with_externals().await;

        let staged = import_parked_with_externals_staged(&fake, archive).await;
        assert_eq!(
            external_temporaries(&fake),
            Vec::<String>::new(),
            "control: the import staged a file this host has"
        );
        std::fs::remove_file(&externals.image_disk).unwrap();
        staged.release.send(()).unwrap();
        let error = match staged.import.await.unwrap() {
            Ok(imported) => panic!("the import installed {}", imported.tag),
            Err(error) => format!("{error:#}"),
        };

        assert!(error.contains("is gone from this host"), "{error}");
        assert!(!fake.dir().exists() && !staging_of(&fake).exists());
        assert!(!externals.image_disk.exists());
    }

    /// The import fails after it has published its external files: the tag's name is taken
    /// while the import stages, so the rename that publishes the snapshot is refused. The
    /// external files stay, whole and under their names, as entries of their stores. The
    /// tag is not installed and the staging directory is removed.
    #[tokio::test]
    async fn a_failure_after_the_external_files_are_published_keeps_them_and_no_tag() {
        let (fake, externals, archive) = exported_with_externals().await;
        let entry = PathBuf::from(&externals.pmem.path);
        let (disk_before, entry_before) = (file_image(&externals.image_disk), file_image(&entry));
        remove_externals(&fake, &externals);

        let staged = import_parked_with_externals_staged(&fake, archive).await;
        std::fs::create_dir(fake.dir()).unwrap();
        staged.release.send(()).unwrap();
        let error = match staged.import.await.unwrap() {
            Ok(imported) => panic!("the import installed {}", imported.tag),
            Err(error) => format!("{error:#}"),
        };

        assert!(
            error.contains("appeared on this host while its archive was imported"),
            "{error}"
        );
        assert!(
            file_image(&externals.image_disk) == disk_before && file_image(&entry) == entry_before,
            "the external files the import published are not the exported ones"
        );
        assert_eq!(
            files_under(&fake.dir()),
            Vec::<PathBuf>::new(),
            "the import wrote into the directory that took the tag's name"
        );
        assert!(
            !staging_of(&fake).exists(),
            "the failed import left its staging directory"
        );
        assert_eq!(staging_records(&fake), Vec::<String>::new());
        assert_eq!(external_temporaries(&fake), Vec::<String>::new());
    }

    /// The import hashed the image disk as it wrote it, and records that digest for the
    /// published file, so the first check of the disk against the snapshot does not read
    /// it whole. The test changes a byte the lookup's fingerprint does not sample and puts
    /// the modification time back: a check that read the disk would find another digest.
    #[tokio::test]
    async fn the_digest_of_an_installed_image_disk_is_recorded_for_its_first_check() {
        let (fake, externals, archive) = exported_with_externals().await;
        std::fs::remove_file(&externals.image_disk).unwrap();
        import_snapshot(&host_of(&fake), Cursor::new(archive))
            .await
            .unwrap();
        let recorded = installed_config(&fake)
            .await
            .metadata
            .image_disk_identity
            .unwrap();

        let disk = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&externals.image_disk)
            .unwrap();
        let modified = disk.metadata().unwrap().modified().unwrap();
        // Past the first MiB and between two of the blocks the fingerprint reads.
        let at = MIB + 8192;
        let mut byte = [0u8; 1];
        disk.read_exact_at(&mut byte, at).unwrap();
        disk.write_all_at(&[byte[0] ^ 0x01], at).unwrap();
        disk.set_modified(modified).unwrap();

        crate::utils::verify_image_disk_identity(&externals.image_disk, &recorded)
            .expect("the installed disk was read to learn its digest");
        remove_snapshot_dir(&fake.dir()).await.unwrap();
    }

    /// Another process publishes the same external files while the import has them staged:
    /// a launch that built the image disk, and a `--pmem` run of the same image. The import
    /// keeps the files that have the names, removes its own copies, and records the pmem
    /// entry that is there.
    #[tokio::test]
    async fn external_files_that_appear_while_the_import_stages_are_kept() {
        let (fake, externals, archive) = exported_with_externals().await;
        let entry = PathBuf::from(&externals.pmem.path);
        let disk_bytes = std::fs::read(&externals.image_disk).unwrap();
        remove_externals(&fake, &externals);

        let staged = import_parked_with_externals_staged(&fake, archive).await;
        std::fs::write(&externals.image_disk, &disk_bytes).unwrap();
        let store = PmemStore::open(&fake.host.data_dir).unwrap();
        assert_eq!(
            store.ingest(Path::new(&externals.pmem.source)).unwrap(),
            entry,
            "control: a run of the same image publishes the recorded entry"
        );
        let appeared = (
            inode_and_mtime(&externals.image_disk),
            inode_and_mtime(&entry),
        );

        staged.release.send(()).unwrap();
        let imported = staged.import.await.unwrap().unwrap();

        assert_eq!(
            imported
                .externals
                .iter()
                .map(|external| external.installed)
                .collect::<Vec<_>>(),
            [false, false],
            "the import reports a file it did not install"
        );
        assert_eq!(
            (
                inode_and_mtime(&externals.image_disk),
                inode_and_mtime(&entry)
            ),
            appeared,
            "the import replaced a file that was published while it staged"
        );
        assert_eq!(
            installed_config(&fake).await.metadata.pmem_devices[0].identity,
            crate::storage::pmem::entry_identity(&std::fs::metadata(&entry).unwrap()),
            "the config records the entry that is in the store"
        );
        assert_eq!(external_temporaries(&fake), Vec::<String>::new());
        remove_snapshot_dir(&fake.dir()).await.unwrap();
    }

    /// Another build of the image disk appears at the recorded path while the import has
    /// its own staged. The import fails, the file that appeared is kept, and nothing of the
    /// import is left.
    #[tokio::test]
    async fn another_build_that_appears_while_the_import_stages_fails_it_and_is_kept() {
        let (fake, externals, archive) = exported_with_externals().await;
        let mut other = std::fs::read(&externals.image_disk).unwrap();
        other[MIB as usize + 5] ^= 0x01;
        remove_externals(&fake, &externals);

        let staged = import_parked_with_externals_staged(&fake, archive).await;
        std::fs::write(&externals.image_disk, &other).unwrap();

        staged.release.send(()).unwrap();
        let error = match staged.import.await.unwrap() {
            Ok(imported) => panic!("the import installed {}", imported.tag),
            Err(error) => format!("{error:#}"),
        };

        assert!(
            error.contains("another build of the image disk")
                && error.contains("appeared on this host while the archive was imported"),
            "{error}"
        );
        assert!(
            std::fs::read(&externals.image_disk).unwrap() == other,
            "the import replaced the image disk that appeared"
        );
        assert!(!fake.dir().exists() && !staging_of(&fake).exists());
        assert_eq!(external_temporaries(&fake), Vec::<String>::new());
    }

    /// An input that stops before the first read past `park_after` bytes of the archive
    /// until the test lets it go on. Past the start of the archive, that read is inside the
    /// memory image's data.
    struct Parked {
        archive: Cursor<Vec<u8>>,
        park_after: u64,
        entered: tokio::sync::mpsc::UnboundedSender<()>,
        release: std::sync::mpsc::Receiver<()>,
        parked: bool,
    }

    impl Read for Parked {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if !self.parked && self.archive.position() > self.park_after {
                self.parked = true;
                self.entered.send(()).unwrap();
                self.release.recv().unwrap();
            }
            self.archive.read(buf)
        }
    }

    /// While an import writes, the tag's lock is held exclusively (a restore, which takes
    /// it shared, waits), the staging directory has its reclaim record, and the tag does
    /// not exist. When the import ends the tag exists, the record is gone and the lock is
    /// free.
    #[tokio::test]
    async fn an_import_stages_under_the_tags_lock_with_its_staging_directory_recorded() {
        let (fake, archive) = exported().await;
        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let input = Parked {
            archive: Cursor::new(archive),
            park_after: 0,
            entered: entered_tx,
            release: release_rx,
            parked: false,
        };
        let host = host_of(&fake);
        let import = tokio::spawn(async move { import_snapshot(&host, input).await });
        tokio::time::timeout(Duration::from_secs(60), entered_rx.recv())
            .await
            .expect("the import never read past the start of the archive")
            .unwrap();

        let dir = fake.dir();
        assert!(
            staging_of(&fake).join("disk").is_dir(),
            "control: the import is staging when its input stops"
        );
        assert!(
            !dir.exists(),
            "the tag exists before the archive has verified"
        );
        assert!(
            !staging_records(&fake).is_empty(),
            "the staging directory has no reclaim record while the import writes into it"
        );
        let mut restore = tokio::spawn({
            let dir = dir.clone();
            async move { acquire_snapshot_dir_lock(&dir, false).await }
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(500), &mut restore)
                .await
                .is_err(),
            "a restore took the tag's lock while its import was still writing"
        );

        release_tx.send(()).unwrap();
        import.await.unwrap().unwrap();
        tokio::time::timeout(Duration::from_secs(30), restore)
            .await
            .expect("the restore did not get the lock after the import ended")
            .unwrap()
            .unwrap();
        assert!(dir.join(CONFIG_FILE).is_file());
        assert_eq!(staging_records(&fake), Vec::<String>::new());
        remove_snapshot_dir(&dir).await.unwrap();
    }

    /// A directory appears under the tag's name while the import stages, made by something
    /// that does not take the tag's lock. The import does not replace it, whether it is
    /// empty (a plain rename would replace an empty directory) or holds a file: the
    /// publishing rename fails, the directory is as it was made, and the import leaves
    /// nothing.
    #[tokio::test]
    async fn a_tag_that_appears_while_the_import_stages_is_kept() {
        for holds_a_file in [false, true] {
            let (fake, archive) = exported().await;
            let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
            let input = Parked {
                archive: Cursor::new(archive),
                park_after: 0,
                entered: entered_tx,
                release: release_rx,
                parked: false,
            };
            let host = host_of(&fake);
            let import = tokio::spawn(async move { import_snapshot(&host, input).await });
            tokio::time::timeout(Duration::from_secs(60), entered_rx.recv())
                .await
                .expect("the import never read past the start of the archive")
                .unwrap();
            let dir = fake.dir();
            assert!(
                staging_of(&fake).join("disk").is_dir() && !dir.exists(),
                "control: the import is staging, and the tag does not exist"
            );
            std::fs::create_dir(&dir).unwrap();
            let mut made = Vec::new();
            if holds_a_file {
                std::fs::write(dir.join(CONFIG_FILE), b"a snapshot made without the lock").unwrap();
                made.push(PathBuf::from(CONFIG_FILE));
            }

            release_tx.send(()).unwrap();
            let error = match import.await.unwrap() {
                Ok(imported) => panic!(
                    "the import replaced the directory (holds a file: {holds_a_file}) with {}",
                    imported.tag
                ),
                Err(error) => format!("{error:#}"),
            };

            assert!(
                error.contains("appeared on this host while its archive was imported"),
                "holds a file: {holds_a_file}: {error}"
            );
            assert!(dir.is_dir());
            assert_eq!(tree(&dir), made, "holds a file: {holds_a_file}");
            if holds_a_file {
                assert_eq!(
                    std::fs::read(dir.join(CONFIG_FILE)).unwrap(),
                    b"a snapshot made without the lock"
                );
            }
            assert!(!staging_of(&fake).exists());
            assert_eq!(staging_records(&fake), Vec::<String>::new());
        }
    }

    /// An input that records, for every read, how many bytes were asked for and given.
    struct Measured {
        archive: Cursor<Vec<u8>>,
        reads: std::sync::Arc<std::sync::Mutex<Vec<(usize, usize)>>>,
    }

    impl Read for Measured {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let given = self.archive.read(buf)?;
            self.reads.lock().unwrap().push((buf.len(), given));
            Ok(given)
        }
    }

    /// The files' data goes from the input into the reader's own buffer in one copy: nearly
    /// all of the archive is read with requests larger than the input buffer, which a
    /// `BufReader` passes to its input without copying through itself.
    #[tokio::test]
    async fn file_data_is_read_past_the_input_buffer() {
        let (fake, archive) = exported().await;
        let len = archive.len();
        let reads = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let input = Measured {
            archive: Cursor::new(archive),
            reads: reads.clone(),
        };

        import_snapshot(&host_of(&fake), input).await.unwrap();

        let direct: usize = reads
            .lock()
            .unwrap()
            .iter()
            .filter(|(asked, _)| *asked > INPUT_BUFFER)
            .map(|(_, given)| given)
            .sum();
        assert!(
            direct * 10 >= len * 9,
            "{direct} of the archive's {len} bytes were read past the {INPUT_BUFFER}-byte \
             input buffer; the rest was copied through it"
        );
        remove_snapshot_dir(&fake.dir()).await.unwrap();
    }

    /// `needles` appear in `body` in this order.
    fn assert_in_order(what: &str, body: &str, needles: &[&str]) {
        let mut from = 0;
        for needle in needles {
            let at = body[from..]
                .find(needle)
                .unwrap_or_else(|| panic!("{what}: no `{needle}` after byte {from}"));
            from += at + needle.len();
        }
    }

    /// A function's source, from its signature to the first closing brace in column one.
    fn body_of<'a>(source: &'a str, signature: &str) -> &'a str {
        let start = source
            .find(signature)
            .unwrap_or_else(|| panic!("no `{signature}`"));
        let end = source[start..]
            .find("\n}\n")
            .unwrap_or_else(|| panic!("`{signature}` has no end"));
        &source[start..start + end]
    }

    /// The command reclaims before it imports. `import_snapshot` is what the tests above
    /// call, so the command's own first step is pinned by its source;
    /// `a_sigkilled_import_leaves_its_staging_directory_to_the_next_import`
    /// (tests/test_reclaim.rs) kills a real import and runs the next.
    #[test]
    fn the_command_reclaims_before_it_imports() {
        let source = include_str!("snapshot_import.rs");
        assert_in_order(
            "cmd_snapshot_import",
            body_of(source, "pub async fn cmd_snapshot_import("),
            &["reclaim_abandoned().await", "import_snapshot("],
        );
    }

    /// The staging directory is recorded for reclaim under the tag's exclusive lock, before
    /// a leftover one is cleared and before it is made, as each snapshot creator does
    /// (`every_snapshot_creator_records_its_staging_directory_before_making_it`).
    #[test]
    fn the_staging_directory_is_recorded_before_it_is_cleared_or_made() {
        let source = include_str!("snapshot_import.rs");
        assert_in_order(
            "install",
            body_of(source, "async fn install<R>("),
            &[
                "acquire_snapshot_dir_lock(&dir, true)",
                "ensure_tag_is_free(",
                "let _staging = record_snapshot_staging_in(",
                "clear_staging(",
                "stage_and_publish(",
            ],
        );
        assert_in_order(
            "stage_and_publish",
            body_of(source, "async fn stage_and_publish<R>("),
            &[
                "create_dir_or_subvolume(staging)",
                "create_dir_or_subvolume(&snapshot_disk_dir(staging))",
                "stage_files(",
            ],
        );
    }

    /// Once the archive has verified, the staged files are checked, the external files are
    /// published and the config is written with what they are on this host, and the
    /// directory is synced and only then renamed. A test cannot see a sync from outside, so
    /// the order is pinned by the source.
    #[test]
    fn the_staged_files_are_synced_before_they_are_published() {
        let source = include_str!("snapshot_import.rs");
        assert_in_order(
            "stage_files",
            body_of(source, "fn stage_files<R: Read>("),
            &[
                "reader.next_entry()",
                "check_generation_files(",
                "\"snapshot.import.externals_staged\"",
                "publish_externals(",
                "pmem_identities(",
                "imported_config_json(",
                "sync_generation(",
            ],
        );
        assert_in_order(
            "stage_and_publish",
            body_of(source, "async fn stage_and_publish<R>("),
            &[
                "stage_files(",
                "\"snapshot.import.pre_publish\"",
                "publish(&root, &staging, &dir, &tag)",
            ],
        );
    }
}

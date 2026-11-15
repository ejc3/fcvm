//! `fcvm snapshot import`: install the snapshot that an archive from `snapshot export`
//! carries (`storage::snapshot_archive`), read from a file or from stdin.
//!
//! The archive holds the snapshot's directory, and the import installs it under the tag it
//! was exported from. What a restore reads outside the directory is not carried by this
//! version. An archive whose manifest lists external files is refused, and a snapshot that
//! refers to an image disk or to pmem images is refused unless this host already has them
//! as the snapshot recorded them. For pmem images that is the exporting host only: the
//! snapshot records each store entry by inode, length and modification time.
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
//! - the image disk the snapshot refers to is a file here, with the digest the snapshot
//!   recorded if it recorded one, and its pmem images are the store entries it recorded
//!   (`verify_image_disk_identity` and `check_snapshot_pmem_images`, the checks a restore
//!   makes);
//! - every extra disk is named by one path component;
//! - the tag does not exist here.
//!
//! None of this is trusted yet: the reader verifies nothing until it has read the trailer.
//! The checks decide whether to start, and the trailer decides whether to publish.
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
//! `config.json` is written last: the archived one with a new generation id and no parent
//! snapshot, and every other field as it was. Once the reader has verified the trailer, the
//! staged directory is checked for the files a restore opens (`check_generation_files`, as
//! `podman prepare` checks a generation), synced, and renamed to `<tag>`. That rename is
//! the only step that makes the tag exist, and it replaces nothing: it is made with
//! `RENAME_NOREPLACE`, which the kernel refuses when anything has the name. A cut stream, a
//! digest that does not match, or any other failure removes the staging directory. A killed
//! import leaves it to the next import or the next VM launch, both of which reclaim first.

use std::fs::File;
use std::io::{BufReader, IsTerminal, Read, Write};
use std::os::fd::AsFd;
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
use super::snapshot_export::{not_shipped, sha256_hex, CONFIG_FILE};
use crate::cli::SnapshotImportArgs;
use crate::hypervisor::Backend;
use crate::paths;
use crate::storage::disk::{clear_staging, create_dir_or_subvolume, remove_snapshot_dir};
use crate::storage::snapshot::{
    check_extra_disk_names, check_generation_files, parse_snapshot_config, snapshot_disk_dir,
    SNAPSHOT_DISK_FILE, SNAPSHOT_MEMORY_FILE, SNAPSHOT_VMSTATE_FILE,
};
use crate::storage::snapshot_archive::{ArchiveEntry, ArchiveManifest, ArchiveReader, EntryName};
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
    /// Whether the kernel profile of this name is configured for this architecture.
    pub kernel_profile_configured: fn(&str) -> Result<bool>,
}

impl ImportHost {
    fn configured() -> Self {
        Self {
            snapshots_dir: paths::snapshot_dir(),
            data_dir: paths::data_dir(),
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
    /// What a restore of the installed snapshot reads that the archive did not carry, one
    /// sentence each, from the installed config.
    pub not_shipped: Vec<String>,
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
/// On an error this import has installed nothing and its staging directory is removed,
/// unless the error says the snapshot is installed.
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
    let files = {
        let (staging, generation_lock) = (staging.to_path_buf(), generation_lock.clone());
        tokio::task::spawn_blocking(move || {
            // Held until the last file is written, or the write has failed.
            let _generation_lock = generation_lock;
            stage_files(reader, &staging, &archive)
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
    Ok((
        reader,
        CheckedArchive {
            manifest,
            config,
            config_json,
            config_entry,
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
        manifest.externals.is_empty(),
        "the archive carries {} files from outside the snapshot's directory (an image disk \
         or pmem images), which this fcvm does not import",
        manifest.externals.len()
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

    // Last, because the image disk is read whole. The archive carries neither the image
    // disk nor the pmem images, and a restore opens both at the paths the snapshot recorded,
    // so they have to be on this host already. A snapshot records the digest of an overlay
    // storage image only; where it recorded one, the disk has to have it, which is the
    // check a restore makes. The file has to be there either way.
    let metadata = &config.metadata;
    if let Some(disk) = metadata.image_disk_path.as_deref() {
        ensure!(
            std::fs::metadata(disk).is_ok_and(|file| file.is_file()),
            "the snapshot reads the image disk {}, which the archive does not carry and this \
             host does not have",
            disk.display()
        );
        if let Some(identity) = metadata.image_disk_identity.as_deref() {
            crate::utils::verify_image_disk_identity(disk, identity).with_context(|| {
                format!(
                    "the snapshot reads the image disk {}, which the archive does not carry \
                     and this host does not have as the snapshot recorded it",
                    disk.display()
                )
            })?;
        }
    }
    // A snapshot records each pmem store entry by inode, length and modification time, so
    // this passes on the host that took the snapshot and on no other.
    crate::storage::pmem::check_snapshot_pmem_images(&host.data_dir, &metadata.pmem_devices)
        .context(
            "the snapshot maps pmem images, which the archive does not carry; a restore \
             reopens the store entries the snapshot recorded, and this host does not have \
             them",
        )?;
    Ok(config)
}

/// Write every file left in the archive into `staging`, read the archive to its verified
/// end, write the imported `config.json`, check that the directory holds every file a
/// restore opens, and sync it. One [`ArchiveEntry`] per file written, the config's first.
/// Runs on a blocking thread of the runtime.
fn stage_files<R: Read>(
    mut reader: ArchiveReader<R>,
    staging: &Path,
    archive: &CheckedArchive,
) -> Result<Vec<ArchiveEntry>> {
    let working_set = crate::uffd::WorkingSetStore::path_for(Path::new(SNAPSHOT_MEMORY_FILE));
    let memory = Path::new(SNAPSHOT_MEMORY_FILE);
    let mut files = vec![archive.config_entry.clone()];
    while let Some(entry) = reader.next_entry()? {
        let EntryName::Snapshot(relative) = &entry.name else {
            bail!("the archive carries a file from outside the snapshot's directory");
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
    // The config is written last, as a create writes it. `create_new`, so the archive
    // cannot have supplied it.
    let config_path = staging.join(CONFIG_FILE);
    let config_json = imported_config_json(&archive.config_json)?;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&config_path)
        .and_then(|mut file| file.write_all(&config_json))
        .with_context(|| format!("writing {}", config_path.display()))?;

    // An archive can verify and still lack a file: the trailer says the files are the ones
    // the export wrote, not that they are all a restore opens.
    tokio::runtime::Handle::current().block_on(check_generation_files(
        staging,
        &archive.config,
        &format!("imported snapshot {}", archive.manifest.tag),
    ))?;

    super::podman::sync_generation(staging).context("syncing the staged snapshot")?;
    Ok(files)
}

/// The `config.json` an import writes: the archived one as a generation of its own. Only
/// the two fields are set, on the parsed JSON, so every other field is installed as the
/// export wrote it, also one this build does not know.
fn imported_config_json(archived: &[u8]) -> Result<Vec<u8>> {
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
    serde_json::to_vec_pretty(&config).context("serializing snapshot config")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::snapshot_export::export_snapshot;
    use crate::commands::snapshot_export::tests::{
        data_runs, fake_snapshot, fake_snapshot_with, names_in, pattern, Fake, TAG,
    };
    use crate::storage::snapshot_archive::{ArchiveWriter, ManifestExternal};
    use crate::storage::SnapshotManager;
    use std::io::Cursor;
    use std::time::Duration;

    /// The importing host a fake snapshot was exported on: the same directories, and every
    /// kernel profile configured.
    fn host_of(fake: &Fake) -> ImportHost {
        ImportHost {
            snapshots_dir: fake.host.snapshots_dir.clone(),
            data_dir: fake.host.data_dir.clone(),
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
    }

    impl Unpacked {
        fn of(archive: &[u8]) -> Self {
            let mut reader = ArchiveReader::new(archive).unwrap();
            let manifest = reader.manifest().clone();
            let mut files = Vec::new();
            while let Some(entry) = reader.next_entry().unwrap() {
                let EntryName::Snapshot(name) = entry.name else {
                    panic!("the archive holds an external file");
                };
                let mut bytes = vec![0u8; entry.len as usize];
                while let Some((offset, piece)) = reader.next_run().unwrap() {
                    let start = offset as usize;
                    bytes[start..start + piece.len()].copy_from_slice(piece);
                }
                files.push((name, bytes));
            }
            Self { manifest, files }
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
            for _ in &self.manifest.externals {
                writer.append_external(&file_of(b"external")).unwrap();
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

    /// The image disk and the pmem images are a later version's to install.
    #[tokio::test]
    async fn an_archive_that_carries_external_files_is_refused() {
        let (fake, archive) = exported().await;
        let mut unpacked = Unpacked::of(&archive);
        unpacked.manifest.externals = vec![ManifestExternal {
            role: "image-disk".to_string(),
            path: PathBuf::from("/example/image-cache/layers.img"),
            len: 8,
            sha256: "00".repeat(32),
        }];

        let message = refusal(&fake, &host_of(&fake), unpacked.pack()).await;

        assert!(
            message.contains("carries 1 files from outside the snapshot's directory"),
            "{message}"
        );
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

    /// The archive does not carry the image disk a snapshot attaches. The import takes the
    /// snapshot where the disk is already there, with the digest the snapshot recorded if it
    /// recorded one (it records one for an overlay storage image only), and refuses it where
    /// the disk is absent or is another build.
    #[tokio::test]
    async fn a_snapshot_with_an_image_disk_imports_only_where_the_disk_is_as_recorded() {
        let (fake, exported) = exported().await;
        let disk = fake.root.path().join("image-disk.img");
        std::fs::write(&disk, pattern(8, 4096)).unwrap();
        let identity = crate::utils::image_disk_digest(&disk).unwrap();
        let archive_recording = |identity: Option<String>| {
            let mut unpacked = Unpacked::of(&exported);
            unpacked.edit_config(|config| {
                config.metadata.image_disk_path = Some(disk.clone());
                config.metadata.image_disk_identity = identity;
            });
            unpacked.pack()
        };
        let with_digest = archive_recording(Some(identity));
        let without_digest = archive_recording(None);

        std::fs::write(&disk, pattern(9, 4096)).unwrap();
        let message = refusal(&fake, &host_of(&fake), with_digest.clone()).await;
        assert!(
            message.contains("which the archive does not carry")
                && message.contains("image disk build changed"),
            "another build of the disk: {message}"
        );

        std::fs::remove_file(&disk).unwrap();
        for (recorded, archive) in [("a digest", &with_digest), ("no digest", &without_digest)] {
            let message = refusal(&fake, &host_of(&fake), archive.clone()).await;
            assert!(
                message.contains("which the archive does not carry and this host does not have")
                    && message.contains("image-disk.img"),
                "no disk, {recorded} recorded: {message}"
            );
        }

        // Control: with the disk the snapshot recorded, the same archives import.
        std::fs::write(&disk, pattern(8, 4096)).unwrap();
        for archive in [with_digest, without_digest] {
            import_snapshot(&host_of(&fake), Cursor::new(archive))
                .await
                .unwrap();
            remove_snapshot_dir(&fake.dir()).await.unwrap();
        }
    }

    /// The archive does not carry pmem images either, and a restore reopens each at its
    /// recorded path in this host's store, as the entry the snapshot recorded by inode,
    /// length and modification time. A snapshot whose entries are not there is refused.
    #[tokio::test]
    async fn a_snapshot_whose_pmem_images_are_not_on_this_host_is_refused() {
        let (fake, archive) = exported().await;
        let mut unpacked = Unpacked::of(&archive);
        let entry = fake.host.data_dir.join("pmem/0123456789abcdef.img");
        unpacked.edit_config(|config| {
            config.metadata.pmem_devices = vec![crate::state::types::PmemDevice {
                path: entry.to_string_lossy().into_owned(),
                source: "/example/models/model.img".to_string(),
                mount_path: "/models".to_string(),
                identity: "1:2:3".to_string(),
            }];
        });

        let message = refusal(&fake, &host_of(&fake), unpacked.pack()).await;

        assert!(
            message.contains("the snapshot maps pmem images, which the archive does not carry")
                && message.contains("snapshot pmem image changed"),
            "{message}"
        );
    }

    /// An input that stops before the first read past the start of the archive, which is
    /// inside the memory image's data, until the test lets it go on.
    struct Parked {
        archive: Cursor<Vec<u8>>,
        entered: tokio::sync::mpsc::UnboundedSender<()>,
        release: std::sync::mpsc::Receiver<()>,
        parked: bool,
    }

    impl Read for Parked {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if !self.parked && self.archive.position() > 0 {
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

    /// Once the archive has verified, the staged files are checked, then synced, and only
    /// then renamed. A test cannot see a sync from outside, so the order is pinned by the
    /// source.
    #[test]
    fn the_staged_files_are_synced_before_they_are_published() {
        let source = include_str!("snapshot_import.rs");
        assert_in_order(
            "stage_files",
            body_of(source, "fn stage_files<R: Read>("),
            &[
                "reader.next_entry()",
                "imported_config_json(",
                "check_generation_files(",
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

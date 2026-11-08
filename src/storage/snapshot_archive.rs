//! The archive that carries one snapshot between hosts over a pipe: a streaming writer and a
//! streaming reader. `snapshot export` and `snapshot import` are built on it; nothing here
//! knows what a snapshot is beyond the names of its files.
//!
//! # Format
//!
//! One pax tar stream that GNU tar lists and extracts. Entries, in order:
//!
//! 1. `manifest.json`, an [`ArchiveManifest`];
//! 2. `snapshot/<path>`, the files of the snapshot directory;
//! 3. `external/<n>`, the file `manifest.externals[n]` describes, for every `n` in order;
//! 4. `trailer.json`, one [`EntryRecord`] for every entry before it.
//!
//! Two 512-byte zero blocks end the stream.
//!
//! Every entry is a pax extended header followed by a ustar header for a regular file. The
//! pax records always carry `size`, the bytes the entry stores, so an entry over the 8 GiB a
//! ustar size field holds needs no other encoding.
//!
//! A file under `snapshot/` or `external/` is stored in PAX sparse format 1.0
//! (`GNU.sparse.major=1`, `GNU.sparse.minor=0`, `GNU.sparse.name`, `GNU.sparse.realsize`).
//! Its data starts with the map of its data runs as decimal text, one number per line: the
//! count, then an offset and a length per run, padded with NULs to a 512-byte block. The runs'
//! bytes follow back to back. A file that ends in a hole gets a last run of length 0 at its
//! length, which is how GNU tar's own writer marks it and what makes its extractor restore
//! the length.
//!
//! GNU tar reads and writes each run in 512-byte blocks, so it expects a run whose length is
//! not a multiple of 512 to be padded to the next block. The writer therefore widens every
//! run to 512-byte boundaries (clipped to the file's length) and merges runs that then
//! touch. Only the last run can end off a boundary, where the entry's own padding follows, so
//! the stream reads the same back to back and block by block. The reader refuses a map that
//! breaks this.
//!
//! # What the reader refuses
//!
//! The writer hashes every entry while it streams: SHA-256 over the file's length and, for
//! each data run, its offset, its length and its bytes, so the same bytes at another offset
//! give another digest. `trailer.json` lists each entry's name, length, stored bytes and
//! digest. [`ArchiveReader::next_entry`] returns `Ok(None)` only once it has read the
//! trailer, found exactly the entries it read in it, with the same lengths and digests, and
//! read the end of the archive. A stream that is cut anywhere, has no trailer, or holds an
//! entry the trailer does not list is an error, so an importer that installs only after
//! `Ok(None)` cannot install from a cut pipe.
//!
//! The reader accepts the form the writer produces and nothing wider: no links, devices,
//! directories, GNU long names or global pax headers, only relative names under `snapshot/`
//! and `external/` with no `.` or `..` component, and zeros in every padding byte.
//!
//! # Memory and buffering
//!
//! Data is copied through one buffer of [`CHUNK`] bytes. Format 1.0 puts a file's whole map
//! before its data, so both sides hold the map: 16 bytes per data run, capped at
//! [`MAX_RUNS`]. An archive carries at most [`MAX_ENTRIES`] files.
//!
//! Headers, pax records, padding and the map are read and written 512 bytes or less at a
//! time, each with a call of its own on the stream. The caller gives [`ArchiveWriter`] a
//! buffered output and [`ArchiveReader`] a buffered input, or pays a system call for each.
//!
//! # Why not the `tar` crate's `Builder` and `Archive`
//!
//! `tar` 0.4 writes a sparse file only in the old GNU form (`builder.rs`,
//! `prepare_header_sparse`). Its `Archive` decodes only that form (`archive.rs`,
//! `parse_sparse_header`) and never reads the `GNU.sparse.*` records, so on a format 1.0
//! entry it yields the placeholder name and the map as file data. It also takes end of input
//! at a header boundary, or a single zero block, as a clean end, and accepts links, long
//! names and global headers, which this reader would have to refuse again on top of it. The
//! 512-byte header block is the crate's `Header`; the pax records, the map and the order of
//! entries are written and checked here.

use std::collections::HashSet;
use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tar::{EntryType, Header};

use super::SnapshotKind;
use crate::hypervisor::Backend;

/// The version of the archive format, in `manifest.json`. A reader refuses any other.
pub const ARCHIVE_VERSION: u32 = 1;

/// The most data runs one file may have. Both sides hold a file's map, 16 bytes a run, so
/// 1 GiB at the limit. The layout with the most runs alternates 4 KiB of data and 4 KiB of
/// hole, which reaches the limit at 512 GiB, so any file up to that size fits and a larger
/// one fits unless it is nearly that fragmented.
pub const MAX_RUNS: usize = 1 << 26;

/// The most files one archive may carry, the snapshot's and the external ones together.
/// `trailer.json` has a row for each and one for the manifest, and a reader takes at most
/// 16 MiB of it. A row is its name, which is at most 4096 bytes and at most twice that in
/// JSON, where a quote or a backslash takes two, and under 300 bytes of everything else, so
/// 1025 rows stay under 9 MiB. The writer refuses one file more when it is appended, not
/// when the trailer is written after everything else was sent.
pub const MAX_ENTRIES: usize = 1024;

/// The copy buffer: the most bytes [`ArchiveReader::next_run`] hands over at once.
pub const CHUNK: usize = 1 << 20;

const BLOCK: usize = 512;
const BLOCK_U64: u64 = BLOCK as u64;
const ZERO_BLOCK: [u8; BLOCK] = [0; BLOCK];
/// The largest size eleven octal digits hold, the most a ustar size field carries.
const USTAR_MAX_SIZE: u64 = 0o77_777_777_777;

const MANIFEST_NAME: &str = "manifest.json";
const TRAILER_NAME: &str = "trailer.json";
const SNAPSHOT_PREFIX: &str = "snapshot/";
const EXTERNAL_PREFIX: &str = "external/";

/// The longest entry name, in bytes.
const MAX_NAME_BYTES: usize = 4096;
/// The most pax records one entry may carry, in bytes: a name and four short records.
const MAX_PAX_BYTES: u64 = MAX_NAME_BYTES as u64 + 1024;
/// The largest `manifest.json` or `trailer.json`. Both are read whole.
const MAX_JSON_BYTES: u64 = 16 << 20;

const PAX_SIZE: &str = "size";
const PAX_SPARSE_MAJOR: &str = "GNU.sparse.major";
const PAX_SPARSE_MINOR: &str = "GNU.sparse.minor";
const PAX_SPARSE_NAME: &str = "GNU.sparse.name";
const PAX_SPARSE_REALSIZE: &str = "GNU.sparse.realsize";

/// What the exporting host says about the snapshot, for the importer to check before it
/// writes anything. The codec carries it and checks only `archive_version` and the number of
/// `externals`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveManifest {
    /// [`ARCHIVE_VERSION`].
    pub archive_version: u32,
    /// The exporting host's architecture (`std::env::consts::ARCH`).
    pub arch: String,
    pub network_boundary_version: u32,
    pub container_start_version: u32,
    /// The format version of the working-set sidecar.
    pub working_set_version: u64,
    pub tag: String,
    pub kind: SnapshotKind,
    pub hypervisor: Backend,
    /// The exporting host's data directory, which the VM state embeds paths under.
    pub data_dir: PathBuf,
    /// SHA-256 of the snapshot's `config.json`, in hex.
    pub config_sha256: String,
    /// The memory image's identity on the exporting host. `None` for a disk-only snapshot.
    pub memory: Option<ManifestMemory>,
    pub firecracker: ManifestFirecracker,
    /// The files outside the snapshot directory that the archive carries: `externals[n]` is
    /// the entry `external/<n>`. The image disk the snapshot attaches, then the pmem store
    /// entry of each of its pmem devices.
    pub externals: Vec<ManifestExternal>,
    pub pmem_owner_uid: Option<u32>,
    /// What a restore reads that the archive does not carry, as far as the snapshot's
    /// `config.json` records a path for it: mapped directories, NFS shares and firmware. A
    /// read-only `--disk` file outside the VM's directory and a kernel profile's NVIDIA
    /// driver image are recorded only in Firecracker's VM state, so they are not listed.
    pub not_shipped: Vec<String>,
}

/// The length and modification time of the memory image, which key its working-set sidecar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestMemory {
    pub len: u64,
    pub mtime_sec: i64,
    pub mtime_nsec: i64,
}

/// The Firecracker binary at the path the snapshot recorded, as the exporting host had it
/// when the export ran. `version_output` and `sha256` are those of the file at export time,
/// so a binary replaced at that path after the snapshot was taken is the one described.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestFirecracker {
    pub path: PathBuf,
    pub version_output: String,
    pub sha256: String,
}

/// What an external file is to the snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExternalRole {
    /// The image disk the snapshot attaches read-only (`metadata.image_disk_path`).
    ImageDisk,
    /// The pmem store entry of one of the snapshot's pmem devices.
    Pmem,
}

impl std::fmt::Display for ExternalRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ExternalRole::ImageDisk => "image disk",
            ExternalRole::Pmem => "pmem image",
        })
    }
}

/// One file outside the snapshot directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestExternal {
    pub role: ExternalRole,
    /// The path the snapshot's `config.json` records for the file, which is where it is on
    /// the exporting host and where a restore opens it.
    pub path: PathBuf,
    pub len: u64,
    /// The digest the snapshot's `config.json` binds the file's bytes to, in hex. Both come
    /// from the config, so a manifest is written without reading the file:
    ///
    /// - the image disk: the SHA-256 of its bytes, which is the image disk identity the
    ///   snapshot recorded, or `None` when it recorded none;
    /// - a pmem image: the digest in its store entry's name, SHA-256 over the uid the entry
    ///   belongs to and the store's digest of its bytes.
    ///
    /// The bytes an archive carries for the file are checked against it while they are
    /// streamed ([`ContentSha256`], `PmemStore::begin_stream_ingest`), and the trailer's
    /// digest of the entry says they arrived as they were sent.
    pub sha256: Option<String>,
}

/// What `trailer.json` records about one entry, and what the writer returns for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntryRecord {
    /// The entry's name in the archive, such as `snapshot/memory.bin`.
    pub name: String,
    /// The file's length, holes included.
    pub len: u64,
    /// The bytes of its data runs, which is what the archive stores of it.
    pub data_len: u64,
    /// SHA-256 over the length and every data run's offset, length and bytes, in hex.
    pub sha256: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Trailer {
    archive_version: u32,
    entries: Vec<EntryRecord>,
}

/// Which file an entry is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryName {
    /// A file of the snapshot directory, by its path relative to that directory.
    Snapshot(PathBuf),
    /// `external/<n>`: the file `manifest.externals[n]` describes.
    External(usize),
}

/// An entry [`ArchiveReader::next_entry`] has reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveEntry {
    pub name: EntryName,
    /// The file's length, holes included.
    pub len: u64,
    /// The bytes [`ArchiveReader::next_run`] will hand over for it.
    pub data_len: u64,
}

/// One data run of a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Run {
    offset: u64,
    len: u64,
}

impl Run {
    fn end(&self) -> u64 {
        self.offset + self.len
    }
}

/// The digest of one entry: its length, then each run's offset, length and bytes.
struct EntryDigest {
    hasher: Sha256,
    data_len: u64,
}

impl EntryDigest {
    fn new(len: u64) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(b"fcvm snapshot archive entry v1\0");
        hasher.update(len.to_le_bytes());
        Self {
            hasher,
            data_len: 0,
        }
    }

    fn run(&mut self, run: Run) {
        self.hasher.update(run.offset.to_le_bytes());
        self.hasher.update(run.len.to_le_bytes());
    }

    fn data(&mut self, bytes: &[u8]) {
        self.hasher.update(bytes);
        self.data_len += bytes.len() as u64;
    }

    fn record(self, name: String, len: u64) -> EntryRecord {
        EntryRecord {
            name,
            len,
            data_len: self.data_len,
            sha256: hex::encode(self.hasher.finalize()),
        }
    }
}

/// The record of an entry whose bytes are all in memory: one run over all of it.
fn plain_record(name: &str, bytes: &[u8]) -> EntryRecord {
    let len = bytes.len() as u64;
    let mut digest = EntryDigest::new(len);
    if len > 0 {
        digest.run(Run { offset: 0, len });
        digest.data(bytes);
    }
    digest.record(name.to_string(), len)
}

/// The SHA-256 of a file's bytes, holes read as zeros, computed from its data runs as an
/// archive carries them: the pieces [`ArchiveWriter::append_external_with`] shows its
/// caller and [`ArchiveReader::next_run`] hands over, in order. It is the digest
/// `sha256sum` prints for the file, which an entry's own digest in the trailer is not (that
/// one covers the run offsets too), so a caller that has to compare a file with a digest
/// recorded elsewhere computes this one in the same pass.
pub struct ContentSha256 {
    hasher: Sha256,
    /// The bytes hashed so far, which is the offset the next piece is expected at.
    at: u64,
}

impl Default for ContentSha256 {
    fn default() -> Self {
        Self::new()
    }
}

impl ContentSha256 {
    pub fn new() -> Self {
        Self {
            hasher: Sha256::new(),
            at: 0,
        }
    }

    /// Hash zeros up to `offset`.
    fn zeros_to(&mut self, offset: u64) {
        static ZEROS: [u8; 64 << 10] = [0; 64 << 10];
        while self.at < offset {
            let count =
                usize::try_from(offset - self.at).map_or(ZEROS.len(), |n| n.min(ZEROS.len()));
            self.hasher.update(&ZEROS[..count]);
            self.at += count as u64;
        }
    }

    /// The piece `bytes` of the file at `offset`. Pieces come in file order; what lies
    /// between two of them is a hole.
    pub fn piece(&mut self, offset: u64, bytes: &[u8]) -> Result<()> {
        ensure!(
            offset >= self.at,
            "a piece at offset {offset} follows one that ended at {}",
            self.at
        );
        self.zeros_to(offset);
        self.hasher.update(bytes);
        self.at += bytes.len() as u64;
        Ok(())
    }

    /// The digest of the file of `len` bytes, in hex.
    pub fn finish(mut self, len: u64) -> Result<String> {
        ensure!(
            len >= self.at,
            "{} bytes were hashed of a file of {len}",
            self.at
        );
        self.zeros_to(len);
        Ok(hex::encode(self.hasher.finalize()))
    }
}

/// The zero bytes after `len` bytes of entry data, up to the next block.
fn padding(len: u64) -> usize {
    ((BLOCK_U64 - len % BLOCK_U64) % BLOCK_U64) as usize
}

/// Which file a sparse entry named `name` is, or why the name is refused. A name is
/// relative, has no empty, `.` or `..` component, and is under `snapshot/` or `external/`,
/// so joining it to a staging directory never leaves that directory.
fn parse_entry_name(name: &str) -> Result<EntryName> {
    ensure!(!name.is_empty(), "archive entry has an empty name");
    ensure!(
        name.len() <= MAX_NAME_BYTES,
        "archive entry name is {} bytes long (the limit is {MAX_NAME_BYTES})",
        name.len()
    );
    ensure!(
        !name.chars().any(char::is_control),
        "archive entry name {name:?} has a control character"
    );
    ensure!(
        !name.starts_with('/'),
        "archive entry name {name:?} is an absolute path"
    );
    for component in name.split('/') {
        ensure!(
            component != "..",
            "archive entry name {name:?} has a '..' component"
        );
        ensure!(
            !component.is_empty() && component != ".",
            "archive entry name {name:?} has an empty or '.' component"
        );
    }
    if let Some(path) = name.strip_prefix(SNAPSHOT_PREFIX) {
        return Ok(EntryName::Snapshot(PathBuf::from(path)));
    }
    if let Some(index) = name.strip_prefix(EXTERNAL_PREFIX) {
        let index = parse_decimal(index.as_bytes())
            .ok()
            .and_then(|index| usize::try_from(index).ok())
            .ok_or_else(|| anyhow!("archive entry name {name:?} is not external/<number>"))?;
        return Ok(EntryName::External(index));
    }
    bail!("archive entry name {name:?} is outside snapshot/ and external/")
}

/// A decimal number as the writer prints one: digits only, no sign, no leading zero.
fn parse_decimal(text: &[u8]) -> Result<u64> {
    ensure!(
        !text.is_empty() && text.iter().all(u8::is_ascii_digit),
        "{:?} is not a decimal number",
        String::from_utf8_lossy(text)
    );
    ensure!(
        text.len() == 1 || text[0] != b'0',
        "the number {:?} has a leading zero",
        String::from_utf8_lossy(text)
    );
    let mut value = 0u64;
    for digit in text {
        value = value
            .checked_mul(10)
            .and_then(|value| value.checked_add(u64::from(digit - b'0')))
            .ok_or_else(|| {
                anyhow!(
                    "the number {:?} does not fit 64 bits",
                    String::from_utf8_lossy(text)
                )
            })?;
    }
    Ok(value)
}

/// Append the pax record `<length> <key>=<value>\n`, where the length counts the whole
/// record, its own digits included.
fn push_pax_record(records: &mut Vec<u8>, key: &str, value: &str) {
    let rest = key.len() + value.len() + 3;
    let mut len = rest + 1;
    loop {
        let with_digits = rest + len.to_string().len();
        if with_digits == len {
            break;
        }
        len = with_digits;
    }
    records.extend_from_slice(format!("{len} {key}={value}\n").as_bytes());
}

/// The pax records of one entry.
#[derive(Default)]
struct PaxRecords {
    size: Option<u64>,
    sparse_major: Option<u64>,
    sparse_minor: Option<u64>,
    sparse_name: Option<String>,
    sparse_realsize: Option<u64>,
}

fn parse_pax_records(mut data: &[u8]) -> Result<PaxRecords> {
    fn set<T>(slot: &mut Option<T>, key: &str, value: T) -> Result<()> {
        ensure!(slot.is_none(), "the pax record {key} is given twice");
        *slot = Some(value);
        Ok(())
    }

    let mut records = PaxRecords::default();
    while !data.is_empty() {
        let space = data
            .iter()
            .position(|&byte| byte == b' ')
            .context("a pax record has no length")?;
        let len = usize::try_from(parse_decimal(&data[..space])?)
            .ok()
            .filter(|&len| len > space + 1 && len <= data.len())
            .context("a pax record's length does not fit its header")?;
        let (record, rest) = data.split_at(len);
        let body = record[space + 1..]
            .strip_suffix(b"\n")
            .context("a pax record does not end in a newline")?;
        let body = std::str::from_utf8(body).context("a pax record is not UTF-8")?;
        let (key, value) = body.split_once('=').context("a pax record has no '='")?;
        let number = || {
            parse_decimal(value.as_bytes()).with_context(|| format!("reading the pax record {key}"))
        };
        match key {
            PAX_SIZE => set(&mut records.size, key, number()?)?,
            PAX_SPARSE_MAJOR => set(&mut records.sparse_major, key, number()?)?,
            PAX_SPARSE_MINOR => set(&mut records.sparse_minor, key, number()?)?,
            PAX_SPARSE_REALSIZE => set(&mut records.sparse_realsize, key, number()?)?,
            PAX_SPARSE_NAME => set(&mut records.sparse_name, key, value.to_string())?,
            _ => bail!("the pax record {key:?} is not one this archive format uses"),
        }
        data = rest;
    }
    Ok(records)
}

/// A ustar header block for a regular file or a pax extended header.
fn ustar_header(name: &str, kind: EntryType, size: u64, mtime: u64) -> Result<[u8; BLOCK]> {
    let mut header = Header::new_ustar();
    header
        .set_path(name)
        .with_context(|| format!("writing the tar header name {name:?}"))?;
    header.set_mode(0o600);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(mtime);
    header.set_size(size);
    header.set_entry_type(kind);
    header.set_cksum();
    Ok(*header.as_bytes())
}

/// The fields the reader uses of a header block.
struct UstarFields {
    kind: EntryType,
    size: u64,
    name: Vec<u8>,
}

fn parse_ustar_header(block: &[u8; BLOCK]) -> Result<UstarFields> {
    let header = Header::from_byte_slice(block);
    ensure!(
        header.as_ustar().is_some(),
        "an archive header is not a ustar header: the stream is not a snapshot archive, or \
         is corrupt"
    );
    // The block has to equal itself with its checksum computed again, so a change to any
    // byte of it is seen, the checksum field's own bytes included.
    let mut recomputed = header.clone();
    recomputed.set_cksum();
    ensure!(
        recomputed.as_bytes() == block,
        "an archive header does not match its checksum: the stream is corrupt"
    );
    Ok(UstarFields {
        kind: header.entry_type(),
        size: header
            .entry_size()
            .context("reading an archive header's size")?,
        name: header.path_bytes().into_owned(),
    })
}

/// Widen the data run `[start, end)` of a file of `file_len` bytes to 512-byte boundaries,
/// clipped to the file, and add it to `runs`, merging it into the last run when they touch.
/// Runs arrive in ascending order. The bytes a run gains are a hole's, which read as zeros.
fn push_aligned_run(runs: &mut Vec<Run>, start: u64, end: u64, file_len: u64) {
    let start = start / BLOCK_U64 * BLOCK_U64;
    let end = (end.div_ceil(BLOCK_U64) * BLOCK_U64).min(file_len);
    match runs.last_mut() {
        Some(last) if start <= last.end() => last.len = last.len.max(end - last.offset),
        _ => runs.push(Run {
            offset: start,
            len: end - start,
        }),
    }
}

/// The data runs of the first `len` bytes of `file`, each widened to 512-byte boundaries
/// ([`push_aligned_run`]). A file with more than `max_runs` of them is an error, and the walk
/// stops there, so neither the walk nor the list goes on to the end of such a file.
fn aligned_data_runs(file: &File, len: u64, max_runs: usize) -> Result<Vec<Run>> {
    let mut runs: Vec<Run> = Vec::new();
    crate::uffd::try_for_each_data_run(file, len, |start, end| {
        ensure!(
            runs.len() < max_runs,
            "the file has more than {max_runs} data runs, the most an archive holds for one \
             file"
        );
        push_aligned_run(&mut runs, start, end, len);
        Ok(())
    })?;
    Ok(runs)
}

/// Fill `buf` from `input`. An input that ends first was cut short, which `what` names.
fn read_full<R: Read>(input: &mut R, buf: &mut [u8], what: &str) -> Result<()> {
    let mut filled = 0;
    while filled < buf.len() {
        match input.read(&mut buf[filled..]) {
            Ok(0) => bail!(
                "the archive stream ended inside {what}: it was cut short, so nothing in it \
                 can be trusted"
            ),
            Ok(count) => filled += count,
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e).with_context(|| format!("reading {what} from the archive")),
        }
    }
    Ok(())
}

/// Read `count` bytes of padding, which have to be zeros.
fn read_padding<R: Read>(input: &mut R, count: usize, what: &str) -> Result<()> {
    let mut pad = [0u8; BLOCK];
    read_full(input, &mut pad[..count], what)?;
    ensure!(
        pad[..count] == ZERO_BLOCK[..count],
        "{what} is not zeros: the stream is corrupt"
    );
    Ok(())
}

/// Writes one archive to `out`, which need not seek. See the module docs for the format.
///
/// The order of calls is the order of entries: [`ArchiveWriter::new`] writes the manifest,
/// then every [`ArchiveWriter::append_snapshot_file`], then one
/// [`ArchiveWriter::append_external`] per `manifest.externals`, then
/// [`ArchiveWriter::finish`], which writes the trailer. A stream without `finish` has no
/// trailer and no reader accepts it. After any call fails the stream is torn, and every
/// later call fails too.
///
/// The caller holds the snapshot's generation lock for as long as it appends, so nothing
/// replaces or rewrites a file while it is archived. The writer reads a file's length and
/// modification time before it finds the data runs and again after it has copied them, and
/// fails the append when they differ. That is a backstop for a caller without the lock, not
/// a substitute for it: a write that changes neither is not seen.
///
/// `out` gets many writes of 512 bytes or less, so it should be buffered.
pub struct ArchiveWriter<W: Write> {
    out: W,
    /// One record per entry written, in order, for the trailer.
    entries: Vec<EntryRecord>,
    names: HashSet<String>,
    externals_expected: usize,
    externals_written: usize,
    /// The modification time the two JSON entries carry.
    created: u64,
    /// The largest size written into a header's size field; a larger one is written as 0
    /// and read from the pax `size` record. Lower only in tests.
    header_size_limit: u64,
    buf: Vec<u8>,
    /// The bytes of file data read so far, over every file appended.
    data_read: u64,
    failed: bool,
}

impl<W: Write> ArchiveWriter<W> {
    /// Start an archive on `out` with `manifest` as its first entry.
    pub fn new(out: W, manifest: &ArchiveManifest) -> Result<Self> {
        Self::with_header_size_limit(out, manifest, USTAR_MAX_SIZE)
    }

    fn with_header_size_limit(
        out: W,
        manifest: &ArchiveManifest,
        header_size_limit: u64,
    ) -> Result<Self> {
        ensure!(
            manifest.archive_version == ARCHIVE_VERSION,
            "the manifest says archive version {}, and this fcvm writes version \
             {ARCHIVE_VERSION}",
            manifest.archive_version
        );
        let created = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_secs());
        let mut writer = Self {
            out,
            entries: Vec::new(),
            names: HashSet::new(),
            externals_expected: manifest.externals.len(),
            externals_written: 0,
            created,
            header_size_limit,
            buf: vec![0; CHUNK],
            data_read: 0,
            failed: false,
        };
        let json = serde_json::to_vec_pretty(manifest).context("serializing manifest.json")?;
        writer.write_plain(MANIFEST_NAME, &json)?;
        writer.entries.push(plain_record(MANIFEST_NAME, &json));
        Ok(writer)
    }

    /// Append the file of the snapshot directory at `relative` as `snapshot/<relative>`,
    /// reading only its data runs. Fails once an external file has been appended. Finding
    /// the runs moves the offset of `file`; the data is read at explicit offsets.
    pub fn append_snapshot_file(&mut self, relative: &Path, file: &File) -> Result<EntryRecord> {
        self.guard(|writer| {
            writer.room_for_a_file()?;
            ensure!(
                writer.externals_written == 0,
                "snapshot files come before external files in an archive"
            );
            let relative_str = relative
                .to_str()
                .with_context(|| format!("the snapshot file name {relative:?} is not UTF-8"))?;
            let name = format!("{SNAPSHOT_PREFIX}{relative_str}");
            ensure!(
                parse_entry_name(&name)? == EntryName::Snapshot(relative.to_path_buf()),
                "the snapshot file name {relative:?} is not a plain relative path"
            );
            writer.append_sparse(name, file, &mut |_, _| Ok(()))
        })
    }

    /// Append the next external file, reading only its data runs. The first call writes
    /// `external/0`, the file `manifest.externals[0]` describes, and so on.
    pub fn append_external(&mut self, file: &File) -> Result<EntryRecord> {
        self.append_external_with(file, |_, _| Ok(()))
    }

    /// [`ArchiveWriter::append_external`], showing `piece` every piece of the file's data
    /// with its offset, in file order, after the piece is written: the pieces a reader's
    /// [`ArchiveReader::next_run`] hands over. A caller that has to know something about the
    /// bytes it sent (their SHA-256, with [`ContentSha256`]) learns it from this one read of
    /// the file. An error from `piece` fails the append and tears the stream.
    pub fn append_external_with(
        &mut self,
        file: &File,
        mut piece: impl FnMut(u64, &[u8]) -> Result<()>,
    ) -> Result<EntryRecord> {
        self.guard(|writer| {
            writer.room_for_a_file()?;
            ensure!(
                writer.externals_written < writer.externals_expected,
                "the manifest lists {} external files and one more was appended",
                writer.externals_expected
            );
            let name = format!("{EXTERNAL_PREFIX}{}", writer.externals_written);
            let record = writer.append_sparse(name, file, &mut piece)?;
            writer.externals_written += 1;
            Ok(record)
        })
    }

    /// The bytes of file data read so far, over every file appended, whether or not the
    /// append went on to succeed. A caller that gives a file's pages back to the kernel
    /// compares this before and after an append to learn whether the append read any.
    pub fn data_bytes_read(&self) -> u64 {
        self.data_read
    }

    /// Write the trailer and the end of the archive, flush, and give `out` back.
    pub fn finish(mut self) -> Result<W> {
        self.guard(|writer| {
            ensure!(
                writer.externals_written == writer.externals_expected,
                "the manifest lists {} external files and {} were appended",
                writer.externals_expected,
                writer.externals_written
            );
            let trailer = Trailer {
                archive_version: ARCHIVE_VERSION,
                entries: std::mem::take(&mut writer.entries),
            };
            let json = serde_json::to_vec_pretty(&trailer).context("serializing trailer.json")?;
            writer.write_plain(TRAILER_NAME, &json)?;
            writer.put(&ZERO_BLOCK)?;
            writer.put(&ZERO_BLOCK)?;
            writer.out.flush().context("flushing the archive")
        })?;
        Ok(self.out)
    }

    /// An archive carries at most [`MAX_ENTRIES`] files. `entries` holds the manifest's row
    /// as well.
    fn room_for_a_file(&self) -> Result<()> {
        ensure!(
            self.entries.len() <= MAX_ENTRIES,
            "an archive carries at most {MAX_ENTRIES} files, and this would be one more"
        );
        Ok(())
    }

    /// Run `step`, and remember a failure: the stream is then torn mid-entry.
    fn guard<T>(&mut self, step: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        ensure!(
            !self.failed,
            "an earlier write to this archive failed, so the stream is unusable"
        );
        let result = step(self);
        self.failed = result.is_err();
        result
    }

    fn put(&mut self, bytes: &[u8]) -> Result<()> {
        self.out.write_all(bytes).context("writing the archive")
    }

    /// Write the two headers of an entry that stores `stored` bytes: the pax extended header
    /// with `records` and `size`, then the file's own header under `header_name`.
    fn write_headers(
        &mut self,
        mut records: Vec<u8>,
        header_name: &str,
        stored: u64,
        mtime: u64,
    ) -> Result<()> {
        push_pax_record(&mut records, PAX_SIZE, &stored.to_string());
        let ordinal = self.names.len();
        let pax_header = ustar_header(
            &format!("PaxHeaders/{ordinal}"),
            EntryType::XHeader,
            records.len() as u64,
            mtime,
        )?;
        self.put(&pax_header)?;
        self.put(&records)?;
        self.put(&ZERO_BLOCK[..padding(records.len() as u64)])?;
        let size = if stored <= self.header_size_limit {
            stored
        } else {
            0
        };
        let header = ustar_header(header_name, EntryType::Regular, size, mtime)?;
        self.put(&header)
    }

    /// Write `bytes` as the entry `name`, stored whole.
    fn write_plain(&mut self, name: &str, bytes: &[u8]) -> Result<()> {
        let len = bytes.len() as u64;
        ensure!(
            len <= MAX_JSON_BYTES,
            "{name} is {len} bytes, more than the {MAX_JSON_BYTES} a reader accepts"
        );
        self.write_headers(Vec::new(), name, len, self.created)?;
        self.names.insert(name.to_string());
        self.put(bytes)?;
        self.put(&ZERO_BLOCK[..padding(len)])
    }

    /// Write `file` as the sparse entry `name`: its map, then its data runs. `piece` is
    /// shown each piece of data with its offset once the piece is written.
    fn append_sparse(
        &mut self,
        name: String,
        file: &File,
        piece: &mut dyn FnMut(u64, &[u8]) -> Result<()>,
    ) -> Result<EntryRecord> {
        ensure!(
            !self.names.contains(&name),
            "the archive already has an entry named {name}"
        );
        let meta = file
            .metadata()
            .with_context(|| format!("stat-ing the file for {name}"))?;
        ensure!(meta.is_file(), "{name} is not a regular file");
        let len = meta.len();

        let runs = aligned_data_runs(file, len, MAX_RUNS)
            .with_context(|| format!("finding the data runs of {name}"))?;
        let data_len: u64 = runs.iter().map(|run| run.len).sum();
        // A file that ends in a hole gets a run of length 0 at its length.
        let ends_in_hole = runs.last().map_or(0, Run::end) < len;

        let count = runs.len() + usize::from(ends_in_hole);
        let map_len = {
            let digits = |value: u64| value.to_string().len() as u64 + 1;
            let mut map_len = digits(count as u64);
            for run in &runs {
                map_len += digits(run.offset) + digits(run.len);
            }
            if ends_in_hole {
                map_len += digits(len) + digits(0);
            }
            map_len
        };
        let stored = map_len + padding(map_len) as u64 + data_len;

        let mut records = Vec::new();
        push_pax_record(&mut records, PAX_SPARSE_MAJOR, "1");
        push_pax_record(&mut records, PAX_SPARSE_MINOR, "0");
        push_pax_record(&mut records, PAX_SPARSE_NAME, &name);
        push_pax_record(&mut records, PAX_SPARSE_REALSIZE, &len.to_string());
        let ordinal = self.names.len();
        let mtime = u64::try_from(meta.mtime()).unwrap_or(0);
        self.write_headers(
            records,
            &format!("GNUSparseFile.0/{ordinal}"),
            stored,
            mtime,
        )?;
        self.names.insert(name.clone());

        // The map, written a piece at a time: a file can have millions of runs. The header
        // already says how long it is, so the bytes written are counted against that.
        const MAP_FLUSH: usize = 64 << 10;
        let mut map = Vec::new();
        let mut map_written = 0u64;
        map.extend_from_slice(format!("{count}\n").as_bytes());
        for run in &runs {
            map.extend_from_slice(format!("{}\n{}\n", run.offset, run.len).as_bytes());
            if map.len() >= MAP_FLUSH {
                self.put(&map)?;
                map_written += map.len() as u64;
                map.clear();
            }
        }
        if ends_in_hole {
            map.extend_from_slice(format!("{len}\n0\n").as_bytes());
        }
        self.put(&map)?;
        map_written += map.len() as u64;
        ensure!(
            map_written == map_len,
            "the sparse map of {name} is {map_written} bytes, and its header was written for \
             {map_len}"
        );
        self.put(&ZERO_BLOCK[..padding(map_len)])?;

        let mut digest = EntryDigest::new(len);
        for &run in &runs {
            digest.run(run);
            let mut offset = run.offset;
            while offset < run.end() {
                let count = usize::try_from(run.end() - offset).map_or(CHUNK, |n| n.min(CHUNK));
                file.read_exact_at(&mut self.buf[..count], offset)
                    .with_context(|| {
                        format!("reading {count} bytes of {name} at offset {offset}")
                    })?;
                self.data_read += count as u64;
                self.out
                    .write_all(&self.buf[..count])
                    .context("writing the archive")?;
                digest.data(&self.buf[..count]);
                piece(offset, &self.buf[..count])?;
                offset += count as u64;
            }
        }
        self.put(&ZERO_BLOCK[..padding(stored)])?;

        // The length was read once, the runs found after it and the data copied after that.
        // A file that changed in between is archived as a mix of two states.
        let after = file
            .metadata()
            .with_context(|| format!("stat-ing the file for {name} after copying it"))?;
        ensure!(
            after.len() == len
                && after.mtime() == meta.mtime()
                && after.mtime_nsec() == meta.mtime_nsec(),
            "{name} changed while it was archived: it was {len} bytes, modified at {}.{:09}, \
             and is now {} bytes, modified at {}.{:09}",
            meta.mtime(),
            meta.mtime_nsec(),
            after.len(),
            after.mtime(),
            after.mtime_nsec()
        );

        let record = digest.record(name, len);
        self.entries.push(record.clone());
        Ok(record)
    }
}

/// The headers of one entry, as the reader found them.
struct RawEntry {
    name: String,
    /// The bytes the entry stores, before its padding.
    stored: u64,
    /// The file's length when the entry is in sparse format 1.0.
    sparse_len: Option<u64>,
}

/// The entry a reader is inside.
struct OpenEntry {
    name: String,
    len: u64,
    /// The bytes the entry stores, for its padding.
    stored: u64,
    runs: std::vec::IntoIter<Run>,
    /// Where the next byte to hand over goes in the file.
    offset: u64,
    /// Bytes left of the run being handed over.
    run_left: u64,
    /// Whether any of the entry's data has been asked for.
    read_from: bool,
    digest: EntryDigest,
}

/// Reads one archive from `input`, which need not seek. See the module docs for what it
/// refuses.
///
/// [`ArchiveReader::new`] reads the manifest. Then [`ArchiveReader::next_entry`] gives each
/// file in turn and, for the one it gave last, [`ArchiveReader::next_run`] gives its data as
/// `(offset, bytes)` pieces, or [`ArchiveReader::copy_entry_to`] writes them into a file.
///
/// Nothing read is verified until `next_entry` returns `Ok(None)`. A caller writes what it
/// reads into a staging place and installs it only then. After any call fails, every later
/// call fails too.
///
/// `input` gets many reads of 512 bytes or less, so it should be buffered.
pub struct ArchiveReader<R: Read> {
    input: R,
    manifest: ArchiveManifest,
    /// One record per entry read, in order, to check against the trailer.
    seen: Vec<EntryRecord>,
    names: HashSet<String>,
    current: Option<OpenEntry>,
    externals_seen: usize,
    verified: bool,
    failed: bool,
    buf: Vec<u8>,
}

impl<R: Read> ArchiveReader<R> {
    /// Read the manifest from the start of `input`. An archive of another
    /// [`ARCHIVE_VERSION`] is refused here.
    pub fn new(mut input: R) -> Result<Self> {
        let entry = read_entry_headers(&mut input)?
            .context("the archive is empty: it ends before manifest.json")?;
        ensure!(
            entry.name == MANIFEST_NAME && entry.sparse_len.is_none(),
            "the archive starts with {:?}, not manifest.json: it is not a snapshot archive",
            entry.name
        );
        let json = read_json(&mut input, &entry)?;
        let manifest = parse_manifest(&json)?;
        Ok(Self {
            input,
            manifest,
            seen: vec![plain_record(MANIFEST_NAME, &json)],
            names: HashSet::from([MANIFEST_NAME.to_string()]),
            current: None,
            externals_seen: 0,
            verified: false,
            failed: false,
            buf: vec![0; CHUNK],
        })
    }

    /// The archive's manifest. Like everything read, it is unverified until
    /// [`ArchiveReader::next_entry`] returns `Ok(None)`.
    pub fn manifest(&self) -> &ArchiveManifest {
        &self.manifest
    }

    /// Move to the next file, reading past whatever is left of the current one.
    ///
    /// `Ok(None)` means the whole archive was read and verified: the trailer lists exactly
    /// the entries read, every length and digest matches, and the stream ended as an archive
    /// does. Any other end is an error.
    pub fn next_entry(&mut self) -> Result<Option<ArchiveEntry>> {
        ensure!(
            !self.failed,
            "an earlier read of this archive failed, so nothing in it can be trusted"
        );
        let result = self.advance();
        self.failed = result.is_err();
        result
    }

    /// The next piece of the current file's data: the offset it belongs at and at most
    /// [`CHUNK`] bytes. `Ok(None)` once the file has no more data. Everything between the
    /// pieces is a hole, up to the entry's `len`.
    pub fn next_run(&mut self) -> Result<Option<(u64, &[u8])>> {
        ensure!(
            !self.failed,
            "an earlier read of this archive failed, so nothing in it can be trusted"
        );
        match self.fill() {
            Ok(Some((offset, count))) => Ok(Some((offset, &self.buf[..count]))),
            Ok(None) => Ok(None),
            Err(e) => {
                self.failed = true;
                Err(e)
            }
        }
    }

    /// Write the current file into `file`, which has to be empty: its length is set and
    /// each data run is written at its offset, so its holes are never written.
    ///
    /// The entry has to be unread: after a [`ArchiveReader::next_run`] the pieces already
    /// handed over would be missing from `file`. An error here fails the reader like any
    /// other, the destination's included, because part of the entry may then be consumed and
    /// a second copy into another file would lack it.
    pub fn copy_entry_to(&mut self, file: &File) -> Result<()> {
        ensure!(
            !self.failed,
            "an earlier read of this archive failed, so nothing in it can be trusted"
        );
        let result = self.copy_unread_entry_to(file);
        self.failed = result.is_err();
        result
    }

    fn copy_unread_entry_to(&mut self, file: &File) -> Result<()> {
        let (name, len) = match &self.current {
            Some(entry) => {
                ensure!(
                    !entry.read_from,
                    "part of {} was already read with next_run, so a copy of it now would \
                     lack that part",
                    entry.name
                );
                (entry.name.clone(), entry.len)
            }
            None => bail!("the archive reader is not inside an entry"),
        };
        let existing = file
            .metadata()
            .with_context(|| format!("stat-ing the destination of {name}"))?
            .len();
        ensure!(
            existing == 0,
            "the destination of {name} already holds {existing} bytes"
        );
        file.set_len(len)
            .with_context(|| format!("setting the length of {name} to {len}"))?;
        while let Some((offset, bytes)) = self.next_run()? {
            file.write_all_at(bytes, offset)
                .with_context(|| format!("writing {name} at offset {offset}"))?;
        }
        Ok(())
    }

    fn advance(&mut self) -> Result<Option<ArchiveEntry>> {
        if self.verified {
            return Ok(None);
        }
        // What the caller left unread still has to be hashed.
        while self.fill()?.is_some() {}

        let Some(entry) = read_entry_headers(&mut self.input)? else {
            bail!(
                "the archive ends without trailer.json: the export did not finish, so \
                 nothing in it can be trusted"
            );
        };
        ensure!(
            self.names.insert(entry.name.clone()),
            "the archive has two entries named {:?}",
            entry.name
        );
        let Some(len) = entry.sparse_len else {
            ensure!(
                entry.name == TRAILER_NAME,
                "the archive entry {:?} is not stored as a snapshot file is",
                entry.name
            );
            self.verify_trailer(&entry)?;
            self.verified = true;
            return Ok(None);
        };

        // `seen` holds the manifest's row as well.
        ensure!(
            self.seen.len() <= MAX_ENTRIES,
            "the archive holds more than {MAX_ENTRIES} files"
        );
        let name = parse_entry_name(&entry.name)?;
        match &name {
            EntryName::Snapshot(_) => ensure!(
                self.externals_seen == 0,
                "the archive entry {:?} comes after an external file",
                entry.name
            ),
            EntryName::External(index) => {
                ensure!(
                    *index == self.externals_seen && *index < self.manifest.externals.len(),
                    "the archive entry {:?} is out of place: the manifest lists {} external \
                     files and the next one is external/{}",
                    entry.name,
                    self.manifest.externals.len(),
                    self.externals_seen
                );
                self.externals_seen += 1;
            }
        }
        let runs = read_sparse_map(&mut self.input, &entry.name, len, entry.stored)?;
        let data_len = runs.iter().map(|run| run.len).sum();
        self.current = Some(OpenEntry {
            name: entry.name,
            len,
            stored: entry.stored,
            runs: runs.into_iter(),
            offset: 0,
            run_left: 0,
            read_from: false,
            digest: EntryDigest::new(len),
        });
        Ok(Some(ArchiveEntry {
            name,
            len,
            data_len,
        }))
    }

    /// Read the next piece of the current entry into the buffer: where it goes and how many
    /// bytes. `None` when the entry is finished, or there is none.
    fn fill(&mut self) -> Result<Option<(u64, usize)>> {
        let Some(entry) = self.current.as_mut() else {
            return Ok(None);
        };
        entry.read_from = true;
        if entry.run_left == 0 {
            match entry.runs.next() {
                Some(run) => {
                    entry.digest.run(run);
                    entry.offset = run.offset;
                    entry.run_left = run.len;
                }
                None => {
                    read_padding(
                        &mut self.input,
                        padding(entry.stored),
                        &format!("the padding after {}", entry.name),
                    )?;
                    let entry = self.current.take().expect("checked above");
                    self.seen.push(entry.digest.record(entry.name, entry.len));
                    return Ok(None);
                }
            }
        }
        let count = usize::try_from(entry.run_left).map_or(CHUNK, |left| left.min(CHUNK));
        read_full(
            &mut self.input,
            &mut self.buf[..count],
            &format!("the data of {}", entry.name),
        )?;
        entry.digest.data(&self.buf[..count]);
        let offset = entry.offset;
        entry.offset += count as u64;
        entry.run_left -= count as u64;
        Ok(Some((offset, count)))
    }

    /// Read `trailer.json` and the end of the archive, and check every entry read against it.
    fn verify_trailer(&mut self, entry: &RawEntry) -> Result<()> {
        let json = read_json(&mut self.input, entry)?;
        let trailer: Trailer = serde_json::from_slice(&json).context("parsing trailer.json")?;
        ensure!(
            trailer.archive_version == ARCHIVE_VERSION,
            "trailer.json says archive version {}, not {ARCHIVE_VERSION}",
            trailer.archive_version
        );
        check_trailer(&self.seen, &trailer.entries)?;
        ensure!(
            self.externals_seen == self.manifest.externals.len(),
            "the manifest lists {} external files and the archive holds {}",
            self.manifest.externals.len(),
            self.externals_seen
        );

        // Two zero blocks end an archive, and only zeros may follow them.
        let mut block = [0u8; BLOCK];
        for _ in 0..2 {
            read_full(&mut self.input, &mut block, "the end of the archive")?;
            ensure!(
                block == ZERO_BLOCK,
                "the archive has an entry after trailer.json"
            );
        }
        loop {
            match self.input.read(&mut self.buf) {
                Ok(0) => return Ok(()),
                Ok(count) => ensure!(
                    self.buf[..count].iter().all(|&byte| byte == 0),
                    "the archive has data after its end"
                ),
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(e).context("reading past the end of the archive"),
            }
        }
    }
}

/// Check the entries read, `seen`, against the ones the trailer lists: the same names in
/// the same order, each with the same length, stored bytes and digest.
fn check_trailer(seen: &[EntryRecord], listed: &[EntryRecord]) -> Result<()> {
    for record in seen {
        ensure!(
            listed.iter().any(|other| other.name == record.name),
            "the archive entry {} is not listed in trailer.json",
            record.name
        );
    }
    for record in listed {
        ensure!(
            seen.iter().any(|other| other.name == record.name),
            "trailer.json lists {}, which the archive does not hold",
            record.name
        );
    }
    ensure!(
        seen.len() == listed.len(),
        "trailer.json lists {} entries and the archive holds {}",
        listed.len(),
        seen.len()
    );
    for (read, recorded) in seen.iter().zip(listed) {
        ensure!(
            read.name == recorded.name,
            "the archive holds {} where trailer.json lists {}",
            read.name,
            recorded.name
        );
        ensure!(
            read.len == recorded.len && read.data_len == recorded.data_len,
            "the archive entry {} is {} bytes long with {} bytes of data, and trailer.json \
             records {} and {}",
            read.name,
            read.len,
            read.data_len,
            recorded.len,
            recorded.data_len
        );
        ensure!(
            read.sha256 == recorded.sha256,
            "the archive entry {} is corrupt: its SHA-256 is {} and trailer.json records {}",
            read.name,
            read.sha256,
            recorded.sha256
        );
    }
    Ok(())
}

/// `manifest.json`, refused by its version before anything else about it is read: another
/// version may not have this version's fields.
fn parse_manifest(json: &[u8]) -> Result<ArchiveManifest> {
    #[derive(Deserialize)]
    struct Version {
        archive_version: u32,
    }
    let version: Version =
        serde_json::from_slice(json).context("manifest.json has no readable archive_version")?;
    ensure!(
        version.archive_version == ARCHIVE_VERSION,
        "the archive is version {} and this fcvm reads version {ARCHIVE_VERSION}",
        version.archive_version
    );
    serde_json::from_slice(json).context("parsing manifest.json")
}

/// Read the pax extended header and the file header of the next entry. `None` for a zero
/// block, which is where an archive ends.
fn read_entry_headers<R: Read>(input: &mut R) -> Result<Option<RawEntry>> {
    let mut block = [0u8; BLOCK];
    read_full(input, &mut block, "an entry's header")?;
    if block == ZERO_BLOCK {
        return Ok(None);
    }
    let pax_header = parse_ustar_header(&block)?;
    ensure!(
        pax_header.kind == EntryType::XHeader,
        "an archive entry of type {:?} does not start with a pax extended header: the \
         stream is not a snapshot archive",
        pax_header.kind
    );
    ensure!(
        pax_header.size <= MAX_PAX_BYTES,
        "an archive entry has {} bytes of pax records (the limit is {MAX_PAX_BYTES})",
        pax_header.size
    );
    let mut records = vec![0u8; pax_header.size as usize];
    read_full(input, &mut records, "an entry's pax records")?;
    read_padding(
        input,
        padding(pax_header.size),
        "the padding after an entry's pax records",
    )?;
    let records = parse_pax_records(&records)?;

    read_full(input, &mut block, "an entry's header")?;
    let header = parse_ustar_header(&block)?;
    ensure!(
        header.kind == EntryType::Regular,
        "an archive entry of type {:?} is not a regular file",
        header.kind
    );
    let stored = records
        .size
        .context("an archive entry has no pax size record")?;
    ensure!(
        header.size == stored || header.size == 0,
        "an archive entry's header says {} bytes and its pax size record says {stored}",
        header.size
    );

    let sparse = (
        records.sparse_major,
        records.sparse_minor,
        records.sparse_name,
        records.sparse_realsize,
    );
    match sparse {
        (None, None, None, None) => Ok(Some(RawEntry {
            name: String::from_utf8(header.name)
                .ok()
                .context("an archive entry's name is not UTF-8")?,
            stored,
            sparse_len: None,
        })),
        (Some(1), Some(0), Some(name), Some(len)) => Ok(Some(RawEntry {
            name,
            stored,
            sparse_len: Some(len),
        })),
        _ => bail!("an archive entry's GNU.sparse records are not those of sparse format 1.0"),
    }
}

/// Read the whole of a JSON entry and its padding.
fn read_json<R: Read>(input: &mut R, entry: &RawEntry) -> Result<Vec<u8>> {
    ensure!(
        entry.stored <= MAX_JSON_BYTES,
        "{} is {} bytes (the limit is {MAX_JSON_BYTES})",
        entry.name,
        entry.stored
    );
    let mut json = vec![0u8; entry.stored as usize];
    read_full(input, &mut json, &entry.name)?;
    read_padding(
        input,
        padding(entry.stored),
        &format!("the padding after {}", entry.name),
    )?;
    Ok(json)
}

/// Read the map of the sparse entry `name`, a file of `len` bytes that stores `stored`
/// bytes, and return its data runs. The run of length 0 that marks a hole at the end is
/// checked and left out.
fn read_sparse_map<R: Read>(input: &mut R, name: &str, len: u64, stored: u64) -> Result<Vec<Run>> {
    let what = format!("the sparse map of {name}");
    let mut runs: Vec<Run> = Vec::new();
    // The numbers of the map in order: the count, then an offset and a length per run.
    let mut count: Option<u64> = None;
    let mut pairs_read = 0u64;
    let mut offset: Option<u64> = None;
    let mut marked_end = false;
    let mut digits: Vec<u8> = Vec::new();
    let mut map_bytes = 0u64;

    let mut block = [0u8; BLOCK];
    'blocks: loop {
        map_bytes += BLOCK_U64;
        ensure!(
            map_bytes <= stored,
            "{what} is longer than the {stored} bytes the entry stores"
        );
        read_full(input, &mut block, &what)?;
        for (at, &byte) in block.iter().enumerate() {
            if byte != b'\n' {
                ensure!(
                    byte.is_ascii_digit() && digits.len() < 20,
                    "{what} has the byte {byte:#04x} where a digit or a newline belongs"
                );
                digits.push(byte);
                continue;
            }
            let number = parse_decimal(&digits).with_context(|| format!("reading {what}"))?;
            digits.clear();
            let Some(count) = count else {
                ensure!(
                    number <= MAX_RUNS as u64 + 1,
                    "{what} has {number} runs (the limit is {MAX_RUNS})"
                );
                count = Some(number);
                if number == 0 {
                    ensure!(
                        block[at + 1..].iter().all(|&byte| byte == 0),
                        "{what} is followed by bytes that are not zeros"
                    );
                    break 'blocks;
                }
                continue;
            };
            let Some(run_offset) = offset.take() else {
                offset = Some(number);
                continue;
            };
            pairs_read += 1;
            let last = pairs_read == count;
            let end_of_data = runs.last().map_or(0, Run::end);
            if number == 0 {
                ensure!(
                    last && run_offset == len && end_of_data < len,
                    "{what} has an empty run at offset {run_offset} that does not mark a \
                     hole at the end of the file's {len} bytes"
                );
                marked_end = true;
            } else {
                ensure!(
                    run_offset >= end_of_data,
                    "{what} has a run at offset {run_offset}, before the end of the run \
                     ahead of it"
                );
                ensure!(
                    run_offset.checked_add(number).is_some_and(|end| end <= len),
                    "{what} has a run of {number} bytes at offset {run_offset}, past the \
                     file's {len} bytes"
                );
                // GNU tar reads each run in 512-byte blocks, so only the last may end off
                // a block; see the module docs.
                if let Some(before) = runs.last() {
                    ensure!(
                        before.len % BLOCK_U64 == 0,
                        "{what} has a run of {} bytes ahead of another, which is not a \
                         multiple of {BLOCK}",
                        before.len
                    );
                }
                runs.push(Run {
                    offset: run_offset,
                    len: number,
                });
            }
            if last {
                ensure!(
                    block[at + 1..].iter().all(|&byte| byte == 0),
                    "{what} is followed by bytes that are not zeros"
                );
                break 'blocks;
            }
        }
    }

    let end_of_data = runs.last().map_or(0, Run::end);
    ensure!(
        marked_end || end_of_data == len,
        "{what} ends at offset {end_of_data} and does not mark the hole up to the file's \
         {len} bytes"
    );
    let data_len: u64 = runs.iter().map(|run| run.len).sum();
    ensure!(
        map_bytes.checked_add(data_len) == Some(stored),
        "{what} describes {data_len} bytes of data after a map of {map_bytes} bytes, and \
         the entry stores {stored}"
    );
    Ok(runs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::time::Instant;

    const PAGE: u64 = 4096;
    const MIB: u64 = 1 << 20;

    /// A directory for the test's files on the filesystem the snapshot store is on, which
    /// keeps a 4 KiB hole. `sparse_file` checks that it does.
    fn test_dir() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("snapshot-archive-test-")
            .tempdir_in("/mnt/fcvm-btrfs")
            .expect("a test directory under /mnt/fcvm-btrfs")
    }

    fn manifest(externals: usize) -> ArchiveManifest {
        ArchiveManifest {
            archive_version: ARCHIVE_VERSION,
            arch: "aarch64".into(),
            network_boundary_version: 1,
            container_start_version: 1,
            working_set_version: 2,
            tag: "example-tag".into(),
            kind: SnapshotKind::Full,
            hypervisor: Backend::Firecracker,
            data_dir: "/example/data".into(),
            config_sha256: "00".repeat(32),
            memory: Some(ManifestMemory {
                len: 1 << 30,
                mtime_sec: 1_700_000_000,
                mtime_nsec: 123_456_789,
            }),
            firecracker: ManifestFirecracker {
                path: "/example/firecracker".into(),
                version_output: "Firecracker v0.0.0-example".into(),
                sha256: "11".repeat(32),
            },
            externals: (0..externals)
                .map(|n| ManifestExternal {
                    role: ExternalRole::ImageDisk,
                    path: format!("/example/external-{n}").into(),
                    len: 0,
                    sha256: Some("22".repeat(32)),
                })
                .collect(),
            pmem_owner_uid: None,
            not_shipped: vec!["volume /example/volume".into()],
        }
    }

    /// `len` bytes that are never zero and differ from offset to offset, so data that lands
    /// at the wrong place or goes missing changes a comparison.
    fn pattern(seed: u64, len: usize) -> Vec<u8> {
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

    /// A file of `len` bytes in `dir` holding `runs` and nothing else, synced so its extents
    /// are settled.
    fn sparse_file(dir: &Path, name: &str, len: u64, runs: &[(u64, Vec<u8>)]) -> File {
        let file = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(dir.join(name))
            .unwrap();
        assert_eq!(
            file.metadata().unwrap().blksize(),
            PAGE,
            "control: {} is on a filesystem that cannot keep a 4 KiB hole",
            dir.display()
        );
        file.set_len(len).unwrap();
        for (offset, bytes) in runs {
            file.write_all_at(bytes, *offset).unwrap();
        }
        file.sync_all().unwrap();
        file
    }

    fn empty_file(dir: &Path, name: &str) -> File {
        File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(dir.join(name))
            .unwrap()
    }

    /// The data runs of `file` as `(start, end)`, from SEEK_DATA and SEEK_HOLE.
    fn data_runs(file: &File) -> Vec<(u64, u64)> {
        file.sync_all().unwrap();
        let mut runs = Vec::new();
        crate::uffd::for_each_data_run(file, file.metadata().unwrap().len(), |start, end| {
            runs.push((start, end))
        })
        .unwrap();
        runs
    }

    /// `out` holds what `source` holds: the same length, the same data runs, and the same
    /// bytes in every run. Everything else is a hole in both, which reads as zeros.
    fn assert_same_file(out: &File, source: &File, what: &str) {
        let len = source.metadata().unwrap().len();
        assert_eq!(out.metadata().unwrap().len(), len, "{what}: length");
        let runs = data_runs(source);
        assert_eq!(data_runs(out), runs, "{what}: data runs");
        for (start, end) in runs {
            let mut expected = vec![0; (end - start) as usize];
            source.read_exact_at(&mut expected, start).unwrap();
            let mut found = vec![0; expected.len()];
            out.read_exact_at(&mut found, start).unwrap();
            assert!(
                found == expected,
                "{what}: the run at offset {start} differs"
            );
        }
    }

    /// One entry as a reader gave it: its header and its `(offset, bytes)` pieces.
    type ReadEntry = (ArchiveEntry, Vec<(u64, Vec<u8>)>);

    /// Read all of `stream`. `Ok` only when the reader verified it.
    fn read_all(stream: &[u8]) -> Result<(ArchiveManifest, Vec<ReadEntry>)> {
        let mut reader = ArchiveReader::new(stream)?;
        let mut entries = Vec::new();
        while let Some(entry) = reader.next_entry()? {
            let mut pieces = Vec::new();
            while let Some((offset, bytes)) = reader.next_run()? {
                pieces.push((offset, bytes.to_vec()));
            }
            entries.push((entry, pieces));
        }
        Ok((reader.manifest().clone(), entries))
    }

    /// The error a reader gives for `stream`, which it has to refuse.
    fn rejection(stream: &[u8]) -> String {
        match read_all(stream) {
            Ok((_, entries)) => panic!(
                "the reader accepted a stream it has to refuse, with {} entries",
                entries.len()
            ),
            Err(e) => format!("{e:#}"),
        }
    }

    /// A small archive: two snapshot files, an empty one and one external file. About 20 KiB,
    /// so a test can try every byte of it. Also returns where `trailer.json` starts.
    fn small_archive(dir: &Path) -> (Vec<u8>, usize) {
        let dense = sparse_file(dir, "small-dense", 700, &[(0, pattern(1, 700))]);
        let holed = sparse_file(
            dir,
            "small-holed",
            3 * PAGE,
            &[(PAGE, pattern(2, PAGE as usize))],
        );
        let empty = empty_file(dir, "small-empty");
        let external = sparse_file(dir, "small-external", 100, &[(0, pattern(3, 100))]);

        let mut writer = ArchiveWriter::new(Vec::new(), &manifest(1)).unwrap();
        writer
            .append_snapshot_file(Path::new("config.json"), &dense)
            .unwrap();
        writer
            .append_snapshot_file(Path::new("disk/disk.raw"), &holed)
            .unwrap();
        writer
            .append_snapshot_file(Path::new("empty"), &empty)
            .unwrap();
        writer.append_external(&external).unwrap();
        let trailer_at = writer.out.len();
        let archive = writer.finish().unwrap();
        // Control: the archive the rejection tests cut and corrupt is one the reader accepts.
        let (_, entries) = read_all(&archive).expect("the small archive verifies");
        assert_eq!(entries.len(), 4);
        (archive, trailer_at)
    }

    #[test]
    fn round_trip_keeps_holes_and_bytes() {
        let dir = test_dir();
        // Data at the start, a run longer than the copy buffer in the middle, a page near
        // the end, and a hole after it up to a length that is not a multiple of a block.
        let memory_len = 64 * MIB + 123;
        let memory = sparse_file(
            dir.path(),
            "memory",
            memory_len,
            &[
                (0, pattern(10, 2 * PAGE as usize)),
                (MIB + PAGE, pattern(11, (3 * MIB) as usize)),
                (40 * MIB, pattern(12, PAGE as usize)),
            ],
        );
        // A file that ends in data, part way into its last page.
        let tail_len = 5 * PAGE + 100;
        let tail = sparse_file(
            dir.path(),
            "tail",
            tail_len,
            &[(4 * PAGE, pattern(13, PAGE as usize + 100))],
        );
        let empty = empty_file(dir.path(), "empty");
        let hole = sparse_file(dir.path(), "hole", 8 * MIB, &[]);
        let external = sparse_file(
            dir.path(),
            "external",
            2 * MIB,
            &[(MIB, pattern(14, PAGE as usize))],
        );
        // Control: the filesystem reports the holes this test is about.
        assert_eq!(
            data_runs(&memory),
            [
                (0, 2 * PAGE),
                (MIB + PAGE, 4 * MIB + PAGE),
                (40 * MIB, 40 * MIB + PAGE)
            ]
        );
        assert!(data_runs(&hole).is_empty());

        let sources = [
            (EntryName::Snapshot("memory.bin".into()), &memory),
            (EntryName::Snapshot("disk/disk.raw".into()), &tail),
            (EntryName::Snapshot("memory.bin.working-set".into()), &empty),
            (EntryName::Snapshot("volume-table.json".into()), &hole),
            (EntryName::External(0), &external),
        ];
        let mut writer = ArchiveWriter::new(Vec::new(), &manifest(1)).unwrap();
        let mut written = Vec::new();
        for (name, file) in &sources {
            written.push(match name {
                EntryName::Snapshot(path) => writer.append_snapshot_file(path, file).unwrap(),
                EntryName::External(_) => writer.append_external(file).unwrap(),
            });
        }
        let archive = writer.finish().unwrap();

        let total: u64 = sources
            .iter()
            .map(|(_, file)| file.metadata().unwrap().len())
            .sum();
        let data: u64 = written.iter().map(|record| record.data_len).sum();
        println!(
            "files: {total} bytes, data runs: {data} bytes, archive: {} bytes",
            archive.len()
        );
        assert_eq!(written[0].name, "snapshot/memory.bin");
        assert_eq!(written[0].len, memory_len);
        assert_eq!(written[0].data_len, 2 * PAGE + 3 * MIB + PAGE);
        assert_eq!(written[4].name, "external/0");
        assert!(
            (archive.len() as u64) < data + 64 * 1024 && (archive.len() as u64) < total / 16,
            "the archive is {} bytes for {data} bytes of data in {total} bytes of files",
            archive.len()
        );

        let mut reader = ArchiveReader::new(archive.as_slice()).unwrap();
        assert_eq!(reader.manifest(), &manifest(1));
        let mut restored = Vec::new();
        for (index, (name, source)) in sources.iter().enumerate() {
            let entry = reader.next_entry().unwrap().expect("an entry per file");
            assert_eq!(&entry.name, name);
            assert_eq!(entry.len, source.metadata().unwrap().len());
            assert_eq!(entry.data_len, written[index].data_len);
            let out = empty_file(dir.path(), &format!("restored-{index}"));
            reader.copy_entry_to(&out).unwrap();
            restored.push(out);
        }
        assert_eq!(
            reader.next_entry().unwrap(),
            None,
            "verified at the trailer"
        );
        assert_eq!(reader.next_entry().unwrap(), None, "and it stays verified");

        for ((name, source), out) in sources.iter().zip(&restored) {
            assert_same_file(out, source, &format!("{name:?}"));
        }
    }

    #[test]
    fn a_reader_hands_over_at_most_one_chunk_and_never_a_hole() {
        let dir = test_dir();
        let run = pattern(20, (2 * MIB + PAGE) as usize);
        let file = sparse_file(dir.path(), "file", 16 * MIB, &[(4 * MIB, run.clone())]);
        let mut writer = ArchiveWriter::new(Vec::new(), &manifest(0)).unwrap();
        writer
            .append_snapshot_file(Path::new("memory.bin"), &file)
            .unwrap();
        let archive = writer.finish().unwrap();

        let (_, entries) = read_all(&archive).unwrap();
        let (entry, pieces) = &entries[0];
        assert_eq!(entry.len, 16 * MIB);
        let sizes: Vec<usize> = pieces.iter().map(|(_, bytes)| bytes.len()).collect();
        assert_eq!(sizes, [CHUNK, CHUNK, PAGE as usize]);
        let mut expected_offset = 4 * MIB;
        let mut joined = Vec::new();
        for (offset, bytes) in pieces {
            assert_eq!(*offset, expected_offset);
            expected_offset += bytes.len() as u64;
            joined.extend_from_slice(bytes);
        }
        assert!(joined == run);
    }

    #[test]
    fn a_tebibyte_file_costs_only_its_two_runs() {
        const TIB: u64 = 1 << 40;
        let dir = test_dir();
        let first = (123 * PAGE, pattern(30, 2 * PAGE as usize));
        let second = (TIB - 1024 * MIB, pattern(31, PAGE as usize));
        let source = sparse_file(dir.path(), "source", TIB, &[first.clone(), second.clone()]);

        let started = Instant::now();
        let mut writer = ArchiveWriter::new(Vec::new(), &manifest(0)).unwrap();
        let record = writer
            .append_snapshot_file(Path::new("memory.bin"), &source)
            .unwrap();
        let archive = writer.finish().unwrap();
        let export = started.elapsed();
        assert_eq!(record.len, TIB);
        // `data_len` counts the bytes read from the file, so the holes were not read.
        assert_eq!(record.data_len, 3 * PAGE);
        assert!(
            archive.len() < 32 * 1024,
            "the archive of two small runs is {} bytes",
            archive.len()
        );

        let started = Instant::now();
        let out = empty_file(dir.path(), "restored");
        let mut reader = ArchiveReader::new(archive.as_slice()).unwrap();
        let entry = reader.next_entry().unwrap().unwrap();
        assert_eq!((entry.len, entry.data_len), (TIB, 3 * PAGE));
        reader.copy_entry_to(&out).unwrap();
        assert_eq!(reader.next_entry().unwrap(), None);
        let import = started.elapsed();
        println!(
            "1 TiB file: archive {} bytes, export {export:?}, import {import:?}",
            archive.len()
        );

        let expected = [(first.0, first.0 + 2 * PAGE), (second.0, second.0 + PAGE)];
        assert_eq!(data_runs(&source), expected, "control: the source's runs");
        assert_same_file(&out, &source, "the 1 TiB file");
        assert!(
            out.metadata().unwrap().blocks() * 512 < MIB,
            "the restored file allocates {} bytes",
            out.metadata().unwrap().blocks() * 512
        );
    }

    #[test]
    fn a_stream_cut_at_any_length_is_rejected() {
        let dir = test_dir();
        let (archive, _) = small_archive(dir.path());
        for cut in 0..archive.len() {
            let error = rejection(&archive[..cut]);
            assert!(
                error.contains("cut short"),
                "cut at {cut} of {}: {error}",
                archive.len()
            );
        }
        println!(
            "all {} cuts rejected; cut at the half: {}",
            archive.len(),
            rejection(&archive[..archive.len() / 2])
        );
    }

    #[test]
    fn a_stream_without_a_trailer_is_rejected() {
        let dir = test_dir();
        let (archive, trailer_at) = small_archive(dir.path());

        // Every entry but the trailer, ended as a tar archive ends.
        let mut ended = archive[..trailer_at].to_vec();
        ended.extend_from_slice(&[0; 2 * BLOCK]);
        let error = rejection(&ended);
        assert!(error.contains("ends without trailer.json"), "{error}");
        println!("ended without a trailer: {error}");

        // The same entries and then end of input, which is where a dead exporter leaves it.
        let error = rejection(&archive[..trailer_at]);
        assert!(error.contains("cut short"), "{error}");
        println!("cut before the trailer: {error}");
    }

    #[test]
    fn a_corrupted_data_byte_is_rejected() {
        let dir = test_dir();
        let (archive, _) = small_archive(dir.path());
        let data = pattern(2, PAGE as usize);
        let at = archive
            .windows(data.len())
            .position(|window| window == data)
            .expect("the data of disk/disk.raw is in the archive")
            + 1000;

        let mut corrupt = archive.clone();
        corrupt[at] ^= 0x01;
        let error = rejection(&corrupt);
        assert!(
            error.contains("snapshot/disk/disk.raw is corrupt: its SHA-256 is"),
            "{error}"
        );
        println!("one bit of data flipped: {error}");

        // A reader that skips the file's data refuses it all the same.
        let mut reader = ArchiveReader::new(corrupt.as_slice()).unwrap();
        let error = loop {
            match reader.next_entry() {
                Ok(Some(_)) => {}
                Ok(None) => panic!("the reader verified a corrupt archive"),
                Err(e) => break format!("{e:#}"),
            }
        };
        assert!(error.contains("is corrupt"), "{error}");
        // And it stays refused.
        let error = format!("{:#}", reader.next_entry().unwrap_err());
        assert!(error.contains("an earlier read"), "{error}");
    }

    #[test]
    fn inverting_any_one_byte_is_rejected() {
        let dir = test_dir();
        let (archive, _) = small_archive(dir.path());
        let mut corrupt = archive.clone();
        for at in 0..archive.len() {
            corrupt[at] = !archive[at];
            if let Ok((_, entries)) = read_all(&corrupt) {
                panic!(
                    "the reader verified the archive with byte {at} of {} inverted ({} entries)",
                    archive.len(),
                    entries.len()
                );
            }
            corrupt[at] = archive[at];
        }
        println!("all {} single-byte inversions rejected", archive.len());
    }

    /// An archive with one sparse entry named `name`, written past the writer's own check
    /// of names, with a trailer that lists it.
    fn archive_with_entry_named(file: &File, name: &str) -> Vec<u8> {
        let mut writer = ArchiveWriter::new(Vec::new(), &manifest(0)).unwrap();
        writer
            .append_sparse(name.to_string(), file, &mut |_, _| Ok(()))
            .unwrap();
        writer.finish().unwrap()
    }

    #[test]
    fn an_entry_name_that_leaves_the_staging_directory_is_rejected() {
        let dir = test_dir();
        let file = sparse_file(dir.path(), "file", 100, &[(0, pattern(4, 100))]);
        // Control: the crafted archive is refused for its name and nothing else.
        let control = archive_with_entry_named(&file, "snapshot/disk/disk.raw");
        let (_, entries) = read_all(&control).unwrap();
        assert_eq!(
            entries[0].0.name,
            EntryName::Snapshot("disk/disk.raw".into())
        );

        for (name, expected) in [
            ("snapshot/../escape", "has a '..' component"),
            ("../escape", "has a '..' component"),
            ("snapshot/disk/../../escape", "has a '..' component"),
            ("external/..", "has a '..' component"),
            ("/etc/escape", "is an absolute path"),
            ("//snapshot/escape", "is an absolute path"),
            ("snapshot//escape", "has an empty or '.' component"),
            ("snapshot/./escape", "has an empty or '.' component"),
            ("snapshot/", "has an empty or '.' component"),
            ("elsewhere/escape", "is outside snapshot/ and external/"),
            ("snapshot", "is outside snapshot/ and external/"),
            ("external/one", "is not external/<number>"),
            ("external/0/escape", "is not external/<number>"),
            ("snapshot/new\nline", "has a control character"),
        ] {
            let error = rejection(&archive_with_entry_named(&file, name));
            assert!(error.contains(expected), "{name:?}: {error}");
            println!("{name:?}: {error}");
        }

        // The writer refuses to produce them.
        let mut writer = ArchiveWriter::new(Vec::new(), &manifest(0)).unwrap();
        let error = writer
            .append_snapshot_file(Path::new("../escape"), &file)
            .unwrap_err();
        assert!(format!("{error:#}").contains("'..' component"), "{error:#}");
        let mut writer = ArchiveWriter::new(Vec::new(), &manifest(0)).unwrap();
        let error = writer
            .append_snapshot_file(Path::new("/etc/escape"), &file)
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("has an empty or '.' component"),
            "{error:#}"
        );
    }

    #[test]
    fn an_entry_the_trailer_does_not_list_is_rejected() {
        let dir = test_dir();
        let file = sparse_file(dir.path(), "file", 100, &[(0, pattern(6, 100))]);

        let mut writer = ArchiveWriter::new(Vec::new(), &manifest(0)).unwrap();
        writer
            .append_snapshot_file(Path::new("config.json"), &file)
            .unwrap();
        writer
            .append_snapshot_file(Path::new("unlisted"), &file)
            .unwrap();
        writer.entries.pop();
        let error = rejection(&writer.finish().unwrap());
        assert!(
            error.contains("snapshot/unlisted is not listed in trailer.json"),
            "{error}"
        );
        println!("unlisted entry: {error}");

        let mut writer = ArchiveWriter::new(Vec::new(), &manifest(0)).unwrap();
        let mut record = writer
            .append_snapshot_file(Path::new("config.json"), &file)
            .unwrap();
        record.name = "snapshot/missing".into();
        writer.entries.push(record);
        let error = rejection(&writer.finish().unwrap());
        assert!(
            error.contains("trailer.json lists snapshot/missing, which the archive does not hold"),
            "{error}"
        );

        let mut writer = ArchiveWriter::new(Vec::new(), &manifest(0)).unwrap();
        writer
            .append_snapshot_file(Path::new("config.json"), &file)
            .unwrap();
        writer.entries[1].len += 1;
        let error = rejection(&writer.finish().unwrap());
        assert!(
            error.contains("is 100 bytes long") && error.contains("trailer.json records 101"),
            "{error}"
        );
    }

    #[test]
    fn an_unknown_archive_version_is_rejected() {
        // Refused by its version, whatever else the manifest of that version holds.
        let error = parse_manifest(br#"{"archive_version": 2, "a_later_field": true}"#)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("the archive is version 2 and this fcvm reads version 1"),
            "{error}"
        );

        // The same from a stream: this version's manifest with the number changed in place.
        let dir = test_dir();
        let (archive, _) = small_archive(dir.path());
        let field = br#""archive_version": 1"#;
        let at = archive
            .windows(field.len())
            .position(|window| window == field)
            .expect("the manifest's version field")
            + field.len()
            - 1;
        let mut other = archive.clone();
        other[at] = b'7';
        let error = match ArchiveReader::new(other.as_slice()) {
            Ok(_) => panic!("the reader accepted archive version 7"),
            Err(e) => format!("{e:#}"),
        };
        assert!(
            error.contains("the archive is version 7 and this fcvm reads version 1"),
            "{error}"
        );
        println!("{error}");

        // The writer does not write one either.
        let mut later = manifest(0);
        later.archive_version = 2;
        assert!(ArchiveWriter::new(Vec::new(), &later).is_err());
    }

    #[test]
    fn the_writer_keeps_entries_in_order() {
        let dir = test_dir();
        let file = sparse_file(dir.path(), "file", 100, &[(0, pattern(7, 100))]);

        let mut writer = ArchiveWriter::new(Vec::new(), &manifest(1)).unwrap();
        let error = writer.finish().map(|_| ()).unwrap_err().to_string();
        assert!(
            error.contains("lists 1 external files and 0 were appended"),
            "{error}"
        );

        writer = ArchiveWriter::new(Vec::new(), &manifest(1)).unwrap();
        writer.append_external(&file).unwrap();
        let error = writer
            .append_snapshot_file(Path::new("late"), &file)
            .unwrap_err()
            .to_string();
        assert!(error.contains("come before external files"), "{error}");
        // The stream may be torn after a failure, so the writer refuses to go on.
        let error = writer.finish().map(|_| ()).unwrap_err().to_string();
        assert!(error.contains("an earlier write"), "{error}");

        writer = ArchiveWriter::new(Vec::new(), &manifest(1)).unwrap();
        writer.append_external(&file).unwrap();
        let error = writer.append_external(&file).unwrap_err().to_string();
        assert!(error.contains("one more was appended"), "{error}");

        writer = ArchiveWriter::new(Vec::new(), &manifest(0)).unwrap();
        writer
            .append_snapshot_file(Path::new("twice"), &file)
            .unwrap();
        let error = writer
            .append_snapshot_file(Path::new("twice"), &file)
            .unwrap_err()
            .to_string();
        assert!(error.contains("already has an entry named"), "{error}");
    }

    #[test]
    fn data_runs_are_widened_to_blocks_and_merged() {
        let mut runs = Vec::new();
        for (start, end) in [
            (10, 20),
            (700, 1030),
            (1536, 1537),
            (5000, 5001),
            (9990, 10_000),
        ] {
            push_aligned_run(&mut runs, start, end, 10_000);
        }
        assert_eq!(
            runs,
            [
                // [0, 512) and [512, 1536) and [1536, 2048) touch.
                Run {
                    offset: 0,
                    len: 2048
                },
                Run {
                    offset: 4608,
                    len: 512
                },
                // Clipped to the file's length.
                Run {
                    offset: 9728,
                    len: 272
                },
            ]
        );
    }

    /// The error `read_sparse_map` gives for the map `text` of a file of `len` bytes, with
    /// `data` bytes of runs after it.
    fn map_rejection(text: &str, len: u64, data: u64) -> String {
        let mut block = text.as_bytes().to_vec();
        block.resize(block.len().next_multiple_of(BLOCK), 0);
        let stored = block.len() as u64 + data;
        match read_sparse_map(&mut block.as_slice(), "snapshot/file", len, stored) {
            Ok(runs) => panic!("the map {text:?} was accepted as {runs:?}"),
            Err(e) => format!("{e:#}"),
        }
    }

    #[test]
    fn a_sparse_map_gnu_tar_would_read_differently_is_rejected() {
        // Control: the form the writer produces, with the hole at the end marked.
        let mut good = b"3\n0\n1024\n4096\n100\n8192\n0\n".to_vec();
        good.resize(BLOCK, 0);
        let runs = read_sparse_map(&mut good.as_slice(), "snapshot/file", 8192, 512 + 1124);
        assert_eq!(
            runs.unwrap(),
            [
                Run {
                    offset: 0,
                    len: 1024
                },
                Run {
                    offset: 4096,
                    len: 100
                }
            ]
        );

        for (text, len, data, expected) in [
            // GNU tar pads a run to a block, so it would read the second run 412 bytes on.
            ("2\n0\n100\n4096\n512\n", 4608, 612, "not a multiple of 512"),
            (
                "2\n0\n1024\n512\n1024\n",
                4096,
                2048,
                "before the end of the run",
            ),
            ("1\n4096\n1024\n", 4608, 1024, "past the file's 4608 bytes"),
            (
                "1\n18446744073709551615\n2\n",
                4096,
                2,
                "past the file's 4096",
            ),
            // Without the mark GNU tar leaves the file 1024 bytes long.
            ("1\n0\n1024\n", 8192, 1024, "does not mark the hole"),
            (
                "2\n0\n1024\n4096\n0\n",
                8192,
                1024,
                "does not mark a hole at the end",
            ),
            (
                "2\n0\n0\n0\n1024\n",
                1024,
                1024,
                "does not mark a hole at the end",
            ),
            ("1\n0\n1024\n", 1024, 512, "the entry stores"),
            ("1\n0\n1024\n7\n", 1024, 1024, "bytes that are not zeros"),
            ("1\n0\n01024\n", 1024, 1024, "leading zero"),
            ("1\n0\n-1\n", 1024, 1024, "the byte 0x2d where a digit"),
            // Fewer runs than the count says.
            ("2\n0\n1024\n", 1024, 1024, "the byte 0x00 where a digit"),
            ("1\n\n1024\n", 1024, 1024, "not a decimal number"),
            ("67108866\n", 1024, 0, "the limit is 67108864"),
        ] {
            let error = map_rejection(text, len, data);
            assert!(error.contains(expected), "{text:?}: {error}");
        }
    }

    /// Run GNU tar on `archive`, failing the test when it cannot run or says anything on
    /// stderr.
    fn gnu_tar(args: &[&str], archive: &Path) -> String {
        let output = Command::new("tar")
            .env("LC_ALL", "C")
            .args(args)
            .arg(archive)
            .output()
            .expect(
                "GNU tar is not installed, so this test cannot check that tar reads the \
                 archive; install tar",
            );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success() && stderr.is_empty(),
            "tar {args:?} {}: {}, stderr: {stderr}",
            archive.display(),
            output.status
        );
        String::from_utf8(output.stdout).unwrap()
    }

    #[test]
    fn gnu_tar_lists_and_extracts_the_archive() {
        let version = Command::new("tar").arg("--version").output().expect(
            "GNU tar is not installed, so this test cannot check that tar reads the \
                 archive; install tar",
        );
        let version = String::from_utf8_lossy(&version.stdout).into_owned();
        assert!(
            version.contains("GNU tar"),
            "`tar` is not GNU tar, so this test cannot check GNU tar compatibility: {version}"
        );
        println!("{}", version.lines().next().unwrap());

        let dir = test_dir();
        let memory = sparse_file(
            dir.path(),
            "memory",
            64 * MIB + 123,
            &[
                (0, pattern(40, PAGE as usize)),
                (3 * MIB, pattern(41, 20 * PAGE as usize)),
                (40 * MIB, pattern(42, PAGE as usize)),
            ],
        );
        let tail = sparse_file(
            dir.path(),
            "tail",
            5 * PAGE + 100,
            &[(4 * PAGE, pattern(43, PAGE as usize + 100))],
        );
        let empty = empty_file(dir.path(), "empty");
        let hole = sparse_file(dir.path(), "hole", 8 * MIB, &[]);
        let external = sparse_file(
            dir.path(),
            "external",
            2 * MIB,
            &[(MIB, pattern(44, PAGE as usize))],
        );
        // A length far over the 8 GiB a ustar size field holds, carried by a pax record.
        let tebibyte = sparse_file(
            dir.path(),
            "tebibyte",
            1 << 40,
            &[
                (5 * PAGE, pattern(45, PAGE as usize)),
                ((1 << 40) - 3 * MIB, pattern(46, 2 * PAGE as usize)),
            ],
        );
        let files: [(&str, &File); 6] = [
            ("snapshot/memory.bin", &memory),
            ("snapshot/tebibyte.bin", &tebibyte),
            ("snapshot/disk/disk.raw", &tail),
            ("snapshot/empty", &empty),
            ("snapshot/hole", &hole),
            ("external/0", &external),
        ];

        // Once as the writer writes it, and once with every size over 1000 bytes left out of
        // the header and carried by the pax `size` record alone, as a size over 8 GiB is.
        for (label, header_size_limit) in [("ustar-size", USTAR_MAX_SIZE), ("pax-size", 1000)] {
            let out = File::create(dir.path().join(format!("{label}.tar"))).unwrap();
            let mut writer =
                ArchiveWriter::with_header_size_limit(out, &manifest(1), header_size_limit)
                    .unwrap();
            for (name, file) in &files {
                match name.strip_prefix(SNAPSHOT_PREFIX) {
                    Some(path) => writer.append_snapshot_file(Path::new(path), file).unwrap(),
                    None => writer.append_external(file).unwrap(),
                };
            }
            writer.finish().unwrap().sync_all().unwrap();
            let archive = dir.path().join(format!("{label}.tar"));
            // Control: this module's reader verifies the same bytes tar is given.
            let bytes = std::fs::read(&archive).unwrap();
            assert_eq!(read_all(&bytes).unwrap().1.len(), files.len());
            if header_size_limit == 1000 {
                // Control: the size field of the memory image's header is 0 in this archive.
                let name = b"GNUSparseFile.0/1\0";
                let at = bytes
                    .windows(name.len())
                    .position(|window| window == name)
                    .unwrap();
                assert_eq!(&bytes[at + 124..at + 136], b"00000000000\0");
            }

            let listing = gnu_tar(&["-tf"], &archive);
            let mut expected: Vec<&str> = vec![MANIFEST_NAME];
            expected.extend(files.iter().map(|(name, _)| *name));
            expected.push(TRAILER_NAME);
            assert_eq!(listing.lines().collect::<Vec<_>>(), expected, "{label}");
            println!("{label}: tar -tf lists {expected:?}");

            let extracted = dir.path().join(format!("{label}-extracted"));
            std::fs::create_dir(&extracted).unwrap();
            gnu_tar(&["-C", extracted.to_str().unwrap(), "-xf"], &archive);
            for (name, source) in &files {
                let out = File::open(extracted.join(name)).unwrap();
                assert_same_file(&out, source, &format!("{label} {name}"));
                println!(
                    "{label}: tar -xf extracted {name}: {} bytes, data runs {:?}",
                    out.metadata().unwrap().len(),
                    data_runs(&out)
                );
            }
            let extracted_manifest: ArchiveManifest =
                serde_json::from_slice(&std::fs::read(extracted.join(MANIFEST_NAME)).unwrap())
                    .unwrap();
            assert_eq!(extracted_manifest, manifest(1));
        }
    }

    /// An output that changes the file being archived when the first bytes of its data
    /// arrive, which is after the writer has read the file's length and found its runs.
    struct MeddlingSink {
        out: Vec<u8>,
        /// The first bytes of the file's data.
        trigger: Vec<u8>,
        meddle: Option<Box<dyn FnOnce()>>,
    }

    impl Write for MeddlingSink {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.starts_with(&self.trigger) {
                if let Some(meddle) = self.meddle.take() {
                    meddle();
                }
            }
            self.out.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_file_that_changes_while_it_is_archived_fails_the_append() {
        let dir = test_dir();
        for case in ["untouched", "grown", "rewritten"] {
            let data = pattern(50, PAGE as usize);
            let file = sparse_file(dir.path(), case, 4 * PAGE, &[(PAGE, data.clone())]);
            let other = file.try_clone().unwrap();
            let modified = file.metadata().unwrap().modified().unwrap();
            let meddle: Box<dyn FnOnce()> = match case {
                "untouched" => Box::new(|| {}),
                "grown" => Box::new(move || other.write_all_at(b"more", 4 * PAGE).unwrap()),
                // The same length, and a modification time set apart from the old one: a
                // write this soon after the last can land on the same coarse clock tick.
                _ => Box::new(move || {
                    other.write_all_at(b"x", PAGE).unwrap();
                    other
                        .set_modified(modified + std::time::Duration::from_secs(1))
                        .unwrap();
                }),
            };
            let sink = MeddlingSink {
                out: Vec::new(),
                trigger: data[..64].to_vec(),
                meddle: Some(meddle),
            };
            let mut writer = ArchiveWriter::new(sink, &manifest(0)).unwrap();
            let appended = writer.append_snapshot_file(Path::new("memory.bin"), &file);
            assert!(
                writer.out.meddle.is_none(),
                "control ({case}): the output saw the file's data and ran its change"
            );
            if case == "untouched" {
                // Control: the same file through the same output is archived when nothing
                // changes it.
                appended.expect("an unchanged file");
                assert!(read_all(&writer.finish().unwrap().out).is_ok());
                continue;
            }
            let error = format!("{:#}", appended.expect_err("the file changed"));
            assert!(
                error.contains("snapshot/memory.bin changed while it was archived"),
                "{case}: {error}"
            );
            println!("{case}: {error}");
            let error = writer
                .finish()
                .err()
                .expect("a torn stream is not finished");
            assert!(error.to_string().contains("an earlier write"), "{error:#}");
        }
    }

    #[test]
    fn a_copy_that_fails_fails_the_reader() {
        let dir = test_dir();
        let (archive, _) = small_archive(dir.path());
        let mut reader = ArchiveReader::new(archive.as_slice()).unwrap();
        let entry = reader.next_entry().unwrap().unwrap();
        assert_eq!(entry.name, EntryName::Snapshot("config.json".into()));

        // A destination whose length cannot be set: it is open for reading only.
        let path = dir.path().join("read-only");
        File::create(&path).unwrap();
        let read_only = File::open(&path).unwrap();
        let error = format!("{:#}", reader.copy_entry_to(&read_only).unwrap_err());
        assert!(
            error.contains("setting the length of snapshot/config.json"),
            "{error}"
        );

        // A second copy, into a file that could take it, is refused, and so is the rest.
        let good = empty_file(dir.path(), "good");
        let error = match reader.copy_entry_to(&good) {
            Ok(()) => panic!("the reader copied an entry again after a copy of it failed"),
            Err(e) => format!("{e:#}"),
        };
        assert!(error.contains("an earlier read"), "{error}");
        assert_eq!(good.metadata().unwrap().len(), 0);
        assert!(reader.next_run().is_err());
        assert!(reader.next_entry().is_err());
        println!("after a failed copy: {error}");
    }

    #[test]
    fn a_partly_read_entry_cannot_be_copied() {
        let dir = test_dir();
        let (archive, _) = small_archive(dir.path());
        let mut reader = ArchiveReader::new(archive.as_slice()).unwrap();
        let entry = reader.next_entry().unwrap().unwrap();
        assert_eq!((entry.len, entry.data_len), (700, 700));
        let (offset, bytes) = reader.next_run().unwrap().expect("the entry's data");
        assert_eq!((offset, bytes.len()), (0, 700));

        let out = empty_file(dir.path(), "out");
        let error = match reader.copy_entry_to(&out) {
            Ok(()) => panic!(
                "the reader copied an entry whose data was already handed over: the copy is \
                 {} bytes with {} bytes of data",
                out.metadata().unwrap().len(),
                data_runs(&out)
                    .iter()
                    .map(|(start, end)| end - start)
                    .sum::<u64>()
            ),
            Err(e) => format!("{e:#}"),
        };
        assert!(
            error.contains("part of snapshot/config.json was already read with next_run"),
            "{error}"
        );
        assert_eq!(out.metadata().unwrap().len(), 0, "nothing was written");
        assert!(reader.next_entry().is_err());
        println!("{error}");
    }

    /// An archive of `files` empty files, written past the writer's own limit on them.
    fn archive_of_files(empty: &File, files: usize) -> Vec<u8> {
        let mut writer = ArchiveWriter::new(Vec::new(), &manifest(0)).unwrap();
        for n in 0..files {
            writer
                .append_sparse(format!("snapshot/file-{n}"), empty, &mut |_, _| Ok(()))
                .unwrap();
        }
        writer.finish().unwrap()
    }

    #[test]
    fn the_writer_refuses_one_file_more_than_the_limit_when_it_is_appended() {
        let dir = test_dir();
        let empty = empty_file(dir.path(), "empty");
        let mut writer = ArchiveWriter::new(Vec::new(), &manifest(1)).unwrap();
        for n in 0..MAX_ENTRIES {
            writer
                .append_snapshot_file(Path::new(&format!("file-{n}")), &empty)
                .unwrap();
        }
        let sent = writer.out.len();
        let error = writer.append_external(&empty).unwrap_err().to_string();
        assert!(error.contains("at most 1024 files"), "{error}");
        assert_eq!(writer.out.len(), sent, "nothing of the file was sent");

        let mut writer = ArchiveWriter::new(Vec::new(), &manifest(0)).unwrap();
        for n in 0..MAX_ENTRIES {
            writer
                .append_snapshot_file(Path::new(&format!("file-{n}")), &empty)
                .unwrap();
        }
        let error = writer
            .append_snapshot_file(Path::new("one-more"), &empty)
            .unwrap_err()
            .to_string();
        assert!(error.contains("at most 1024 files"), "{error}");
    }

    #[test]
    fn the_reader_refuses_one_file_more_than_the_limit_when_it_reaches_it() {
        let dir = test_dir();
        let empty = empty_file(dir.path(), "empty");
        // Control: an archive at the limit verifies, and its trailer is far under 16 MiB.
        let full = archive_of_files(&empty, MAX_ENTRIES);
        assert_eq!(read_all(&full).unwrap().1.len(), MAX_ENTRIES);

        let over = archive_of_files(&empty, MAX_ENTRIES + 1);
        let mut reader = ArchiveReader::new(over.as_slice()).unwrap();
        for _ in 0..MAX_ENTRIES {
            reader
                .next_entry()
                .unwrap()
                .expect("a file under the limit");
        }
        let error = format!("{:#}", reader.next_entry().unwrap_err());
        assert!(
            error.contains("the archive holds more than 1024 files"),
            "{error}"
        );
    }

    #[test]
    fn a_file_with_more_runs_than_the_limit_is_refused_where_the_limit_is_passed() {
        let dir = test_dir();
        let file = sparse_file(
            dir.path(),
            "runs",
            8 * PAGE,
            &[
                (0, pattern(60, PAGE as usize)),
                (2 * PAGE, pattern(61, PAGE as usize)),
                (4 * PAGE, pattern(62, PAGE as usize)),
            ],
        );
        let runs = aligned_data_runs(&file, 8 * PAGE, 3).expect("three runs fit a limit of three");
        assert_eq!(runs.len(), 3);
        let error = aligned_data_runs(&file, 8 * PAGE, 2)
            .unwrap_err()
            .to_string();
        assert!(error.contains("more than 2 data runs"), "{error}");
    }

    /// `append_external_with` shows its caller the pieces a reader hands over, and
    /// [`ContentSha256`] over either side's pieces is the SHA-256 of the file's bytes, holes
    /// read as zeros: the digest a caller compares with one recorded elsewhere. An error
    /// from the caller fails the append and the writer.
    #[test]
    fn the_pieces_of_an_external_file_give_the_sha256_of_its_bytes() {
        let dir = test_dir();
        const LEN: u64 = 5 << 20;
        // A hole at each end, a run longer than one copy buffer, and a run that neither
        // starts nor ends on a 512-byte block.
        let file = sparse_file(
            dir.path(),
            "external",
            LEN,
            &[
                (PAGE, pattern(31, CHUNK + 3 * PAGE as usize)),
                (3 << 20, pattern(32, 700)),
            ],
        );
        let mut whole = vec![0u8; LEN as usize];
        file.read_exact_at(&mut whole, 0).unwrap();
        let expected = hex::encode(Sha256::digest(&whole));

        let mut written = ContentSha256::new();
        let mut shown = Vec::new();
        let mut writer = ArchiveWriter::new(Vec::new(), &manifest(1)).unwrap();
        writer
            .append_external_with(&file, |offset, bytes| {
                shown.push((offset, bytes.len()));
                written.piece(offset, bytes)
            })
            .unwrap();
        let stream = writer.finish().unwrap();
        assert_eq!(
            written.finish(LEN).unwrap(),
            expected,
            "the writer's pieces"
        );
        assert!(
            shown.len() > 2,
            "control: the file was shown in several pieces: {shown:?}"
        );

        let mut reader = ArchiveReader::new(stream.as_slice()).unwrap();
        let entry = reader.next_entry().unwrap().unwrap();
        assert_eq!(entry.name, EntryName::External(0));
        let mut read = ContentSha256::new();
        let mut handed = Vec::new();
        while let Some((offset, bytes)) = reader.next_run().unwrap() {
            handed.push((offset, bytes.len()));
            read.piece(offset, bytes).unwrap();
        }
        assert_eq!(read.finish(LEN).unwrap(), expected, "the reader's pieces");
        assert_eq!(
            handed, shown,
            "the writer shows the pieces a reader hands over"
        );

        // Pieces out of order, and more bytes than the file has, are refused.
        let mut content = ContentSha256::new();
        content.piece(4096, b"later").unwrap();
        assert!(content.piece(0, b"earlier").is_err());
        assert!(content.finish(4000).is_err());

        let mut writer = ArchiveWriter::new(Vec::new(), &manifest(1)).unwrap();
        let error = writer
            .append_external_with(&file, |_, _| bail!("the caller refused the piece"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("the caller refused the piece"), "{error}");
        let error = writer.finish().map(|_| ()).unwrap_err().to_string();
        assert!(error.contains("an earlier write"), "{error}");
    }
}

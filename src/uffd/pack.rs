//! A snapshot's recorded working set, packed: the bytes of every recorded run, in first-touch
//! order ([`OrderedRuns`]), compressed as independent zstd frames, in one file beside the
//! memory file (`<memory.bin>.working-set.pack`).
//!
//! # Why
//!
//! Replay reads the recorded set from the memory file: millions of runs a few pages long,
//! scattered over the file (a 128 GiB www guest: 72 GiB in 2.55 million runs). Read with
//! eight threads from a cold page cache that took 40.0 s uncompressed and 65.7 s from a
//! memory file btrfs had compressed. The same bytes packed as zstd level 1 frames, read and
//! decompressed by 32 threads, took 4.1 s (2026-11-04). Frames here are a few MiB, so replay
//! can start on the first frame without waiting for the rest.
//!
//! # Layout
//!
//! All integers little-endian.
//!
//! - Header ([`HEADER_LEN`] bytes): magic `FCVMWPAK`, version, the memory file's length, its
//!   [`ImageKey`], the set's [`PageSet::order_digest`], the number of runs, the number of frames.
//! - The runs, in stream order: `(file_offset: u64, len: u64)` each.
//! - The frames: `(compressed_len: u64, raw_len: u64)` each.
//! - The frames' compressed bytes, back to back.
//!
//! The uncompressed stream is every run's bytes in run order. Frames cut that stream at
//! [`FRAME_RAW`] bytes, so a run can end in a later frame than it starts in.
//!
//! # Building
//!
//! [`build_temp`] writes a pack under a unique temporary name beside the memory file, recorded
//! with [`crate::reclaim`] before it is created, so a builder killed mid-way leaves nothing the
//! next launch does not remove. Threads each read one frame's runs from the memory file and
//! compress it; the writer puts the frames back in order. The caller installs the temporary
//! with a rename, under the rules in [`WorkingSetStore`](super::WorkingSetStore).
//!
//! # Trust
//!
//! A pack is a cache of bytes the memory file holds, so unlike the recorded set it is data, not
//! a hint. It is opened only when its key and order digest match the image and the set being
//! replayed and its run table is the set's runs in first-touch order, and each frame carries a
//! zstd content checksum: a frame that does not decompress to the length its table says makes
//! the rest of the replay read the memory file ([`PackStream`]).

use anyhow::{bail, ensure, Context, Result};
use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use tracing::warn;

#[cfg(test)]
use super::working_set::PageSet;
use super::working_set::{ImageKey, OrderedRuns, Run};

const MAGIC: &[u8; 8] = b"FCVMWPAK";
const VERSION: u64 = 1;
/// magic, version, mem_len, key, order digest, runs, frames.
pub const HEADER_LEN: usize = 8 + 8 + 8 + 32 + 32 + 8 + 8;
/// Uncompressed bytes per frame.
pub const FRAME_RAW: usize = 4 << 20;
/// zstd level: level 1 decompresses as fast as any level and compresses fastest.
const LEVEL: i32 = 1;

/// Where the pack of the memory file at `mem_path` lives.
pub fn path_for(mem_path: &Path) -> PathBuf {
    let mut name = mem_path.as_os_str().to_owned();
    name.push(".working-set.pack");
    PathBuf::from(name)
}

/// The runs a pack of `set` holds, in stream order: the set's runs in first-touch order,
/// clipped to a memory file of `mem_len` bytes.
pub fn runs_of(runs: OrderedRuns, mem_len: u64) -> impl Iterator<Item = Run> {
    runs.filter_map(move |run| {
        let end = run.offset.saturating_add(run.len).min(mem_len);
        (end > run.offset).then(|| Run {
            offset: run.offset,
            len: end - run.offset,
        })
    })
}

/// One frame's place in the file and in the uncompressed stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame {
    /// Offset of its compressed bytes in the file.
    pub file_pos: u64,
    pub compressed_len: u64,
    /// Offset of its first byte in the uncompressed stream.
    pub stream_pos: u64,
    pub raw_len: u64,
}

/// The error of a build its `cancel` stopped ([`build_temp`]).
#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the working-set pack build was cancelled")
    }
}

impl std::error::Error for Cancelled {}

/// What [`build_temp`] wrote.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BuildStats {
    pub runs: u64,
    pub frames: u64,
    pub raw_bytes: u64,
    pub compressed_bytes: u64,
}

/// A pack written under a temporary name, and the reclaim record that names it. Dropping it
/// removes the file; [`PackTemp::install`] renames it into place.
pub struct PackTemp {
    path: PathBuf,
    owner: Option<crate::reclaim::Owner>,
}

impl PackTemp {
    /// Rename the pack to `dest`, replacing what is there. The caller holds whatever makes the
    /// rename safe (see `WorkingSetStore::install_pack`).
    pub fn install(mut self, dest: &Path) -> Result<()> {
        std::fs::rename(&self.path, dest)
            .with_context(|| format!("renaming {} into place", self.path.display()))?;
        if let Some(owner) = self.owner.take() {
            owner.discard().ok();
        }
        Ok(())
    }
}

impl Drop for PackTemp {
    fn drop(&mut self) {
        let Some(owner) = self.owner.take() else {
            return;
        };
        match std::fs::remove_file(&self.path) {
            Ok(()) => {
                owner.discard().ok();
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                owner.discard().ok();
            }
            // The record stays, so the next launch removes the file.
            Err(error) => warn!(
                target: "uffd",
                path = %self.path.display(),
                %error,
                "could not remove an unfinished working-set pack; the next launch reclaims it"
            ),
        }
    }
}

/// Write the pack of `runs` (the set's runs in stream order, [`runs_of`]) from `mem`, the
/// memory file of `mem_len` bytes whose key is `key`, to a temporary file beside `out`. The
/// temporary is recorded under an owner in `reclaim_dir` before it is created. `threads`
/// threads read and compress frames; `cancel` is asked before each frame, and a build it
/// cancels returns a [`Cancelled`] error and leaves no file. The temp is marked nodatacow, so btrfs does
/// not try to compress zstd frames a second time.
#[allow(clippy::too_many_arguments)]
pub fn build_temp(
    mem: &File,
    mem_len: u64,
    key: &ImageKey,
    digest: &[u8; 32],
    runs: &[Run],
    out: &Path,
    threads: usize,
    reclaim_dir: &Path,
    cancel: &(dyn Fn() -> bool + Sync),
) -> Result<(PackTemp, BuildStats)> {
    for run in runs {
        ensure!(
            run.len > 0
                && run
                    .offset
                    .checked_add(run.len)
                    .is_some_and(|end| end <= mem_len),
            "a run lies outside the memory file"
        );
    }
    let id = uuid::Uuid::new_v4().simple().to_string();
    let mut name = out.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{id}.tmp"));
    let path = out.with_file_name(name);
    let owner = crate::reclaim::Owner::begin_in(reclaim_dir, &format!("ws-pack-{id}"))
        .context("recording the working-set pack's temporary")?;
    owner.record(crate::reclaim::Resource::Temp { path: path.clone() })?;
    let temp = PackTemp {
        path,
        owner: Some(owner),
    };
    let file = File::options()
        .write(true)
        .create_new(true)
        .open(&temp.path)
        .with_context(|| format!("creating {}", temp.path.display()))?;
    crate::storage::inode_flags::make_nodatacow(&file);
    let stats = write_pack(mem, mem_len, key, digest, runs, file, threads, cancel)?;
    Ok((temp, stats))
}

/// The pieces of the memory file that make up frame `index`: `(file_offset, len)` each.
fn frame_pieces(runs: &[Run], starts: &[u64], index: u64, total: u64) -> Vec<(u64, u64)> {
    let begin = index * FRAME_RAW as u64;
    let end = (begin + FRAME_RAW as u64).min(total);
    let mut pieces = Vec::new();
    let mut at = starts
        .partition_point(|&start| start <= begin)
        .saturating_sub(1);
    while at < runs.len() && starts[at] < end {
        let run = runs[at];
        let from = begin.max(starts[at]);
        let to = end.min(starts[at] + run.len);
        if to > from {
            pieces.push((run.offset + (from - starts[at]), to - from));
        }
        at += 1;
    }
    pieces
}

#[allow(clippy::too_many_arguments)]
fn write_pack(
    mem: &File,
    mem_len: u64,
    key: &ImageKey,
    digest: &[u8; 32],
    runs: &[Run],
    file: File,
    threads: usize,
    cancel: &(dyn Fn() -> bool + Sync),
) -> Result<BuildStats> {
    let mut starts = Vec::with_capacity(runs.len());
    let mut total = 0u64;
    for run in runs {
        starts.push(total);
        total += run.len;
    }
    let frame_count = total.div_ceil(FRAME_RAW as u64);

    let mut out = BufWriter::with_capacity(8 << 20, file);
    out.write_all(MAGIC)?;
    out.write_all(&VERSION.to_le_bytes())?;
    out.write_all(&mem_len.to_le_bytes())?;
    out.write_all(key.as_bytes())?;
    out.write_all(digest)?;
    out.write_all(&(runs.len() as u64).to_le_bytes())?;
    out.write_all(&frame_count.to_le_bytes())?;
    for run in runs {
        out.write_all(&run.offset.to_le_bytes())?;
        out.write_all(&run.len.to_le_bytes())?;
    }
    let table_pos = (HEADER_LEN + runs.len() * 16) as u64;
    // The frame table is written after the frames, once their compressed lengths are known.
    out.write_all(&vec![0u8; frame_count as usize * 16])?;

    // Each thread claims the next frame, reads its pieces and compresses it. A thread claims
    // a frame only while it is within `window` of the next frame the writer needs, so frames
    // waiting to be written stay bounded.
    struct Claims {
        next: u64,
        written: u64,
        failed: bool,
    }
    let threads = threads.max(1);
    let window = threads as u64 * 2;
    let claims = Mutex::new(Claims {
        next: 0,
        written: 0,
        failed: false,
    });
    let moved = Condvar::new();
    let fail = |claims: &Mutex<Claims>| {
        claims.lock().expect("pack claims").failed = true;
        moved.notify_all();
    };
    let (done_tx, done_rx) = std::sync::mpsc::channel::<Result<(u64, u64, Vec<u8>)>>();
    let mut lengths = vec![(0u64, 0u64); frame_count as usize];
    let mut compressed_bytes = 0u64;
    let written = std::thread::scope(|scope| -> Result<u64> {
        for _ in 0..threads {
            let done_tx = done_tx.clone();
            let (claims, moved, starts, fail) = (&claims, &moved, &starts, &fail);
            scope.spawn(move || {
                let mut raw = Vec::with_capacity(FRAME_RAW);
                let mut compressor = None;
                loop {
                    let index = {
                        let mut state = claims.lock().expect("pack claims");
                        loop {
                            if state.failed || state.next >= frame_count {
                                return;
                            }
                            if state.next < state.written + window {
                                break;
                            }
                            state = moved.wait(state).expect("pack claims");
                        }
                        state.next += 1;
                        state.next - 1
                    };
                    let result = if cancel() {
                        Err(anyhow::Error::new(Cancelled))
                    } else {
                        raw.clear();
                        read_frame(mem, &frame_pieces(runs, starts, index, total), &mut raw)
                            .and_then(|()| {
                                compress(&mut compressor, &raw)
                                    .context("compressing a working-set frame")
                            })
                            .map(|compressed| (index, raw.len() as u64, compressed))
                    };
                    if result.is_err() {
                        fail(claims);
                    }
                    if done_tx.send(result).is_err() {
                        return;
                    }
                }
            });
        }
        drop(done_tx);
        let mut pending = std::collections::BTreeMap::new();
        let mut next = 0u64;
        let mut error = None;
        for result in done_rx {
            match result {
                Ok((index, raw_len, compressed)) if error.is_none() => {
                    pending.insert(index, (raw_len, compressed));
                }
                Ok(_) => {}
                Err(e) => {
                    error.get_or_insert(e);
                }
            }
            while let Some((raw_len, compressed)) = pending.remove(&next) {
                if let Err(e) = out.write_all(&compressed) {
                    error.get_or_insert(e.into());
                    fail(&claims);
                    pending.clear();
                    break;
                }
                lengths[next as usize] = (compressed.len() as u64, raw_len);
                compressed_bytes += compressed.len() as u64;
                next += 1;
                claims.lock().expect("pack claims").written = next;
                moved.notify_all();
            }
        }
        match error {
            Some(error) => Err(error),
            None => Ok(next),
        }
    })?;
    ensure!(
        written == frame_count,
        "wrote {written} of {frame_count} working-set frames"
    );
    let mut file = out.into_inner().map_err(|e| e.into_error())?;
    file.seek(SeekFrom::Start(table_pos))?;
    let mut table = Vec::with_capacity(lengths.len() * 16);
    for (compressed_len, raw_len) in &lengths {
        table.extend_from_slice(&compressed_len.to_le_bytes());
        table.extend_from_slice(&raw_len.to_le_bytes());
    }
    file.write_all(&table)?;
    file.sync_all().context("syncing the working-set pack")?;
    Ok(BuildStats {
        runs: runs.len() as u64,
        frames: frame_count,
        raw_bytes: total,
        compressed_bytes,
    })
}

/// Read `pieces` of the memory file, back to back, onto the end of `raw`.
fn read_frame(mem: &File, pieces: &[(u64, u64)], raw: &mut Vec<u8>) -> Result<()> {
    for &(offset, len) in pieces {
        let start = raw.len();
        raw.resize(start + len as usize, 0);
        mem.read_exact_at(&mut raw[start..], offset)
            .with_context(|| format!("reading the memory file at {offset}"))?;
    }
    Ok(())
}

/// One zstd frame of `raw`, with a content checksum, so a damaged frame fails to decompress
/// instead of returning wrong bytes. `compressor` is the calling thread's, made on first use.
fn compress(
    compressor: &mut Option<zstd::bulk::Compressor<'static>>,
    raw: &[u8],
) -> std::io::Result<Vec<u8>> {
    let compressor = match compressor {
        Some(compressor) => compressor,
        None => {
            let mut made = zstd::bulk::Compressor::new(LEVEL)?;
            made.include_checksum(true)?;
            compressor.insert(made)
        }
    };
    compressor.compress(raw)
}

fn header_names(
    header: &[u8; HEADER_LEN],
    key: &ImageKey,
    digest: &[u8; 32],
    mem_len: u64,
) -> bool {
    let u64_at = |off: usize| u64::from_le_bytes(header[off..off + 8].try_into().unwrap());
    &header[0..8] == MAGIC
        && u64_at(8) == VERSION
        && u64_at(16) == mem_len
        && &header[24..56] == key.as_bytes()
        && &header[56..88] == digest
}

/// Where a range of the memory file comes from, as [`Pack::pieces`] splits it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Piece {
    /// Bytes the pack holds, at `stream_pos` in its uncompressed stream.
    Stream {
        file_offset: u64,
        stream_pos: u64,
        len: u64,
    },
    /// Bytes the pack does not hold.
    File { file_offset: u64, len: u64 },
}

/// An opened pack whose key, order digest and run table match.
pub struct Pack {
    file: File,
    digest: [u8; 32],
    /// The runs, in stream order.
    runs: Vec<Run>,
    /// Each run's offset in the uncompressed stream.
    starts: Vec<u64>,
    frames: Vec<Frame>,
    /// Indices into `runs`, sorted by file offset, for [`Pack::pieces`].
    by_offset: Vec<u32>,
}

impl Pack {
    /// Open the pack at `path` if it describes the image `key` names and the set whose order
    /// digest is `digest` and whose runs, in stream order, are `expected` ([`runs_of`]).
    /// `Ok(None)` when there is no pack or it describes something else; an error for a pack
    /// that could not be read or whose tables are inconsistent.
    pub fn open(
        path: &Path,
        key: &ImageKey,
        digest: &[u8; 32],
        mem_len: u64,
        expected: impl Iterator<Item = Run>,
    ) -> Result<Option<Self>> {
        let file = match File::open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("opening {}", path.display())),
        };
        let mut header = [0u8; HEADER_LEN];
        if file.read_exact_at(&mut header, 0).is_err()
            || !header_names(&header, key, digest, mem_len)
        {
            return Ok(None);
        }
        let u64_at = |off: usize| u64::from_le_bytes(header[off..off + 8].try_into().unwrap());
        let run_count = u64_at(88);
        let frame_count = u64_at(96);
        let file_len = file.metadata()?.len();
        let tables = run_count
            .checked_add(frame_count)
            .and_then(|entries| entries.checked_mul(16))
            .filter(|tables| (HEADER_LEN as u64).saturating_add(*tables) <= file_len);
        let Some(tables) = tables else {
            bail!("{}: tables run past the end of the file", path.display());
        };
        let (run_count, frame_count) = (run_count as usize, frame_count as usize);
        let mut buf = vec![0u8; tables as usize];
        file.read_exact_at(&mut buf, HEADER_LEN as u64)?;
        let at = |off: usize| u64::from_le_bytes(buf[off..off + 8].try_into().unwrap());
        let mut runs = Vec::with_capacity(run_count);
        let mut starts = Vec::with_capacity(run_count);
        let mut expected = expected;
        let mut stream = 0u64;
        for i in 0..run_count {
            let run = Run {
                offset: at(i * 16),
                len: at(i * 16 + 8),
            };
            ensure!(
                expected.next() == Some(run),
                "{}: run {i} is not the recorded set's",
                path.display()
            );
            runs.push(run);
            starts.push(stream);
            stream += run.len;
        }
        ensure!(
            expected.next().is_none(),
            "{}: the recorded set has runs the pack does not",
            path.display()
        );
        let bound = zstd::zstd_safe::compress_bound(FRAME_RAW) as u64;
        let mut frames = Vec::with_capacity(frame_count);
        let mut file_pos = HEADER_LEN as u64 + tables;
        let mut stream_pos = 0u64;
        for i in 0..frame_count {
            let base = run_count * 16 + i * 16;
            let frame = Frame {
                file_pos,
                compressed_len: at(base),
                stream_pos,
                raw_len: at(base + 8),
            };
            ensure!(
                frame.raw_len > 0
                    && frame.raw_len <= FRAME_RAW as u64
                    && frame.compressed_len <= bound,
                "{}: frame {i} has an impossible length",
                path.display()
            );
            file_pos = file_pos.saturating_add(frame.compressed_len);
            stream_pos += frame.raw_len;
            frames.push(frame);
        }
        ensure!(
            file_pos == file_len && stream_pos == stream,
            "{}: the frames do not cover the file and the runs",
            path.display()
        );
        let mut by_offset: Vec<u32> = (0..run_count as u32).collect();
        by_offset.sort_unstable_by_key(|&i| runs[i as usize].offset);
        Ok(Some(Self {
            file,
            digest: *digest,
            runs,
            starts,
            frames,
            by_offset,
        }))
    }

    pub fn digest(&self) -> &[u8; 32] {
        &self.digest
    }

    pub fn file(&self) -> &File {
        &self.file
    }

    pub fn frames(&self) -> &[Frame] {
        &self.frames
    }

    /// Split the memory file's `[offset, offset + len)` into the stretches the pack holds and
    /// the ones it does not, in file order, into `out`. Adjacent stretches that are adjacent in
    /// the stream too come out as one.
    pub fn pieces(&self, offset: u64, len: u64, out: &mut Vec<Piece>) {
        out.clear();
        let end = offset.saturating_add(len);
        let mut at = offset;
        let mut index = self
            .by_offset
            .partition_point(|&i| self.runs[i as usize].offset <= offset)
            .saturating_sub(1);
        let mut push = |piece: Piece| match (out.last_mut(), piece) {
            (
                Some(Piece::Stream {
                    stream_pos: p,
                    len: l,
                    ..
                }),
                Piece::Stream {
                    stream_pos, len, ..
                },
            ) if *p + *l == stream_pos => *l += len,
            (Some(Piece::File { len: l, .. }), Piece::File { len, .. }) => *l += len,
            _ => out.push(piece),
        };
        while at < end {
            let Some(&run_index) = self.by_offset.get(index) else {
                push(Piece::File {
                    file_offset: at,
                    len: end - at,
                });
                break;
            };
            let run = self.runs[run_index as usize];
            let run_end = run.offset + run.len;
            if run_end <= at {
                index += 1;
                continue;
            }
            if run.offset > at {
                let to = run.offset.min(end);
                push(Piece::File {
                    file_offset: at,
                    len: to - at,
                });
                at = to;
                continue;
            }
            let to = run_end.min(end);
            push(Piece::Stream {
                file_offset: at,
                stream_pos: self.starts[run_index as usize] + (at - run.offset),
                len: to - at,
            });
            at = to;
            index += 1;
        }
    }

    /// The index of the frame holding stream byte `pos`.
    pub fn frame_at(&self, pos: u64) -> Option<usize> {
        let index = self
            .frames
            .partition_point(|frame| frame.stream_pos <= pos)
            .checked_sub(1)?;
        let frame = self.frames[index];
        (pos < frame.stream_pos + frame.raw_len).then_some(index)
    }

    /// Decompress frame `index` into `out`, which must hold its raw length.
    pub fn decompress(&self, index: usize, scratch: &mut Vec<u8>, out: &mut [u8]) -> Result<()> {
        let frame = self
            .frames
            .get(index)
            .context("no such working-set frame")?;
        ensure!(
            out.len() as u64 == frame.raw_len,
            "frame buffer of the wrong length"
        );
        scratch.resize(frame.compressed_len as usize, 0);
        self.file
            .read_exact_at(scratch, frame.file_pos)
            .context("reading a working-set frame")?;
        let written = zstd::bulk::decompress_to_buffer(scratch, out)
            .context("decompressing a working-set frame")?;
        ensure!(
            written as u64 == frame.raw_len,
            "a working-set frame decompressed to {written} bytes, not {}",
            frame.raw_len
        );
        Ok(())
    }
}

/// What one replay's [`PackStream`] did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PackStats {
    /// Frames decompressed.
    pub frames: u64,
    /// Compressed bytes read for them.
    pub compressed_bytes: u64,
    /// Time spent decompressing, summed over the threads that did it.
    pub decompress_ns: u64,
    /// Bytes replay copied out of decompressed frames.
    pub pack_bytes: u64,
    /// A frame failed to decompress, and every run after that read the memory file.
    pub failed: bool,
}

/// One frame's state in a [`PackStream`].
enum Slot {
    NotStarted,
    InProgress,
    Ready(Arc<Vec<u8>>),
    /// Dropped after replay moved past it, or never decompressed because a frame failed.
    Gone,
}

struct StreamState {
    slots: Vec<Slot>,
    /// The highest frame a copy has asked for.
    wanted: usize,
    /// The next frame a read-ahead thread decompresses.
    next: usize,
    stop: bool,
}

/// Where a replay copies one run's bytes from ([`PackStream::bytes_for`]).
pub enum RunBytes {
    /// The memory file: the pack holds none of the run, or cannot provide it.
    File,
    /// One decompressed frame, from this offset in it.
    Frame(Arc<Vec<u8>>, usize),
    /// The caller's scratch buffer, which now holds the run.
    Scratch,
}

/// One replay's view of a pack: read-ahead threads decompress frames in stream order a bounded
/// window ahead of the frames the copies ask for, and drop frames the copies have moved past.
///
/// A copy that needs a frame no thread has started decompresses it itself; one that needs a
/// frame being decompressed waits for it. A frame dropped behind the window, one that failed,
/// and every frame after a failure are read from the memory file instead, so a copy always gets
/// the snapshot's bytes. Replay plans the runs in the pack's stream order (both walk
/// [`OrderedRuns`]), so frames are asked for nearly in order; a run planned again after a
/// balloon REMOVE can ask for an older frame, which by then is read from the file.
pub struct PackStream {
    pack: Arc<Pack>,
    state: Mutex<StreamState>,
    moved: Condvar,
    failed: AtomicBool,
    /// Frames decompressed ahead of the highest one asked for.
    window: usize,
    /// Frames kept behind it.
    keep: usize,
    frames: AtomicU64,
    compressed_bytes: AtomicU64,
    decompress_ns: AtomicU64,
    pack_bytes: AtomicU64,
}

impl PackStream {
    /// A stream over `pack` for a replay copying on `threads` threads, whose read-ahead keeps
    /// `2 * threads` frames ahead (at most 64, 256 MiB) and `threads` behind (at most 32).
    pub fn new(pack: Arc<Pack>, threads: usize) -> Arc<Self> {
        let frames = pack.frames.len();
        let threads = threads.max(1);
        Arc::new(Self {
            pack,
            state: Mutex::new(StreamState {
                slots: (0..frames).map(|_| Slot::NotStarted).collect(),
                wanted: 0,
                next: 0,
                stop: false,
            }),
            moved: Condvar::new(),
            failed: AtomicBool::new(false),
            window: (2 * threads).min(64),
            keep: threads.min(32),
            frames: AtomicU64::new(0),
            compressed_bytes: AtomicU64::new(0),
            decompress_ns: AtomicU64::new(0),
            pack_bytes: AtomicU64::new(0),
        })
    }

    /// Start `count` read-ahead threads. They end when every frame is decompressed, when a
    /// frame fails, or at [`PackStream::stop`]; the caller joins them.
    pub fn spawn_read_ahead(self: &Arc<Self>, count: usize) -> Vec<std::thread::JoinHandle<()>> {
        let mut handles = Vec::with_capacity(count);
        for index in 0..count {
            let stream = Arc::clone(self);
            match std::thread::Builder::new()
                .name(format!("fcvm-pack-{index}"))
                .spawn(move || stream.read_ahead())
            {
                Ok(handle) => handles.push(handle),
                // Fewer threads only means copies decompress more frames themselves.
                Err(error) => {
                    warn!(target: "uffd", %error, "could not start a working-set pack read-ahead thread");
                    break;
                }
            }
        }
        handles
    }

    /// End the read-ahead: each thread finishes the frame it is on and returns.
    pub fn stop(&self) {
        self.state.lock().expect("pack stream").stop = true;
        self.moved.notify_all();
    }

    pub fn stats(&self) -> PackStats {
        PackStats {
            frames: self.frames.load(Ordering::Relaxed),
            compressed_bytes: self.compressed_bytes.load(Ordering::Relaxed),
            decompress_ns: self.decompress_ns.load(Ordering::Relaxed),
            pack_bytes: self.pack_bytes.load(Ordering::Relaxed),
            failed: self.failed.load(Ordering::Relaxed),
        }
    }

    fn read_ahead(&self) {
        let mut scratch = Vec::new();
        loop {
            let index = {
                let mut state = self.state.lock().expect("pack stream");
                loop {
                    if state.stop
                        || self.failed.load(Ordering::Relaxed)
                        || state.next >= state.slots.len()
                    {
                        return;
                    }
                    if state.next > state.wanted + self.window {
                        state = self.moved.wait(state).expect("pack stream");
                        continue;
                    }
                    let index = state.next;
                    state.next += 1;
                    if matches!(state.slots[index], Slot::NotStarted) {
                        state.slots[index] = Slot::InProgress;
                        break index;
                    }
                }
            };
            self.decompress_into_slot(index, &mut scratch);
        }
    }

    /// Decompress frame `index`, which the caller set `InProgress`, and publish the result.
    /// Returns the frame, `None` when it failed; a frame that fell behind the window while it
    /// was decompressed is returned but not kept.
    fn decompress_into_slot(&self, index: usize, scratch: &mut Vec<u8>) -> Option<Arc<Vec<u8>>> {
        /// Publishes a failure if the decompression unwinds, so no waiter waits forever.
        struct Unwind<'a>(&'a PackStream, usize, bool);
        impl Drop for Unwind<'_> {
            fn drop(&mut self) {
                if !self.2 {
                    self.0.failed.store(true, Ordering::Relaxed);
                    self.0.state.lock().expect("pack stream").slots[self.1] = Slot::Gone;
                    self.0.moved.notify_all();
                }
            }
        }
        let mut guard = Unwind(self, index, false);
        let started = std::time::Instant::now();
        let frame = self.pack.frames[index];
        let mut raw = vec![0u8; frame.raw_len as usize];
        let result = self.pack.decompress(index, scratch, &mut raw);
        self.decompress_ns
            .fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
        let slot = match result {
            Ok(()) => {
                self.frames.fetch_add(1, Ordering::Relaxed);
                self.compressed_bytes
                    .fetch_add(frame.compressed_len, Ordering::Relaxed);
                Some(Arc::new(raw))
            }
            Err(error) => {
                if !self.failed.swap(true, Ordering::Relaxed) {
                    warn!(
                        target: "uffd",
                        frame = index,
                        error = %format!("{error:#}"),
                        "a working-set pack frame failed to decompress; the rest of this \
                         replay reads the memory file"
                    );
                }
                None
            }
        };
        let mut state = self.state.lock().expect("pack stream");
        state.slots[index] = match &slot {
            Some(bytes) if index + self.keep >= state.wanted => Slot::Ready(Arc::clone(bytes)),
            _ => Slot::Gone,
        };
        guard.2 = true;
        drop(state);
        self.moved.notify_all();
        slot
    }

    /// Frame `index`, decompressed, or `None` when it has to be read from the memory file.
    fn frame(&self, index: usize, scratch: &mut Vec<u8>) -> Option<Arc<Vec<u8>>> {
        let mut state = self.state.lock().expect("pack stream");
        if index > state.wanted {
            // Drop the frames that fell behind the window, and let the read-ahead move on.
            let from = state.wanted.saturating_sub(self.keep);
            let to = index.saturating_sub(self.keep);
            for slot in &mut state.slots[from..to] {
                if matches!(slot, Slot::Ready(_)) {
                    *slot = Slot::Gone;
                }
            }
            state.wanted = index;
            self.moved.notify_all();
        }
        loop {
            if self.failed.load(Ordering::Relaxed) {
                return None;
            }
            match &state.slots[index] {
                Slot::Ready(bytes) => return Some(Arc::clone(bytes)),
                Slot::Gone => return None,
                Slot::InProgress => state = self.moved.wait(state).expect("pack stream"),
                Slot::NotStarted => {
                    state.slots[index] = Slot::InProgress;
                    drop(state);
                    return self.decompress_into_slot(index, scratch);
                }
            }
        }
    }

    /// Where to copy the memory file's `[offset, offset + len)` from. `file` is the memory
    /// file's mapping, for the bytes the pack does not provide; `scratch` is the caller's, and
    /// holds the run when the answer is [`RunBytes::Scratch`]. `pieces` and `frame_scratch`
    /// are the caller's reusable buffers.
    pub fn bytes_for(
        &self,
        offset: u64,
        len: usize,
        file: &[u8],
        scratch: &mut Vec<u8>,
        pieces: &mut Vec<Piece>,
        frame_scratch: &mut Vec<u8>,
    ) -> RunBytes {
        if self.failed.load(Ordering::Relaxed) {
            return RunBytes::File;
        }
        self.pack.pieces(offset, len as u64, pieces);
        if pieces
            .iter()
            .all(|piece| matches!(piece, Piece::File { .. }))
        {
            return RunBytes::File;
        }
        // The whole run inside one frame: copy straight out of it.
        if let [Piece::Stream {
            stream_pos,
            len: piece_len,
            ..
        }] = pieces[..]
        {
            if let Some(index) = self.pack.frame_at(stream_pos) {
                let frame = self.pack.frames[index];
                if stream_pos + piece_len <= frame.stream_pos + frame.raw_len {
                    return match self.frame(index, frame_scratch) {
                        Some(bytes) => {
                            self.pack_bytes.fetch_add(piece_len, Ordering::Relaxed);
                            RunBytes::Frame(bytes, (stream_pos - frame.stream_pos) as usize)
                        }
                        None => RunBytes::File,
                    };
                }
            }
        }
        // Otherwise put the run together: from frames where the pack holds it, from the file
        // elsewhere and wherever a frame is not available.
        scratch.clear();
        scratch.resize(len, 0);
        let mut at = 0usize;
        let mut from_pack = 0u64;
        for &piece in pieces.iter() {
            let (file_offset, piece_len, mut stream) = match piece {
                Piece::File { file_offset, len } => (file_offset, len, None),
                Piece::Stream {
                    file_offset,
                    stream_pos,
                    len,
                } => (file_offset, len, Some(stream_pos)),
            };
            let mut done = 0u64;
            while done < piece_len {
                let out = &mut scratch[at..];
                let copied = match stream {
                    Some(pos) => match self.pack.frame_at(pos) {
                        Some(index) => {
                            let frame = self.pack.frames[index];
                            let take =
                                (frame.stream_pos + frame.raw_len - pos).min(piece_len - done);
                            match self.frame(index, frame_scratch) {
                                Some(bytes) => {
                                    let from = (pos - frame.stream_pos) as usize;
                                    out[..take as usize]
                                        .copy_from_slice(&bytes[from..from + take as usize]);
                                    from_pack += take;
                                    stream = Some(pos + take);
                                    Some(take)
                                }
                                None => {
                                    stream = Some(pos + take);
                                    copy_from_file(
                                        file,
                                        file_offset + done,
                                        &mut out[..take as usize],
                                    )
                                    .then_some(take)
                                }
                            }
                        }
                        None => None,
                    },
                    None => {
                        let take = piece_len - done;
                        copy_from_file(file, file_offset + done, &mut out[..take as usize])
                            .then_some(take)
                    }
                };
                let Some(copied) = copied else {
                    return RunBytes::File;
                };
                done += copied;
                at += copied as usize;
            }
        }
        self.pack_bytes.fetch_add(from_pack, Ordering::Relaxed);
        RunBytes::Scratch
    }
}

/// Copy `out.len()` bytes of the mapped memory file at `offset` into `out`; `false` when the
/// mapping does not hold them.
fn copy_from_file(file: &[u8], offset: u64, out: &mut [u8]) -> bool {
    let Ok(start) = usize::try_from(offset) else {
        return false;
    };
    match start
        .checked_add(out.len())
        .and_then(|end| file.get(start..end))
    {
        Some(bytes) => {
            out.copy_from_slice(bytes);
            true
        }
        None => false,
    }
}

/// Test only: build the pack of `set` from the memory file at `mem` and install it at
/// [`path_for`], with its temporary recorded in `reclaim_dir`.
#[cfg(test)]
pub(crate) fn build_for_test(
    mem: &Path,
    key: &ImageKey,
    set: &PageSet,
    reclaim_dir: &Path,
    threads: usize,
) -> BuildStats {
    let file = File::open(mem).unwrap();
    let mem_len = file.metadata().unwrap().len();
    let runs: Vec<Run> = runs_of(OrderedRuns::new(set), mem_len).collect();
    let (temp, stats) = build_temp(
        &file,
        mem_len,
        key,
        &set.order_digest(),
        &runs,
        &path_for(mem),
        threads,
        reclaim_dir,
        &|| false,
    )
    .unwrap();
    temp.install(&path_for(mem)).unwrap();
    stats
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::uffd::working_set::GRANULE;

    fn image(dir: &Path, len: u64) -> PathBuf {
        let path = dir.join("memory.bin");
        let bytes: Vec<u8> = (0..len)
            .map(|i| (i / GRANULE % 251) as u8 ^ (i % 7) as u8)
            .collect();
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn key(path: &Path) -> ImageKey {
        ImageKey::of(path, &[7u8; 32]).unwrap()
    }

    fn reclaim_dir(dir: &Path) -> PathBuf {
        crate::reclaim::owner_dir_in(&dir.join("data")).unwrap()
    }

    fn open(path: &Path, key: &ImageKey, set: &PageSet, mem_len: u64) -> Result<Option<Pack>> {
        Pack::open(
            path,
            key,
            &set.order_digest(),
            mem_len,
            runs_of(OrderedRuns::new(set), mem_len),
        )
    }

    /// Every recorded byte comes back out of the pack at the stream position its run maps to,
    /// in first-touch order, across frame boundaries.
    #[test]
    fn a_pack_returns_the_memory_files_bytes_in_first_touch_order() {
        let dir = tempfile::tempdir().unwrap();
        let mem_len = 4096 * GRANULE; // 16 MiB: several frames
        let mem = image(dir.path(), mem_len);
        let mut set = PageSet::empty(mem_len);
        set.insert_range_at(3000 * GRANULE, 1000 * GRANULE, 2);
        set.insert_range_at(10 * GRANULE, 1500 * GRANULE, 700);
        set.insert_range_at(2000 * GRANULE, 3 * GRANULE, 0);
        let key = key(&mem);
        let stats = build_for_test(&mem, &key, &set, &reclaim_dir(dir.path()), 3);
        assert_eq!(stats.raw_bytes, set.bytes());
        assert_eq!(stats.frames, set.bytes().div_ceil(FRAME_RAW as u64));
        let pack = open(&path_for(&mem), &key, &set, mem_len).unwrap().unwrap();
        let mut stream = Vec::new();
        let mut scratch = Vec::new();
        for (index, frame) in pack.frames().iter().enumerate() {
            let mut raw = vec![0u8; frame.raw_len as usize];
            pack.decompress(index, &mut scratch, &mut raw).unwrap();
            stream.extend_from_slice(&raw);
        }
        let bytes = std::fs::read(&mem).unwrap();
        let mut pieces = Vec::new();
        // The earliest-touched run is first in the stream.
        pack.pieces(3000 * GRANULE, GRANULE, &mut pieces);
        assert_eq!(
            pieces,
            [Piece::Stream {
                file_offset: 3000 * GRANULE,
                stream_pos: 0,
                len: GRANULE
            }]
        );
        for (offset, len) in [(3000, 1000), (10, 1500), (2000, 3), (1400, 100), (3999, 1)] {
            let (offset, len) = (offset * GRANULE, len * GRANULE);
            pack.pieces(offset, len, &mut pieces);
            let [Piece::Stream { stream_pos, .. }] = pieces[..] else {
                panic!("{offset:#x}+{len:#x} is one recorded run: {pieces:?}");
            };
            assert!(
                stream[stream_pos as usize..(stream_pos + len) as usize]
                    == bytes[offset as usize..(offset + len) as usize],
                "the pack's bytes for {offset:#x}+{len:#x} differ from the memory file's"
            );
        }
        pack.pieces(5 * GRANULE, GRANULE, &mut pieces);
        assert_eq!(
            pieces,
            [Piece::File {
                file_offset: 5 * GRANULE,
                len: GRANULE
            }],
            "not recorded"
        );
        // A range running past a run is split where the run ends.
        pack.pieces(1500 * GRANULE, 20 * GRANULE, &mut pieces);
        assert_eq!(pieces.len(), 2, "{pieces:?}");
        assert!(
            !dir.path().read_dir().unwrap().any(|e| e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")),
            "the build left its temporary behind"
        );
    }

    /// A pack for another set order, another image, or a different length is not used, and a
    /// frame whose bytes are damaged fails to decompress instead of returning them.
    #[test]
    fn a_pack_for_another_set_or_image_is_not_opened() {
        let dir = tempfile::tempdir().unwrap();
        let mem_len = 256 * GRANULE;
        let mem = image(dir.path(), mem_len);
        let mut set = PageSet::empty(mem_len);
        set.insert_range_at(0, 8 * GRANULE, 3);
        let key = key(&mem);
        let out = path_for(&mem);
        build_for_test(&mem, &key, &set, &reclaim_dir(dir.path()), 1);
        assert!(open(&out, &key, &set, mem_len).unwrap().is_some());
        let mut reordered = set.clone();
        reordered.insert_range_at(0, GRANULE, 0);
        reordered.insert_range_at(100 * GRANULE, GRANULE, 1);
        assert!(open(&out, &key, &reordered, mem_len).unwrap().is_none());
        let other = ImageKey::of(&mem, &[8u8; 32]).unwrap();
        assert!(open(&out, &other, &set, mem_len).unwrap().is_none());
        assert!(open(&out, &key, &set, mem_len + GRANULE).unwrap().is_none());
        let pack = open(&out, &key, &set, mem_len).unwrap().unwrap();
        let frame = pack.frames()[0];
        let f = File::options().write(true).open(&out).unwrap();
        f.write_all_at(&[0xFF; 16], frame.file_pos + 4).unwrap();
        let mut raw = vec![0u8; frame.raw_len as usize];
        assert!(pack.decompress(0, &mut Vec::new(), &mut raw).is_err());
    }

    /// The header's digest only says which set the pack claims to hold. A run table that
    /// disagrees with that set (a damaged entry, a builder bug) would put one run's bytes at
    /// another run's offset, so the table is checked against the set's own runs.
    #[test]
    fn a_pack_whose_run_table_is_not_the_sets_is_not_opened() {
        let dir = tempfile::tempdir().unwrap();
        let mem_len = 256 * GRANULE;
        let mem = image(dir.path(), mem_len);
        let mut set = PageSet::empty(mem_len);
        set.insert_range_at(0, 8 * GRANULE, 3);
        set.insert_range_at(20 * GRANULE, 8 * GRANULE, 1);
        let key = key(&mem);
        let out = path_for(&mem);
        build_for_test(&mem, &key, &set, &reclaim_dir(dir.path()), 1);
        // Move the second run of the table by one granule; the digest still names the set.
        let f = File::options().write(true).open(&out).unwrap();
        f.write_all_at(&(GRANULE).to_le_bytes(), HEADER_LEN as u64 + 16)
            .unwrap();
        assert!(
            open(&out, &key, &set, mem_len).is_err(),
            "a pack whose runs are not the set's was opened"
        );
    }

    /// Replay through a pack gets the memory file's bytes for every range: runs inside one
    /// frame, runs across frames, ranges that are partly unrecorded. A frame that fails to
    /// decompress makes it and every later range read the memory file, and the stream stops
    /// decompressing.
    #[test]
    fn a_pack_stream_gives_the_memory_files_bytes_and_falls_back_after_a_bad_frame() {
        let dir = tempfile::tempdir().unwrap();
        let mem_len = 4096 * GRANULE; // 16 MiB
        let mem = image(dir.path(), mem_len);
        let mut set = PageSet::empty(mem_len);
        // 12 MiB recorded in two tiers: three frames.
        set.insert_range_at(100 * GRANULE, 1024 * GRANULE, 2);
        set.insert_range_at(2000 * GRANULE, 2048 * GRANULE, 9);
        let key = key(&mem);
        build_for_test(&mem, &key, &set, &reclaim_dir(dir.path()), 2);
        let bytes = std::fs::read(&mem).unwrap();
        let ranges: Vec<(u64, usize)> = vec![
            (100 * GRANULE, 16 * GRANULE as usize),  // frame 0
            (1100 * GRANULE, 48 * GRANULE as usize), // runs past the first run's end
            (2000 * GRANULE, 64 * GRANULE as usize), // frame 1
            (3000 * GRANULE, 16 * GRANULE as usize), // across frames 1 and 2
            (3500 * GRANULE, 16 * GRANULE as usize), // frame 2
        ];
        let check = |stream: &PackStream| {
            let (mut scratch, mut pieces, mut frame_scratch) = (Vec::new(), Vec::new(), Vec::new());
            for &(offset, len) in &ranges {
                let want = &bytes[offset as usize..offset as usize + len];
                let got: Vec<u8> = match stream.bytes_for(
                    offset,
                    len,
                    &bytes,
                    &mut scratch,
                    &mut pieces,
                    &mut frame_scratch,
                ) {
                    RunBytes::File => want.to_vec(),
                    RunBytes::Frame(frame, at) => frame[at..at + len].to_vec(),
                    RunBytes::Scratch => scratch.clone(),
                };
                assert!(got == want, "bytes for {offset:#x}+{len:#x} differ");
            }
        };
        let pack = Arc::new(open(&path_for(&mem), &key, &set, mem_len).unwrap().unwrap());
        assert_eq!(pack.frames().len(), 3);
        let stream = PackStream::new(Arc::clone(&pack), 2);
        check(&stream);
        let stats = stream.stats();
        assert!(!stats.failed);
        assert_eq!(stats.frames, 3);
        assert!(
            stats.pack_bytes >= 16 * 4 * GRANULE,
            "the pack provided {} bytes",
            stats.pack_bytes
        );

        // Damage frame 1: the first range still comes from frame 0, the rest from the file.
        let frame = pack.frames()[1];
        File::options()
            .write(true)
            .open(path_for(&mem))
            .unwrap()
            .write_all_at(&[0xA5; 64], frame.file_pos + 8)
            .unwrap();
        let stream = PackStream::new(Arc::clone(&pack), 2);
        check(&stream);
        let stats = stream.stats();
        assert!(stats.failed, "the damaged frame was not noticed");
        assert_eq!(
            stats.frames, 1,
            "frames after the failure were decompressed"
        );
        assert!(stats.pack_bytes > 0, "frame 0 was not used");
    }

    const CHILD_DIR: &str = "FCVM_PACK_TEST_CHILD_DIR";
    const CHILD_READY: &str = "pack-child: building";

    /// The child half of `a_sigkilled_build_leaves_its_temporary_to_the_next_launch`: start a
    /// build, report once its temporary exists, and wait inside it to be killed.
    fn run_child(root: &Path) -> ! {
        let mem_len = 2048 * GRANULE;
        let mem = image(root, mem_len);
        let mut set = PageSet::empty(mem_len);
        set.insert_range_at(0, 1500 * GRANULE, 1);
        let file = File::open(&mem).unwrap();
        let runs: Vec<Run> = runs_of(OrderedRuns::new(&set), mem_len).collect();
        let _ = build_temp(
            &file,
            mem_len,
            &key(&mem),
            &set.order_digest(),
            &runs,
            &path_for(&mem),
            1,
            &reclaim_dir(root),
            &|| {
                println!("{CHILD_READY}");
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(60));
                }
            },
        );
        unreachable!("the build waits to be killed");
    }

    /// A build killed with SIGKILL leaves its temporary, which can be tens of GiB. It was
    /// recorded before it was created, so the next launch's reclaim pass removes it, and
    /// leaves it alone while the builder lives.
    #[tokio::test]
    async fn a_sigkilled_build_leaves_its_temporary_to_the_next_launch() {
        use std::io::BufRead;
        if let Some(root) = std::env::var_os(CHILD_DIR) {
            run_child(Path::new(&root));
        }
        let root = tempfile::tempdir().unwrap();
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "uffd::pack::tests::a_sigkilled_build_leaves_its_temporary_to_the_next_launch",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD_DIR, root.path())
            .stdout(std::process::Stdio::piped());
        // The child dies with this test, so a failed assertion cannot leave it running.
        unsafe {
            use std::os::unix::process::CommandExt;
            command.pre_exec(|| {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();
        let stdout = child.stdout.take().unwrap();
        let ready = std::io::BufReader::new(stdout)
            .lines()
            .map_while(|line| line.ok())
            .any(|line| line.contains(CHILD_READY));
        assert!(ready, "the child exited before its build started");
        let temps = || -> Vec<PathBuf> {
            std::fs::read_dir(root.path())
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| path.to_string_lossy().ends_with(".tmp"))
                .collect()
        };
        assert_eq!(temps().len(), 1, "the build's temporary: {:?}", temps());

        let dir = reclaim_dir(root.path());
        assert_eq!(
            crate::reclaim::reclaim_abandoned_in(&dir).await,
            crate::reclaim::Reclaimed::default(),
            "a pass reclaimed the temporary of a live build"
        );
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(child.id() as i32),
            nix::sys::signal::Signal::SIGKILL,
        )
        .unwrap();
        child.wait().unwrap();
        assert_eq!(
            temps().len(),
            1,
            "the temporary is gone before any pass ran"
        );
        assert_eq!(
            crate::reclaim::reclaim_abandoned_in(&dir).await,
            crate::reclaim::Reclaimed {
                owners: 1,
                resources: 1,
                remaining: 0
            }
        );
        assert_eq!(temps(), Vec::<PathBuf>::new(), "the temporary is left");
    }

    /// Read-ahead decompresses frames before any copy asks for them, a window ahead.
    #[test]
    fn read_ahead_decompresses_frames_before_they_are_asked_for() {
        let dir = tempfile::tempdir().unwrap();
        let mem_len = 4096 * GRANULE;
        let mem = image(dir.path(), mem_len);
        let mut set = PageSet::empty(mem_len);
        set.insert_range_at(0, 3072 * GRANULE, 1);
        let key = key(&mem);
        build_for_test(&mem, &key, &set, &reclaim_dir(dir.path()), 2);
        let pack = Arc::new(open(&path_for(&mem), &key, &set, mem_len).unwrap().unwrap());
        let stream = PackStream::new(pack, 1);
        let handles = stream.spawn_read_ahead(2);
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(
            stream.stats().frames,
            3,
            "the window covers all three frames"
        );
        assert_eq!(stream.stats().pack_bytes, 0, "nothing was copied yet");
    }
}

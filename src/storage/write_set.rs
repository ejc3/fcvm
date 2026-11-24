//! The pages a File-backend clone writes soon after its restore, recorded once per snapshot
//! generation and populated writable by Firecracker before the next clone of it runs.
//!
//! A File-backend restore maps `memory.bin` privately. The guest's first write to a page it has
//! read then takes a stage-2 fault whose copy-on-write runs the MMU notifier's stage-2 unmap and
//! a PTE clear-and-flush: two broadcast TLB invalidations per page while the vCPU waits. Under
//! load that copy-on-write was 66% of `user_mem_abort`'s time, about 6,700 such pages a clone
//! (bench/start-rate/RESTORE.md). Firecracker (`mem_backend.write_set_path`, in builds whose
//! `--version` says `write-set`) instead copies those pages into the clone with
//! `MADV_POPULATE_WRITE` before the VM runs.
//!
//! The set is read from one clone's own page tables, [`RECORD_AFTER`] after its restore
//! completes: a page of the private mapping of the memory image that is present and no longer a
//! file page (`/proc/<pid>/pagemap` bit 61 clear) is one the guest wrote. It is a hint like the
//! UFFD working set: the bytes always come from the memory file, so a set from an atypical clone
//! costs, per page it lacks, the copy-on-write fault the next clone would have taken anyway,
//! and per page it has in excess, one private page the next clone would not have held. Every
//! populated page is private to its clone, so a set larger than 1/[`MAX_SET_FRACTION`] of the
//! image is not recorded: shared clean pages are what let many clones fit.
//!
//! Identity, taken when the clone restores ([`Target::at_restore`]):
//! - The sidecar's name carries the snapshot generation's key, the one the UFFD working-set
//!   sidecar is bound to (`uffd::working_set::ImageKey`: the SHA-256 of `config.json` with the
//!   memory image's length and modification time), so a set recorded for one generation is never
//!   applied to another, including a replaced image of the same length and modification time:
//!   its `config.json` differs. It sits in the snapshot directory, so it goes with the generation.
//! - The mapping is matched by the image's path and inode as the restore found them. A clone of
//!   a generation that is replaced while it runs maps a file that is no longer at that path, so
//!   it matches nothing and records nothing; the key is checked again before the set is
//!   published.
//! - The recorder opens the VMM's `maps` and `pagemap` when its restore completes, before it
//!   waits. The open files hold that process's address space, so a VMM that exits meanwhile reads
//!   as nothing mapped, never as another process that reused its PID.
//!
//! One clone records at a time: the one holding an exclusive `flock` on `<sidecar>.claim`, taken
//! without waiting when its restore completes and held until its recording ends. The kernel
//! releases the lock when the clone's fcvm exits, killed or not, so a clone that does not get to
//! record (cancelled inside the wait, its VMM gone, the maps it could not open) leaves the next
//! clone of the generation to record instead. A `flock` is local to one kernel: on a store shared
//! through fuse-pipe, clones in two guests can both record, and each publishes a whole set by
//! rename. An outcome that would repeat for every clone (a set over the bound, an empty set)
//! marks the generation `<sidecar>.unrecordable`, once, and no clone of it records again.
//! Nothing is published when no mapping matched or the set is empty.

use anyhow::{Context, Result};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// How long after a clone's restore completes (fc-agent's output connection) its writes are
/// read: the restore's own writes and the first health checks.
pub const RECORD_AFTER: std::time::Duration = std::time::Duration::from_secs(2);

const PAGE: u64 = 4096;

/// A recorded set covers at most 1/MAX_SET_FRACTION of the memory image; a clone that wrote
/// more is not recorded.
pub const MAX_SET_FRACTION: u64 = 4;

/// Page-map entries read at a time: 64 KiB.
const PAGEMAP_CHUNK: usize = 8192;

/// The marker in a sidecar's name, which snapshot export leaves out.
pub const SIDECAR_MARK: &str = ".write-set-";

/// The write set of one snapshot generation, as a restore found it.
#[derive(Debug, Clone)]
pub struct Target {
    sidecar: PathBuf,
    memory: PathBuf,
    config: PathBuf,
    canonical: PathBuf,
    inode: u64,
}

impl Target {
    /// The write set of the generation whose memory image is `memory` and whose config is
    /// `config`, with its sidecar in `snapshot_dir`. Taken at restore, while the generation
    /// being restored is the one on disk.
    pub fn at_restore(snapshot_dir: &Path, memory: &Path, config: &Path) -> Result<Self> {
        let canonical = std::fs::canonicalize(memory)
            .with_context(|| format!("resolving {}", memory.display()))?;
        let inode = std::fs::metadata(&canonical)?.ino();
        Ok(Self {
            sidecar: sidecar_path(snapshot_dir, memory, config)?,
            memory: memory.to_path_buf(),
            config: config.to_path_buf(),
            canonical,
            inode,
        })
    }

    /// The sidecar's path.
    pub fn sidecar(&self) -> &Path {
        &self.sidecar
    }

    /// The sidecar, when a set is recorded.
    pub fn recorded(&self) -> Option<&Path> {
        self.sidecar.is_file().then_some(self.sidecar.as_path())
    }

    /// Claims the recording for this clone, whose VMM is `vmm_pid`, and opens that process's
    /// `maps` and `pagemap` now, before the wait. None when a set is recorded, another clone
    /// claimed it, or the claim cannot be made (a directory this clone cannot write).
    pub fn claim(&self, vmm_pid: u32) -> Result<Option<Recording>> {
        use std::os::fd::AsRawFd;
        if self.recorded().is_some() || self.marked_unrecordable() {
            return Ok(None);
        }
        let path = self.with_suffix(".claim");
        let claim = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        // SAFETY: flock on a descriptor this function owns.
        if unsafe { libc::flock(claim.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
                return Ok(None);
            }
            return Err(error).with_context(|| format!("locking {}", path.display()));
        }
        // A clone that took the claim after another clone recorded and released it.
        if self.recorded().is_some() || self.marked_unrecordable() {
            return Ok(None);
        }
        let maps = std::fs::File::open(format!("/proc/{vmm_pid}/maps"))
            .with_context(|| format!("opening the memory map of {vmm_pid}"))?;
        let pagemap = std::fs::File::open(format!("/proc/{vmm_pid}/pagemap"))
            .with_context(|| format!("opening the page map of {vmm_pid}"))?;
        Ok(Some(Recording {
            target: self.clone(),
            _claim: claim,
            maps,
            pagemap,
        }))
    }

    /// Whether an earlier clone found this generation's set over the bound or empty.
    pub fn marked_unrecordable(&self) -> bool {
        self.with_suffix(".unrecordable").exists()
    }

    fn with_suffix(&self, suffix: &str) -> PathBuf {
        let mut path = self.sidecar.clone().into_os_string();
        path.push(suffix);
        PathBuf::from(path)
    }

    /// Marks the generation as one no clone records, saying why, once.
    fn mark_unrecordable(&self, why: &str) {
        let path = self.with_suffix(".unrecordable");
        match std::fs::write(&path, format!("{why}\n")) {
            Ok(()) => tracing::info!(
                sidecar = %self.sidecar.display(),
                why,
                "this snapshot generation gets no write set"
            ),
            Err(error) => tracing::warn!(
                path = %path.display(),
                %error,
                "could not mark the snapshot generation as getting no write set"
            ),
        }
    }
}

/// A claimed recording: the claim's lock, held until the recording ends or is dropped, and the
/// VMM's `maps` and `pagemap`, open.
pub struct Recording {
    target: Target,
    _claim: std::fs::File,
    maps: std::fs::File,
    pagemap: std::fs::File,
}

impl Recording {
    /// Reads the pages the VMM wrote and publishes them as the generation's set. Returns the
    /// number of ranges. Publishes nothing, and says why, when the VMM no longer maps the image
    /// (it exited, or the generation was replaced), when it wrote nothing, when the set is over
    /// the bound, or when the generation on disk is no longer the one restored. An empty set or
    /// one over the bound marks the generation, so no later clone records it again.
    pub fn record(mut self) -> Result<usize> {
        let mut maps = String::new();
        self.maps.read_to_string(&mut maps)?;
        let runs = written_runs(
            &maps,
            &mut self.pagemap,
            &self.target.canonical,
            self.target.inode,
        )?
        .context("the VMM maps no memory image of the restored generation")?;
        if runs.is_empty() {
            let why = "the VMM wrote no page of the memory image";
            self.target.mark_unrecordable(why);
            anyhow::bail!(why);
        }
        let bytes: u64 = runs.iter().map(|(_, length)| length).sum();
        let image = std::fs::metadata(&self.target.canonical)?.len();
        if bytes > image / MAX_SET_FRACTION {
            let why = format!(
                "the clone wrote {bytes} of the image's {image} bytes, more than \
                 1/{MAX_SET_FRACTION}; no write set is recorded"
            );
            self.target.mark_unrecordable(&why);
            anyhow::bail!(why);
        }
        let now = sidecar_path(
            self.target.sidecar.parent().unwrap_or(Path::new(".")),
            &self.target.memory,
            &self.target.config,
        )?;
        anyhow::ensure!(
            now == self.target.sidecar,
            "the snapshot generation changed since the restore; no write set is recorded"
        );
        let mut text = String::new();
        for (offset, length) in &runs {
            text.push_str(&format!("{offset} {length}\n"));
        }
        let mut temporary = self.target.sidecar.clone().into_os_string();
        temporary.push(format!(".{}.tmp", uuid::Uuid::new_v4()));
        let temporary = PathBuf::from(temporary);
        std::fs::write(&temporary, text)
            .with_context(|| format!("writing {}", temporary.display()))?;
        std::fs::rename(&temporary, &self.target.sidecar).with_context(|| {
            let _ = std::fs::remove_file(&temporary);
            format!("installing {}", self.target.sidecar.display())
        })?;
        Ok(runs.len())
    }
}

/// The write-set sidecar, in `snapshot_dir`, of the generation whose memory image is `memory`
/// and whose config is `config`: named for the generation's key.
pub fn sidecar_path(snapshot_dir: &Path, memory: &Path, config: &Path) -> Result<PathBuf> {
    let key = crate::uffd::ImageKey::of_generation(memory, config)?;
    Ok(snapshot_dir.join(format!("memory.bin{SIDECAR_MARK}{key:?}")))
}

/// One line of `/proc/<pid>/maps`: the range, permissions, offset, inode and path (the rest of
/// the line, which may hold spaces).
fn maps_line(line: &str) -> Option<(u64, u64, &str, u64, u64, &str)> {
    let mut rest = line;
    let mut field = || {
        rest = rest.trim_start();
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let (word, tail) = rest.split_at(end);
        rest = tail;
        word
    };
    let (range, perms, offset, _dev, inode) = (field(), field(), field(), field(), field());
    let (start, end) = range.split_once('-')?;
    Some((
        u64::from_str_radix(start, 16).ok()?,
        u64::from_str_radix(end, 16).ok()?,
        perms,
        u64::from_str_radix(offset, 16).ok()?,
        inode.parse().ok()?,
        rest.strip_prefix(char::is_whitespace)
            .map(str::trim_start)
            .unwrap_or(""),
    ))
}

/// The runs, in byte offsets of the file `path` (inode `inode`), of the pages that `maps` maps
/// privately from it and that `pagemap` reports present and no longer file pages: the pages
/// the process wrote. None when no mapping of that file is in `maps`.
fn written_runs(
    maps: &str,
    pagemap: &mut std::fs::File,
    path: &Path,
    inode: u64,
) -> Result<Option<Vec<(u64, u64)>>> {
    let mut runs: Vec<(u64, u64)> = Vec::new();
    let mut matched = false;
    let mut entries = vec![0u8; PAGEMAP_CHUNK * 8];
    for (start, end, perms, file_offset, line_inode, line_path) in
        maps.lines().filter_map(maps_line)
    {
        if line_inode != inode || Path::new(line_path) != path || !perms.ends_with('p') {
            continue;
        }
        matched = true;
        let pages = (end - start) / PAGE;
        let mut done = 0u64;
        while done < pages {
            let count = (pages - done).min(PAGEMAP_CHUNK as u64) as usize;
            let chunk = &mut entries[..count * 8];
            pagemap.seek(SeekFrom::Start((start / PAGE + done) * 8))?;
            pagemap.read_exact(chunk)?;
            for (index, entry) in chunk.chunks_exact(8).enumerate() {
                let entry = u64::from_ne_bytes(entry.try_into().unwrap());
                let present = entry >> 63 & 1 == 1;
                let file_page = entry >> 61 & 1 == 1;
                if !present || file_page {
                    continue;
                }
                let offset = file_offset + (done + index as u64) * PAGE;
                match runs.last_mut() {
                    Some((run_start, run_len)) if *run_start + *run_len == offset => {
                        *run_len += PAGE
                    }
                    _ => runs.push((offset, PAGE)),
                }
            }
            done += count as u64;
        }
    }
    Ok(matched.then_some(runs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;

    /// A snapshot directory (its name holds a space, as a path in `maps` may) with a memory
    /// image of `pages` pages and a config.
    fn generation(pages: usize) -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let snapshot = dir.path().join("snap shot");
        std::fs::create_dir(&snapshot).unwrap();
        let memory = snapshot.join("memory.bin");
        std::fs::write(&memory, vec![0x5au8; pages * PAGE as usize]).unwrap();
        let config = snapshot.join("config.json");
        std::fs::write(&config, r#"{"generation":1}"#).unwrap();
        (dir, snapshot, memory, config)
    }

    /// Maps `pages` pages of `memory` privately from file page `from`, writes the pages
    /// `written`, reads the pages `read`, and returns the mapping.
    fn map(memory: &Path, from: usize, pages: usize, written: &[usize], read: &[usize]) -> *mut u8 {
        let file = std::fs::File::open(memory).unwrap();
        // SAFETY: a fresh private mapping of an open file, unmapped by the caller.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                pages * PAGE as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE,
                file.as_raw_fd(),
                (from as u64 * PAGE) as i64,
            )
        };
        assert_ne!(base, libc::MAP_FAILED);
        let bytes = base.cast::<u8>();
        // SAFETY: every index lies inside the mapping.
        unsafe {
            for page in written {
                *bytes.add(page * PAGE as usize) = 1;
            }
            for page in read {
                std::ptr::read_volatile(bytes.add(page * PAGE as usize));
            }
        }
        bytes
    }

    /// A private mapping of a file in this process, three of whose pages are then written and
    /// two read: the recorded set is the written pages, in file offsets, as runs, and a second
    /// clone records nothing.
    #[test]
    fn the_written_pages_of_a_private_mapping_are_recorded() {
        let (_dir, snapshot, memory, config) = generation(16);
        let target = Target::at_restore(&snapshot, &memory, &config).unwrap();
        // Mapped from file offset 4 pages, as a second guest region would be.
        let bytes = map(&memory, 4, 12, &[2, 3, 7], &[0, 9]);
        let recording = target.claim(std::process::id()).unwrap().unwrap();
        assert_eq!(recording.record().unwrap(), 2);
        let recorded = std::fs::read_to_string(target.sidecar()).unwrap();
        assert_eq!(
            recorded,
            format!("{} {}\n{} {}\n", 6 * PAGE, 2 * PAGE, 11 * PAGE, PAGE)
        );
        assert!(target.claim(std::process::id()).unwrap().is_none());
        // SAFETY: the mapping made above.
        unsafe { libc::munmap(bytes.cast(), 12 * PAGE as usize) };
    }

    /// A clone that wrote more than 1/MAX_SET_FRACTION of the image records nothing, and marks
    /// the generation so no later clone tries again.
    #[test]
    fn a_set_over_the_bound_is_not_recorded() {
        let (_dir, snapshot, memory, config) = generation(8);
        let target = Target::at_restore(&snapshot, &memory, &config).unwrap();
        let bytes = map(&memory, 0, 8, &[0, 1, 2], &[]);
        assert!(target
            .claim(std::process::id())
            .unwrap()
            .unwrap()
            .record()
            .is_err());
        assert!(target.recorded().is_none());
        assert!(target.claim(std::process::id()).unwrap().is_none());
        // SAFETY: the mapping made above.
        unsafe { libc::munmap(bytes.cast(), 8 * PAGE as usize) };
    }

    /// A claim given up without recording (the clone cancelled inside the wait) leaves the next
    /// clone to record: the claim is a lock, not a mark.
    #[test]
    fn a_claim_given_up_lets_the_next_clone_record() {
        let (_dir, snapshot, memory, config) = generation(8);
        let target = Target::at_restore(&snapshot, &memory, &config).unwrap();
        let first = target.claim(std::process::id()).unwrap().unwrap();
        assert!(
            target.claim(std::process::id()).unwrap().is_none(),
            "held by the first"
        );
        drop(first);
        assert!(target.claim(std::process::id()).unwrap().is_some());
    }

    /// A clone killed while it holds the claim, without any cleanup, is taken over: the kernel
    /// releases its lock.
    #[test]
    fn a_claimer_killed_without_cleanup_is_taken_over() {
        use std::io::BufRead;
        use std::os::unix::process::CommandExt;
        let (_dir, snapshot, memory, config) = generation(8);
        let target = Target::at_restore(&snapshot, &memory, &config).unwrap();
        let claim = target.with_suffix(".claim");
        let mut holder = std::process::Command::new("flock")
            .args(["-x", "-o"])
            .arg(&claim)
            .args(["sh", "-c", "echo held; exec sleep 60"])
            .stdout(std::process::Stdio::piped())
            .process_group(0)
            .spawn()
            .unwrap();
        let mut line = String::new();
        std::io::BufReader::new(holder.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert_eq!(line.trim(), "held");
        assert!(target.claim(std::process::id()).unwrap().is_none());
        // SAFETY: the holder's own process group, made above.
        unsafe { libc::killpg(holder.id() as i32, libc::SIGKILL) };
        holder.wait().unwrap();
        assert!(target.claim(std::process::id()).unwrap().is_some());
    }

    /// A VMM that does not map the image (another process's PID, a VMM that mapped another
    /// generation) records nothing: no empty set is published.
    #[test]
    fn a_vmm_that_maps_no_image_records_nothing() {
        let (_dir, snapshot, memory, config) = generation(8);
        let target = Target::at_restore(&snapshot, &memory, &config).unwrap();
        let error = target
            .claim(std::process::id())
            .unwrap()
            .unwrap()
            .record()
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("maps no memory image"),
            "{error:#}"
        );
        assert!(target.recorded().is_none());
        // A transient outcome: the generation is not marked, and the next clone records.
        assert!(!target.marked_unrecordable());
        assert!(target.claim(std::process::id()).unwrap().is_some());
    }

    /// A VMM that exits between its restore and the recording records nothing, though its
    /// `maps` and `pagemap` were opened while it ran: the open files hold its address space,
    /// never another process's.
    #[test]
    fn a_vmm_that_exits_before_the_recording_records_nothing() {
        let (_dir, snapshot, memory, config) = generation(8);
        let target = Target::at_restore(&snapshot, &memory, &config).unwrap();
        // The child inherits the mapping and the pages this process wrote.
        let bytes = map(&memory, 0, 8, &[1], &[]);
        // SAFETY: the child only pauses until it is killed.
        let child = unsafe { libc::fork() };
        assert!(child >= 0);
        if child == 0 {
            loop {
                // SAFETY: pause is async-signal-safe.
                unsafe { libc::pause() };
            }
        }
        let recording = target.claim(child as u32).unwrap().unwrap();
        // SAFETY: the child forked above.
        unsafe { libc::kill(child, libc::SIGKILL) };
        // Recorded while the child is a zombie, its PID still taken and its maps empty.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !std::fs::read_to_string(format!("/proc/{child}/stat")).is_ok_and(|stat| {
            stat.rsplit_once(')')
                .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z'))
        }) {
            assert!(
                std::time::Instant::now() < deadline,
                "the child never exited"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(recording.record().is_err());
        assert!(target.recorded().is_none());
        assert!(!target.marked_unrecordable());
        assert!(target.claim(std::process::id()).unwrap().is_some());
        // SAFETY: the child forked above.
        unsafe { libc::waitpid(child, std::ptr::null_mut(), 0) };
        // SAFETY: the mapping made above.
        unsafe { libc::munmap(bytes.cast(), 8 * PAGE as usize) };
    }

    /// A VMM that maps the image but wrote none of its pages gets no set, and the outcome
    /// would repeat for every clone, so the generation is marked and no clone claims again.
    #[test]
    fn a_vmm_that_wrote_no_page_marks_the_generation() {
        let (_dir, snapshot, memory, config) = generation(8);
        let target = Target::at_restore(&snapshot, &memory, &config).unwrap();
        let bytes = map(&memory, 0, 8, &[], &[0, 3]);
        let error = target
            .claim(std::process::id())
            .unwrap()
            .unwrap()
            .record()
            .unwrap_err();
        assert!(format!("{error:#}").contains("wrote no page"), "{error:#}");
        assert!(target.recorded().is_none());
        assert!(target.marked_unrecordable());
        assert!(target.claim(std::process::id()).unwrap().is_none());
        // SAFETY: the mapping made above.
        unsafe { libc::munmap(bytes.cast(), 8 * PAGE as usize) };
    }

    /// A generation replaced while its clone runs records nothing: the clone maps a file that
    /// is no longer at the restored path, and the new generation does not receive its set.
    #[test]
    fn a_generation_replaced_before_the_recording_records_nothing() {
        let (_dir, snapshot, memory, config) = generation(8);
        let target = Target::at_restore(&snapshot, &memory, &config).unwrap();
        let bytes = map(&memory, 0, 8, &[1], &[]);
        let recording = target.claim(std::process::id()).unwrap().unwrap();
        // A new generation installed by rename, as a snapshot create installs one.
        let fresh = snapshot.join("memory.bin.new");
        std::fs::write(&fresh, vec![0u8; 8 * PAGE as usize]).unwrap();
        std::fs::rename(&fresh, &memory).unwrap();
        std::fs::write(&config, r#"{"generation":2}"#).unwrap();
        assert!(recording.record().is_err());
        assert!(!target.sidecar().exists());
        assert!(!target.marked_unrecordable());
        let next = Target::at_restore(&snapshot, &memory, &config).unwrap();
        assert!(next.recorded().is_none());
        // SAFETY: the mapping made above.
        unsafe { libc::munmap(bytes.cast(), 8 * PAGE as usize) };
    }

    /// A generation replaced after the mapping matched (only `config.json` changes, so the
    /// VMM still maps the same image) records nothing and marks nothing: the next clone of
    /// the generation it restored may still record. Reached through the generation check in
    /// `record`, after `written_runs` found the mapping.
    #[test]
    fn a_generation_changed_after_the_mapping_matched_records_nothing() {
        let (_dir, snapshot, memory, config) = generation(8);
        let target = Target::at_restore(&snapshot, &memory, &config).unwrap();
        let bytes = map(&memory, 0, 8, &[1], &[]);
        let recording = target.claim(std::process::id()).unwrap().unwrap();
        std::fs::write(&config, r#"{"generation":2}"#).unwrap();
        let error = recording.record().unwrap_err().to_string();
        assert!(error.contains("generation changed"), "{error}");
        assert!(!target.marked_unrecordable());
        assert!(!target.sidecar().exists());
        assert!(target.claim(std::process::id()).unwrap().is_some());
        // SAFETY: the mapping made above.
        unsafe { libc::munmap(bytes.cast(), 8 * PAGE as usize) };
    }

    /// A set is named for its snapshot generation: another generation whose memory image has
    /// the same length and the same modification time, to the nanosecond, but another
    /// `config.json` gets another name, so its clones never apply this set; the same
    /// generation always gets the same name.
    #[test]
    fn a_set_never_applies_to_another_generation() {
        let dir = tempfile::tempdir().unwrap();
        let (memory, config) = (
            dir.path().join("memory.bin"),
            dir.path().join("config.json"),
        );
        let stamp = std::time::UNIX_EPOCH + std::time::Duration::new(1_800_000_000, 5);
        let generation = |bytes: &[u8], config_text: &str| {
            std::fs::write(&memory, bytes).unwrap();
            std::fs::File::options()
                .write(true)
                .open(&memory)
                .unwrap()
                .set_modified(stamp)
                .unwrap();
            std::fs::write(&config, config_text).unwrap();
            sidecar_path(dir.path(), &memory, &config).unwrap()
        };
        let first = generation(&[1u8; 4096], r#"{"generation":1}"#);
        assert_eq!(first, generation(&[1u8; 4096], r#"{"generation":1}"#));
        // A replaced image: same length, same mtime, new config.
        assert_ne!(first, generation(&[2u8; 4096], r#"{"generation":2}"#));
    }

    /// The path in a maps line is the rest of the line, spaces included.
    #[test]
    fn a_maps_path_may_hold_spaces() {
        let line = "7f00-7f02 rw-p 00004000 fd:01 257  /mnt/a b/memory.bin";
        let (start, end, perms, offset, inode, path) = maps_line(line).unwrap();
        assert_eq!(
            (start, end, perms, offset, inode),
            (0x7f00, 0x7f02, "rw-p", 0x4000, 257)
        );
        assert_eq!(path, "/mnt/a b/memory.bin");
    }
}

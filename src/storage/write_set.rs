//! The pages a File-backend clone writes soon after its restore, recorded once per memory
//! image and populated writable by Firecracker before the next clone of it runs.
//!
//! A File-backend restore maps `memory.bin` privately. The guest's first write to a page it has
//! read then takes a stage-2 fault whose copy-on-write runs the MMU notifier's stage-2 unmap and
//! a PTE clear-and-flush: two broadcast TLB invalidations per page while the vCPU waits. At 128
//! clones at once that was 71% of a clone's stage-2 fault time (about 6,400 such pages a clone).
//! Firecracker (`mem_backend.write_set_path`) instead copies those pages into the clone with
//! `MADV_POPULATE_WRITE` before the VM runs.
//!
//! The set is read from the first clone's own page tables, [`RECORD_AFTER`] after its restore
//! completes: a page of the private mapping that is present and no longer a file page
//! (`/proc/<pid>/pagemap` bit 61 clear) is one the guest wrote. It is a hint like the UFFD
//! working set: the bytes always come from the memory file, so a set from an atypical clone
//! costs, per page it lacks, the copy-on-write fault the next clone would have taken anyway,
//! and per page it has in excess, one private page the next clone would not have held. Every
//! populated page is private to its clone, so a set larger than [`MAX_SET_FRACTION`] of the image
//! is not recorded: shared clean pages are what let many clones fit.
//!
//! The sidecar's name carries the memory image's identity (device, inode, length, modification
//! time), so a new generation of the snapshot records its own set. Two clones that both find no
//! set record one each: each writes a temporary of a unique name and renames it onto the
//! sidecar, so a reader sees one whole set, the last one renamed.

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
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

/// The write-set sidecar of the memory image at `memory`, named for that image's identity.
pub fn sidecar_path(memory: &Path) -> std::io::Result<PathBuf> {
    let meta = std::fs::metadata(memory)?;
    let mut hasher = Sha256::new();
    for part in [
        meta.dev(),
        meta.ino(),
        meta.len(),
        meta.mtime() as u64,
        meta.mtime_nsec() as u64,
    ] {
        hasher.update(part.to_le_bytes());
    }
    let digest = hasher.finalize();
    let key: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    let mut name = memory
        .file_name()
        .ok_or_else(|| std::io::Error::other("the memory image has no file name"))?
        .to_os_string();
    name.push(format!(".write-set-{key}"));
    Ok(memory.with_file_name(name))
}

/// The recorded write set of the memory image at `memory`, if there is one.
pub fn existing(memory: &Path) -> Option<PathBuf> {
    sidecar_path(memory).ok().filter(|p| p.is_file())
}

/// Records the write set of the memory image at `memory` from the process `vmm_pid`, which maps
/// it privately as guest memory, unless one is recorded already. Returns the number of ranges
/// written, or None when a set existed.
pub fn record(vmm_pid: u32, memory: &Path) -> Result<Option<usize>> {
    let sidecar = sidecar_path(memory).context("naming the write-set sidecar")?;
    if sidecar.is_file() {
        return Ok(None);
    }
    let canonical =
        std::fs::canonicalize(memory).with_context(|| format!("resolving {}", memory.display()))?;
    let inode = std::fs::metadata(&canonical)?.ino();
    let maps = std::fs::read_to_string(format!("/proc/{vmm_pid}/maps"))
        .with_context(|| format!("reading the memory map of {vmm_pid}"))?;
    let mut pagemap = std::fs::File::open(format!("/proc/{vmm_pid}/pagemap"))
        .with_context(|| format!("opening the page map of {vmm_pid}"))?;
    let runs = written_runs(&maps, &mut pagemap, &canonical, inode)?;
    let bytes: u64 = runs.iter().map(|(_, length)| length).sum();
    let image = std::fs::metadata(&canonical)?.len();
    if bytes > image / MAX_SET_FRACTION {
        anyhow::bail!(
            "the clone wrote {bytes} of the image's {image} bytes, more than 1/{MAX_SET_FRACTION}; \
             no write set is recorded"
        );
    }
    let mut text = String::new();
    for (offset, length) in &runs {
        text.push_str(&format!("{offset} {length}\n"));
    }
    // A unique temporary and an atomic rename: two clones that record at once write the same
    // kind of set, and the last rename wins whole.
    let mut temporary = sidecar.clone().into_os_string();
    temporary.push(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let temporary = PathBuf::from(temporary);
    std::fs::write(&temporary, text).with_context(|| format!("writing {}", temporary.display()))?;
    std::fs::rename(&temporary, &sidecar).with_context(|| {
        let _ = std::fs::remove_file(&temporary);
        format!("installing {}", sidecar.display())
    })?;
    Ok(Some(runs.len()))
}

/// The runs, in byte offsets of the file `path` (inode `inode`), of the pages that `maps` maps
/// privately from it and that `pagemap` reports present and no longer file pages: the pages
/// the process wrote.
fn written_runs(
    maps: &str,
    pagemap: &mut std::fs::File,
    path: &Path,
    inode: u64,
) -> Result<Vec<(u64, u64)>> {
    let mut runs: Vec<(u64, u64)> = Vec::new();
    for line in maps.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 6 || fields[4].parse::<u64>().ok() != Some(inode) {
            continue;
        }
        if Path::new(fields[5]) != path || !fields[1].ends_with('p') {
            continue;
        }
        let (start, end) = fields[0]
            .split_once('-')
            .context("a maps line without a range")?;
        let start = u64::from_str_radix(start, 16)?;
        let end = u64::from_str_radix(end, 16)?;
        let file_offset = u64::from_str_radix(fields[2], 16)?;
        let pages = (end - start) / PAGE;
        let mut entries = vec![0u8; (pages * 8) as usize];
        pagemap.seek(SeekFrom::Start(start / PAGE * 8))?;
        pagemap.read_exact(&mut entries)?;
        for (index, entry) in entries.chunks_exact(8).enumerate() {
            let entry = u64::from_ne_bytes(entry.try_into().unwrap());
            let present = entry >> 63 & 1 == 1;
            let file_page = entry >> 61 & 1 == 1;
            if !present || file_page {
                continue;
            }
            let offset = file_offset + index as u64 * PAGE;
            match runs.last_mut() {
                Some((run_start, run_len)) if *run_start + *run_len == offset => *run_len += PAGE,
                _ => runs.push((offset, PAGE)),
            }
        }
    }
    Ok(runs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;

    /// A private mapping of a file in this process, three of whose pages are then written and
    /// two read: the recorded set is the written pages, in file offsets, as runs.
    #[test]
    fn the_written_pages_of_a_private_mapping_are_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let memory = dir.path().join("memory.bin");
        std::fs::write(&memory, vec![0x5au8; 16 * PAGE as usize]).unwrap();
        let file = std::fs::File::open(&memory).unwrap();
        // Mapped from file offset 4 pages, as a second guest region would be.
        let length = 12 * PAGE as usize;
        // SAFETY: a fresh private mapping of an open file, unmapped below.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                length,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE,
                file.as_raw_fd(),
                4 * PAGE as i64,
            )
        };
        assert_ne!(base, libc::MAP_FAILED);
        let bytes = base.cast::<u8>();
        // SAFETY: every index lies inside the mapping.
        unsafe {
            for page in [2usize, 3, 7] {
                *bytes.add(page * PAGE as usize) = 1;
            }
            for page in [0usize, 9] {
                std::ptr::read_volatile(bytes.add(page * PAGE as usize));
            }
        }
        assert_eq!(record(std::process::id(), &memory).unwrap(), Some(2));
        let recorded = std::fs::read_to_string(sidecar_path(&memory).unwrap()).unwrap();
        assert_eq!(
            recorded,
            format!("{} {}\n{} {}\n", 6 * PAGE, 2 * PAGE, 11 * PAGE, PAGE)
        );
        // Recorded once: a second call leaves it.
        assert_eq!(record(std::process::id(), &memory).unwrap(), None);
        // SAFETY: the mapping made above.
        unsafe { libc::munmap(base, length) };
    }

    /// A clone that wrote more than 1/MAX_SET_FRACTION of the image records nothing.
    #[test]
    fn a_set_over_the_bound_is_not_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let memory = dir.path().join("memory.bin");
        std::fs::write(&memory, vec![0u8; 8 * PAGE as usize]).unwrap();
        let file = std::fs::File::open(&memory).unwrap();
        let length = 8 * PAGE as usize;
        // SAFETY: a fresh private mapping of an open file, unmapped below.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                length,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE,
                file.as_raw_fd(),
                0,
            )
        };
        assert_ne!(base, libc::MAP_FAILED);
        // SAFETY: three pages of the mapping, 3/8 of the image.
        unsafe {
            for page in 0..3usize {
                *base.cast::<u8>().add(page * PAGE as usize) = 1;
            }
        }
        assert!(record(std::process::id(), &memory).is_err());
        assert!(existing(&memory).is_none());
        // SAFETY: the mapping made above.
        unsafe { libc::munmap(base, length) };
    }

    /// A new generation of the image (here, rewritten) gets a sidecar of its own name.
    #[test]
    fn the_sidecar_is_named_for_the_image_it_was_recorded_from() {
        let dir = tempfile::tempdir().unwrap();
        let memory = dir.path().join("memory.bin");
        std::fs::write(&memory, [0u8; 4096]).unwrap();
        let first = sidecar_path(&memory).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&memory, [1u8; 8192]).unwrap();
        assert_ne!(first, sidecar_path(&memory).unwrap());
        assert!(existing(&memory).is_none());
    }
}

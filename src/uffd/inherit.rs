//! A diff snapshot's memory file starts with its parent's page cache.
//!
//! A diff snapshot's `memory.bin` is a new inode: a btrfs subvolume snapshot of the parent's
//! directory, a FICLONE of the parent's file, or a copy that the merge writes the diff into.
//! It shares the parent's extents on disk and none of its page cache, so apart from the pages
//! the snapshot writes, a restore from it reads every page from disk. On the File backend
//! that read is a page fault on the vCPU thread: the guest's first touch of a page runs the
//! kernel's synchronous readahead (4 MiB on btrfs) before the guest can go on.
//!
//! Measured on a c5d.metal, potato wake cycles (256 MiB guest, File backend, in-place diff
//! snapshots, 30 cycles per arm, two rounds): from resume to the guest's acknowledgement took
//! 156.5 and 159.9 ms with the new memory file cold and 55.1 and 53.6 ms with it read into the
//! page cache before its restore. In the first 150 ms of a cold restore the vCPU took about
//! 9,600 EPT violations, 41 of them 1 to 2 ms each in `filemap_fault` readahead, and added
//! about 196 MiB of the memory file to the page cache.
//!
//! So the create asks the kernel to read into the new file's page cache the pages the parent's
//! page cache holds (`mincore` on a mapping of the parent, `POSIX_FADV_WILLNEED` on the new
//! file). The parent's resident set is what its restores touched and read ahead, so the new
//! generation starts where the parent was, and costs the page cache no more than the parent
//! did, which publication then removes.
//!
//! The reads run on threads of their own from the moment the new memory file exists until
//! just before the snapshot is published, beside the pause, the save and the resume, and the
//! create does not wait for them: at most [`IN_FLIGHT_BYTES`] of them are outstanding, as in
//! the serve's warm-up, so a running guest's demand faults on the same device never queue
//! behind more than that. A large parent's set that the window does not cover stays as it is
//! today: read on demand. Nothing depends on the reads, and a refused one is counted.
//!
//! `mincore` reports every page of a mapping resident for a file the process neither owns nor
//! may write (`can_do_mincore`), which would read the whole file, so a parent this process
//! cannot write is not inherited from.

use std::fs::File;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use tracing::{info, warn};

use super::warmup::{
    host_page_size, request_read, Pacer, Probe, Request, IN_FLIGHT_BYTES, REQUEST_BYTES,
};

/// How much of the parent one `mincore` call covers. 64 MiB of 4 KiB pages is a 16 KiB vector.
const RESIDENCY_WINDOW: u64 = 64 * 1024 * 1024;

/// Threads that issue the requests, each over every `WORKERS`th window. A `WILLNEED` request
/// returns once its pages are allocated and their reads submitted, which is CPU time in the
/// caller: on a c5d.metal (btrfs, 256 MiB cold) one thread took 130.6 ms to submit the whole
/// file, four 56.3 ms and eight 54.4 ms. The pacer's window is shared among them.
const WORKERS: usize = 4;

/// What one inheritance did, for its log line.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct InheritStats {
    /// Pages of the parent's file its page cache held, over the part of it walked.
    pub(super) parent_resident_pages: u64,
    /// `WILLNEED` requests made on the new file, and their bytes.
    pub(super) requests: u64,
    pub(super) requested_bytes: u64,
    /// Requests the kernel refused.
    pub(super) failures: u64,
    /// The whole parent was walked before the stop.
    pub(super) complete: bool,
}

/// The reads of one inheritance, running until [`Inheritance::finish`] stops them.
pub struct Inheritance {
    snapshot: String,
    started: Instant,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<std::io::Result<InheritStats>>>,
}

/// Start reading into `child`'s page cache the pages of `parent` its page cache holds. `None`
/// when the thread cannot start, which is logged; the snapshot is made as it would be without.
pub fn inherit_page_cache(snapshot: &str, parent: &Path, child: &Path) -> Option<Inheritance> {
    let stop = Arc::new(AtomicBool::new(false));
    let (parent, child, thread_stop) = (parent.to_path_buf(), child.to_path_buf(), stop.clone());
    let spawned = std::thread::Builder::new()
        .name("fcvm-cache-inherit".to_string())
        .spawn(move || inherit_files(&parent, &child, &thread_stop));
    match spawned {
        Ok(thread) => Some(Inheritance {
            snapshot: snapshot.to_string(),
            started: Instant::now(),
            stop,
            thread: Some(thread),
        }),
        Err(error) => {
            warn!(snapshot, error = %error,
                "could not start the thread that reads the parent's cached pages into the new memory file");
            None
        }
    }
}

impl Inheritance {
    /// Stop the reads and log what they did. Waits for the request in progress at most: the
    /// thread reads the stop flag before every request.
    pub async fn finish(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let Some(thread) = self.thread.take() else {
            return;
        };
        let elapsed_ms = self.started.elapsed().as_millis() as u64;
        let joined = tokio::task::spawn_blocking(move || thread.join()).await;
        match joined {
            Ok(Ok(Ok(stats))) => info!(
                snapshot = %self.snapshot,
                parent_resident_mib = (stats.parent_resident_pages * host_page_size()) >> 20,
                requested_mib = stats.requested_bytes >> 20,
                requests = stats.requests,
                failures = stats.failures,
                complete = stats.complete,
                elapsed_ms,
                "read the parent's cached pages into the new memory file"
            ),
            Ok(Ok(Err(error))) => warn!(snapshot = %self.snapshot, error = %error,
                "did not read the parent's cached pages into the new memory file"),
            Ok(Err(_)) | Err(_) => warn!(snapshot = %self.snapshot,
                "the thread reading the parent's cached pages into the new memory file panicked"),
        }
    }
}

impl Drop for Inheritance {
    /// A create that fails stops the reads and does not wait for them.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn inherit_files(parent: &Path, child: &Path, stop: &AtomicBool) -> std::io::Result<InheritStats> {
    if !may_ask_residency(parent) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "{} is neither owned nor writable by this process, so mincore would report \
                 every page of it cached",
                parent.display()
            ),
        ));
    }
    let parent = File::open(parent)?;
    let child = Arc::new(File::open(child)?);
    let len = parent.metadata()?.len().min(child.metadata()?.len());
    if len == 0 {
        return Ok(InheritStats {
            complete: true,
            ..InheritStats::default()
        });
    }
    // SAFETY: the mapping is only passed to mincore, which never reads the pages, so a file
    // another process truncates cannot fault through it.
    let map = unsafe { memmap2::Mmap::map(&parent) }?;
    let page = host_page_size();
    let workers = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(WORKERS) as u64;
    let walk = |worker: u64| {
        let mut probe = Probe::new(child.clone());
        let mut pacer = Pacer::new(IN_FLIGHT_BYTES / workers, |request| probe.resident(request));
        inherit(
            len,
            page,
            (worker, workers),
            stop,
            |offset, vec| mincore(&map, offset, vec),
            |request| {
                if !pacer.wait_for_room(request, stop) {
                    return None;
                }
                let result = request_read(&child, request);
                pacer.issued(request);
                Some(result)
            },
        )
    };
    let walk = &walk;
    let walks: Vec<std::io::Result<InheritStats>> =
        std::thread::scope(|scope| {
            // A worker whose thread cannot start walks its windows on this thread, after its own.
            let others: Vec<_> = (1..workers)
                .map(|worker| {
                    std::thread::Builder::new()
                        .name("fcvm-cache-inherit".to_string())
                        .spawn_scoped(scope, move || walk(worker))
                        .map_err(|_| worker)
                })
                .collect();
            let mut walks = vec![walk(0)];
            walks.extend(others.into_iter().map(|other| match other {
                Ok(thread) => thread.join().unwrap_or_else(|_| {
                    Err(std::io::Error::other("an inheritance worker panicked"))
                }),
                Err(worker) => walk(worker),
            }));
            walks
        });
    let mut total = InheritStats {
        complete: true,
        ..InheritStats::default()
    };
    for stats in walks {
        let stats = stats?;
        total.parent_resident_pages += stats.parent_resident_pages;
        total.requests += stats.requests;
        total.requested_bytes += stats.requested_bytes;
        total.failures += stats.failures;
        total.complete &= stats.complete;
    }
    Ok(total)
}

/// Walk `len` bytes of the parent in [`RESIDENCY_WINDOW`]s, ask `residency` which of its pages
/// are cached, and hand each cached run, cut into [`REQUEST_BYTES`] pieces, to `read`. `read`
/// returns `None` when the stop came while it waited. `stop` is read before every request.
/// `(worker, workers)` picks this walk's windows: every `workers`th one from the `worker`th.
fn inherit(
    len: u64,
    page: u64,
    (worker, workers): (u64, u64),
    stop: &AtomicBool,
    mut residency: impl FnMut(u64, &mut Vec<u8>) -> std::io::Result<()>,
    mut read: impl FnMut(Request) -> Option<std::io::Result<()>>,
) -> std::io::Result<InheritStats> {
    let mut stats = InheritStats::default();
    let mut vec = Vec::new();
    let mut offset = worker * RESIDENCY_WINDOW;
    while offset < len {
        let window = (len - offset).min(RESIDENCY_WINDOW);
        vec.clear();
        vec.resize(window.div_ceil(page) as usize, 0);
        residency(offset, &mut vec)?;
        for (start, end) in resident_runs(&vec, offset, page, len) {
            stats.parent_resident_pages += (end - start).div_ceil(page);
            let mut at = start;
            while at < end {
                if stop.load(Ordering::Relaxed) {
                    return Ok(stats);
                }
                let request = Request {
                    offset: at,
                    len: (end - at).min(REQUEST_BYTES),
                };
                match read(request) {
                    None => return Ok(stats),
                    Some(Ok(())) => {}
                    Some(Err(error)) => {
                        if stats.failures == 0 {
                            warn!(offset = at, error = %error,
                                "the kernel refused to read part of the new memory file ahead");
                        }
                        stats.failures += 1;
                    }
                }
                stats.requests += 1;
                stats.requested_bytes += request.len;
                at += request.len;
            }
        }
        offset += workers * RESIDENCY_WINDOW;
    }
    stats.complete = true;
    Ok(stats)
}

/// The byte ranges of the pages `vec` marks cached, for the window at `offset`, clipped to `len`.
fn resident_runs(vec: &[u8], offset: u64, page: u64, len: u64) -> Vec<(u64, u64)> {
    let mut runs = Vec::new();
    let mut run: Option<u64> = None;
    for (index, byte) in vec.iter().enumerate() {
        let at = offset + index as u64 * page;
        match (byte & 1 == 1, run) {
            (true, None) => run = Some(at),
            (false, Some(start)) => {
                runs.push((start, at.min(len)));
                run = None;
            }
            _ => {}
        }
    }
    if let Some(start) = run {
        runs.push((start, (offset + vec.len() as u64 * page).min(len)));
    }
    runs
}

/// `mincore` over `vec.len()` pages of `map` from `offset`.
fn mincore(map: &memmap2::Mmap, offset: u64, vec: &mut [u8]) -> std::io::Result<()> {
    let start = usize::try_from(offset).map_err(|_| std::io::ErrorKind::InvalidInput)?;
    let len = (vec.len() * host_page_size() as usize).min(map.len().saturating_sub(start));
    if len == 0 {
        return Ok(());
    }
    // SAFETY: the range lies inside `map`, which stays mapped for the call, and `vec` has a
    // byte for each of its pages.
    let rc = unsafe {
        libc::mincore(
            map.as_ptr().add(start) as *mut libc::c_void,
            len,
            vec.as_mut_ptr().cast(),
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Whether `mincore` answers for `path`'s page cache (`can_do_mincore`): the process owns the
/// file, may override its owner (root), or may write it, checked with the effective ids.
fn may_ask_residency(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let euid = nix::unistd::geteuid();
    let owned = std::fs::metadata(path).is_ok_and(|meta| meta.uid() == euid.as_raw());
    euid.is_root()
        || owned
        || nix::unistd::faccessat(
            nix::fcntl::AT_FDCWD,
            path,
            nix::unistd::AccessFlags::W_OK,
            nix::fcntl::AtFlags::AT_EACCESS,
        )
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: u64 = 4096;

    #[test]
    fn resident_runs_are_the_cached_pages_clipped_to_the_file() {
        let vec = [1, 1, 0, 1, 0, 0, 1, 1];
        assert_eq!(
            resident_runs(&vec, 0, PAGE, 8 * PAGE),
            vec![(0, 2 * PAGE), (3 * PAGE, 4 * PAGE), (6 * PAGE, 8 * PAGE)]
        );
        // The window starts at its offset, and a last page past the file's end is clipped.
        assert_eq!(
            resident_runs(&[1, 1], 64 * PAGE, PAGE, 65 * PAGE + 100),
            vec![(64 * PAGE, 65 * PAGE + 100)]
        );
        // Only the low bit means cached.
        assert_eq!(resident_runs(&[2, 0], 0, PAGE, 2 * PAGE), vec![]);
    }

    #[test]
    fn inherit_requests_every_cached_run_in_pieces_and_stops_on_the_flag() {
        let len = RESIDENCY_WINDOW + 3 * REQUEST_BYTES;
        let pages_per_request = REQUEST_BYTES / PAGE;
        // The second window caches its first 2.5 requests' worth; the first window nothing.
        let residency = |offset: u64, vec: &mut Vec<u8>| {
            if offset == RESIDENCY_WINDOW {
                let cached = (2 * pages_per_request + pages_per_request / 2) as usize;
                vec[..cached].fill(1);
            }
            Ok(())
        };
        let stop = AtomicBool::new(false);
        let mut asked = Vec::new();
        let stats = inherit(len, PAGE, (0, 1), &stop, residency, |request| {
            asked.push(request);
            Some(Ok(()))
        })
        .unwrap();
        assert_eq!(
            asked,
            vec![
                Request {
                    offset: RESIDENCY_WINDOW,
                    len: REQUEST_BYTES
                },
                Request {
                    offset: RESIDENCY_WINDOW + REQUEST_BYTES,
                    len: REQUEST_BYTES
                },
                Request {
                    offset: RESIDENCY_WINDOW + 2 * REQUEST_BYTES,
                    len: REQUEST_BYTES / 2
                },
            ]
        );
        assert!(stats.complete);
        assert_eq!(stats.requested_bytes, 5 * REQUEST_BYTES / 2);
        assert_eq!(stats.parent_resident_pages, 5 * pages_per_request / 2);

        // Two workers take alternate windows and between them ask for the same requests.
        let mut split = Vec::new();
        for worker in 0..2 {
            inherit(len, PAGE, (worker, 2), &stop, residency, |request| {
                split.push(request);
                Some(Ok(()))
            })
            .unwrap();
        }
        assert_eq!(split, asked);

        stop.store(true, Ordering::Relaxed);
        let mut asked = 0;
        let stats = inherit(len, PAGE, (0, 1), &stop, residency, |_| {
            asked += 1;
            Some(Ok(()))
        })
        .unwrap();
        assert_eq!((asked, stats.complete), (0, false));
    }

    /// End to end on a real file: the new file ends up holding in its page cache the pages
    /// the parent's held, and no others. A tmpfs cannot drop or read ahead its pages, so the
    /// files are made beside the test binary, on the build's filesystem.
    #[test]
    fn the_new_file_caches_the_pages_the_parent_cached() {
        use std::io::Write;
        use std::os::unix::fs::FileExt;
        let exe = std::env::current_exe().unwrap();
        let dir = tempfile::tempdir_in(exe.parent().unwrap()).unwrap();
        let page = host_page_size();
        let pages = 64u64;
        let (parent_path, child_path) = (dir.path().join("parent"), dir.path().join("child"));
        let bytes: Vec<u8> = (0..pages * page).map(|i| (i / page) as u8 | 1).collect();
        for path in [&parent_path, &child_path] {
            let mut file = File::create(path).unwrap();
            file.write_all(&bytes).unwrap();
            file.sync_all().unwrap();
            nix::fcntl::posix_fadvise(
                &file,
                0,
                0,
                nix::fcntl::PosixFadviseAdvice::POSIX_FADV_DONTNEED,
            )
            .unwrap();
        }
        let resident = |path: &Path| -> Vec<bool> {
            let file = File::open(path).unwrap();
            let map = unsafe { memmap2::Mmap::map(&file) }.unwrap();
            let mut vec = vec![0u8; pages as usize];
            mincore(&map, 0, &mut vec).unwrap();
            vec.iter().map(|b| b & 1 == 1).collect()
        };
        if resident(&child_path).iter().any(|&r| r) {
            // The filesystem keeps pages DONTNEED cannot drop; nothing here can be measured.
            panic!(
                "{} kept its pages cached after POSIX_FADV_DONTNEED",
                child_path.display()
            );
        }
        // The parent caches pages 3..8 and 40, read with readahead off so nothing else comes in.
        let parent = File::open(&parent_path).unwrap();
        nix::fcntl::posix_fadvise(
            &parent,
            0,
            0,
            nix::fcntl::PosixFadviseAdvice::POSIX_FADV_RANDOM,
        )
        .unwrap();
        let mut buf = vec![0u8; page as usize];
        for index in (3..8).chain([40]) {
            parent.read_exact_at(&mut buf, index * page).unwrap();
        }
        let before = resident(&parent_path);
        let stats = inherit_files(&parent_path, &child_path, &AtomicBool::new(false)).unwrap();
        assert!(stats.complete);
        // The reads are queued, not finished, when the call returns.
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while resident(&child_path) != before && Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(resident(&child_path), before);
        assert_eq!(
            stats.parent_resident_pages,
            before.iter().filter(|&&r| r).count() as u64
        );
    }
}

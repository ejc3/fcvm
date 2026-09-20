//! Reading a snapshot's recorded working set into the page cache.
//!
//! In copy mode the server resolves every fault, replayed or demanded, by copying out of its
//! mapping of the memory image. With the image in the page cache that copy runs at memory
//! speed. With a cold cache each source page is a synchronous major fault inside the server,
//! on the replay path, ahead of the guest: issue #955 measured one 36 GiB replay at 108.6 s
//! with the image evicted and 24.7 s with it cached. The kernel's readahead around each
//! fault does not help, because a recorded set is scattered (3.7M pages in 1.6M runs after
//! one clone of a 128 GiB guest), so most pages next to a fault are pages no clone touched.
//!
//! The server knows the recorded set, so one background thread asks the kernel for it with
//! `posix_fadvise(POSIX_FADV_WILLNEED)`. A warm-up starts when the serve starts, and again
//! when a clone is admitted and no warm-up is running. The second case is the long-lived
//! serve of #955: on a host where the guest is more than half of RAM the kernel evicts the
//! image as clones grow, so a later restore can find a colder cache than the first one did.
//!
//! # What is requested, and what the kernel reads
//!
//! Requests follow the recorded runs in ascending file offset and are never merged across a
//! gap, so the warm-up asks for exactly the recorded runs, widened to whole host pages. What
//! the kernel reads for a request is up to the filesystem. Measured on btrfs mounted
//! `compress-force=zstd`: a request for one 4 KiB page inside a compressed extent left 27
//! pages resident, the 128 KiB extent it sits in, and exactly 1 when the data was
//! incompressible and stored as plain extents. A read fault on the same page, with the
//! kernel's read-around switched off, left the same 27 and 1 resident. Replay's own reads
//! through the mapping bring in the same extents, so the warm-up does not add to them.
//!
//! # Order, and the race with a clone
//!
//! Replay plans every recorded run in ascending file offset
//! ([`plan`](super::prefetch::plan)), which is the order the warm-up walks, and a request
//! returns when its read is queued, not when it completes. A clone that reaches a page whose
//! read is already queued waits for that read instead of issuing its own. Whether a clone
//! that connects during a warm-up finds its pages cached is a race this module does not
//! decide. One request measured 6 to 7 us on a cold uncompressed extent, 21 us on a cold
//! compressed one, and 0.1 to 0.2 us when the page was already cached, so walking a fully
//! cached set of 3M runs costs the thread under a second. Another measurement put a cached
//! request at 0.7 us, which makes it about two seconds.
//!
//! # Nothing waits for it
//!
//! The thread is detached and nothing joins it. The serve's ready record, clone admission,
//! fault service and the shutdown sequence do not depend on it, and `POSIX_FADV_WILLNEED`
//! never touches the mapping, so it cannot fault on a truncated image. Cancelling or
//! dropping the server stops it before its next request. A request stuck in a hung
//! filesystem cannot be interrupted, and a thread in that state delays the kernel reaping
//! the process after it exits. The fault handlers have the same exposure, because they read
//! the same file through the mapping.
//!
//! # A hint
//!
//! A refused request is counted, the first one is logged, and the warm-up carries on. Clones
//! restore correctly from a cold cache, only slower.

use std::fs::File;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use nix::fcntl::{posix_fadvise, PosixFadviseAdvice};
use tracing::{info, warn};

use super::working_set::{PageSet, Run, GRANULE};

/// Largest single `WILLNEED` request, in bytes.
///
/// A recorded run cannot always be one call. The kernel truncates a readahead request to
/// `max(bdi->io_pages, ra_pages)` pages and still returns 0 (`force_page_cache_ra` in
/// `mm/readahead.c`), so the tail of a long run would stay cold with nothing reported.
/// Measured on btrfs with a 4 MiB readahead window: one call over a 64 MiB range left 4 MiB
/// resident, and the same range in calls of this size left all of it resident. 128 KiB is
/// the kernel default for both limits (`VM_READAHEAD_PAGES`), so a request this size is only
/// truncated on a host that lowered them, and it is a whole number of 4, 16 or 64 KiB pages.
const REQUEST_BYTES: u64 = 128 * 1024;

/// One `WILLNEED` request: `len` bytes of the memory image at `offset`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Request {
    offset: u64,
    len: u64,
}

/// The requests that cover one recorded run, in ascending offset.
struct Requests {
    at: u64,
    end: u64,
}

impl Iterator for Requests {
    type Item = Request;

    fn next(&mut self) -> Option<Request> {
        if self.at >= self.end {
            return None;
        }
        let request = Request {
            offset: self.at,
            len: (self.end - self.at).min(REQUEST_BYTES),
        };
        self.at += request.len;
        Some(request)
    }
}

/// Plan the requests for one recorded run.
///
/// The run is widened to whole host pages, because the kernel caches whole pages and a
/// request that ends inside one still reads it. It is clipped to the image, and it starts no
/// earlier than `floor`, the end of the previous run's requests, so two runs that share a
/// host page larger than [`GRANULE`] do not request it twice. What is left is cut into
/// pieces of at most [`REQUEST_BYTES`]. The pieces stay page-aligned, so a request never
/// spans more pages than the kernel's smallest window holds.
fn requests_for(run: Run, floor: u64, page: u64, mem_len: u64) -> Requests {
    let run_end = run.offset.saturating_add(run.len).min(mem_len);
    let start = (run.offset - run.offset % page).max(floor);
    let end = run_end
        .checked_next_multiple_of(page)
        .unwrap_or(run_end)
        .min(mem_len);
    Requests { at: start, end }
}

/// What a warm-up did, for its one log line.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct WarmStats {
    /// Recorded runs that needed at least one request.
    runs: u64,
    /// `WILLNEED` calls made. More than `runs` when long runs were split.
    requests: u64,
    /// Bytes the kernel accepted a request for.
    bytes: u64,
    /// Requests the kernel refused.
    failures: u64,
    /// The serve shut down before the last recorded run was requested.
    stopped_early: bool,
}

/// Issue the requests for `recorded` through `advise`, in ascending file offset.
///
/// `stop` is checked before every request, so a stopped warm-up ends within one call.
fn warm(
    recorded: &PageSet,
    mem_len: u64,
    page: u64,
    stop: &AtomicBool,
    mut advise: impl FnMut(Request) -> std::io::Result<()>,
) -> WarmStats {
    let mut stats = WarmStats::default();
    let mut floor = 0u64;
    'runs: for run in recorded.runs() {
        let mut counted = false;
        for request in requests_for(run, floor, page, mem_len) {
            if stop.load(Ordering::Relaxed) {
                stats.stopped_early = true;
                break 'runs;
            }
            if !counted {
                stats.runs += 1;
                counted = true;
            }
            stats.requests += 1;
            match advise(request) {
                Ok(()) => stats.bytes += request.len,
                Err(error) => {
                    if stats.failures == 0 {
                        warn!(
                            target: "uffd",
                            offset = request.offset,
                            len = request.len,
                            error = %error,
                            "page cache warm-up request was refused; clones still restore \
                             correctly, and further refusals are only counted"
                        );
                    }
                    stats.failures += 1;
                }
            }
            floor = request.offset + request.len;
        }
    }
    stats
}

/// The host's page size, or [`GRANULE`] if `sysconf` has no usable answer.
fn host_page_size() -> u64 {
    // SAFETY: sysconf(3) has no preconditions.
    let raw = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    u64::try_from(raw)
        .ok()
        .filter(|size| size.is_power_of_two())
        .unwrap_or(GRANULE)
}

/// Ask the kernel to read one request's bytes of `image` into the page cache.
///
/// Returns when the read is queued. `madvise(MADV_WILLNEED)` on the server's mapping reaches
/// the same kernel code (`vfs_fadvise`), but the descriptor needs no page-aligned address
/// and no reference to the mapping, so the thread that calls this only has to own the file.
fn request_read(image: &File, request: Request) -> std::io::Result<()> {
    let (Ok(offset), Ok(len)) = (
        libc::off_t::try_from(request.offset),
        libc::off_t::try_from(request.len),
    ) else {
        return Err(std::io::ErrorKind::InvalidInput.into());
    };
    posix_fadvise(image, offset, len, PosixFadviseAdvice::POSIX_FADV_WILLNEED)
        .map_err(std::io::Error::from)
}

/// What one serve's warm-ups share: at most one runs at a time, and none starts once the
/// serve is stopping.
#[derive(Default)]
struct Shared {
    running: AtomicBool,
    stop: AtomicBool,
}

/// The claim on the one warm-up slot. Dropping it frees the slot, which happens when the
/// thread ends, however it ends, or when the thread could not be spawned at all.
struct Claim(Arc<Shared>);

impl Drop for Claim {
    fn drop(&mut self) {
        self.0.running.store(false, Ordering::Release);
    }
}

/// A copy-mode serve's page cache warm-ups.
///
/// Holds the memory image open for the life of the server, because a warm-up can start at
/// any clone admission. Stopping or dropping it ends a running warm-up before its next
/// request and keeps any further one from starting. Nothing ever joins a warm-up thread.
pub(crate) struct Warmer {
    snapshot: String,
    image: Arc<File>,
    mem_len: u64,
    shared: Arc<Shared>,
}

impl Warmer {
    pub(crate) fn new(snapshot_id: &str, image: File, mem_len: u64) -> Self {
        Self {
            snapshot: snapshot_id.to_string(),
            image: Arc::new(image),
            mem_len,
            shared: Arc::new(Shared::default()),
        }
    }

    /// Start a warm-up of `recorded()` on a detached thread, unless one is already running
    /// or the serve is stopping. Returns whether a thread was started.
    ///
    /// The caller pays for one compare-and-swap and one thread spawn. `recorded` runs on the
    /// new thread, so copying a large guest's bitmap never delays the caller, which is the
    /// server's accept loop when the trigger is a clone admission.
    pub(crate) fn warm_if_idle(
        &self,
        trigger: &'static str,
        recorded: impl FnOnce() -> PageSet + Send + 'static,
    ) -> bool {
        let image = Arc::clone(&self.image);
        self.warm_if_idle_with(trigger, recorded, move |request| {
            request_read(&image, request)
        })
    }

    /// [`Warmer::warm_if_idle`] with the kernel call passed in, so tests can park it.
    fn warm_if_idle_with(
        &self,
        trigger: &'static str,
        recorded: impl FnOnce() -> PageSet + Send + 'static,
        advise: impl FnMut(Request) -> std::io::Result<()> + Send + 'static,
    ) -> bool {
        if self.shared.stop.load(Ordering::Relaxed) {
            return false;
        }
        if self
            .shared
            .running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        let claim = Claim(Arc::clone(&self.shared));
        let shared = Arc::clone(&self.shared);
        let snapshot = self.snapshot.clone();
        let mem_len = self.mem_len;
        let worker = std::thread::Builder::new()
            .name("fcvm-ws-warm".to_string())
            .spawn(move || {
                let _claim = claim;
                let started = Instant::now();
                let recorded = recorded();
                if recorded.is_empty() {
                    // The first clone of a fresh snapshot has recorded nothing yet.
                    return;
                }
                let stats = warm(&recorded, mem_len, host_page_size(), &shared.stop, advise);
                info!(
                    target: "uffd",
                    snapshot = %snapshot,
                    trigger,
                    runs = stats.runs,
                    pages = stats.bytes / GRANULE,
                    mib = stats.bytes / (1024 * 1024),
                    requests = stats.requests,
                    failures = stats.failures,
                    elapsed_ms = started.elapsed().as_millis(),
                    stopped_early = stats.stopped_early,
                    "asked the kernel to read the recorded working set into the page cache"
                );
            });
        match worker {
            // Detached on purpose, like the working-set writer: nothing joins this thread,
            // so a request stuck in the filesystem cannot hold up clone admission, fault
            // service or the shutdown sequence.
            Ok(worker) => {
                drop(worker);
                true
            }
            // The closure, and the claim inside it, were dropped with the failed spawn.
            Err(error) => {
                warn!(
                    target: "uffd",
                    snapshot = %self.snapshot,
                    trigger,
                    error = %error,
                    "could not start the page cache warm-up; the restore reads the recorded \
                     working set from disk on demand"
                );
                false
            }
        }
    }

    /// End a running warm-up before its next request and start no more. Returns immediately.
    pub(crate) fn stop(&self) {
        self.shared.stop.store(true, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(crate) fn is_running(&self) -> bool {
        self.shared.running.load(Ordering::Acquire)
    }
}

impl Drop for Warmer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Page cache test support, shared with the server's tests.
#[cfg(test)]
pub(crate) mod testing {
    use super::host_page_size;
    use memmap2::{Mmap, MmapOptions};
    use nix::fcntl::{posix_fadvise, PosixFadviseAdvice};
    use std::fs::File;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    pub(crate) const MIB: u64 = 1024 * 1024;

    /// How long the kernel gets to finish the reads a warm-up requested. They are
    /// asynchronous, so the tests poll. Measured at about 6 ms for 16 MiB on an idle
    /// NVMe-backed btrfs; the bound is for a loaded CI host.
    pub(crate) const RESIDENT_WITHIN: Duration = Duration::from_secs(30);

    /// How long an unrecorded range is watched after the recorded ones arrived, in case a
    /// read that should not have been requested lands late.
    pub(crate) const SETTLE: Duration = Duration::from_millis(250);

    /// How long eviction gets. One `POSIX_FADV_DONTNEED` skips pages that are dirty or under
    /// writeback and starts that writeback itself, so a second call a moment later can drop
    /// what the first one could not.
    const COLD_WITHIN: Duration = Duration::from_secs(2);

    /// Size of the file [`probe_eviction`] writes. A whole number of 4, 16 and 64 KiB pages.
    const PROBE_BYTES: u64 = 256 * 1024;

    /// Fill `file` with `len` bytes and flush them, so every page is clean and evictable.
    ///
    /// The bytes are incompressible, so a compressing filesystem stores plain extents and a
    /// read brings in only the pages that were asked for.
    pub(crate) fn write_incompressible(file: &mut File, len: u64) -> std::io::Result<()> {
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut block = vec![0u8; MIB as usize];
        let mut written = 0u64;
        while written < len {
            for word in block.chunks_exact_mut(8) {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                word.copy_from_slice(&state.to_le_bytes());
            }
            let n = (len - written).min(MIB) as usize;
            file.write_all(&block[..n])?;
            written += n as u64;
        }
        file.sync_all()
    }

    /// Ask the kernel to drop the file's clean pages from the page cache.
    pub(crate) fn evict(file: &File) {
        posix_fadvise(file, 0, 0, PosixFadviseAdvice::POSIX_FADV_DONTNEED).unwrap();
    }

    /// How many host pages of `[offset, offset + len)` are in the page cache. `mincore`
    /// reads the cache without touching the pages, so asking does not change the answer.
    pub(crate) fn resident_pages(mmap: &Mmap, offset: u64, len: u64) -> u64 {
        let page = host_page_size();
        assert_eq!(offset % page, 0, "mincore needs a page-aligned start");
        assert!(offset + len <= mmap.len() as u64);
        let mut vec = vec![0u8; len.div_ceil(page) as usize];
        // SAFETY: the range lies inside `mmap`, and `vec` holds one byte per page of it.
        let rc = unsafe {
            libc::mincore(
                mmap.as_ptr().add(offset as usize) as *mut libc::c_void,
                len as usize,
                vec.as_mut_ptr(),
            )
        };
        assert_eq!(rc, 0, "mincore: {}", std::io::Error::last_os_error());
        vec.iter().filter(|byte| **byte & 1 != 0).count() as u64
    }

    /// Call `evict` until none of `ranges` is resident or [`COLD_WITHIN`] runs out. Returns
    /// how many pages were still resident at the end.
    fn evict_until_cold(
        file: &File,
        mmap: &Mmap,
        ranges: &[(u64, u64)],
        evict: &dyn Fn(&File),
    ) -> u64 {
        let deadline = Instant::now() + COLD_WITHIN;
        loop {
            evict(file);
            let left: u64 = ranges
                .iter()
                .map(|&(offset, len)| resident_pages(mmap, offset, len))
                .sum();
            if left == 0 || Instant::now() >= deadline {
                return left;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Whether a test can make a file in `dir` cold, and if not, why not.
    ///
    /// These tests prove "cold before, warm after", so they need a directory where a written
    /// file's pages can be evicted. The filesystem type does not say. tmpfs never evicts, and
    /// the container CI lane's `/tmp` is an overlay that kept every page resident after
    /// `fsync` and `POSIX_FADV_DONTNEED`. So this probes the behaviour itself, with the same
    /// calls the tests use: write, flush, evict through `evict`, and count what `mincore`
    /// still reports.
    pub(crate) fn probe_eviction(dir: &Path, evict: impl Fn(&File)) -> Result<(), String> {
        let mut file =
            tempfile::tempfile_in(dir).map_err(|error| format!("cannot create a file: {error}"))?;
        write_incompressible(&mut file, PROBE_BYTES)
            .map_err(|error| format!("cannot write a file: {error}"))?;
        // SAFETY: the file is private to this probe and is not written again.
        let mmap = unsafe { MmapOptions::new().len(PROBE_BYTES as usize).map(&file) }
            .map_err(|error| format!("cannot map a file: {error}"))?;
        match evict_until_cold(&file, &mmap, &[(0, PROBE_BYTES)], &evict) {
            0 => Ok(()),
            left => Err(format!(
                "{left} of {} pages still resident after fsync and POSIX_FADV_DONTNEED",
                PROBE_BYTES / host_page_size()
            )),
        }
    }

    /// The first candidate that passes `probe`, or every candidate with why it was refused.
    pub(crate) fn first_usable(
        candidates: &[PathBuf],
        probe: impl Fn(&Path) -> Result<(), String>,
    ) -> Result<PathBuf, String> {
        let mut refused = Vec::new();
        for dir in candidates {
            match probe(dir) {
                Ok(()) => return Ok(dir.clone()),
                Err(why) => refused.push(format!("{} ({why})", dir.display())),
            }
        }
        Err(refused.join("; "))
    }

    /// A directory where a test can evict a file's pages: the first of the temp directory,
    /// `/var/tmp` and the build's target directory that passes [`probe_eviction`].
    ///
    /// The test fails, rather than skips, if none does, because without eviction it could
    /// not tell a warm-up from a cache that was never cold.
    pub(crate) fn disk_dir() -> PathBuf {
        let candidates = [
            std::env::temp_dir(),
            PathBuf::from("/var/tmp"),
            PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/target")),
        ];
        first_usable(&candidates, |dir| probe_eviction(dir, evict)).unwrap_or_else(|refused| {
            panic!("no directory lets a test evict a file from the page cache: {refused}")
        })
    }

    /// An unnamed file of `len` bytes on a disk. Unnamed, so nothing else on the host can
    /// open it and read pages in behind the test's back.
    pub(crate) fn image_on_disk(len: u64) -> File {
        let mut file = tempfile::tempfile_in(disk_dir()).unwrap();
        write_incompressible(&mut file, len).unwrap();
        file
    }

    /// Evict `file` and prove every listed range is cold.
    pub(crate) fn make_cold(file: &File, mmap: &Mmap, ranges: &[(u64, u64)]) {
        let left = evict_until_cold(file, mmap, ranges, &evict);
        assert_eq!(
            left, 0,
            "eviction left pages resident, so this test could not tell a warm-up from a \
             cache that was never cold"
        );
    }

    /// Map `file`, evict it, and prove every listed range starts out cold.
    pub(crate) fn cold_mapping(file: &File, len: u64, ranges: &[(u64, u64)]) -> Mmap {
        // SAFETY: the file is private to this test and is not written again.
        let mmap = unsafe { MmapOptions::new().len(len as usize).map(file) }.unwrap();
        make_cold(file, &mmap, ranges);
        mmap
    }

    /// Wait until every range has been seen fully resident, or name the first that was not.
    ///
    /// A range counts from the poll that first saw all of it. The kernel is free to evict an
    /// early range again before a late one arrives, and that is not the warm-up failing.
    pub(crate) fn wait_until_resident(mmap: &Mmap, ranges: &[(u64, u64)]) -> Result<(), String> {
        let page = host_page_size();
        let deadline = Instant::now() + RESIDENT_WITHIN;
        let mut seen = vec![false; ranges.len()];
        loop {
            let mut short = None;
            for (seen, &(offset, len)) in seen.iter_mut().zip(ranges) {
                if *seen {
                    continue;
                }
                let want = len.div_ceil(page);
                let have = resident_pages(mmap, offset, len);
                *seen = have == want;
                if !*seen && short.is_none() {
                    short = Some(format!(
                        "run at {offset:#x}: {have} of {want} pages resident after \
                         {RESIDENT_WITHIN:?}"
                    ));
                }
            }
            match short {
                None => return Ok(()),
                Some(short) if Instant::now() >= deadline => return Err(short),
                Some(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    }

    /// Resident pages of `range` now and again after [`SETTLE`], whichever is larger.
    pub(crate) fn resident_after_settling(mmap: &Mmap, range: (u64, u64)) -> u64 {
        let now = resident_pages(mmap, range.0, range.1);
        std::thread::sleep(SETTLE);
        now.max(resident_pages(mmap, range.0, range.1))
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::AtomicU64;
    use std::sync::mpsc;
    use std::sync::Mutex;
    use std::time::Duration;

    const G: u64 = GRANULE;
    const PAGE: u64 = 4096;

    fn set_of(mem_len: u64, runs: &[(u64, u64)]) -> PageSet {
        let mut set = PageSet::empty(mem_len);
        for &(offset, len) in runs {
            set.insert_range(offset, len);
        }
        set
    }

    /// Run the driver with an `advise` that accepts everything and records what it was asked.
    fn requests_of(set: &PageSet, mem_len: u64, page: u64) -> (Vec<(u64, u64)>, WarmStats) {
        let mut seen = Vec::new();
        let stats = warm(set, mem_len, page, &AtomicBool::new(false), |request| {
            seen.push((request.offset, request.len));
            Ok(())
        });
        (seen, stats)
    }

    /// One request per recorded run, in ascending offset, and nothing for the gaps between
    /// them: the warm-up asks for exactly the recorded runs.
    #[test]
    fn each_run_is_requested_once_in_offset_order_and_gaps_are_not_bridged() {
        let mem_len = 64 * G;
        // Inserted out of order; a one-granule gap separates the first two runs.
        let set = set_of(mem_len, &[(40 * G, 2 * G), (2 * G, 3 * G), (6 * G, G)]);

        let (seen, stats) = requests_of(&set, mem_len, PAGE);

        assert_eq!(seen, vec![(2 * G, 3 * G), (6 * G, G), (40 * G, 2 * G)]);
        assert_eq!(
            stats,
            WarmStats {
                runs: 3,
                requests: 3,
                bytes: 6 * G,
                failures: 0,
                stopped_early: false,
            }
        );
        assert_eq!(stats.bytes, set.bytes());
    }

    /// The kernel truncates one readahead request to its window, so a long run is cut into
    /// pieces that fit.
    #[test]
    fn a_long_run_is_split_at_the_request_size() {
        let mem_len = 4 * MIB;
        let set = set_of(mem_len, &[(MIB, 300 * 1024)]);

        let (seen, stats) = requests_of(&set, mem_len, PAGE);

        assert_eq!(
            seen,
            vec![
                (MIB, REQUEST_BYTES),
                (MIB + REQUEST_BYTES, REQUEST_BYTES),
                (MIB + 2 * REQUEST_BYTES, 300 * 1024 - 2 * REQUEST_BYTES),
            ]
        );
        assert_eq!(
            (stats.runs, stats.requests, stats.bytes),
            (1, 3, 300 * 1024)
        );
    }

    /// On a host whose pages are larger than the 4 KiB recording granule, a run is widened
    /// to the pages it touches, and a page shared by two runs is requested once.
    #[test]
    fn runs_widen_to_whole_host_pages_and_share_a_page_once() {
        const HOST_PAGE: u64 = 64 * 1024;
        let mem_len = 4 * HOST_PAGE;
        // Granules 1 and 3 share host page 0; granule 20 (0x14000) is in host page 1.
        let set = set_of(mem_len, &[(G, G), (3 * G, G), (20 * G, G)]);

        let (seen, stats) = requests_of(&set, mem_len, HOST_PAGE);

        assert_eq!(seen, vec![(0, HOST_PAGE), (HOST_PAGE, HOST_PAGE)]);
        assert_eq!((stats.runs, stats.requests), (2, 2));
    }

    /// A request never runs past the end of the image.
    #[test]
    fn requests_stop_at_the_end_of_the_image() {
        const HOST_PAGE: u64 = 64 * 1024;
        // The image ends 100 bytes into granule 10.
        let mem_len = 10 * G + 100;
        let set = set_of(mem_len, &[(9 * G, 2 * G)]);

        assert_eq!(
            requests_of(&set, mem_len, PAGE).0,
            vec![(9 * G, G + 100)],
            "clipped to the image"
        );
        assert_eq!(
            requests_of(&set, mem_len, HOST_PAGE).0,
            vec![(0, mem_len)],
            "widened down to the host page, still clipped to the image"
        );
    }

    /// Shutdown is observed before every request, including the first.
    #[test]
    fn a_stopped_warm_up_makes_no_further_requests() {
        let mem_len = 64 * G;
        let set = set_of(mem_len, &[(G, G), (5 * G, G), (9 * G, G), (13 * G, G)]);

        let stop = AtomicBool::new(false);
        let mut calls = 0;
        let stats = warm(&set, mem_len, PAGE, &stop, |_| {
            calls += 1;
            if calls == 2 {
                stop.store(true, Ordering::Relaxed);
            }
            Ok(())
        });
        assert_eq!(
            stats,
            WarmStats {
                runs: 2,
                requests: 2,
                bytes: 2 * G,
                failures: 0,
                stopped_early: true,
            }
        );

        let stats = warm(&set, mem_len, PAGE, &AtomicBool::new(true), |_| {
            panic!("a warm-up stopped before it began must not call the kernel")
        });
        assert_eq!(
            stats,
            WarmStats {
                stopped_early: true,
                ..WarmStats::default()
            }
        );
    }

    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
        type Writer = Captured;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// A refusal never ends the warm-up, and a filesystem that refuses every request costs
    /// one log line, not one per run.
    #[test]
    fn refused_requests_are_counted_and_only_the_first_is_logged() {
        let mem_len = 64 * G;
        let set = set_of(mem_len, &[(G, G), (5 * G, G), (9 * G, G), (13 * G, G)]);

        let captured = Captured::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(captured.clone())
            .with_ansi(false)
            .finish();
        let mut calls = 0;
        let stats = tracing::subscriber::with_default(subscriber, || {
            warm(&set, mem_len, PAGE, &AtomicBool::new(false), |_| {
                calls += 1;
                if calls % 2 == 0 {
                    Err(std::io::Error::from_raw_os_error(libc::EIO))
                } else {
                    Ok(())
                }
            })
        });

        assert_eq!(
            stats,
            WarmStats {
                runs: 4,
                requests: 4,
                bytes: 2 * G,
                failures: 2,
                stopped_early: false,
            }
        );
        let log = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
        assert_eq!(
            log.matches("page cache warm-up request was refused")
                .count(),
            1,
            "two refusals, one log line: {log}"
        );
    }

    // =========================================================================
    // The warmer: one warm-up at a time, nobody waiting, stopped by a drop.
    // =========================================================================

    /// How long a parked request waits for the test to let it go. Bounded, so a regression
    /// that makes the caller wait for the warm-up fails an assertion instead of hanging.
    const PARKED_FOR: Duration = Duration::from_secs(5);

    /// A warmer over an empty scratch file. These tests pass their own `advise`, so the file
    /// is never read.
    fn scratch_warmer(mem_len: u64) -> Warmer {
        Warmer::new("test", tempfile::tempfile().unwrap(), mem_len)
    }

    /// An `advise` that reports each request on `entered` and then parks until `release`
    /// yields or closes.
    fn parking_advise(
        entered: mpsc::Sender<Request>,
        release: mpsc::Receiver<()>,
    ) -> impl FnMut(Request) -> std::io::Result<()> + Send + 'static {
        move |request| {
            entered.send(request).unwrap();
            let _ = release.recv_timeout(PARKED_FOR);
            Ok(())
        }
    }

    fn wait_until_idle(warmer: &Warmer) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while warmer.is_running() {
            assert!(Instant::now() < deadline, "the warm-up thread never ended");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Nothing waits for a warm-up: the call that starts one returns while the thread is
    /// still inside its first request.
    #[test]
    fn starting_a_warm_up_returns_while_its_first_request_is_still_parked() {
        let mem_len = 64 * G;
        let set = set_of(mem_len, &[(G, G), (5 * G, G)]);
        let warmer = scratch_warmer(mem_len);
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();

        let begun = Instant::now();
        let started =
            warmer.warm_if_idle_with("test", move || set, parking_advise(entered_tx, release_rx));
        let returned_after = begun.elapsed();

        assert!(started);
        let first = entered_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the thread makes its first request");
        assert_eq!((first.offset, first.len), (G, G));
        assert!(
            returned_after < PARKED_FOR,
            "starting a warm-up took {returned_after:?}, so the caller waited for a request \
             that stays parked for {PARKED_FOR:?}"
        );
        assert!(warmer.is_running(), "the first request is still parked");
        assert!(
            entered_rx.try_recv().is_err(),
            "the second request must not begin before the first is released"
        );

        drop(release_tx);
        wait_until_idle(&warmer);
    }

    /// The server stops a warm-up by dropping its warmer. The thread notices before its next
    /// request.
    #[test]
    fn dropping_the_warmer_stops_a_running_warm_up_before_its_next_request() {
        let mem_len = 64 * G;
        let set = set_of(mem_len, &[(G, G), (5 * G, G), (9 * G, G), (13 * G, G)]);
        let warmer = scratch_warmer(mem_len);
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();

        assert!(warmer.warm_if_idle_with(
            "test",
            move || set,
            parking_advise(entered_tx, release_rx)
        ));
        entered_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the thread makes its first request");

        drop(warmer);
        // Closing the channel releases the parked request and any later one at once, so a
        // warm-up that ignored the drop would run straight through its other three runs.
        drop(release_tx);

        // The thread drops `entered_tx` when it ends, which ends this iterator.
        let after_the_drop: Vec<Request> = entered_rx.iter().collect();
        assert_eq!(
            after_the_drop,
            vec![],
            "requests made after the warmer was dropped"
        );
    }

    /// At most one warm-up runs. A clone admitted while one is running starts nothing, and
    /// the next admission after it ended starts another.
    #[test]
    fn a_warm_up_starts_only_when_none_is_running() {
        let mem_len = 64 * G;
        let warmer = scratch_warmer(mem_len);
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();

        let set = set_of(mem_len, &[(G, G)]);
        assert!(warmer.warm_if_idle_with(
            "serve start",
            move || set,
            parking_advise(entered_tx, release_rx)
        ));
        entered_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the first warm-up makes its request");

        let asked = Arc::new(AtomicU64::new(0));
        let second = warmer.warm_if_idle_with(
            "clone admission",
            || panic!("the recorded set must not be copied for a warm-up that does not start"),
            {
                let asked = Arc::clone(&asked);
                move |_| {
                    asked.fetch_add(1, Ordering::Relaxed);
                    Ok(())
                }
            },
        );
        assert!(!second, "a warm-up was already running");

        drop(release_tx);
        wait_until_idle(&warmer);

        let set = set_of(mem_len, &[(G, G), (5 * G, G)]);
        let third = warmer.warm_if_idle_with("clone admission", move || set, {
            let asked = Arc::clone(&asked);
            move |_| {
                asked.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
        });
        assert!(third, "the earlier warm-up had ended");
        wait_until_idle(&warmer);
        assert_eq!(asked.load(Ordering::Relaxed), 2);
    }

    /// A fresh snapshot has nothing recorded, and a stopping serve starts nothing new.
    #[test]
    fn an_empty_set_asks_for_nothing_and_a_stopped_warmer_starts_nothing() {
        let mem_len = 64 * G;
        let warmer = scratch_warmer(mem_len);

        assert!(warmer.warm_if_idle_with(
            "serve start",
            move || PageSet::empty(mem_len),
            |_| panic!("an empty set has no run to request")
        ));
        wait_until_idle(&warmer);

        warmer.stop();
        let set = set_of(mem_len, &[(G, G)]);
        assert!(!warmer.warm_if_idle_with(
            "clone admission",
            move || set,
            |_| panic!("a stopped warmer must not call the kernel")
        ));
    }

    // =========================================================================
    // The directory the page cache tests run in.
    // =========================================================================

    /// A directory is refused when eviction leaves its pages resident, whatever its
    /// filesystem type says. The no-op `evict` stands in for a filesystem that ignores
    /// `POSIX_FADV_DONTNEED`, which is what the container CI lane's `/tmp` did.
    #[test]
    fn a_directory_where_eviction_leaves_pages_resident_is_refused() {
        let dir = disk_dir();

        assert_eq!(probe_eviction(&dir, evict), Ok(()));
        let refused = probe_eviction(&dir, |_| {}).expect_err("no page was evicted");
        assert!(
            refused.contains("pages still resident"),
            "the reason names what was observed: {refused}"
        );
    }

    /// Candidates are tried in order, and when none works the failure names every one of
    /// them with its reason.
    #[test]
    fn the_first_usable_directory_wins_and_a_total_failure_names_every_candidate() {
        let candidates = [
            PathBuf::from("/first"),
            PathBuf::from("/second"),
            PathBuf::from("/third"),
        ];
        let refuse_first = |dir: &Path| {
            if dir == Path::new("/first") {
                Err("64 of 64 pages still resident".to_string())
            } else {
                Ok(())
            }
        };
        assert_eq!(
            first_usable(&candidates, refuse_first),
            Ok(PathBuf::from("/second"))
        );

        let refused = first_usable(&candidates, |dir| Err(format!("no {}", dir.display())))
            .expect_err("every candidate was refused");
        assert_eq!(
            refused,
            "/first (no /first); /second (no /second); /third (no /third)"
        );
    }

    // =========================================================================
    // Mechanism: a real file, the real page cache, the real posix_fadvise.
    // =========================================================================

    /// The warm-up makes the recorded runs resident, all of each one, and leaves the gap
    /// between two of them and a far range cold.
    #[test]
    fn warm_up_makes_the_recorded_runs_resident_and_leaves_the_gaps_cold() {
        let page = host_page_size();
        let len = 48 * MIB;
        // Two tiny runs, one that needs three requests, and one longer than the 4 MiB
        // readahead window measured above, which a single request leaves partly cold.
        let recorded = [
            (MIB + 3 * page, 2 * page),
            (8 * MIB, 6 * MIB),
            (20 * MIB + 7 * page, page),
            (30 * MIB, 3 * REQUEST_BYTES),
        ];
        // Inside the gap between the second and third runs, 1 MiB clear of both, and well
        // past the last run.
        let unrecorded = [(15 * MIB, 4 * MIB), (40 * MIB, 4 * MIB)];

        let image = image_on_disk(len);
        let mut ranges = recorded.to_vec();
        ranges.extend(unrecorded);
        let mmap = cold_mapping(&image, len, &ranges);

        let stats = warm(
            &set_of(len, &recorded),
            len,
            page,
            &AtomicBool::new(false),
            |request| request_read(&image, request),
        );

        assert_eq!(wait_until_resident(&mmap, &recorded), Ok(()));
        for range in unrecorded {
            assert_eq!(
                resident_after_settling(&mmap, range),
                0,
                "the warm-up read {range:#x?}, which no clone recorded"
            );
        }
        assert_eq!(
            stats,
            WarmStats {
                runs: 4,
                requests: 1 + 48 + 1 + 3,
                bytes: recorded.iter().map(|&(_, len)| len).sum(),
                failures: 0,
                stopped_early: false,
            }
        );
    }

    /// The warmer does the same work from its own thread, with the real kernel call.
    #[test]
    fn a_warmer_reads_the_recorded_set_from_its_own_thread() {
        let len = 8 * MIB;
        let recorded = [(MIB, 2 * REQUEST_BYTES), (5 * MIB, host_page_size())];

        let image = image_on_disk(len);
        let mmap = cold_mapping(&image, len, &recorded);
        let warmer = Warmer::new("test", image, len);

        let set = set_of(len, &recorded);
        assert!(warmer.warm_if_idle("serve start", move || set));

        assert_eq!(wait_until_resident(&mmap, &recorded), Ok(()));
    }
}

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
//! # Measured
//!
//! One snapshot of a 128 GiB guest in copy mode, 41.4 GiB recorded in 3.39M runs, with the
//! memory file evicted before every run. Replay took 94.5 s and 87.7 s without the warm-up.
//! With it, replay took 32.3 s and 32.9 s when the clone connected as soon as the serve was
//! ready, and 22.3 s when it connected after the warm-up had logged, 19 s after the serve
//! started. Demand faults during replay fell from 992,535 to 562,321 between the first run
//! of each arm. The warm-up at serve start issued 3.49M requests for 41.4 GiB in 18.4 s.
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
//! Replay plans every recorded run in first-touch order, tier by tier and each tier in
//! ascending file offset ([`plan`](super::prefetch::plan)), which is the order the warm-up
//! walks, and a request
//! returns when its read is queued, not when it completes. A clone that reaches a page whose
//! read is already queued waits for that read instead of issuing its own. Whether a clone
//! that connects during a warm-up finds its pages cached is a race this module does not
//! decide. One request measured 6 to 7 us on a cold uncompressed extent and 21 us on a cold
//! compressed one. A request for a page that is already cached measured 0.1 to 0.7 us across
//! three measurements, which puts walking a fully cached set of 3M runs between 0.3 and two
//! seconds of one thread. On the 128 GiB guest above, the warm-up issued at a clone's
//! admission over an already cached set of 3.5M requests took 0.8 s.
//!
//! # Nothing waits for it
//!
//! The thread is detached and nothing joins it. The serve's ready record, clone admission,
//! fault service and the shutdown sequence do not depend on it, and `POSIX_FADV_WILLNEED`
//! never touches the mapping, so it cannot fault on a truncated image.
//!
//! Cancelling or dropping the server sets a flag and returns. The thread reads the flag
//! before every request, and nothing serialises the two, because a stop that waited for the
//! thread would wait on file I/O, which is what detaching it avoids. So at most one further
//! request starts after a stop returns: one asynchronous readahead of up to 128 KiB that
//! nothing waits for. A request stuck in a hung filesystem cannot be interrupted, and a
//! thread in that state delays the kernel reaping the process after it exits. The fault
//! handlers have the same exposure, because they read the same file through the mapping.
//!
//! # A hint
//!
//! A refused request is counted, the first one is logged, and the warm-up carries on. Clones
//! restore correctly from a cold cache, only slower.
//!
//! # Known limits
//!
//! * **It is paced against the disk to [`IN_FLIGHT_BYTES`].** Unpaced, it queued a whole
//!   recorded set, and every demand fault on a page it had not reached read behind all of it
//!   (#986, #1069). Within the window, and within [`PACED_WINDOW_BYTES`] of replay once its
//!   requests pass the free memory it started with, a running clone's major fault on an
//!   uncached page still waits behind readahead. A microbenchmark (6 GiB file on btrfs over a virtio disk, `nr_requests` 256,
//!   scheduler `none`) measured a cold single-page fault at a median of 125 us alone and
//!   1,193 us while back-to-back 128 KiB requests ran, with the p90 going from 141 us to
//!   6.5 ms. With scattered 8 KiB runs the median went from 119 to 152 us and the p99 from
//!   266 to 1,314 us. The admitted clone gains far more than that costs (see Measured), and
//!   on a cached image the walk does no I/O.
//! * **Pacing against replay starts once free memory is spent.** Until the requests pass
//!   the free memory the warm-up started with (less a sixteenth of the host), they run
//!   unpaced; after that each one waits until it is within [`PACED_WINDOW_BYTES`] of the
//!   furthest point a running replay has reached, and waits while none runs (#1066). A
//!   warm-up still waiting when its clone's replay has ended keeps the one warm-up slot, so
//!   a clone admitted then starts no warm-up of its own; when that clone's replay begins, a
//!   new group of replays, the waiting warm-up starts over from the beginning of the set,
//!   taking the recorded set and the free memory again (#1676).
//! * **A minor-mode server undoes it.** A minor-mode server that starts on the same image
//!   ends `create_backing_memfd` with a whole-file `POSIX_FADV_DONTNEED`, which drops what a
//!   copy-mode warm-up loaded. Only the hint is lost.

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
/// Measured on btrfs with a 4 MiB readahead window: one call over a 64 MiB range left 1,024
/// of its 16,384 pages resident, and the same range in calls of this size left all 16,384.
/// 128 KiB is the kernel default for both limits (`VM_READAHEAD_PAGES`), so a request this
/// size is only truncated on a host that lowered them, and it is a whole number of 4, 16 or
/// 64 KiB pages.
const REQUEST_BYTES: u64 = 128 * 1024;

/// How many requested bytes may be read ahead and not yet in the page cache.
///
/// A demand fault on a page the warm-up has not reached reads behind every queued readahead
/// request, so an unpaced warm-up that queued a whole recorded set made each such fault wait
/// for all of it (#986, #1069). Measured in one session on a c8g.metal-48xl: a 128 GiB, 32
/// vCPU guest whose 28 workers touch 40 GiB, restored four times from a 46 GiB recorded set
/// with the memory file read at 400 MB/s (cgroup io.max) after the page cache was dropped.
/// The guest acknowledgement of restores 2 to 4, and the soft lockups the guest logged:
///
/// | warm-up | acknowledgement (s) | soft lockups |
/// |---|---|---|
/// | unpaced (two series) | 89, 121, 121 and 98, 132, 132 | 28 to 30, and 3, per restore |
/// | none | 100, 118, 108 | 0 |
/// | 32 MiB in flight | 10.2, 8.9, 9.1 | 0 |
/// | 8 MiB in flight | 4.6, 3.4, 3.5 | 0 |
/// | 2 MiB in flight | 5.3, 4.8, 4.7 | 0 |
///
/// A first restore, with no recorded set, acknowledged in 1.8 to 3.5 s. Paced at 8 MiB the
/// warm-up read 9.1 to 10.4 M pages in 95 to 98 s, the device's rate, so pacing cost it no
/// throughput.
const IN_FLIGHT_BYTES: u64 = 8 * 1024 * 1024;

/// How long one request may stay not resident before the pacer stops waiting for it: a page
/// the kernel read and evicted again would otherwise hold the warm-up for ever. Retiring a
/// request that is still being read lets the next window out early, so this is long: a
/// request still unread after it means the device moves less than 8 MiB in 30 s.
const RESIDENT_WITHIN: std::time::Duration = std::time::Duration::from_secs(30);

/// Keeps at most [`IN_FLIGHT_BYTES`] of the warm-up's requests outstanding: before the next
/// request is issued, the oldest outstanding ones are retired once `resident` reports them
/// in the page cache, or once they have been outstanding [`RESIDENT_WITHIN`].
struct Pacer<R: FnMut(Request) -> bool> {
    in_flight: std::collections::VecDeque<(Request, Instant)>,
    bytes: u64,
    window: u64,
    resident: R,
}

impl<R: FnMut(Request) -> bool> Pacer<R> {
    fn new(window: u64, resident: R) -> Self {
        Self {
            in_flight: std::collections::VecDeque::new(),
            bytes: 0,
            window,
            resident,
        }
    }

    /// Wait until `next` fits the window, or `stop` is set. Returns false on a stop.
    fn wait_for_room(&mut self, next: Request, stop: &AtomicBool) -> bool {
        while self.bytes + next.len > self.window && !self.in_flight.is_empty() {
            if stop.load(Ordering::Relaxed) {
                return false;
            }
            let (oldest, issued) = self.in_flight[0];
            if (self.resident)(oldest) || issued.elapsed() >= RESIDENT_WITHIN {
                self.in_flight.pop_front();
                self.bytes -= oldest.len;
            } else {
                std::thread::sleep(std::time::Duration::from_micros(200));
            }
        }
        true
    }

    fn issued(&mut self, request: Request) {
        self.in_flight.push_back((request, Instant::now()));
        self.bytes += request.len;
    }
}

/// Whether every page of `request` is in `image`'s page cache: a `preadv2` of the whole
/// request with `RWF_NOWAIT`, which reads only what the cache holds and stops at the first
/// page it does not, so a full read means the request is resident. `scratch` takes the bytes
/// and is reused across calls.
///
/// Not one page: readahead skips pages already cached, so a cached last page says nothing
/// about the pages before it. Not `mincore` first: for a file the process neither owns nor
/// may write, the kernel reports every page of a mapping resident (`can_do_mincore`), which
/// would retire every request at once. A filesystem that refuses `RWF_NOWAIT` (overlayfs
/// answers EOPNOTSUPP) is asked with `mincore` on a mapping of the file, made the first time
/// it is needed and kept in `map`, which is right for the files fcvm writes itself; with no
/// mapping, and on any other error, the request counts as resident and the pacer lets the
/// warm-up run as it did before it existed. The mapping is made here, on the warm-up's own
/// thread: making it reads the file's metadata, which a stalled filesystem can hold, and the
/// server admits clones on the thread that starts a warm-up.
fn resident(
    image: &File,
    map: &mut Option<Option<memmap2::Mmap>>,
    request: Request,
    scratch: &mut Vec<u8>,
) -> bool {
    use std::os::fd::AsRawFd;
    let (Ok(offset), Ok(len)) = (
        libc::off_t::try_from(request.offset),
        usize::try_from(request.len),
    ) else {
        return true;
    };
    if len == 0 {
        return true;
    }
    if scratch.len() < len {
        scratch.resize(len, 0);
    }
    let iov = libc::iovec {
        iov_base: scratch.as_mut_ptr().cast(),
        iov_len: len,
    };
    // SAFETY: one iovec over `len` bytes of a live buffer, on a descriptor this file owns.
    let rc = unsafe { libc::preadv2(image.as_raw_fd(), &iov, 1, offset, libc::RWF_NOWAIT) };
    if let Ok(read) = usize::try_from(rc) {
        return read >= len;
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::EAGAIN) => false,
        Some(libc::EOPNOTSUPP) => map
            // SAFETY: the mapping is only passed to mincore, which never reads the pages,
            // so a file another process truncates cannot fault through it.
            .get_or_insert_with(|| unsafe { memmap2::Mmap::map(image) }.ok())
            .as_ref()
            .is_none_or(|map| mapped_resident(map, request)),
        _ => true,
    }
}

/// The pacer's view of the page cache, made on the warm-up's own thread the first time it is
/// asked: opening a file and mapping it read its metadata, which a stalled filesystem can
/// hold, and the server admits clones on the thread that starts a warm-up.
///
/// The probes read through a descriptor of their own, advised `POSIX_FADV_RANDOM`: a
/// `RWF_NOWAIT` read of a page the cache does not hold may start the kernel's readahead,
/// and on a descriptor without readahead that reads only the probed request's own pages,
/// which the warm-up has asked for already, and none of their neighbours. The server's
/// descriptor keeps its readahead, which its fault path's reads use.
struct Probe {
    image: Arc<File>,
    file: Option<Option<File>>,
    map: Option<Option<memmap2::Mmap>>,
    scratch: Vec<u8>,
}

/// The residency question the server's shared pacer asks.
type ResidentProbe = Box<dyn FnMut(Request) -> bool + Send>;

impl Probe {
    fn new(image: Arc<File>) -> Self {
        Self {
            image,
            file: None,
            map: None,
            scratch: Vec::new(),
        }
    }

    fn resident(&mut self, request: Request) -> bool {
        use std::os::fd::AsRawFd;
        let image = &self.image;
        let file = self.file.get_or_insert_with(|| {
            let file = File::open(format!("/proc/self/fd/{}", image.as_raw_fd())).ok()?;
            let _ = posix_fadvise(&file, 0, 0, PosixFadviseAdvice::POSIX_FADV_RANDOM);
            Some(file)
        });
        match file {
            Some(file) => resident(file, &mut self.map, request, &mut self.scratch),
            None => resident(image, &mut self.map, request, &mut self.scratch),
        }
    }
}

/// Whether every page of `request` is resident by `mincore` on `map`, a mapping of the file.
/// `mincore` does not touch the pages, so a truncated file cannot fault here either.
fn mapped_resident(map: &memmap2::Mmap, request: Request) -> bool {
    let page = host_page_size() as usize;
    let (Ok(start), Ok(len)) = (
        usize::try_from(request.offset),
        usize::try_from(request.len),
    ) else {
        return true;
    };
    if start.saturating_add(len) > map.len() {
        return true;
    }
    let mut vec = vec![0u8; len.div_ceil(page)];
    // SAFETY: the range lies inside `map`, which stays mapped for the call, and `vec` has a
    // byte for each of its pages.
    let rc = unsafe {
        libc::mincore(
            map.as_ptr().add(start) as *mut libc::c_void,
            len,
            vec.as_mut_ptr().cast(),
        )
    };
    rc != 0 || vec.iter().all(|byte| byte & 1 == 1)
}

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
    /// The warm-up waited for replay at least once.
    paced: bool,
    /// A new group of replays began, and the warm-up stopped to start over with it.
    restart: bool,
}

/// How far the warm-up may read ahead of the replay that copies the pages, once the free
/// memory it started with is spent (#1066).
///
/// A page the warm-up reads starts on the kernel's inactive list, unreferenced. With free
/// memory short and other recently used file pages on the active list, reclaim takes the
/// warm-up's pages first, so a warm-up far ahead of replay reads pages that are gone again by
/// the time replay copies them, and replay reads them a second time: one restore read 465 GiB
/// for a 41 GiB recorded set. Kept within this window of the replay cursor, a page is read
/// shortly before it is copied.
pub(crate) const PACED_WINDOW_BYTES: u64 = 1 << 30;

/// Free memory a warm-up leaves alone before it paces itself: one sixteenth of the host.
const PACING_RESERVE_DIVISOR: u64 = 16;

/// Replay's progress through the recorded set, which a paced warm-up stays ahead of.
///
/// A replay and a warm-up walk the set in the same order, ascending file offset, so the
/// recorded bytes each has passed place both in it. The position is the furthest any running
/// replay has reached since the first of them began. Overlapping replays form one group,
/// numbered so a warm-up can tell a new group from the one it paced with. One lock holds
/// all of it, so a reader never sees a new group's count with the last group's position;
/// replay takes it once per chunk it copies.
#[derive(Debug, Default)]
pub(crate) struct ReplayProgress {
    state: std::sync::Mutex<ReplayState>,
    /// Test only: every position reported, in order.
    #[cfg(test)]
    reported: std::sync::Mutex<Vec<u64>>,
}

/// What [`ReplayProgress`] holds: the replays running, the group they are, how far they have
/// got. A warm-up reads a copy.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ReplayState {
    active: usize,
    group: u64,
    bytes: u64,
}

impl ReplayState {
    fn running(&self) -> bool {
        self.active > 0
    }
}

impl ReplayProgress {
    fn lock(&self) -> std::sync::MutexGuard<'_, ReplayState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A replay starts at the beginning of the set. The first of a group of overlapping
    /// replays resets the position and starts a new group.
    pub(crate) fn begin(&self) {
        let mut state = self.lock();
        if state.active == 0 {
            state.bytes = 0;
            state.group += 1;
        }
        state.active += 1;
    }

    /// The replay that has passed `bytes` of the recorded set reports it.
    pub(crate) fn passed(&self, bytes: u64) {
        let mut state = self.lock();
        state.bytes = state.bytes.max(bytes);
        drop(state);
        #[cfg(test)]
        self.reported
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(bytes);
    }

    pub(crate) fn end(&self) {
        let mut state = self.lock();
        state.active = state.active.saturating_sub(1);
    }

    fn state(&self) -> ReplayState {
        *self.lock()
    }

    /// Where replay is, or `None` while none runs.
    #[cfg(test)]
    fn position(&self) -> Option<u64> {
        let state = self.state();
        state.running().then_some(state.bytes)
    }
}

/// Whether the warm-up may issue a request `requested` bytes into the recorded set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pace {
    Go,
    Wait,
    /// A new group of replays began after the one the warm-up paced with. Its replay
    /// copies from the start of the set, so the warm-up starts over too.
    Restart,
}

/// Unpaced while the bytes requested fit in the free memory the warm-up started with; after
/// that, only within [`PACED_WINDOW_BYTES`] of a running replay.
fn pace(requested: u64, unpaced_budget: u64, replay_at: Option<u64>) -> Pace {
    if requested < unpaced_budget {
        return Pace::Go;
    }
    match replay_at {
        Some(at) if requested <= at.saturating_add(PACED_WINDOW_BYTES) => Pace::Go,
        _ => Pace::Wait,
    }
}

/// [`pace`] for one warm-up, which says [`Pace::Restart`] once a group of replays later than
/// the one it can pace with is running. That is the group after the one running when the
/// warm-up began, or, when none was, the group after the next one: the next is the replay
/// of the clone whose admission started it, and a later one means that replay has ended,
/// whether or not the warm-up saw it run.
#[derive(Debug)]
struct Pacing {
    restart_at: u64,
}

impl Pacing {
    fn starting_at(replay: ReplayState) -> Self {
        let next = replay.group + 1;
        Self {
            restart_at: if replay.running() { next } else { next + 1 },
        }
    }

    fn pace(&mut self, requested: u64, unpaced_budget: u64, replay: ReplayState) -> Pace {
        if replay.running() && replay.group >= self.restart_at {
            self.restart_at = replay.group + 1;
            return Pace::Restart;
        }
        pace(
            requested,
            unpaced_budget,
            replay.running().then_some(replay.bytes),
        )
    }
}

/// Free memory a warm-up may fill without pacing: `MemFree` less a reserve, from
/// `/proc/meminfo`. Memory that cannot be read paces from the first request.
pub(crate) fn unpaced_budget(meminfo: &str) -> u64 {
    let field = |name: &str| {
        meminfo.lines().find_map(|line| {
            let rest = line.strip_prefix(name)?.strip_prefix(':')?;
            let kib: u64 = rest.trim().strip_suffix("kB")?.trim().parse().ok()?;
            kib.checked_mul(1024)
        })
    };
    match (field("MemFree"), field("MemTotal")) {
        (Some(free), Some(total)) => free.saturating_sub(total / PACING_RESERVE_DIVISOR),
        _ => 0,
    }
}

/// Issue the requests for `recorded` through `advise`, in replay order: tier by tier, each
/// tier in ascending file offset.
///
/// `stop` is read before every request and nothing serialises it with the call that
/// follows, so at most one further request starts after a stop.
#[cfg(test)]
fn warm(
    recorded: &PageSet,
    mem_len: u64,
    page: u64,
    stop: &AtomicBool,
    advise: impl FnMut(Request) -> std::io::Result<()>,
) -> WarmStats {
    warm_paced(recorded, mem_len, page, stop, &mut |_| Pace::Go, advise)
}

/// [`warm`], asking `pace` before each request how far into the recorded set it is, and
/// waiting while it says to. `stop` is read while it waits.
fn warm_paced(
    recorded: &PageSet,
    mem_len: u64,
    page: u64,
    stop: &AtomicBool,
    pace: &mut dyn FnMut(u64) -> Pace,
    mut advise: impl FnMut(Request) -> std::io::Result<()>,
) -> WarmStats {
    let mut stats = WarmStats::default();
    let mut floor = 0u64;
    let mut requested = 0u64;
    let mut runs = super::working_set::OrderedRuns::new(recorded);
    let mut tier = 0;
    // A host page larger than a granule can hold granules of several tiers; a bit per host
    // page keeps it to one request, as replay plans each clone page once.
    let mut asked =
        (page > GRANULE).then(|| vec![0u64; mem_len.div_ceil(page).div_ceil(64) as usize]);
    'runs: while let Some(run) = runs.next() {
        // Runs come tier by tier, each tier in ascending offset; a new tier starts again from
        // the bottom of the image.
        if runs.tier() != tier {
            tier = runs.tier();
            floor = 0;
        }
        let mut counted = false;
        for whole in requests_for(run, floor, page, mem_len) {
            for request in unasked(&mut asked, whole, page) {
                loop {
                    match pace(requested) {
                        Pace::Go => break,
                        Pace::Restart => {
                            stats.restart = true;
                            break 'runs;
                        }
                        Pace::Wait => {}
                    }
                    if stop.load(Ordering::Relaxed) {
                        stats.stopped_early = true;
                        break 'runs;
                    }
                    stats.paced = true;
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
                if stop.load(Ordering::Relaxed) {
                    stats.stopped_early = true;
                    break 'runs;
                }
                requested += request.len;
                if !counted {
                    stats.runs += 1;
                    counted = true;
                }
                stats.requests += 1;
                match advise(request) {
                    Ok(()) => stats.bytes += request.len,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                        stats.requests -= 1;
                        stats.stopped_early = true;
                        break 'runs;
                    }
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
            }
            floor = whole.offset + whole.len;
        }
    }
    stats
}

/// The parts of `request` whose host pages no earlier request asked for, marking them asked.
/// Without a bitmap (host pages of one granule) the request itself.
fn unasked(asked: &mut Option<Vec<u64>>, request: Request, page: u64) -> Pieces {
    let Some(bits) = asked.as_mut() else {
        return Pieces::Whole(Some(request));
    };
    let mut pieces: Vec<Request> = Vec::new();
    let mut open: Option<Request> = None;
    let first = request.offset / page;
    for index in 0..request.len.div_ceil(page) {
        let bit = (first + index) as usize;
        let (word, mask) = (bit / 64, 1u64 << (bit % 64));
        let fresh = bits.get(word).is_some_and(|w| w & mask == 0);
        let at = request.offset + index * page;
        let len = page.min(request.offset + request.len - at);
        if fresh {
            bits[word] |= mask;
            match open.as_mut() {
                Some(piece) => piece.len += len,
                None => open = Some(Request { offset: at, len }),
            }
        } else if let Some(piece) = open.take() {
            pieces.push(piece);
        }
    }
    pieces.extend(open);
    Pieces::Parts(pieces.into_iter())
}

/// What [`unasked`] gives back: the request itself, with no allocation on the common path,
/// or its unasked parts.
enum Pieces {
    Whole(Option<Request>),
    Parts(std::vec::IntoIter<Request>),
}

impl Iterator for Pieces {
    type Item = Request;

    fn next(&mut self) -> Option<Request> {
        match self {
            Pieces::Whole(request) => request.take(),
            Pieces::Parts(parts) => parts.next(),
        }
    }
}

/// The host's page size, or [`GRANULE`] if `sysconf` has no usable answer.
pub(super) fn host_page_size() -> u64 {
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

/// One warm-up from start to finish, on whatever thread calls it: read the recorded set,
/// issue the requests, and log one line for each pass. A pass that a new group of replays
/// interrupts ([`Pace::Restart`]) is followed by another over the set as it is then, with
/// the free memory measured again.
fn warm_up_once(
    snapshot: &str,
    trigger: &'static str,
    recorded: &dyn Fn() -> PageSet,
    mem_len: u64,
    stop: &AtomicBool,
    progress: Option<&ReplayProgress>,
    mut advise: impl FnMut(Request) -> std::io::Result<()>,
) {
    // With no replay to follow, nothing is paced.
    let mut budget = || match progress {
        Some(_) => unpaced_budget(&std::fs::read_to_string("/proc/meminfo").unwrap_or_default()),
        None => u64::MAX,
    };
    let mut replay_at = || progress.map(ReplayProgress::state).unwrap_or_default();
    warm_passes(
        recorded,
        &mut budget,
        &mut replay_at,
        mem_len,
        host_page_size(),
        stop,
        &mut advise,
        &mut |pass, stats, budget, elapsed| match stats {
            None => info!(
                target: "uffd",
                snapshot = %snapshot,
                trigger,
                pass,
                "no working set is recorded yet, so there is nothing to read into the page cache"
            ),
            Some(stats) => info!(
                target: "uffd",
                snapshot = %snapshot,
                trigger,
                pass,
                runs = stats.runs,
                pages = stats.bytes / GRANULE,
                mib = stats.bytes / (1024 * 1024),
                requests = stats.requests,
                failures = stats.failures,
                elapsed_ms = elapsed.as_millis(),
                stopped_early = stats.stopped_early,
                paced = stats.paced,
                restarted = stats.restart,
                unpaced_budget_mib = budget.min(u64::MAX / 2) / (1024 * 1024),
                "asked the kernel to read the recorded working set into the page cache"
            ),
        },
    );
}

/// The passes of one warm-up: each takes the recorded set and the free-memory budget when
/// it begins, and a pass that ends on [`Pace::Restart`] is followed by another. `report`
/// gets each pass's number, its stats (`None` for an empty set, which ends the warm-up),
/// its budget and how long it took.
#[allow(clippy::too_many_arguments)]
fn warm_passes(
    recorded: &dyn Fn() -> PageSet,
    budget: &mut dyn FnMut() -> u64,
    replay_at: &mut dyn FnMut() -> ReplayState,
    mem_len: u64,
    page: u64,
    stop: &AtomicBool,
    advise: &mut dyn FnMut(Request) -> std::io::Result<()>,
    report: &mut dyn FnMut(u64, Option<WarmStats>, u64, std::time::Duration),
) {
    let mut pacing = Pacing::starting_at(replay_at());
    for pass in 1.. {
        let started = Instant::now();
        let set = recorded();
        let budget = budget();
        if set.is_empty() {
            // The first clone of a fresh snapshot has recorded nothing yet.
            report(pass, None, budget, started.elapsed());
            return;
        }
        let stats = warm_paced(
            &set,
            mem_len,
            page,
            stop,
            &mut |requested| pacing.pace(requested, budget, replay_at()),
            &mut *advise,
        );
        report(pass, Some(stats), budget, started.elapsed());
        if !stats.restart {
            return;
        }
    }
}

/// What one serve's warm-ups share: at most one runs at a time, and none starts once the
/// serve is stopping.
#[derive(Default)]
struct Shared {
    running: AtomicBool,
    stop: AtomicBool,
    progress: ReplayProgress,
    /// Test only: warm-ups ignore free memory and replay, so a test's requests never wait on
    /// the host's state.
    #[cfg(test)]
    unpaced: AtomicBool,
}

impl Shared {
    fn pacing_progress(&self) -> Option<&ReplayProgress> {
        #[cfg(test)]
        if self.unpaced.load(Ordering::Relaxed) {
            return None;
        }
        Some(&self.progress)
    }
}

/// A clone's view of its serve's [`ReplayProgress`].
pub(crate) struct ReplayProgressHandle(Arc<Shared>);

impl ReplayProgressHandle {
    pub(crate) fn begin(&self) {
        self.0.progress.begin();
    }

    pub(crate) fn passed(&self, bytes: u64) {
        self.0.progress.passed(bytes);
    }

    pub(crate) fn end(&self) {
        self.0.progress.end();
    }

    /// The furthest point reported since the last replay began, running or not.
    #[cfg(test)]
    pub(crate) fn furthest(&self) -> u64 {
        self.0.progress.state().bytes
    }

    /// Every position reported, in order.
    #[cfg(test)]
    pub(crate) fn reported(&self) -> Vec<u64> {
        self.0
            .progress
            .reported
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
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
/// any clone admission. Stopping or dropping it sets a flag and returns: a running warm-up
/// starts at most one further request, and no further warm-up starts. Nothing ever joins a
/// warm-up thread.
pub(crate) struct Warmer {
    snapshot: String,
    image: Arc<File>,
    mem_len: u64,
    shared: Arc<Shared>,
    /// One pacer for every warm-up of this server, so a warm-up that starts while the last
    /// one's final window is still being read waits for it: a pacer per warm-up let two
    /// windows be outstanding at once.
    pacer: Arc<std::sync::Mutex<Pacer<ResidentProbe>>>,
}

impl Warmer {
    pub(crate) fn new(snapshot_id: &str, image: Arc<File>, mem_len: u64) -> Self {
        let mut probe = Probe::new(Arc::clone(&image));
        let resident: ResidentProbe = Box::new(move |request| probe.resident(request));
        Self {
            snapshot: snapshot_id.to_string(),
            image,
            mem_len,
            shared: Arc::new(Shared::default()),
            pacer: Arc::new(std::sync::Mutex::new(Pacer::new(IN_FLIGHT_BYTES, resident))),
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
        recorded: impl Fn() -> PageSet + Send + 'static,
    ) -> bool {
        let image = Arc::clone(&self.image);
        let stop = Arc::clone(&self.shared);
        let pacer = Arc::clone(&self.pacer);
        self.warm_if_idle_with(trigger, recorded, move |request| {
            let mut pacer = pacer.lock().unwrap_or_else(|e| e.into_inner());
            if !pacer.wait_for_room(request, &stop.stop) {
                // Not asked for: the warm-up counts it as a stop, not as requested bytes.
                return Err(std::io::Error::from(std::io::ErrorKind::Interrupted));
            }
            request_read(&image, request)?;
            pacer.issued(request);
            Ok(())
        })
    }

    /// [`Warmer::warm_if_idle`] with the kernel call passed in, so tests can park it.
    fn warm_if_idle_with(
        &self,
        trigger: &'static str,
        recorded: impl Fn() -> PageSet + Send + 'static,
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
                warm_up_once(
                    &snapshot,
                    trigger,
                    &recorded,
                    mem_len,
                    &shared.stop,
                    shared.pacing_progress(),
                    advise,
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

    /// Where the clones' replays are, for a warm-up to pace itself by. A clone's handler
    /// reports to this while it replays.
    pub(crate) fn progress(&self) -> Arc<ReplayProgressHandle> {
        Arc::new(ReplayProgressHandle(Arc::clone(&self.shared)))
    }

    /// Tell a running warm-up to end, and start no more. Returns without waiting for the
    /// thread, so at most one further request starts.
    pub(crate) fn stop(&self) {
        self.shared.stop.store(true, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(crate) fn is_running(&self) -> bool {
        self.shared.running.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn is_stopped(&self) -> bool {
        self.shared.stop.load(Ordering::Relaxed)
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

    /// One eviction attempt: call `evict` once and count what is still resident in `ranges`.
    ///
    /// Once, on purpose. `POSIX_FADV_DONTNEED` skips pages that are dirty or under writeback
    /// and starts that writeback itself, so where fsync cleans nothing a second call a moment
    /// later succeeds. A test that retried would race the asynchronous writeback of its whole
    /// file against a timeout. These tests run where one flush and one call are enough.
    fn evict_once(file: &File, mmap: &Mmap, ranges: &[(u64, u64)], evict: &dyn Fn(&File)) -> u64 {
        evict(file);
        ranges
            .iter()
            .map(|&(offset, len)| resident_pages(mmap, offset, len))
            .sum()
    }

    /// Whether a test can make a file in `dir` cold with one flush and one eviction, and if
    /// not, why not.
    ///
    /// These tests prove "cold before, warm after", so they need a directory where a written
    /// file's pages can be evicted on demand. The filesystem type does not say. tmpfs never
    /// evicts. The container CI lane's `/tmp` is an overlay that `podman run --rm` mounts
    /// `volatile`, which makes fsync a no-op, so a file written there is still dirty when the
    /// test evicts it. Measured in that container: all 64 pages of a 256 KiB file were
    /// resident after the first `POSIX_FADV_DONTNEED` and none 50 ms later, once the
    /// writeback that call started had finished. A bind mount in the same container flushed
    /// in 16 ms and was cold after the first call. So this probes the behaviour itself, with
    /// the calls the tests use: write, flush, evict once through `evict`, and count what
    /// `mincore` still reports.
    pub(crate) fn probe_eviction(dir: &Path, evict: impl Fn(&File)) -> Result<(), String> {
        let mut file =
            tempfile::tempfile_in(dir).map_err(|error| format!("cannot create a file: {error}"))?;
        write_incompressible(&mut file, PROBE_BYTES)
            .map_err(|error| format!("cannot write a file: {error}"))?;
        // SAFETY: the file is private to this probe and is not written again.
        let mmap = unsafe { MmapOptions::new().len(PROBE_BYTES as usize).map(&file) }
            .map_err(|error| format!("cannot map a file: {error}"))?;
        match evict_once(&file, &mmap, &[(0, PROBE_BYTES)], &evict) {
            0 => Ok(()),
            left => Err(format!(
                "{left} of {} pages still resident after one fsync and one POSIX_FADV_DONTNEED",
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
        let dir =
            first_usable(&candidates, |dir| probe_eviction(dir, evict)).unwrap_or_else(|refused| {
                panic!("no directory lets a test evict a file from the page cache: {refused}")
            });
        // Shown for a failed test and under --nocapture: which directory this lane used.
        eprintln!("page cache tests run in {}", dir.display());
        dir
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
        let left = evict_once(file, mmap, ranges, &evict);
        assert_eq!(
            left, 0,
            "one POSIX_FADV_DONTNEED left pages resident in a directory the probe accepted, \
             so this test could not tell a warm-up from a cache that was never cold"
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

    /// Wait for a warmer's thread to end.
    pub(crate) fn wait_until_idle(warmer: &super::Warmer) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while warmer.is_running() {
            assert!(Instant::now() < deadline, "the warm-up thread never ended");
            std::thread::sleep(Duration::from_millis(5));
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

    /// The pacer's residency check reads the page cache, not the mapping's permissions: a
    /// request is not resident while its file is cold, and is once the file has been read.
    /// `mincore`, its first form, reports every page of a file the process may not write as
    /// resident, which turned the pacer off for a serve of another user's snapshot.
    #[test]
    fn residency_follows_the_page_cache() {
        use testing::{evict, image_on_disk, MIB};
        let file = image_on_disk(4 * MIB);
        let at = |offset: u64| Request {
            offset,
            len: REQUEST_BYTES,
        };
        let mut map = None;
        let mut scratch = Vec::new();
        evict(&file);
        // Each request is asked about before anything else touches it: the probe itself
        // starts the kernel's readahead of the pages it finds missing.
        assert!(
            !resident(&file, &mut map, at(3 * MIB), &mut scratch),
            "a cold request reads as resident"
        );
        // Only its last page cached: readahead would fill the rest, so it is not resident.
        let mut page = vec![0u8; PAGE as usize];
        std::os::unix::fs::FileExt::read_exact_at(&file, &mut page, MIB + REQUEST_BYTES - PAGE)
            .unwrap();
        assert!(
            !resident(&file, &mut map, at(MIB), &mut scratch),
            "a request whose last page alone is cached reads as resident"
        );
        let mut buf = vec![0u8; REQUEST_BYTES as usize];
        std::os::unix::fs::FileExt::read_exact_at(&file, &mut buf, MIB).unwrap();
        assert!(
            resident(&file, &mut map, at(MIB), &mut scratch),
            "a request just read reads as cold"
        );
    }

    /// A request the pacer gave up on because the serve is stopping is a stop: nothing was
    /// asked of the kernel, so it is neither requested bytes nor a refusal.
    #[test]
    fn a_request_the_pacer_stopped_counts_as_a_stop() {
        let mem_len = 64 * G;
        let set = set_of(mem_len, &[(0, 8 * G)]);
        let stop = AtomicBool::new(false);
        let stats = warm_paced(&set, mem_len, PAGE, &stop, &mut |_| Pace::Go, |_| {
            Err(std::io::Error::from(std::io::ErrorKind::Interrupted))
        });
        assert!(stats.stopped_early);
        assert_eq!(stats.bytes, 0);
        assert_eq!(stats.failures, 0);
        assert_eq!(stats.requests, 0);
    }

    /// The warm-up the server starts goes through the pacer: each request waits for room in
    /// the window before it is issued, and is counted once issued. Unpaced, the warm-up
    /// queued a whole recorded set (#986, #1069).
    #[test]
    fn the_server_s_warm_up_is_paced() {
        let source = include_str!("warmup.rs");
        let code = &source[..source.find("#[cfg(test)]\nmod tests {").unwrap()];
        let start = code
            .find("pub(crate) fn warm_if_idle(")
            .expect("no warm_if_idle");
        let body = &code[start..start + code[start..].find("\n    }\n").unwrap()];
        let wait = body
            .find("pacer.wait_for_room(request")
            .expect("the warm-up does not wait for room in the window");
        let read = body
            .find("request_read(&image, request)")
            .expect("the warm-up issues no request");
        let issued = body
            .find("pacer.issued(request)")
            .expect("the warm-up does not count what it issued");
        assert!(wait < read && read < issued, "{body}");
        // The server admits clones on the thread that calls this, so nothing here may wait on
        // the filesystem: the fallback mapping is made on the warm-up's own thread.
        assert!(!body.contains("Mmap::map"), "{body}");
        // One pacer for every warm-up of the server, so a new one waits for the last window.
        assert!(
            !body.contains("Pacer::new("),
            "a warm-up makes a pacer of its own: {body}"
        );
        assert!(body.contains("Arc::clone(&self.pacer)"), "{body}");
        assert!(
            code.contains("Pacer::new(IN_FLIGHT_BYTES, resident)"),
            "the pacer's window is not IN_FLIGHT_BYTES"
        );
        // The probes read through a descriptor without readahead.
        let start = code.find("impl Probe {").expect("no Probe");
        let probe = &code[start..start + code[start..].find("\n}\n").unwrap()];
        assert!(probe.contains("POSIX_FADV_RANDOM"), "{probe}");
        // Residency comes from reading the page cache, not from mincore, which reports every
        // page of a file the process may not write as resident.
        let start = code.find("fn resident(").expect("no resident");
        let resident = &code[start..start + code[start..].find("\n}\n").unwrap()];
        assert!(resident.contains("libc::RWF_NOWAIT"), "{resident}");
        assert!(!resident.contains("mincore("), "{resident}");
        let squeezed: String = resident.split_whitespace().collect();
        let fallback = squeezed
            .find("Some(libc::EOPNOTSUPP)=>map")
            .expect("no fallback for a filesystem that refuses RWF_NOWAIT");
        let mapped = squeezed
            .find("mapped_resident(map,request)")
            .expect("the fallback does not ask the mapping");
        let other = squeezed.find("_=>true").expect("no default arm");
        assert!(
            fallback < mapped && mapped < other,
            "mincore is asked for anything but a filesystem that refuses RWF_NOWAIT: {resident}"
        );
    }

    /// The pacer never lets more than its window of requests be outstanding: with the
    /// device reading nothing, the next request waits; once the oldest is resident, it is
    /// retired and the next one goes out. Unpaced, the warm-up queued every request at once
    /// (#986, #1069).
    #[test]
    fn the_pacer_keeps_at_most_its_window_outstanding() {
        let resident_upto = std::sync::Arc::new(AtomicU64::new(0));
        let seen = std::sync::Arc::clone(&resident_upto);
        let mut pacer = Pacer::new(4 * REQUEST_BYTES, move |request: Request| {
            request.offset + request.len <= seen.load(Ordering::Relaxed)
        });
        let stop = AtomicBool::new(false);
        let request = |i: u64| Request {
            offset: i * REQUEST_BYTES,
            len: REQUEST_BYTES,
        };
        for i in 0..4 {
            assert!(pacer.wait_for_room(request(i), &stop));
            pacer.issued(request(i));
        }
        assert_eq!(pacer.bytes, 4 * REQUEST_BYTES);

        // Nothing is resident: the fifth request waits until the device reads the first. The
        // clock starts before the reader does, so the wait measured is at least its sleep.
        let started = Instant::now();
        let reader = {
            let resident_upto = std::sync::Arc::clone(&resident_upto);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(100));
                resident_upto.store(REQUEST_BYTES, Ordering::Relaxed);
            })
        };
        assert!(pacer.wait_for_room(request(4), &stop));
        assert!(
            started.elapsed() >= Duration::from_millis(90),
            "the fifth request went out while four were outstanding"
        );
        pacer.issued(request(4));
        assert_eq!(pacer.bytes, 4 * REQUEST_BYTES);
        reader.join().unwrap();

        // A stop ends the wait.
        stop.store(true, Ordering::Relaxed);
        assert!(!pacer.wait_for_room(request(5), &stop));
    }

    fn set_of(mem_len: u64, runs: &[(u64, u64)]) -> PageSet {
        let mut set = PageSet::empty(mem_len);
        for &(offset, len) in runs {
            set.insert_range(offset, len);
        }
        set
    }

    /// Past the free memory it started with, the warm-up reads only within
    /// PACED_WINDOW_BYTES of a running replay, and with no replay running it waits (#1066).
    #[test]
    fn past_its_free_memory_the_warm_up_stays_within_a_window_of_replay() {
        let window = PACED_WINDOW_BYTES;
        assert_eq!(pace(0, 10, None), Pace::Go, "inside the free memory");
        assert_eq!(
            pace(10, 10, None),
            Pace::Wait,
            "past it, with no replay running"
        );
        assert_eq!(pace(window, 0, Some(0)), Pace::Go);
        assert_eq!(pace(window + 1, 0, Some(0)), Pace::Wait);
        assert_eq!(pace(window + 1, 0, Some(1)), Pace::Go);
        assert_eq!(pace(u64::MAX, 0, Some(u64::MAX)), Pace::Go, "no overflow");

        let progress = ReplayProgress::default();
        assert_eq!(progress.position(), None);
        progress.begin();
        progress.passed(5);
        progress.begin();
        progress.passed(3);
        assert_eq!(progress.position(), Some(5), "the furthest running replay");
        progress.end();
        progress.end();
        assert_eq!(progress.position(), None);
        progress.begin();
        assert_eq!(
            progress.position(),
            Some(0),
            "a new replay starts at the beginning"
        );
    }

    /// A warm-up told to wait asks for nothing more until it may go on, and a stop while it
    /// waits ends it. Without pacing it requested the whole set at once.
    #[test]
    fn a_waiting_warm_up_asks_for_nothing_and_stops_when_told() {
        let mem_len = 64 * G;
        let set = set_of(mem_len, &[(0, G), (8 * G, G), (16 * G, G)]);
        let stop = Arc::new(AtomicBool::new(false));
        let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
        let worker = {
            let (stop, asked) = (Arc::clone(&stop), Arc::clone(&asked));
            std::thread::spawn(move || {
                warm_paced(
                    &set,
                    mem_len,
                    PAGE,
                    &stop,
                    // One request's worth may go; everything after waits.
                    &mut |requested| if requested < G { Pace::Go } else { Pace::Wait },
                    |request| {
                        asked.lock().unwrap().push(request.offset);
                        Ok(())
                    },
                )
            })
        };
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(*asked.lock().unwrap(), [0], "the second request waits");
        stop.store(true, Ordering::Relaxed);
        let stats = worker.join().unwrap();
        assert!(stats.stopped_early && stats.paced);
        assert_eq!(*asked.lock().unwrap(), [0]);
    }

    /// A warm-up that paced with one group of replays starts over when the next group
    /// begins: a clone admitted while it still held the one slot gets no warm-up of its
    /// own, and its replay copies from the start of the set (#1676). The new pass takes the
    /// set as it is then, which has grown, and the free memory as it is then, which is
    /// gone. Without the restart the warm-up went on from where it waited, and the low
    /// pages the second clone recorded were never asked for.
    #[test]
    fn a_warm_up_starts_over_with_the_current_set_and_free_memory() {
        use std::cell::{Cell, RefCell};
        const MIB: u64 = 1 << 20;
        const GIB: u64 = 1 << 30;
        let run = |group, bytes| ReplayState {
            active: 1,
            group,
            bytes,
        };
        let idle = |group| ReplayState {
            active: 0,
            group,
            bytes: 0,
        };

        // Started at a clone's admission, ahead of its replay: that replay (group 1) is the
        // one to pace with, and the next group restarts the warm-up.
        let mut pacing = Pacing::starting_at(idle(0));
        assert_eq!(pacing.pace(10, 0, idle(0)), Pace::Wait, "no replay yet");
        assert_eq!(pacing.pace(10, 0, run(1, 0)), Pace::Go);
        assert_eq!(pacing.pace(GIB + 1, 0, run(1, 0)), Pace::Wait);
        assert_eq!(
            pacing.pace(GIB + 1, 0, idle(1)),
            Pace::Wait,
            "its replay ended"
        );
        assert_eq!(pacing.pace(GIB + 1, 0, run(2, 0)), Pace::Restart);
        assert_eq!(pacing.pace(0, 0, run(2, 0)), Pace::Go, "once per group");
        // A replay that began and ended between two looks still counts.
        let mut pacing = Pacing::starting_at(idle(0));
        assert_eq!(pacing.pace(GIB + 1, 0, idle(1)), Pace::Wait);
        assert_eq!(pacing.pace(GIB + 1, 0, run(2, 0)), Pace::Restart);
        // Started while a group runs: that group is the one to pace with.
        let mut pacing = Pacing::starting_at(run(3, 0));
        assert_eq!(pacing.pace(10, 0, run(3, 0)), Pace::Go);
        assert_eq!(pacing.pace(10, 0, run(4, 0)), Pace::Restart);

        let progress = ReplayProgress::default();
        progress.begin();
        progress.passed(7);
        progress.end();
        progress.begin();
        assert_eq!(
            progress.state(),
            run(2, 0),
            "one copy: the new group at its start"
        );

        // Pass 1: a 2 GiB run at 4 GiB and a budget of 1.5 GiB, so the pass waits once it is
        // spent. Its replay ends and the next one begins. Pass 2: the set has grown a run at
        // 0, the budget is 0, and the second replay stays at its start.
        let mem_len = 8 * GIB;
        let first = set_of(mem_len, &[(4 * GIB, 2 * GIB)]);
        let second = set_of(mem_len, &[(0, 2 * MIB), (4 * GIB, 2 * GIB)]);
        let pass = Cell::new(0u32);
        let recorded = || {
            pass.set(pass.get() + 1);
            if pass.get() == 1 {
                first.clone()
            } else {
                second.clone()
            }
        };
        let mut budgets = [3 * GIB / 2, 0].into_iter();
        let asked: RefCell<Vec<(u32, u64, u64)>> = RefCell::new(Vec::new());
        let asked_in_pass = [Cell::new(0u64), Cell::new(0u64)];
        let stop = AtomicBool::new(false);
        let waits = Cell::new(0u64);
        let calls_in_second = Cell::new(0u64);
        let mut replay_at = || match pass.get() {
            // Before the first pass: the clone's replay has not begun.
            0 => idle(0),
            1 => {
                if asked_in_pass[0].get() * REQUEST_BYTES < 3 * GIB / 2 {
                    return run(1, 0);
                }
                // The first replay has ended; the next group begins after one wait. A
                // warm-up that never restarts waits on, and is stopped.
                waits.set(waits.get() + 1);
                if waits.get() > 50 {
                    stop.store(true, Ordering::Relaxed);
                }
                if waits.get() == 1 {
                    idle(1)
                } else {
                    run(2, 0)
                }
            }
            _ => {
                // A call more than the requests asked is a wait: the pass is paced.
                calls_in_second.set(calls_in_second.get() + 1);
                if calls_in_second.get() > asked_in_pass[1].get() + 1 {
                    stop.store(true, Ordering::Relaxed);
                }
                run(2, 0)
            }
        };
        let mut passes = Vec::new();
        warm_passes(
            &recorded,
            &mut || budgets.next().unwrap_or(0),
            &mut replay_at,
            mem_len,
            PAGE,
            &stop,
            &mut |request| {
                let n = pass.get();
                asked_in_pass[n as usize - 1].set(asked_in_pass[n as usize - 1].get() + 1);
                asked.borrow_mut().push((n, request.offset, request.len));
                Ok(())
            },
            &mut |number, stats, budget, _| passes.push((number, stats, budget)),
        );
        assert_eq!(passes.len(), 2, "one pass, then one after the restart");
        assert!(passes[0]
            .1
            .is_some_and(|stats| stats.restart && stats.paced));
        assert_eq!(
            passes[1].2, 0,
            "the second pass measured the free memory again"
        );
        let asked = asked.into_inner();
        let second_pass: Vec<_> = asked.iter().filter(|a| a.0 == 2).collect();
        assert_eq!(
            second_pass.first().map(|a| a.1),
            Some(0),
            "the second pass asked for the run the second clone recorded first"
        );
        let bytes: u64 = second_pass.iter().map(|a| a.2).sum();
        assert!(
            bytes <= PACED_WINDOW_BYTES + REQUEST_BYTES,
            "the second pass read {bytes} bytes unpaced past a replay at 0"
        );
    }

    /// With host pages larger than a granule, a page holding granules of several tiers is
    /// asked for once, so the warm-up's count of bytes asked stays a count of distinct pages.
    /// Before, each tier asked for the page again.
    #[test]
    fn a_large_host_page_is_asked_for_once_across_tiers() {
        const PAGE16: u64 = 16 * 1024;
        let mem_len = 64 * G;
        let mut set = PageSet::empty(mem_len);
        for (granule, time) in [(0u64, 1), (1, 15), (4, 30)] {
            set.insert_range_at(granule * G, G, time);
        }
        let mut seen = Vec::new();
        let stats = warm(&set, mem_len, PAGE16, &AtomicBool::new(false), |request| {
            seen.push((request.offset, request.len));
            Ok(())
        });
        assert_eq!(seen, vec![(0, PAGE16), (PAGE16, PAGE16)]);
        assert_eq!(stats.bytes, 2 * PAGE16);
    }

    /// The free memory a warm-up may fill unpaced is MemFree less a sixteenth of the host,
    /// and none when meminfo cannot be read.
    #[test]
    fn the_unpaced_budget_is_free_memory_less_a_reserve() {
        let meminfo = "MemTotal:       16777216 kB\nMemFree:         2097152 kB\n";
        assert_eq!(unpaced_budget(meminfo), (2 << 30) - (1 << 30));
        assert_eq!(unpaced_budget("MemTotal: 16 kB\nMemFree: 1 kB\n"), 0);
        assert_eq!(unpaced_budget(""), 0);
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
                paced: false,
                restart: false,
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
                paced: false,
                restart: false,
            }
        );

        // The check is per request, not per run: a stop that lands inside a run of four
        // requests ends it after the second.
        let long_len = 4 * MIB;
        let long = set_of(long_len, &[(MIB, 4 * REQUEST_BYTES)]);
        let stop = AtomicBool::new(false);
        let mut calls = 0;
        let stats = warm(&long, long_len, PAGE, &stop, |_| {
            calls += 1;
            if calls == 2 {
                stop.store(true, Ordering::Relaxed);
            }
            Ok(())
        });
        assert_eq!(
            stats,
            WarmStats {
                runs: 1,
                requests: 2,
                bytes: 2 * REQUEST_BYTES,
                failures: 0,
                stopped_early: true,
                paced: false,
                restart: false,
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
                paced: false,
                restart: false,
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
        let warmer = Warmer::new("test", Arc::new(tempfile::tempfile().unwrap()), mem_len);
        warmer.shared.unpaced.store(true, Ordering::Relaxed);
        warmer
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
        let started = warmer.warm_if_idle_with(
            "test",
            move || set.clone(),
            parking_advise(entered_tx, release_rx),
        );
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
            move || set.clone(),
            parking_advise(entered_tx, release_rx)
        ));
        entered_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the thread makes its first request");

        let dropping = Instant::now();
        drop(warmer);
        let dropped_after = dropping.elapsed();
        assert!(
            dropped_after < PARKED_FOR,
            "dropping the warmer took {dropped_after:?}, so it waited for a request that \
             stays parked for {PARKED_FOR:?}"
        );
        assert!(
            entered_rx.try_recv().is_err(),
            "the first request is still parked, so nothing else can have started"
        );
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

    /// The recorded set is read on the warm-up's thread, so copying a large guest's bitmap
    /// never holds up the caller, which is the accept loop when a clone is admitted.
    #[test]
    fn the_recorded_set_is_read_on_the_warm_up_thread() {
        let mem_len = 64 * G;
        let warmer = scratch_warmer(mem_len);
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();

        let begun = Instant::now();
        let started = warmer.warm_if_idle_with(
            "test",
            move || {
                entered_tx
                    .send(std::thread::current().name().map(str::to_string))
                    .unwrap();
                let _ = release_rx.recv_timeout(PARKED_FOR);
                PageSet::empty(mem_len)
            },
            |_| panic!("an empty set has no run to request"),
        );
        let returned_after = begun.elapsed();

        assert!(started);
        let thread = entered_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the recorded set is read");
        assert_eq!(thread.as_deref(), Some("fcvm-ws-warm"));
        assert!(
            returned_after < PARKED_FOR,
            "starting a warm-up took {returned_after:?}, so the caller waited for the \
             recorded set, which stays parked for {PARKED_FOR:?}"
        );

        drop(release_tx);
        wait_until_idle(&warmer);
    }

    /// Every warm-up logs one line, and a warm-up with nothing recorded says so.
    #[test]
    fn every_warm_up_logs_one_line_whether_or_not_anything_was_recorded() {
        let mem_len = 64 * G;
        let logged = |recorded: PageSet| {
            let captured = Captured::default();
            let subscriber = tracing_subscriber::fmt()
                .with_writer(captured.clone())
                .with_ansi(false)
                .finish();
            tracing::subscriber::with_default(subscriber, || {
                warm_up_once(
                    "snap",
                    "clone admission",
                    &move || recorded.clone(),
                    mem_len,
                    &AtomicBool::new(false),
                    None,
                    |_| Ok(()),
                );
            });
            let log = captured.0.lock().unwrap().clone();
            String::from_utf8(log).unwrap()
        };

        let warmed = logged(set_of(mem_len, &[(G, G), (5 * G, G)]));
        assert_eq!(warmed.lines().count(), 1, "{warmed}");
        for part in [
            "asked the kernel to read the recorded working set into the page cache",
            "snapshot=snap",
            "trigger=\"clone admission\"",
            "runs=2",
            "pages=2",
            "requests=2",
            "failures=0",
            "stopped_early=false",
        ] {
            assert!(warmed.contains(part), "{part} is missing from: {warmed}");
        }

        let empty = logged(PageSet::empty(mem_len));
        assert_eq!(empty.lines().count(), 1, "{empty}");
        for part in [
            "no working set is recorded yet",
            "snapshot=snap",
            "trigger=\"clone admission\"",
        ] {
            assert!(empty.contains(part), "{part} is missing from: {empty}");
        }
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
            move || set.clone(),
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
        let third = warmer.warm_if_idle_with("clone admission", move || set.clone(), {
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
            move || set.clone(),
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

    /// A directory that gives its pages up only at a second attempt is refused too. That is
    /// the container CI lane's `/tmp`: its overlay is mounted `volatile`, so fsync cleans
    /// nothing, and the first `POSIX_FADV_DONTNEED` only starts the writeback that lets a
    /// later one succeed. A test that retried there would race that writeback.
    #[test]
    fn a_directory_that_evicts_only_at_a_second_attempt_is_refused() {
        let dir = disk_dir();
        let attempts = AtomicU64::new(0);

        let refused = probe_eviction(&dir, |file| {
            if attempts.fetch_add(1, Ordering::Relaxed) > 0 {
                evict(file);
            }
        })
        .expect_err("the first attempt evicted nothing");

        assert!(refused.contains("pages still resident"), "{refused}");
        assert_eq!(attempts.load(Ordering::Relaxed), 1, "the probe asks once");
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
                paced: false,
                restart: false,
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
        let warmer = Warmer::new("test", Arc::new(image), len);
        warmer.shared.unpaced.store(true, Ordering::Relaxed);

        let set = set_of(len, &recorded);
        assert!(warmer.warm_if_idle("serve start", move || set.clone()));

        assert_eq!(wait_until_resident(&mmap, &recorded), Ok(()));
    }
}

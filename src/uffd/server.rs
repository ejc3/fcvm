use anyhow::{anyhow, Context, Result};
use std::fs::File;
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::unix::AsyncFd;
use tokio::net::UnixListener;
use tokio::task::JoinSet;
use tracing::{debug, error, info, warn};

use memmap2::MmapOptions;
use userfaultfd::{Event, FaultKind, Uffd};
use vmm_sys_util::sock_ctrl_msg::ScmSocket;

use crate::paths;
use crate::uffd::working_set::{PageSet, Run, WorkingSetStore};

/// 2MiB — the only huge page size fcvm supports for guest memory.
const HUGE_PAGE_2M: usize = 2 * 1024 * 1024;

/// How the server materialises snapshot pages into a clone's guest memory.
///
/// Both modes serve the same snapshot; they differ in whether the physical page a clone
/// reads is *shared* with the other clones.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UffdBacking {
    /// MISSING faults on anonymous guest memory, resolved with `UFFDIO_COPY` out of a
    /// mapping of the snapshot memory file.
    ///
    /// Lazy, but every clone ends up with its own private copy of every page it touches:
    /// N clones that touch the same page cost N physical pages.
    Copy,
    /// MINOR faults on a `MAP_PRIVATE` mapping of a shared memfd, resolved with
    /// `UFFDIO_CONTINUE`.
    ///
    /// The server holds the snapshot's guest memory in one memfd and hands that fd to every
    /// clone's Firecracker over the handshake socket. Because the clone maps it
    /// `MAP_PRIVATE`, `UFFDIO_CONTINUE` installs a **read-only** PTE pointing at the shared
    /// page-cache folio (`mm/userfaultfd.c`: `if (page_in_cache && !vm_shared) writable =
    /// false`), so reads across all clones hit ONE physical copy and the first guest write
    /// takes an ordinary copy-on-write fault into private memory. The snapshot stays
    /// pristine and reusable.
    ///
    /// `hugepages` selects `MFD_HUGETLB|MFD_HUGE_2MB` for 2MiB-backed guests.
    Minor { hugepages: bool },
}

impl UffdBacking {
    /// Parse the `FCVM_UFFD_MODE` env var / `--uffd-mode` flag value.
    pub fn parse_mode(value: &str, hugepages: bool) -> Result<Self> {
        match value {
            "copy" => Ok(Self::Copy),
            "minor" => Ok(Self::Minor { hugepages }),
            other => anyhow::bail!("invalid UFFD mode {other:?} (expected \"copy\" or \"minor\")"),
        }
    }

    /// Resolve the backing mode from `FCVM_UFFD_MODE`, defaulting to [`UffdBacking::Copy`].
    pub fn from_env(hugepages: bool) -> Result<Self> {
        match std::env::var("FCVM_UFFD_MODE") {
            Ok(v) => Self::parse_mode(&v, hugepages),
            Err(_) => Ok(Self::Copy),
        }
    }

    /// The Firecracker `mem_backend.backend_type` string this mode requires.
    pub fn firecracker_backend_type(&self) -> &'static str {
        match self {
            Self::Copy => "Uffd",
            Self::Minor { .. } => "UffdMinor",
        }
    }

    /// Short name used in state files and logs.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Copy => "copy",
            Self::Minor { .. } => "minor",
        }
    }
}

/// Where a clone's pages come from — the per-mode server state, shared by all clones.
enum PageSource {
    /// Read-only mapping of the snapshot memory file; pages are `UFFDIO_COPY`'d out of it.
    Copy { mmap: memmap2::Mmap },
    /// A memfd holding the whole snapshot image. Handed to each Firecracker over
    /// `SCM_RIGHTS`; faults are resolved in place with `UFFDIO_CONTINUE`.
    Minor { backing: File, len: usize },
}

impl PageSource {
    /// Bytes of snapshot image this source can actually serve. Offsets at or past it have
    /// no snapshot content behind them.
    fn usable_len(&self) -> usize {
        match self {
            Self::Copy { mmap } => mmap.len(),
            Self::Minor { len, .. } => *len,
        }
    }
}

/// Counters for one server, shared by every clone it serves.
///
/// `faults` is the number that shows whether working-set replay worked: a restore that
/// replays its recorded set should fault a small fraction of what the recording restore did.
#[derive(Debug, Default)]
pub struct UffdStats {
    faults: AtomicU64,
    prefetched_bytes: AtomicU64,
    prefetch_micros: AtomicU64,
}

impl UffdStats {
    /// Page faults served on demand across all clones.
    pub fn faults(&self) -> u64 {
        self.faults.load(Ordering::Relaxed)
    }

    /// Guest memory bulk-populated from recorded working sets across all clones.
    ///
    /// Bytes, not pages: clones of one snapshot can be restored at different page sizes
    /// (`--hugepages`), so a page count would not be summable across them.
    pub fn prefetched_bytes(&self) -> u64 {
        self.prefetched_bytes.load(Ordering::Relaxed)
    }

    /// Wall time spent bulk-populating, summed over clones.
    pub fn prefetch_micros(&self) -> u64 {
        self.prefetch_micros.load(Ordering::Relaxed)
    }
}

/// Async UFFD server that serves memory pages for multiple VMs from a single snapshot
pub struct UffdServer {
    snapshot_id: String,
    socket_path: PathBuf,
    source: Arc<PageSource>,
    backing: UffdBacking,
    /// Records the working set of each clone and replays it into the next one. `None` when
    /// `FCVM_UFFD_WORKING_SET=off`, or when the memory image cannot carry one.
    working_set: Option<Arc<WorkingSetStore>>,
    stats: Arc<UffdStats>,
}

/// Whether to record and replay restore working sets.
///
/// `FCVM_UFFD_WORKING_SET=off` turns both halves off, which is how a before/after latency
/// measurement gets its control arm. An unrecognised value is an error: a typo must not
/// silently leave the feature in the opposite state from the one the operator intended.
fn working_set_enabled() -> Result<bool> {
    match std::env::var("FCVM_UFFD_WORKING_SET") {
        Ok(v) if v == "off" => Ok(false),
        Ok(v) if v == "on" => Ok(true),
        Ok(v) => anyhow::bail!("invalid FCVM_UFFD_WORKING_SET={v:?} (expected \"on\" or \"off\")"),
        Err(_) => Ok(true),
    }
}

impl UffdServer {
    /// Create a new UFFD server for a snapshot
    pub async fn new(
        snapshot_id: String,
        mem_file_path: &Path,
        backing: UffdBacking,
    ) -> Result<Self> {
        let socket_path = paths::data_dir().join(format!("uffd-{}.sock", snapshot_id));
        Self::new_with_path(snapshot_id, mem_file_path, &socket_path, backing).await
    }

    /// Create a new UFFD server with custom socket path
    pub async fn new_with_path(
        snapshot_id: String,
        mem_file_path: &Path,
        socket_path: &Path,
        backing: UffdBacking,
    ) -> Result<Self> {
        info!(
            target: "uffd",
            snapshot = %snapshot_id,
            mem_file = %mem_file_path.display(),
            socket = %socket_path.display(),
            mode = backing.name(),
            "creating UFFD server"
        );

        // Open the memory snapshot file (shared across all VMs)
        let mem_file = File::open(mem_file_path).context("opening memory file")?;
        let mem_size = mem_file.metadata()?.len() as usize;

        info!(
            target: "uffd",
            mem_size_mb = mem_size / (1024 * 1024),
            mode = backing.name(),
            "preparing snapshot page source"
        );

        let source = match backing {
            UffdBacking::Copy => {
                // Safety: We're mapping a read-only file for serving pages
                let mmap = unsafe {
                    MmapOptions::new()
                        .len(mem_size)
                        .map(&mem_file)
                        .context("mmapping memory file")?
                };
                PageSource::Copy { mmap }
            }
            UffdBacking::Minor { hugepages } => {
                let backing_file = tokio::task::spawn_blocking({
                    let id = snapshot_id.clone();
                    move || create_backing_memfd(&id, mem_file, mem_size, hugepages)
                })
                .await
                .context("joining memfd population task")??;
                PageSource::Minor {
                    backing: backing_file,
                    len: mem_size,
                }
            }
        };

        // Record what clones fault, and replay it into later clones. The store is a cache:
        // if it cannot be opened, clones simply fault on demand, so a failure here is a
        // warning and not the end of the restore. (The switch itself is validated first —
        // a typo in it IS fatal.)
        let working_set = if working_set_enabled()? {
            match WorkingSetStore::open(mem_file_path, mem_size as u64) {
                Ok(store) => Some(Arc::new(store)),
                Err(e) => {
                    warn!(
                        target: "uffd",
                        error = ?e,
                        "working-set record/replay unavailable - clones will fault on demand"
                    );
                    None
                }
            }
        } else {
            info!(target: "uffd", "FCVM_UFFD_WORKING_SET=off - no working-set record or replay");
            None
        };

        // Ensure parent directory exists
        if let Some(parent) = socket_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .context("creating socket directory")?;
        }

        // Remove stale socket (ignore errors if not exists - avoids TOCTOU race)
        let _ = tokio::fs::remove_file(&socket_path).await;

        Ok(Self {
            snapshot_id,
            socket_path: socket_path.to_path_buf(),
            source: Arc::new(source),
            backing,
            working_set,
            stats: Arc::new(UffdStats::default()),
        })
    }

    /// Get the socket path for this server
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// The page-materialisation mode this server was created with.
    pub fn backing(&self) -> UffdBacking {
        self.backing
    }

    /// Live counters for this server (faults served, pages prefetched).
    pub fn stats(&self) -> Arc<UffdStats> {
        Arc::clone(&self.stats)
    }

    /// Run the UFFD server (blocks until cancelled via CancellationToken)
    pub async fn run(&self, cancel: tokio_util::sync::CancellationToken) -> Result<()> {
        info!(
            target: "uffd",
            snapshot = %self.snapshot_id,
            socket = %self.socket_path.display(),
            "starting UFFD server"
        );

        // Bind Unix socket
        let listener = UnixListener::bind(&self.socket_path).context("binding Unix socket")?;

        info!(target: "uffd", "UFFD server listening, waiting for VM connections...");

        let mut vm_tasks: JoinSet<String> = JoinSet::new();
        let mut next_vm_id = 0u64;

        loop {
            tokio::select! {
                // Accept new VM connections
                accept_result = listener.accept() => {
                    match accept_result {
                        Ok((stream, _)) => {
                            let vm_id = format!("vm-{}", next_vm_id);
                            next_vm_id += 1;
                            let source = Arc::clone(&self.source);
                            let working_set = self.working_set.clone();
                            let stats = Arc::clone(&self.stats);

                            info!(target: "uffd", vm_id = %vm_id, "new VM connection");

                            // Spawn per-connection task so the accept loop returns
                            // immediately — no blocking on slow/misbehaving clones.
                            vm_tasks.spawn(async move {
                                match tokio::time::timeout(
                                    Duration::from_secs(30),
                                    handshake(stream, &source),
                                ).await {
                                    Ok(Ok((uffd, mappings))) => {
                                        info!(
                                            target: "uffd",
                                            vm_id = %vm_id,
                                            regions = mappings.len(),
                                            "received UFFD with {} memory regions",
                                            mappings.len()
                                        );
                                        match handle_vm_page_faults(vm_id.clone(), uffd, mappings, source, working_set, stats).await {
                                            Ok(()) => info!(target: "uffd", vm_id = %vm_id, "VM handler exited cleanly"),
                                            Err(e) => error!(
                                                target: "uffd",
                                                vm_id = %vm_id,
                                                error = ?e,
                                                "VM page-fault handler died; its userfaultfd is now closed and the \
                                                 still-running VM will receive zero pages for unfaulted memory \
                                                 (silent corruption) — kill the affected clone"
                                            ),
                                        }
                                    }
                                    Ok(Err(e)) => {
                                        error!(target: "uffd", vm_id = %vm_id, error = ?e, "handshake failed");
                                    }
                                    Err(_) => {
                                        error!(target: "uffd", vm_id = %vm_id, "handshake timed out after 30s");
                                    }
                                }
                                vm_id
                            });

                            info!(target: "uffd", active_vms = vm_tasks.len(), "VM connection spawned");
                        }
                        Err(e) => {
                            error!(target: "uffd", error = %e, "failed to accept connection");
                        }
                    }
                }

                // Handle completed VM tasks
                Some(result) = vm_tasks.join_next() => {
                    match result {
                        Ok(vm_id) => info!(target: "uffd", vm_id = %vm_id, "VM disconnected"),
                        Err(e) => error!(target: "uffd", error = %e, "VM task panicked"),
                    }

                    info!(target: "uffd", active_vms = vm_tasks.len(), "VM exited");
                }

                // Shut down when cancellation token is triggered (Ctrl-C / SIGTERM)
                _ = cancel.cancelled() => {
                    info!(target: "uffd", "cancellation requested, shutting down server");
                    break;
                }
            }
        }

        // Stop accepting new connections, but keep serving page faults for VMs that are
        // already connected until each one closes its uffd (i.e. its Firecracker exits).
        // Aborting the handlers here would close the uffds while those VMs are still
        // running, and the kernel would then resolve their page faults with zero pages
        // instead of snapshot contents — silent guest memory corruption.
        drop(listener);
        if !vm_tasks.is_empty() {
            info!(
                target: "uffd",
                active_vms = vm_tasks.len(),
                "draining VM handlers before shutdown"
            );
        }
        while let Some(result) = vm_tasks.join_next().await {
            match result {
                Ok(vm_id) => info!(target: "uffd", vm_id = %vm_id, "VM disconnected"),
                Err(e) => error!(target: "uffd", error = %e, "VM task panicked"),
            }
        }

        info!(target: "uffd", "UFFD server stopped");
        Ok(())
    }
}

impl Drop for UffdServer {
    fn drop(&mut self) {
        // Clean up socket file (ignore errors - avoids TOCTOU race and handles concurrent cleanup)
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

/// Whether a UFFDIO_ZEROPAGE error means the range (or part of it) is already populated.
///
/// Remove (balloon) events and page faults are not ordered, so a fault served with UFFDIO_COPY
/// before the Remove event is processed leaves pages present in the removed range. The kernel
/// then returns EEXIST (first page already present) or EAGAIN (range partially zeroed before
/// hitting a present page) from UFFDIO_ZEROPAGE. Neither is fatal — the pages are populated.
fn zeropage_hit_present_page(e: &userfaultfd::Error) -> bool {
    matches!(
        e,
        userfaultfd::Error::ZeropageFailed(errno)
            if (*errno as i32) == libc::EEXIST || (*errno as i32) == libc::EAGAIN
    )
}

/// Whether a UFFDIO_CONTINUE error means the page is already mapped in the clone.
///
/// Two threads in the guest can fault on the same page before the first `UFFDIO_CONTINUE`
/// lands; the kernel reports `EEXIST` for the loser. Nothing is wrong — the page is present.
fn continue_hit_present_page(e: &userfaultfd::Error) -> bool {
    // Compare the raw errno: the `userfaultfd` crate pins its own `nix` version, which is
    // not necessarily the one fcvm depends on.
    matches!(
        e,
        userfaultfd::Error::SystemError(errno) if (*errno as i32) == libc::EEXIST
    )
}

/// Whether a UFFDIO_CONTINUE error is the kernel saying "not now, try again".
///
/// The ioctl fails with `EAGAIN` (and zero progress) while the context's `mmap_changing`
/// flag is raised — an event-generating operation (fork, mremap, madvise) is mid-flight
/// and its event is sitting in the uffd queue waiting to be READ. The fault is NOT
/// resolved and the faulting vCPU stays asleep until a successful CONTINUE wakes it, so
/// the caller must drain the event queue (which is what lets the blocked operation finish
/// and clear `mmap_changing`) and retry. Dropping the fault = a permanently hung vCPU.
fn continue_would_block(e: &userfaultfd::Error) -> bool {
    matches!(
        e,
        userfaultfd::Error::SystemError(errno) if (*errno as i32) == libc::EAGAIN
    )
}

/// Whether a UFFDIO_CONTINUE error means the clone's mm is gone (process exited).
fn continue_vm_gone(e: &userfaultfd::Error) -> bool {
    matches!(
        e,
        userfaultfd::Error::SystemError(errno) if (*errno as i32) == libc::ESRCH
    )
}

/// How one attempt at resolving a MINOR fault ended.
enum ContinueOutcome {
    /// The whole granule is mapped in the clone (by us, or by a racing fault — EEXIST).
    Resolved,
    /// `EAGAIN` with no progress: `mmap_changing` is set. The fault is still pending and
    /// MUST be retried after the event queue has been drained.
    Retry,
    /// `ESRCH`: the clone exited while we were resolving. Nothing left to serve.
    VmGone,
}

/// Resolve one faulting granule (`page_size` bytes: 4 KiB shmem / 2 MiB hugetlb) with
/// `UFFDIO_CONTINUE`, handling every outcome the kernel documents:
///
/// * `Ok(mapped)` — the kernel maps from the start of the range and reports the bytes it
///   installed. A short count means it stopped early (the ioctl signals this as `EAGAIN`
///   after partial progress); advance past the mapped bytes and continue the remainder
///   instead of discarding the count.
/// * `EEXIST` — the granule at the current position is already mapped (a racing guest
///   thread won). That sub-range is DONE; skip it, it is not an error.
/// * `EAGAIN` with zero progress — `mmap_changing` is set; report [`ContinueOutcome::Retry`]
///   so the caller drains the event queue and retries. Never drop the fault.
/// * `ESRCH` — the clone died; report [`ContinueOutcome::VmGone`].
/// * anything else — a real error; propagate loudly (a dropped fault is a hung vCPU).
fn continue_page(
    uffd: &Uffd,
    vm_id: &str,
    page: usize,
    page_size: usize,
) -> Result<ContinueOutcome> {
    let mut done = 0usize;
    while done < page_size {
        let addr = (page + done) as *mut std::ffi::c_void;
        let remaining = page_size - done;
        match uffd.r#continue(addr, remaining, true) {
            Ok(mapped) => {
                let mapped = usize::try_from(mapped).unwrap_or(0);
                // The kernel never reports zero-byte success; guard against a spin.
                anyhow::ensure!(
                    mapped > 0,
                    "UFFDIO_CONTINUE reported zero mapped bytes at 0x{:x}",
                    page + done
                );
                done += mapped;
            }
            Err(e) if continue_hit_present_page(&e) => {
                debug!(
                    target: "uffd",
                    vm_id = %vm_id,
                    fault_addr = format!("0x{:x}", page + done),
                    "UFFDIO_CONTINUE skipped - page already mapped (EEXIST)"
                );
                // EEXIST refers to the granule at the current position; with per-granule
                // requests that is the whole remaining range.
                done += remaining;
            }
            Err(e) if continue_would_block(&e) => return Ok(ContinueOutcome::Retry),
            Err(e) if continue_vm_gone(&e) => return Ok(ContinueOutcome::VmGone),
            Err(e) => {
                error!(
                    target: "uffd",
                    vm_id = %vm_id,
                    fault_addr = format!("0x{:x}", page + done),
                    error = ?e,
                    "UFFDIO_CONTINUE failed"
                );
                return Err(e.into());
            }
        }
    }
    Ok(ContinueOutcome::Resolved)
}

/// One slice of a recorded run that lands inside a single guest memory region.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PrefetchChunk {
    /// Offset into the snapshot memory image.
    file_offset: usize,
    /// Host virtual address in the clone's guest memory.
    host_addr: usize,
    len: usize,
}

/// Cut the recorded runs down to what THIS clone actually mapped, at ITS page granularity.
///
/// A recorded set is in memory-image offsets; the host addresses are per clone, so every run
/// is intersected with the regions Firecracker just sent. Anything outside every region, or
/// past the end of the servable image (`usable_len`), is dropped rather than clamped to
/// something arbitrary: those pages just fault on demand.
///
/// The result is then trimmed to whole `page_size` granules. Sets are recorded at 4 KiB
/// granularity so one set serves clones of any page size, but a partial huge page cannot be
/// installed — `--hugepages` on a clone of a 4 KiB-recorded snapshot would otherwise make
/// every ioctl fail `EINVAL`. Trimming replays the fully covered huge pages and leaves the
/// edges to fault.
///
/// Runs and regions are both ordered and disjoint, so the result is ordered too — which is
/// what makes the copies sequential in the source file.
fn plan_chunks(
    runs: impl Iterator<Item = Run>,
    mappings: &[GuestRegionUffdMapping],
    usable_len: u64,
    page_size: u64,
) -> Vec<PrefetchChunk> {
    let mut chunks = Vec::new();
    for run in runs {
        let run_end = run.offset.saturating_add(run.len);
        for mapping in mappings {
            let region_end = mapping.offset.saturating_add(mapping.size as u64);
            let start = run.offset.max(mapping.offset);
            let end = run_end.min(region_end).min(usable_len);
            if start >= end {
                continue;
            }
            // Round in to whole granules of THIS clone's page size.
            let start = start.next_multiple_of(page_size);
            let end = end - (end % page_size);
            if start >= end {
                continue;
            }
            let Some(host_addr) = mapping
                .base_host_virt_addr
                .checked_add(start - mapping.offset)
                .and_then(|a| usize::try_from(a).ok())
            else {
                continue;
            };
            let (Ok(file_offset), Ok(len)) = (usize::try_from(start), usize::try_from(end - start))
            else {
                continue;
            };
            chunks.push(PrefetchChunk {
                file_offset,
                host_addr,
                len,
            });
        }
    }
    chunks
}

/// Upper bound on the bytes one replay ioctl moves.
///
/// The point of replay is one ioctl per run instead of one per page, but a run can be
/// hundreds of megabytes and the handler cannot drain a demand fault while it is inside the
/// ioctl. 2 MiB caps that stall at ~400 us of `memcpy` while still amortising the ioctl over
/// 512 4 KiB pages, and it is exactly one huge page, so a hugepage clone never gets a chunk
/// that splits a granule.
const REPLAY_CHUNK_BYTES: usize = 2 * 1024 * 1024;

/// How populating one chunk of a recorded working set ended.
enum PopulateOutcome {
    /// Bytes installed in the clone (short of the request when pages were already present).
    Populated(usize),
    /// The clone exited (`ESRCH`); stop prefetching, there is nobody left to serve.
    VmGone,
}

/// Bulk-populate one contiguous chunk — one ioctl for the whole run where the kernel allows
/// it, which is the entire point: 56k single-page ioctls become a few hundred bulk ones.
///
/// Prefetching is best effort by construction: nothing is blocked on these pages, so every
/// way the kernel can decline (a page already present, `mmap_changing`, a short count, an
/// unexpected errno) just ends this chunk and leaves the rest to fault on demand. Each
/// iteration advances by at least one granule, so the loop cannot spin.
fn populate_chunk(
    uffd: &Uffd,
    source: &PageSource,
    chunk: &PrefetchChunk,
    page_size: usize,
) -> PopulateOutcome {
    let mut done = 0usize;
    while done < chunk.len {
        // Never ask for more than REPLAY_CHUNK_BYTES at once, but never ask for less than one
        // page either — a sub-page request is not addressable.
        let remaining = (chunk.len - done).min(REPLAY_CHUNK_BYTES.max(page_size));
        let dst = (chunk.host_addr + done) as *mut std::ffi::c_void;
        let step = match source {
            PageSource::Copy { mmap } => {
                let src_start = chunk.file_offset + done;
                // `plan_chunks` clamped the chunk to `source.usable_len()`, so this range is
                // inside the mapping.
                let src = &mmap[src_start..src_start + remaining];
                // SAFETY: `dst` is inside the clone's uffd-registered guest memory, which
                // only the kernel writes through this ioctl; `src` is a live borrow of the
                // read-only snapshot mapping for the whole call.
                match unsafe { uffd.copy(src.as_ptr().cast(), dst, remaining, true) } {
                    Ok(n) if n > 0 && n <= remaining => n,
                    // EEXIST: the granule at `dst` is already present (a guest thread beat
                    // us to it). Skip it and carry on with the rest of the chunk.
                    Err(userfaultfd::Error::CopyFailed(errno))
                        if (errno as i32) == libc::EEXIST =>
                    {
                        page_size
                    }
                    Err(userfaultfd::Error::CopyFailed(errno)) if (errno as i32) == libc::ESRCH => {
                        return PopulateOutcome::VmGone;
                    }
                    // A partial copy reports how far it got. Anything outside 1..=remaining
                    // is not progress — notably a zero-progress EAGAIN (`mmap_changing`),
                    // which the crate reports as a nonsense byte count — so stop.
                    Err(userfaultfd::Error::PartiallyCopied(n)) if n > 0 && n <= remaining => n,
                    Ok(_) | Err(_) => break,
                }
            }
            // MINOR: the folio is already in the shared memfd's page cache; CONTINUE just
            // installs the PTE. No source pointer, no copy.
            // SAFETY: `dst` is inside the clone's uffd-registered guest memory.
            PageSource::Minor { .. } => match uffd.r#continue(dst, remaining, true) {
                Ok(n) => match usize::try_from(n) {
                    Ok(n) if n > 0 && n <= remaining => n,
                    _ => break,
                },
                Err(e) if continue_hit_present_page(&e) => page_size,
                Err(e) if continue_vm_gone(&e) => return PopulateOutcome::VmGone,
                // EAGAIN here means `mmap_changing`. A real fault would be parked and
                // retried because a vCPU is asleep on it; a prefetch has nobody waiting.
                Err(_) => break,
            },
        };
        done += step;
    }
    PopulateOutcome::Populated(done)
}

/// Replay a snapshot's recorded working set into a freshly restored clone.
///
/// Runs right after the handshake, which is the earliest moment the uffd exists and is
/// before the guest can run: Firecracker is still loading the vmstate, and fcvm only sends
/// `Resumed` once that returns. Faults raised while this is in flight are not lost — they
/// sit in the uffd queue and are drained by the handler immediately afterwards — and every
/// page this installs first is a fault that never happens.
///
/// Returns the bytes actually installed.
fn prefetch_working_set(
    uffd: &Uffd,
    vm_id: &str,
    mappings: &[GuestRegionUffdMapping],
    source: &PageSource,
    set: &PageSet,
    page_size: usize,
) -> u64 {
    let chunks = plan_chunks(
        set.runs(),
        mappings,
        source.usable_len() as u64,
        page_size as u64,
    );
    let mut bytes = 0usize;
    let mut declined = 0usize;
    for chunk in &chunks {
        match populate_chunk(uffd, source, chunk, page_size) {
            PopulateOutcome::Populated(n) => {
                bytes += n;
                declined += chunk.len - n;
            }
            PopulateOutcome::VmGone => {
                info!(target: "uffd", vm_id = %vm_id, "clone exited during working-set replay");
                break;
            }
        }
    }
    if declined > 0 {
        debug!(
            target: "uffd",
            vm_id = %vm_id,
            declined_bytes = declined,
            "some recorded pages were not populated - they will fault on demand"
        );
    }
    bytes as u64
}

/// The single per-fault observation point for one clone's handler.
///
/// One `record()` call per resolved fault feeds both consumers, so the hot loop never grows
/// a second instrumentation path:
///
/// * the working set this snapshot replays into the NEXT clone, and
/// * the per-fault trace (`FCVM_UFFD_FAULT_TRACE=<dir>`) the benchmark tooling reads —
///   `<dir>/<serve_pid>-<vm_id>.faults`, little-endian `(image_offset, ns_before, ns_after)`
///   u64 triples relative to handler start.
///
/// Publishing happens in [`Drop`] so that EVERY handler exit contributes: clean VM exit,
/// `VmGone` mid-CONTINUE, or an error return.
struct FaultRecorder {
    store: Option<Arc<WorkingSetStore>>,
    observed: PageSet,
    vm_id: String,
    trace_path: Option<PathBuf>,
    trace: Vec<[u64; 3]>,
    origin: std::time::Instant,
}

impl FaultRecorder {
    fn new(store: Option<Arc<WorkingSetStore>>, vm_id: &str, origin: std::time::Instant) -> Self {
        let trace_path = std::env::var("FCVM_UFFD_FAULT_TRACE").ok().and_then(|dir| {
            let dir = PathBuf::from(dir);
            match std::fs::create_dir_all(&dir) {
                Ok(()) => Some(dir.join(format!("{}-{}.faults", std::process::id(), vm_id))),
                Err(e) => {
                    warn!(
                        target: "uffd",
                        dir = %dir.display(),
                        error = %e,
                        "FCVM_UFFD_FAULT_TRACE directory not usable - fault tracing disabled"
                    );
                    None
                }
            }
        });
        let observed = match store.as_ref() {
            Some(s) => s.recorder(),
            None => PageSet::empty(0),
        };
        Self {
            store,
            observed,
            vm_id: vm_id.to_string(),
            trace_path,
            trace: Vec::new(),
            origin,
        }
    }

    /// True when the trace half wants timestamps, so the handler can skip two clock reads
    /// per fault when nobody is measuring.
    #[inline]
    fn wants_timing(&self) -> bool {
        self.trace_path.is_some()
    }

    #[inline]
    fn now_ns(&self) -> u64 {
        self.origin.elapsed().as_nanos() as u64
    }

    /// Record one resolved fault covering `[image_offset, image_offset + len)`.
    #[inline]
    fn record(&mut self, image_offset: u64, len: u64, before_ns: u64, after_ns: u64) {
        if self.store.is_some() {
            self.observed.insert_range(image_offset, len);
        }
        if self.trace_path.is_some() {
            self.trace.push([image_offset, before_ns, after_ns]);
        }
    }
}

impl Drop for FaultRecorder {
    fn drop(&mut self) {
        if let Some(path) = self.trace_path.take() {
            let mut buf = Vec::with_capacity(self.trace.len() * 24);
            for rec in &self.trace {
                for v in rec {
                    buf.extend_from_slice(&v.to_le_bytes());
                }
            }
            match std::fs::write(&path, &buf) {
                Ok(()) => info!(
                    target: "uffd",
                    path = %path.display(),
                    faults = self.trace.len(),
                    "wrote UFFD fault trace"
                ),
                Err(e) => warn!(
                    target: "uffd",
                    path = %path.display(),
                    error = %e,
                    "failed to write UFFD fault trace"
                ),
            }
        }

        let Some(store) = self.store.take() else {
            return;
        };
        if self.observed.is_empty() {
            return;
        }
        match store.merge_and_persist(&self.observed) {
            Ok(outcome) => info!(
                target: "uffd",
                vm_id = %self.vm_id,
                observed = self.observed.len(),
                added = outcome.added,
                total = outcome.total,
                persisted = outcome.persisted,
                "merged this clone's faults into the snapshot's working set"
            ),
            Err(e) => warn!(
                target: "uffd",
                vm_id = %self.vm_id,
                error = ?e,
                "could not persist the working set - the next restore faults on demand"
            ),
        }
    }
}

/// Handle page faults for a single VM
async fn handle_vm_page_faults(
    vm_id: String,
    uffd: Uffd,
    mappings: Vec<GuestRegionUffdMapping>,
    source: Arc<PageSource>,
    working_set: Option<Arc<WorkingSetStore>>,
    stats: Arc<UffdStats>,
) -> Result<()> {
    // Derive page size from mappings (all regions use the same page size)
    let page_size = mappings.first().map(|m| m.page_size).unwrap_or(4096);
    let page_mask = !(page_size - 1);

    info!(
        target: "uffd",
        vm_id = %vm_id,
        page_size,
        "page fault handler started"
    );

    let start_time = std::time::Instant::now();

    // Replay this snapshot's recorded working set before the guest asks for it. On a
    // blocking thread: this is a multi-hundred-megabyte copy, and stalling the reactor
    // would stall every other clone's handler with it.
    let recorded = working_set
        .as_ref()
        .map(|s| s.to_prefetch())
        .filter(|s| !s.is_empty());
    let uffd = match recorded {
        Some(set) => {
            let source = Arc::clone(&source);
            let vm = vm_id.clone();
            let regions = mappings.clone();
            let started = std::time::Instant::now();
            let (uffd, bytes) = tokio::task::spawn_blocking(move || {
                let bytes = prefetch_working_set(&uffd, &vm, &regions, &source, &set, page_size);
                (uffd, bytes)
            })
            .await
            .context("joining working-set replay task")?;
            let elapsed = started.elapsed();
            stats.prefetched_bytes.fetch_add(bytes, Ordering::Relaxed);
            stats
                .prefetch_micros
                .fetch_add(elapsed.as_micros() as u64, Ordering::Relaxed);
            info!(
                target: "uffd",
                vm_id = %vm_id,
                pages = bytes / page_size as u64,
                mib = bytes / (1024 * 1024),
                elapsed_ms = elapsed.as_millis(),
                "replayed the recorded working set before the guest resumed"
            );
            uffd
        }
        None => uffd,
    };

    // The single per-fault observation point: it feeds the working set the NEXT clone
    // replays and the FCVM_UFFD_FAULT_TRACE measurement trace. Its Drop publishes both,
    // so every exit path below contributes what this clone learned.
    let mut recorder = FaultRecorder::new(working_set, &vm_id, start_time);

    // Set UFFD to non-blocking mode for async integration
    let uffd_fd = uffd.as_raw_fd();
    unsafe {
        let flags = libc::fcntl(uffd_fd, libc::F_GETFL);
        libc::fcntl(uffd_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }

    // Wrap UFFD in AsyncFd for tokio integration
    let async_uffd = AsyncFd::new(uffd).context("creating AsyncFd for UFFD")?;

    let mut fault_count = 0u64;

    // MINOR faults whose UFFDIO_CONTINUE came back `EAGAIN` (`mmap_changing` was set),
    // keyed by page address, value = retry attempts so far. The faulting vCPU sleeps
    // until a successful CONTINUE wakes it and will never re-fault on its own, so every
    // entry here MUST eventually resolve or the handler must die loudly — silently
    // dropping one is a permanently hung guest.
    let mut pending_continues: std::collections::BTreeMap<usize, u32> =
        std::collections::BTreeMap::new();
    // How long to wait for new events before re-attempting parked CONTINUEs. Retries are
    // also attempted after every drain, so this only bounds the idle-queue case.
    const CONTINUE_RETRY_DELAY: Duration = Duration::from_millis(2);
    // Hard bound on retries per fault (~2s at CONTINUE_RETRY_DELAY): mmap_changing
    // clears as soon as the queued event is read, so persistent EAGAIN means something
    // is wedged. Failing the handler is loud; an infinite loop or a drop would not be.
    const MAX_CONTINUE_ATTEMPTS: u32 = 1000;

    loop {
        // Wait for UFFD to be readable — but never park indefinitely while resolved-later
        // faults are waiting, because no new event will ever arrive for THEM.
        let maybe_guard = if pending_continues.is_empty() {
            match async_uffd.readable().await {
                Ok(guard) => Some(guard),
                Err(e) => {
                    error!(target: "uffd", vm_id = %vm_id, error = %e, "error waiting for UFFD readability");
                    return Err(e.into());
                }
            }
        } else {
            match tokio::time::timeout(CONTINUE_RETRY_DELAY, async_uffd.readable()).await {
                Ok(Ok(guard)) => Some(guard),
                Ok(Err(e)) => {
                    error!(target: "uffd", vm_id = %vm_id, error = %e, "error waiting for UFFD readability");
                    return Err(e.into());
                }
                // Timeout: no new events. Fall through and retry the parked faults.
                Err(_) => None,
            }
        };

        if let Some(mut guard) = maybe_guard {
            if !drain_events(
                &mut guard,
                &vm_id,
                &mappings,
                &source,
                page_size,
                page_mask,
                &mut fault_count,
                &mut pending_continues,
                start_time,
                &mut recorder,
                &stats,
            )? {
                return Ok(()); // VM exited
            }
            guard.clear_ready();
        }

        // Retry parked faults now that the queue is drained — reading the queued event is
        // exactly what allows the in-flight operation to finish and `mmap_changing` to
        // clear, so this is the earliest moment a retry can succeed.
        if !pending_continues.is_empty() {
            let mut resolved = Vec::new();
            for (&page, attempts) in pending_continues.iter_mut() {
                match continue_page(async_uffd.get_ref(), &vm_id, page, page_size)? {
                    ContinueOutcome::Resolved => resolved.push(page),
                    ContinueOutcome::VmGone => {
                        info!(target: "uffd", vm_id = %vm_id, "VM exited during CONTINUE retry");
                        return Ok(());
                    }
                    ContinueOutcome::Retry => {
                        *attempts += 1;
                        if *attempts >= MAX_CONTINUE_ATTEMPTS {
                            return Err(anyhow!(
                                "UFFDIO_CONTINUE at 0x{:x} still EAGAIN after {} attempts — \
                                 mmap_changing never cleared. Refusing to drop the fault \
                                 (a dropped fault is a permanently hung vCPU); failing the \
                                 handler for vm {} instead",
                                page,
                                MAX_CONTINUE_ATTEMPTS,
                                vm_id
                            ));
                        }
                    }
                }
            }
            for page in resolved {
                pending_continues.remove(&page);
            }
        }
    }
}

/// Note one resolved fault against both recorder outputs.
///
/// Split out so the several resolution paths below (MINOR continue, zero-fill past the end
/// of the image, EEXIST, ordinary copy) all report through one call and none can be
/// forgotten. The closing timestamp is only read when something is consuming timings.
#[inline]
fn record_fault(recorder: &mut FaultRecorder, offset_in_file: usize, len: usize, t0: u64) {
    let t1 = if recorder.wants_timing() {
        recorder.now_ns()
    } else {
        0
    };
    recorder.record(offset_in_file as u64, len as u64, t0, t1);
}

/// Drain every ready event from the uffd. Returns `Ok(false)` when the VM has exited
/// (uffd closed), `Ok(true)` to keep serving.
#[allow(clippy::too_many_arguments)]
fn drain_events(
    guard: &mut tokio::io::unix::AsyncFdReadyGuard<'_, Uffd>,
    vm_id: &str,
    mappings: &[GuestRegionUffdMapping],
    source: &PageSource,
    page_size: usize,
    page_mask: usize,
    fault_count: &mut u64,
    pending_continues: &mut std::collections::BTreeMap<usize, u32>,
    start_time: std::time::Instant,
    recorder: &mut FaultRecorder,
    stats: &UffdStats,
) -> Result<bool> {
    {
        // Read all available events (non-blocking)
        loop {
            let event = match guard.get_inner().read_event() {
                Ok(Some(event)) => event,
                Ok(None) => break, // No more events ready
                Err(_) => {
                    // UFFD closed = VM exited
                    let elapsed = start_time.elapsed();
                    let rate = if elapsed.as_secs_f64() > 0.0 {
                        *fault_count as f64 / elapsed.as_secs_f64()
                    } else {
                        0.0
                    };
                    info!(
                        target: "uffd",
                        vm_id = %vm_id,
                        fault_count = *fault_count,
                        elapsed_secs = format!("{:.1}", elapsed.as_secs_f64()),
                        pages_per_sec = format!("{:.0}", rate),
                        "VM exited"
                    );
                    return Ok(false);
                }
            };

            match event {
                Event::Pagefault { addr, kind, .. } => {
                    *fault_count += 1;
                    stats.faults.fetch_add(1, Ordering::Relaxed);

                    // Find which memory region this address belongs to
                    let fault_page = (addr as usize) & page_mask;

                    let mapping = mappings
                        .iter()
                        .find(|m| m.contains(fault_page as u64))
                        .ok_or_else(|| {
                            anyhow::anyhow!("page fault at unmapped address: 0x{:x}", fault_page)
                        })?;

                    let base_host = mapping.base_host_virt_addr as usize;
                    if fault_page < base_host {
                        return Err(anyhow!(
                            "page fault address 0x{:x} precedes mapping base 0x{:x}",
                            fault_page,
                            base_host
                        ));
                    }

                    // Offset of this granule within the snapshot memory image. Computed for
                    // both backings — COPY needs it to find the bytes, MINOR does not — because
                    // it is the only fault key that means anything across clones: host virtual
                    // addresses are per process, image offsets are not.
                    let offset_in_region = fault_page - base_host;
                    let mapping_offset = usize::try_from(mapping.offset)
                        .map_err(|_| anyhow!("mapping offset exceeds host address space"))?;
                    let offset_in_file = mapping_offset
                        .checked_add(offset_in_region)
                        .ok_or_else(|| anyhow!("mapping offset overflow"))?;
                    let fault_t0 = if recorder.wants_timing() {
                        recorder.now_ns()
                    } else {
                        0
                    };

                    let mmap = match source {
                        PageSource::Minor { .. } => {
                            // MINOR mode: the page is already in the page cache of the shared
                            // memfd that this clone mapped MAP_PRIVATE. UFFDIO_CONTINUE just
                            // installs a (read-only) PTE onto that folio — no copy, no file
                            // offset arithmetic, because the clone mapped the backing file at
                            // exactly the offsets the snapshot uses.
                            // Only UFFDIO_REGISTER_MODE_MINOR is registered, so the kernel can
                            // only ever deliver minor faults here. Holes in the backing memfd
                            // (all-zero snapshot pages, deliberately not written) are resolved
                            // by the kernel itself without any event, and the folio it
                            // allocates lands in the memfd's page cache so later clones DO
                            // fault MINOR onto it. Anything else means the registration mode
                            // and this handler have drifted apart.
                            if kind != FaultKind::Minor {
                                return Err(anyhow!(
                                    "unexpected {:?} fault at 0x{:x} in MINOR mode — Firecracker \
                                     registered a mode this handler cannot resolve",
                                    kind,
                                    fault_page
                                ));
                            }
                            match continue_page(guard.get_inner(), vm_id, fault_page, page_size)? {
                                ContinueOutcome::Resolved => {}
                                ContinueOutcome::VmGone => return Ok(false),
                                ContinueOutcome::Retry => {
                                    // mmap_changing is set and the event that raised it is
                                    // somewhere in THIS queue. Don't spin here — park the
                                    // fault; the caller retries it right after the drain.
                                    debug!(
                                        target: "uffd",
                                        vm_id = %vm_id,
                                        fault_addr = format!("0x{:x}", fault_page),
                                        "UFFDIO_CONTINUE EAGAIN (mmap_changing) - parked for retry"
                                    );
                                    pending_continues.entry(fault_page).or_insert(0);
                                }
                            }
                            // Recorded when the EVENT is seen, not when the ioctl lands: the
                            // guest touched this page either way, so it belongs in the
                            // working set even if the CONTINUE is parked for retry.
                            record_fault(recorder, offset_in_file, page_size, fault_t0);
                            continue;
                        }
                        PageSource::Copy { mmap } => mmap,
                    };

                    let mmap_len = mmap.len();

                    if offset_in_file >= mmap_len {
                        warn!(
                            target: "uffd",
                            vm_id = %vm_id,
                            fault_addr = format!("0x{:x}", fault_page),
                            "page fault past end of snapshot memory, zero-filling page"
                        );
                        // Heap-allocate zero buffer (2MB on stack would overflow for hugepages)
                        let zero_page: Vec<u8> = vec![0u8; page_size];
                        let result = unsafe {
                            guard.get_inner().copy(
                                zero_page.as_ptr() as *const std::ffi::c_void,
                                fault_page as *mut std::ffi::c_void,
                                page_size,
                                true,
                            )
                        };
                        if let Err(e) = result {
                            error!(
                                target: "uffd",
                                vm_id = %vm_id,
                                fault_addr = format!("0x{:x}", fault_page),
                                error = ?e,
                                "UFFD zero-page copy failed"
                            );
                            return Err(e.into());
                        }
                        record_fault(recorder, offset_in_file, page_size, fault_t0);
                        continue;
                    }

                    let bytes_available = mmap_len - offset_in_file;

                    let copy_result = if bytes_available >= page_size {
                        let page_data = &mmap[offset_in_file..offset_in_file + page_size];
                        unsafe {
                            guard.get_inner().copy(
                                page_data.as_ptr() as *const std::ffi::c_void,
                                fault_page as *mut std::ffi::c_void,
                                page_size,
                                true,
                            )
                        }
                    } else {
                        // Partial page at end of file: copy available data, zero-fill rest
                        // Heap-allocate (2MB on stack would overflow for hugepages)
                        let mut temp: Vec<u8> = vec![0u8; page_size];
                        temp[..bytes_available].copy_from_slice(
                            &mmap[offset_in_file..offset_in_file + bytes_available],
                        );
                        unsafe {
                            guard.get_inner().copy(
                                temp.as_ptr() as *const std::ffi::c_void,
                                fault_page as *mut std::ffi::c_void,
                                page_size,
                                true,
                            )
                        }
                    };

                    if let Err(e) = copy_result {
                        // EEXIST means page was already filled (race with another fault for same page)
                        // This is normal on older kernels with less aggressive page fault coalescing.
                        // See: https://docs.kernel.org/admin-guide/mm/userfaultfd.html
                        // "the kernel must cope with it returning -EEXIST from ioctl(UFFDIO_COPY) as expected"
                        if let userfaultfd::Error::CopyFailed(errno) = &e {
                            // Compare raw errno value since we may have different nix versions
                            if (*errno as i32) == libc::EEXIST {
                                debug!(
                                    target: "uffd",
                                    vm_id = %vm_id,
                                    fault_addr = format!("0x{:x}", fault_page),
                                    "UFFD copy skipped - page already filled (EEXIST)"
                                );
                                record_fault(recorder, offset_in_file, page_size, fault_t0);
                                continue;
                            }
                        }

                        // Real error - log with Debug format to show errno
                        error!(
                            target: "uffd",
                            vm_id = %vm_id,
                            fault_addr = format!("0x{:x}", fault_page),
                            offset_in_file,
                            error = ?e,
                            "UFFD copy failed"
                        );
                        return Err(e.into());
                    }

                    record_fault(recorder, offset_in_file, page_size, fault_t0);
                }
                Event::Remove { start, end } => {
                    // Balloon device removed pages - zero them
                    // Validate bounds: end must be >= start and range must be reasonable
                    let start_addr = start as usize;
                    let end_addr = end as usize;
                    if end_addr < start_addr {
                        warn!(
                            target: "uffd",
                            vm_id = %vm_id,
                            start = format!("0x{:x}", start_addr),
                            end = format!("0x{:x}", end_addr),
                            "Remove event with invalid range (end < start), ignoring"
                        );
                        continue;
                    }
                    let len = end_addr.saturating_sub(start_addr);
                    if len == 0 {
                        continue; // Nothing to zero
                    }

                    if matches!(source, PageSource::Minor { .. }) {
                        // This branch is expected to be DEAD in MINOR mode. Remove events are
                        // generated only by madvise(MADV_DONTNEED/MADV_REMOVE) on a registered
                        // range (fs/userfaultfd.c userfaultfd_remove), but a MINOR clone's
                        // guest memory is a file-backed MAP_PRIVATE mapping, and for those
                        // Firecracker's balloon path (vstate/memory.rs discard_range) does NOT
                        // madvise — it mmap()s fresh anonymous MAP_PRIVATE|MAP_FIXED memory
                        // over the range. That replaces the VMA: the range reads back as
                        // zeros (correct balloon semantics), the MINOR registration for it is
                        // torn down with the old VMA, and no uffd event is generated at all,
                        // so this server is simply never involved in that range again.
                        //
                        // If a Remove event nevertheless arrives (a future Firecracker
                        // switching that path back to madvise), no action is needed or
                        // possible: UFFDIO_ZEROPAGE is EINVAL on hugetlb VMAs and would
                        // defeat page sharing on shmem, and after MADV_DONTNEED on a private
                        // file mapping the next touch minor-faults again and gets served the
                        // pristine snapshot page — an acceptable post-discard state, since
                        // balloon-discarded contents are undefined for the guest. Warn so
                        // drift from the expected Firecracker behaviour is visible.
                        warn!(
                            target: "uffd",
                            vm_id = %vm_id,
                            start = format!("0x{:x}", start_addr),
                            len,
                            "unexpected Remove event in MINOR mode (Firecracker balloon \
                             discard should replace the VMA, not madvise) - ignoring; \
                             still-registered pages will minor-fault normally"
                        );
                        continue;
                    }

                    // Remove events and page faults for the same range arrive in either order,
                    // so a page in this range may already have been filled by UFFDIO_COPY before
                    // we see the Remove event. UFFDIO_ZEROPAGE then fails with EEXIST (first page
                    // already present) or EAGAIN (range partially zeroed before hitting a present
                    // page). Tolerate both — same as the EEXIST handling in the copy path above —
                    // by falling back to per-page zeroing that skips already-present pages.
                    // Killing the handler here would close the uffd and silently corrupt the
                    // still-running VM.
                    let bulk_result = unsafe { guard.get_inner().zeropage(start, len, true) };
                    if let Err(e) = bulk_result {
                        if !zeropage_hit_present_page(&e) {
                            error!(
                                target: "uffd",
                                vm_id = %vm_id,
                                start = format!("0x{:x}", start_addr),
                                len,
                                error = ?e,
                                "UFFD zeropage failed for Remove event"
                            );
                            return Err(e.into());
                        }
                        debug!(
                            target: "uffd",
                            vm_id = %vm_id,
                            start = format!("0x{:x}", start_addr),
                            len,
                            error = ?e,
                            "bulk zeropage hit already-present pages, zeroing per page"
                        );
                        let mut page = start_addr;
                        while page < end_addr {
                            let page_result = unsafe {
                                guard.get_inner().zeropage(
                                    page as *mut std::ffi::c_void,
                                    page_size,
                                    true,
                                )
                            };
                            if let Err(page_err) = page_result {
                                if !zeropage_hit_present_page(&page_err) {
                                    error!(
                                        target: "uffd",
                                        vm_id = %vm_id,
                                        page = format!("0x{:x}", page),
                                        error = ?page_err,
                                        "UFFD zeropage failed for Remove event"
                                    );
                                    return Err(page_err.into());
                                }
                                debug!(
                                    target: "uffd",
                                    vm_id = %vm_id,
                                    page = format!("0x{:x}", page),
                                    "zeropage skipped - page already present"
                                );
                            }
                            page += page_size;
                        }
                    }
                }
                Event::Fork { .. } | Event::Remap { .. } | Event::Unmap { .. } => {
                    // Ignore these events
                }
            }
        }
    }
    Ok(true)
}

/// Memory region mapping from Firecracker.
///
/// Firecracker sends these in the UFFD handshake JSON. The `page_size` field
/// indicates the page granularity for this region:
/// - 4096 (4KB): standard pages
/// - 2097152 (2MB): hugepage-backed memory (`huge_pages: "2M"`)
/// - 16384 (16KB): ARM64 with CONFIG_ARM64_16K_PAGES (future)
///
/// The `page_size` field is required — our Firecracker fork always sends it.
#[derive(Clone, Debug, serde::Deserialize)]
struct GuestRegionUffdMapping {
    base_host_virt_addr: u64,
    size: usize,
    offset: u64,
    /// Page size for this region (from Firecracker handshake).
    /// Standard: 4096, hugepages: 2097152.
    page_size: usize,
}

impl GuestRegionUffdMapping {
    /// Check if address is within this mapping (overflow-safe)
    fn contains(&self, addr: u64) -> bool {
        if addr < self.base_host_virt_addr {
            return false;
        }
        // Use checked arithmetic to prevent overflow
        match self.base_host_virt_addr.checked_add(self.size as u64) {
            Some(end) => addr < end,
            None => true, // If overflow, assume addr is within (max range)
        }
    }

    /// Validate that this mapping has sensible values
    fn validate(&self) -> Result<()> {
        if self.size == 0 {
            anyhow::bail!(
                "mapping has zero size at base 0x{:x}",
                self.base_host_virt_addr
            );
        }
        // Validate page_size is a non-zero power of two
        if self.page_size == 0 || !self.page_size.is_power_of_two() {
            anyhow::bail!(
                "invalid page_size {}: must be a non-zero power of two",
                self.page_size
            );
        }
        // Check for overflow in base + size
        if self
            .base_host_virt_addr
            .checked_add(self.size as u64)
            .is_none()
        {
            anyhow::bail!(
                "mapping range overflow: base 0x{:x}, size {}",
                self.base_host_virt_addr,
                self.size
            );
        }
        Ok(())
    }
}

/// Complete the handshake with a newly connected Firecracker.
///
/// MINOR mode is a two-message exchange (fcvm speaks first):
/// 1. fcvm sends the shared backing memfd over `SCM_RIGHTS`,
/// 2. Firecracker maps it `MAP_PRIVATE`, registers a userfaultfd in
///    `UFFDIO_REGISTER_MODE_MINOR`, and replies with the mappings JSON + that uffd.
///
/// COPY mode skips step 1 — Firecracker allocates anonymous memory itself and sends only
/// the mappings + uffd.
async fn handshake(
    stream: tokio::net::UnixStream,
    source: &PageSource,
) -> Result<(Uffd, Vec<GuestRegionUffdMapping>)> {
    let std_stream = stream.into_std().context("converting to std stream")?;
    // Keep non-blocking — AsyncFd handles readiness
    let async_stream = AsyncFd::new(std_stream).context("creating AsyncFd for handshake socket")?;

    if let PageSource::Minor { backing, .. } = source {
        send_backing_fd(&async_stream, backing).await?;
    }

    // 4096 bytes for JSON message buffer (unrelated to page size)
    let mut message_buf = vec![0u8; 4096];

    // Wait for data to arrive, then recv with fd passing
    let (bytes_read, uffd_fd_opt) = loop {
        let mut guard = async_stream.readable().await?;
        match guard.get_inner().recv_with_fd(&mut message_buf) {
            Ok(result) => break result,
            Err(e) if e.errno() == libc::EWOULDBLOCK || e.errno() == libc::EAGAIN => {
                guard.clear_ready();
                continue;
            }
            Err(e) => return Err(e).context("receiving UFFD from Firecracker"),
        }
    };

    let uffd_file = uffd_fd_opt.ok_or_else(|| anyhow!("no UFFD file descriptor received"))?;

    message_buf.resize(bytes_read, 0);

    // Parse JSON message containing memory region mappings
    let message = String::from_utf8(message_buf).context("parsing message as UTF-8")?;
    let mappings: Vec<GuestRegionUffdMapping> =
        serde_json::from_str(&message).context("parsing memory mappings JSON")?;

    // Validate all received mappings
    if mappings.is_empty() {
        anyhow::bail!("received empty memory mappings from Firecracker");
    }
    for (i, mapping) in mappings.iter().enumerate() {
        mapping
            .validate()
            .with_context(|| format!("invalid mapping at index {}", i))?;
    }

    // Convert File to Uffd
    let uffd = unsafe { Uffd::from_raw_fd(uffd_file.into_raw_fd()) };

    Ok((uffd, mappings))
}

/// Send the shared backing memfd to a connecting Firecracker over `SCM_RIGHTS`.
async fn send_backing_fd(
    async_stream: &AsyncFd<std::os::unix::net::UnixStream>,
    backing: &File,
) -> Result<()> {
    // Payload is ignored by the receiver; SCM_RIGHTS needs at least one data byte.
    const HELLO: &[u8] = b"FCVM_UFFD_MINOR_BACKING";
    loop {
        let mut guard = async_stream.writable().await?;
        match guard.get_inner().send_with_fd(HELLO, backing.as_raw_fd()) {
            Ok(_) => return Ok(()),
            Err(e) if e.errno() == libc::EWOULDBLOCK || e.errno() == libc::EAGAIN => {
                guard.clear_ready();
                continue;
            }
            Err(e) => return Err(e).context("sending backing memfd to Firecracker"),
        }
    }
}

/// Create, populate, **seal**, and reopen read-only the shared memfd that MINOR-mode
/// clones map `MAP_PRIVATE`.
///
/// The returned fd is what every clone receives over `SCM_RIGHTS`, so it must not be able
/// to modify the golden snapshot: kernel CoW only covers *mapped* writes, and an unsealed
/// fd could `pwrite(2)`/`ftruncate(2)` the backing file — or be reopened `O_RDWR` via
/// `/proc/self/fd` — corrupting the snapshot for every other clone. Two independent locks
/// close that door:
///
/// * the inode is sealed `F_SEAL_SEAL|F_SEAL_SHRINK|F_SEAL_GROW|F_SEAL_WRITE` once
///   populated (verified to work for `MFD_HUGETLB` on this kernel), which makes every
///   write path fail no matter how the fd is reopened: `write(2)` EPERM (hugetlbfs has no
///   write support at all and fails EINVAL even unsealed), `ftruncate(2)` EPERM,
///   `mmap(MAP_SHARED, PROT_WRITE)` EPERM;
/// * the fd actually handed out is an `O_RDONLY` reopen, so plain `pwrite(2)` on it is
///   EBADF before the seals even get a say.
///
/// `MAP_PRIVATE` mappings (any PROT) and `UFFDIO_CONTINUE` are unaffected by both —
/// proven by `test_backing_memfd_is_sealed_and_serves_minor_faults` and the
/// `/tmp/uffdproto/seal_reserve.c` kernel probe.
///
/// The memfd is resident for as long as the server lives (shmem is unswappable on a host
/// with no swap), so it is the fixed cost of the whole scheme — every byte written here is
/// paid once and then shared by every clone. Two rules govern what gets written:
///
/// * **All-zero pages are left as holes (shmem only).** The first clone to read a hole takes
///   an ordinary fault; `shmem_fault` allocates the folio *in the inode's page cache*
///   (`shmem_get_folio_gfp(..., SGP_CACHE, ...)`) regardless of `VM_SHARED`, so every later
///   clone finds it there and faults MINOR onto the same physical page. Sharing is preserved
///   and the resident cost drops to the snapshot's non-zero footprint (measured: 63% of a
///   1 GiB idle alpine/nginx snapshot is zero pages).
/// * **hugetlb is populated in full.** `hugetlb_no_page()` only calls
///   `hugetlb_add_to_page_cache()` when `vma->vm_flags & VM_MAYSHARE`; on our `MAP_PRIVATE`
///   VMA a hole fault allocates an *anonymous* huge page instead, one per clone. Leaving
///   holes there would silently destroy the sharing this whole path exists for.
fn create_backing_memfd(
    snapshot_id: &str,
    mut mem_file: File,
    mem_size: usize,
    hugepages: bool,
) -> Result<File> {
    use std::io::Read;

    let page_size = if hugepages { HUGE_PAGE_2M } else { 4096 };
    // hugetlbfs rounds allocations to whole huge pages; size the memfd accordingly so the
    // final partial page is still fully backed.
    let backing_size = mem_size.div_ceil(page_size) * page_size;

    if hugepages {
        preflight_hugepages(
            backing_size / HUGE_PAGE_2M,
            "a MINOR-mode snapshot backing (hugetlb backings are populated in full)",
        )?;
    }

    let name = std::ffi::CString::new(format!("fcvm-snap-{snapshot_id}"))
        .context("building memfd name")?;
    let mut flags = libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING;
    if hugepages {
        flags |= libc::MFD_HUGETLB | libc::MFD_HUGE_2MB;
    }
    // SAFETY: `name` is a valid NUL-terminated string that outlives the call.
    let fd = unsafe { libc::memfd_create(name.as_ptr(), flags) };
    if fd < 0 {
        let err = std::io::Error::last_os_error();
        anyhow::bail!(
            "memfd_create(hugetlb={hugepages}) failed: {err} — MINOR-mode restore needs a \
             shmem or hugetlbfs backing file"
        );
    }
    // SAFETY: `fd` is a fresh, owned file descriptor.
    let backing = unsafe { File::from_raw_fd(fd) };
    backing
        .set_len(backing_size as u64)
        .context("sizing backing memfd")?;

    // Populate: mmap the memfd MAP_SHARED and stream the snapshot file into it.
    let mut dest = unsafe {
        memmap2::MmapOptions::new()
            .len(backing_size)
            .map_mut(&backing)
            .context("mmapping backing memfd for population")?
    };

    let start = std::time::Instant::now();
    // Read through a scratch buffer so an all-zero page can be SKIPPED: touching `dest`
    // is what allocates a folio, so not touching it leaves a hole.
    let chunk_len = (8 * 1024 * 1024usize).next_multiple_of(page_size);
    let mut scratch = vec![0u8; chunk_len];
    let zero_page = vec![0u8; page_size];
    let mut offset = 0usize;
    let mut resident_pages = 0usize;
    let mut hole_pages = 0usize;
    while offset < mem_size {
        let end = (offset + chunk_len).min(mem_size);
        let len = end - offset;
        mem_file
            .read_exact(&mut scratch[..len])
            .with_context(|| format!("reading snapshot memory at offset {offset}"))?;
        let mut p = 0usize;
        while p < len {
            let plen = page_size.min(len - p);
            let src = &scratch[p..p + plen];
            if !hugepages && plen == page_size && src == &zero_page[..] {
                hole_pages += 1;
            } else {
                dest[offset + p..offset + p + plen].copy_from_slice(src);
                resident_pages += 1;
            }
            p += plen;
        }
        offset = end;
    }
    // Tail beyond the snapshot file (hugetlb rounding) stays zero — `set_len` guarantees it.
    // Must be unmapped BEFORE sealing: F_SEAL_WRITE refuses while writable shared
    // mappings exist.
    drop(dest);

    // Seal the inode so nothing — not this fd, not any /proc/self/fd reopen of it in a
    // clone — can ever modify the populated snapshot again.
    let seals = libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE;
    // SAFETY: fcntl(F_ADD_SEALS) on an owned memfd created with MFD_ALLOW_SEALING; the
    // only writable mapping was just dropped.
    if unsafe { libc::fcntl(backing.as_raw_fd(), libc::F_ADD_SEALS, seals) } < 0 {
        return Err(std::io::Error::last_os_error()).context("sealing backing memfd (F_ADD_SEALS)");
    }
    // SAFETY: fcntl(F_GET_SEALS) on an owned fd.
    let got = unsafe { libc::fcntl(backing.as_raw_fd(), libc::F_GET_SEALS) };
    anyhow::ensure!(
        got >= 0 && (got & seals) == seals,
        "backing memfd seals did not stick (want {seals:#x}, got {got:#x}) — refusing to hand \
         a writable snapshot fd to clones"
    );

    // Hand out an O_RDONLY reopen of the sealed inode, not the O_RDWR original.
    let backing_ro = File::open(format!("/proc/self/fd/{}", backing.as_raw_fd()))
        .context("reopening sealed backing memfd read-only")?;
    drop(backing);

    // The snapshot file's page cache is dead weight now: every clone reads the memfd.
    // SAFETY: fadvise on a valid fd with a whole-file range.
    unsafe {
        libc::posix_fadvise(mem_file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED);
    }

    info!(
        target: "uffd",
        snapshot = %snapshot_id,
        mem_size_mb = mem_size / (1024 * 1024),
        backing_mb = backing_size / (1024 * 1024),
        resident_mb = resident_pages * page_size / (1024 * 1024),
        hole_mb = hole_pages * page_size / (1024 * 1024),
        hugepages,
        populate_ms = start.elapsed().as_millis(),
        "populated, sealed and reopened read-only the shared backing memfd for MINOR-mode restore"
    );

    Ok(backing_ro)
}

/// Refuse an allocation the host hugepage pool cannot hold.
///
/// hugetlb pages are neither swappable nor reclaimable, so exhaustion is not "slow" — it
/// is `ENOMEM`/`SIGBUS`. Checking `Free - Rsvd` up front turns that into an explanatory
/// error at the point of the decision instead of a dead process later.
fn preflight_hugepages(needed_pages: usize, what: &str) -> Result<()> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").context("reading /proc/meminfo")?;
    let field = |name: &str| -> Option<usize> {
        meminfo.lines().find_map(|l| {
            let rest = l.strip_prefix(name)?;
            rest.split_whitespace().next()?.parse().ok()
        })
    };
    let free = field("HugePages_Free:").unwrap_or(0);
    let rsvd = field("HugePages_Rsvd:").unwrap_or(0);
    let usable = free.saturating_sub(rsvd);
    if usable < needed_pages {
        anyhow::bail!(
            "hugepage pool too small for {what}: need {needed_pages} x 2MiB ({} MiB) but only \
             {usable} are usable (HugePages_Free={free}, HugePages_Rsvd={rsvd}). \
             Raise vm.nr_hugepages.",
            needed_pages * 2
        );
    }
    Ok(())
}

/// Admission check for spawning one hugepage-backed UFFD clone.
///
/// Every hugepage clone can consume up to the FULL guest size in private huge pages: in
/// MINOR mode each guest write CoWs a 2 MiB page out of the shared backing (and the
/// kernel *reserves* the full range at restore — see `guest_memory_from_uffd_minor` in
/// the Firecracker fork — so an unservable clone fails its mmap with ENOMEM instead of
/// taking a SIGBUS mid-run); in COPY mode every faulted page is a fresh private huge
/// page. This check exists to fail with an explanation *before* spawning Firecracker.
/// It is advisory (another clone can win the race for the same pages); the authoritative,
/// race-free gate for MINOR clones is the kernel's hugetlb reservation at restore time.
pub fn preflight_clone_hugepages(memory_mib: usize) -> Result<()> {
    let needed_pages = (memory_mib * 1024 * 1024).div_ceil(HUGE_PAGE_2M);
    preflight_hugepages(
        needed_pages,
        "a hugepage-backed clone (worst case every guest page is privately copied-on-write; \
         MINOR-mode restore reserves the full guest size up front so a running guest can \
         never SIGBUS)",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mapping_contains_basic() {
        let mapping = GuestRegionUffdMapping {
            base_host_virt_addr: 0x1000,
            size: 0x1000, // 4KB
            offset: 0,
            page_size: 4096,
        };

        // Before mapping
        assert!(!mapping.contains(0x0FFF));
        // Start of mapping
        assert!(mapping.contains(0x1000));
        // Middle of mapping
        assert!(mapping.contains(0x1500));
        // Last byte of mapping
        assert!(mapping.contains(0x1FFF));
        // Just after mapping
        assert!(!mapping.contains(0x2000));
        // Way after mapping
        assert!(!mapping.contains(0x3000));
    }

    #[test]
    fn test_mapping_contains_large_address() {
        // Test with addresses near u64::MAX to verify overflow handling
        let mapping = GuestRegionUffdMapping {
            base_host_virt_addr: u64::MAX - 0x1000,
            size: 0x800,
            offset: 0,
            page_size: 4096,
        };

        // Should contain addresses within range
        assert!(mapping.contains(u64::MAX - 0x1000));
        assert!(mapping.contains(u64::MAX - 0x900));

        // Should not contain addresses before range
        assert!(!mapping.contains(u64::MAX - 0x1001));
    }

    #[test]
    fn test_mapping_contains_overflow() {
        // Test case where base + size would overflow u64
        let mapping = GuestRegionUffdMapping {
            base_host_virt_addr: u64::MAX - 100,
            size: 200, // This would overflow
            offset: 0,
            page_size: 4096,
        };

        // With overflow, contains() returns true for addresses >= base
        assert!(mapping.contains(u64::MAX - 100));
        assert!(mapping.contains(u64::MAX));
        // Still false for addresses before base
        assert!(!mapping.contains(u64::MAX - 101));
    }

    #[test]
    fn test_mapping_validate_success() {
        let mapping = GuestRegionUffdMapping {
            base_host_virt_addr: 0x1000,
            size: 0x1000,
            offset: 0,
            page_size: 4096,
        };
        assert!(mapping.validate().is_ok());
    }

    #[test]
    fn test_mapping_validate_zero_size() {
        let mapping = GuestRegionUffdMapping {
            base_host_virt_addr: 0x1000,
            size: 0, // Invalid
            offset: 0,
            page_size: 4096,
        };
        let result = mapping.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("zero size"));
    }

    #[test]
    fn test_mapping_validate_overflow() {
        let mapping = GuestRegionUffdMapping {
            base_host_virt_addr: u64::MAX - 100,
            size: 200, // Would overflow
            offset: 0,
            page_size: 4096,
        };
        let result = mapping.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("overflow"));
    }

    #[test]
    fn test_mapping_validate_invalid_page_size() {
        let mapping = GuestRegionUffdMapping {
            base_host_virt_addr: 0x1000,
            size: 0x1000,
            offset: 0,
            page_size: 0,
        };
        let result = mapping.validate();
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("invalid page_size"));

        let mapping = GuestRegionUffdMapping {
            base_host_virt_addr: 0x1000,
            size: 0x1000,
            offset: 0,
            page_size: 123, // Not a power of two
        };
        let result = mapping.validate();
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("invalid page_size"));
    }

    #[test]
    fn test_mapping_json_with_page_size() {
        // Firecracker sends page_size in UFFD handshake
        let json = r#"[
            {"base_host_virt_addr": 140000000, "size": 536870912, "offset": 0, "page_size": 2097152}
        ]"#;
        let mappings: Vec<GuestRegionUffdMapping> = serde_json::from_str(json).unwrap();
        assert_eq!(mappings.len(), 1);
        assert_eq!(mappings[0].base_host_virt_addr, 140000000);
        assert_eq!(mappings[0].size, 536870912); // 512MB
        assert_eq!(mappings[0].offset, 0);
        assert_eq!(mappings[0].page_size, 2097152); // 2MB hugepages
    }

    #[test]
    fn test_mapping_json_with_4k_page_size() {
        let json = r#"[
            {"base_host_virt_addr": 140000000, "size": 536870912, "offset": 0, "page_size": 4096}
        ]"#;
        let mappings: Vec<GuestRegionUffdMapping> = serde_json::from_str(json).unwrap();
        assert_eq!(mappings[0].page_size, 4096);
    }

    #[test]
    fn test_mapping_json_with_16k_page_size() {
        // Future-proofing: ARM64 CONFIG_ARM64_16K_PAGES
        let json = r#"[
            {"base_host_virt_addr": 140000000, "size": 536870912, "offset": 0, "page_size": 16384}
        ]"#;
        let mappings: Vec<GuestRegionUffdMapping> = serde_json::from_str(json).unwrap();
        assert_eq!(mappings[0].page_size, 16384);
    }

    #[test]
    fn test_mapping_json_multiple_regions() {
        let json = r#"[
            {"base_host_virt_addr": 140000000, "size": 268435456, "offset": 0, "page_size": 4096},
            {"base_host_virt_addr": 408435456, "size": 268435456, "offset": 268435456, "page_size": 4096}
        ]"#;
        let mappings: Vec<GuestRegionUffdMapping> = serde_json::from_str(json).unwrap();
        assert_eq!(mappings.len(), 2);

        // First region
        assert_eq!(mappings[0].size, 268435456); // 256MB
        assert_eq!(mappings[0].offset, 0);

        // Second region
        assert_eq!(mappings[1].offset, 268435456); // Starts after first
    }

    #[test]
    fn test_mapping_contains_with_hugepage_alignment() {
        // 2MB-aligned mapping
        let mapping = GuestRegionUffdMapping {
            base_host_virt_addr: 0x200000, // 2MB aligned
            size: 0x200000,                // 2MB
            offset: 0,
            page_size: 2097152,
        };
        assert!(mapping.contains(0x200000));
        assert!(mapping.contains(0x300000));
        assert!(!mapping.contains(0x400000));
    }

    // =========================================================================
    // Backing-memfd immutability (the fd every clone receives must not be able
    // to modify the golden snapshot) + MINOR/CONTINUE off the sealed fd.
    // =========================================================================

    /// Write a 3-page snapshot file: page0 = 0xAB, page1 = zeros (hole candidate),
    /// page2 = 0xCD. Returns (path, mem_size).
    fn write_test_snapshot() -> (std::path::PathBuf, usize) {
        use std::io::Write;
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "fcvm-seal-test-{}-{}.mem",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let mut f = File::create(&path).unwrap();
        f.write_all(&[0xABu8; 4096]).unwrap();
        f.write_all(&[0u8; 4096]).unwrap();
        f.write_all(&[0xCDu8; 4096]).unwrap();
        f.sync_all().unwrap();
        (path, 3 * 4096)
    }

    fn errno_of(res: isize) -> i32 {
        assert!(res < 0, "operation unexpectedly succeeded");
        std::io::Error::last_os_error().raw_os_error().unwrap()
    }

    /// Every write door on a backing fd must be shut. The expected errno differs by which
    /// lock stops the door first: for the O_RDONLY reopen we hand out, the fd mode does
    /// (pwrite EBADF, writable shared mmap EACCES); for a sneaky O_RDWR /proc reopen only
    /// the SEAL is left to do it (EPERM everywhere).
    fn assert_fd_cannot_write(
        fd: std::os::unix::io::RawFd,
        expect_pwrite_errno: i32,
        expect_mmap_errno: i32,
    ) {
        let byte = [0x5Au8];
        // pwrite(2)
        let r = unsafe { libc::pwrite(fd, byte.as_ptr().cast(), 1, 0) };
        assert_eq!(
            errno_of(r as isize),
            expect_pwrite_errno,
            "pwrite must be refused"
        );
        // ftruncate(2): shrink (destroys pages other clones are mapping)
        let r = unsafe { libc::ftruncate(fd, 4096) };
        let e = errno_of(r as isize);
        assert!(
            e == libc::EPERM || e == libc::EINVAL,
            "ftruncate(shrink) must be refused, got errno {e}"
        );
        // ftruncate(2): grow
        let r = unsafe { libc::ftruncate(fd, 1024 * 1024 * 1024) };
        let e = errno_of(r as isize);
        assert!(
            e == libc::EPERM || e == libc::EINVAL,
            "ftruncate(grow) must be refused, got errno {e}"
        );
        // mmap(MAP_SHARED, PROT_WRITE) — the door the MAP_SHARED experiment walked through
        let r = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                4096,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        assert_eq!(
            r,
            libc::MAP_FAILED,
            "writable shared mapping must be refused"
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error().unwrap(),
            expect_mmap_errno,
            "writable shared mapping must be refused by fd mode (EACCES) or seal (EPERM)"
        );
    }

    /// The fd `create_backing_memfd` returns (== what `send_backing_fd` hands every
    /// clone) cannot pwrite/ftruncate/map-shared-writable the snapshot, even via a
    /// /proc/self/fd O_RDWR reopen, while read paths still see the snapshot bytes.
    #[test]
    fn test_backing_memfd_is_sealed_and_read_only() {
        let (path, mem_size) = write_test_snapshot();
        let mem_file = File::open(&path).unwrap();
        let backing = create_backing_memfd("seal-test", mem_file, mem_size, false).unwrap();
        std::fs::remove_file(&path).unwrap();

        // All four seals present.
        let seals = unsafe { libc::fcntl(backing.as_raw_fd(), libc::F_GET_SEALS) };
        let want = libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE;
        assert_eq!(seals & want, want, "expected all seals, got {seals:#x}");

        // The handed-out fd is O_RDONLY: pwrite dies on the fd mode (EBADF).
        assert_fd_cannot_write(backing.as_raw_fd(), libc::EBADF, libc::EACCES);

        // Reopening it O_RDWR through /proc (what a malicious/buggy clone could do)
        // must ALSO be unable to write — that is the seal doing its job.
        let rw = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(format!("/proc/self/fd/{}", backing.as_raw_fd()))
            .expect("O_RDWR reopen of a sealed memfd is allowed to open");
        assert_fd_cannot_write(rw.as_raw_fd(), libc::EPERM, libc::EPERM);

        // Read paths still work: MAP_PRIVATE sees the snapshot (and the zero page
        // stayed a hole but reads back as zeros).
        let map = unsafe { MmapOptions::new().map_copy(&backing).unwrap() };
        assert!(map[..4096].iter().all(|&b| b == 0xAB));
        assert!(map[4096..8192].iter().all(|&b| b == 0));
        assert!(map[8192..].iter().all(|&b| b == 0xCD));
    }

    /// Full-fidelity clone view: receive the backing fd over the real `send_backing_fd`
    /// SCM_RIGHTS path, then prove the RECEIVED fd cannot write — and that MAP_PRIVATE +
    /// UFFD MINOR registration + `continue_page` (UFFDIO_CONTINUE incl. the EEXIST race)
    /// all still work off the sealed, read-only fd.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_backing_memfd_is_sealed_and_serves_minor_faults() {
        let (path, mem_size) = write_test_snapshot();
        let mem_file = File::open(&path).unwrap();
        let backing = create_backing_memfd("seal-minor-test", mem_file, mem_size, false).unwrap();
        std::fs::remove_file(&path).unwrap();

        // --- receive the fd exactly the way a clone's Firecracker does ---
        let (tx, rx) = std::os::unix::net::UnixStream::pair().unwrap();
        tx.set_nonblocking(true).unwrap();
        let tx = AsyncFd::new(tx).unwrap();
        send_backing_fd(&tx, &backing).await.unwrap();
        let mut hello = [0u8; 64];
        let (_, received) = rx.recv_with_fd(&mut hello).unwrap();
        let received = received.expect("backing fd must arrive with the hello message");

        // The received fd must be unable to modify the snapshot in every way.
        assert_fd_cannot_write(received.as_raw_fd(), libc::EBADF, libc::EACCES);

        // --- MAP_PRIVATE + MINOR + CONTINUE off the received fd ---
        let mut map = unsafe { MmapOptions::new().map_copy(&received).unwrap() };
        let base = map.as_mut_ptr() as usize;
        let uffd = userfaultfd::UffdBuilder::new()
            .close_on_exec(true)
            .non_blocking(false)
            .user_mode_only(true)
            .create()
            .expect("creating userfaultfd (via /dev/userfaultfd)");
        uffd.register_with_mode(
            base as *mut std::ffi::c_void,
            mem_size,
            userfaultfd::RegisterMode::MINOR,
        )
        .expect("MINOR registration on a MAP_PRIVATE mapping of the sealed fd");

        // Touch page 2 from another thread -> MINOR fault -> resolve with continue_page.
        let reader = std::thread::spawn(move || unsafe {
            std::ptr::read_volatile((base + 8192) as *const u8)
        });
        let event = uffd
            .read_event()
            .unwrap()
            .expect("blocking read yields an event");
        let fault_addr = match event {
            Event::Pagefault {
                kind: FaultKind::Minor,
                addr,
                ..
            } => (addr as usize) & !0xFFF,
            other => panic!("expected a MINOR pagefault, got {other:?}"),
        };
        assert_eq!(fault_addr, base + 8192);
        match continue_page(&uffd, "seal-test", fault_addr, 4096).unwrap() {
            ContinueOutcome::Resolved => {}
            _ => panic!("continue_page must resolve a pending MINOR fault"),
        }
        assert_eq!(
            reader.join().unwrap(),
            0xCD,
            "fault must be served snapshot content"
        );

        // The EEXIST race (page already mapped) is success, not an error.
        match continue_page(&uffd, "seal-test", fault_addr, 4096).unwrap() {
            ContinueOutcome::Resolved => {}
            _ => panic!("EEXIST on an already-mapped page must count as resolved"),
        }

        // Guest-side CoW still works and never reaches the backing file.
        map[8192] = 0x11;
        assert_eq!(map[8192], 0x11);
        let pristine = unsafe { MmapOptions::new().map_copy_read_only(&received).unwrap() };
        assert_eq!(
            pristine[8192], 0xCD,
            "clone write must not reach the snapshot"
        );
    }

    // =========================================================================
    // Working-set replay: cutting a recorded set down to what a clone mapped.
    // =========================================================================

    fn mapping(base: u64, size: usize, offset: u64) -> GuestRegionUffdMapping {
        GuestRegionUffdMapping {
            base_host_virt_addr: base,
            size,
            offset,
            page_size: 4096,
        }
    }

    #[test]
    fn plan_chunks_translates_image_offsets_to_this_clones_addresses() {
        let mappings = [
            mapping(0x1000_0000, 3 * 4096, 0),
            mapping(0x9000_0000, 3 * 4096, 3 * 4096),
        ];
        // One run spanning both regions must be split at the region boundary — the two
        // regions are contiguous in the image but nowhere near each other in the clone.
        let runs = [Run {
            offset: 0,
            len: 6 * 4096,
        }];
        let chunks = plan_chunks(runs.into_iter(), &mappings, 6 * 4096, 4096);
        assert_eq!(
            chunks,
            vec![
                PrefetchChunk {
                    file_offset: 0,
                    host_addr: 0x1000_0000,
                    len: 3 * 4096
                },
                PrefetchChunk {
                    file_offset: 3 * 4096,
                    host_addr: 0x9000_0000,
                    len: 3 * 4096
                },
            ]
        );
    }

    #[test]
    fn plan_chunks_drops_what_this_clone_did_not_map() {
        let mappings = [mapping(0x2000_0000, 4 * 4096, 4096)];
        let runs = [
            // Entirely before the region.
            Run {
                offset: 0,
                len: 4096,
            },
            // Straddles the start: only the mapped tail survives.
            Run {
                offset: 0,
                len: 3 * 4096,
            },
            // Entirely past the region.
            Run {
                offset: 100 * 4096,
                len: 4096,
            },
        ];
        let chunks = plan_chunks(runs.into_iter(), &mappings, 1024 * 4096, 4096);
        assert_eq!(
            chunks,
            vec![PrefetchChunk {
                file_offset: 4096,
                host_addr: 0x2000_0000,
                len: 2 * 4096
            }],
            "unmapped offsets must be dropped, never clamped onto some other page"
        );
    }

    #[test]
    fn plan_chunks_stops_at_the_end_of_the_servable_image() {
        // A recorded run that reaches past the bytes the server can actually serve (a
        // truncated image) is cut, not clamped: the tail faults on demand and takes the
        // existing zero-fill path.
        let mappings = [mapping(0x3000_0000, 8 * 4096, 0)];
        let runs = [Run {
            offset: 0,
            len: 8 * 4096,
        }];
        let chunks = plan_chunks(runs.into_iter(), &mappings, 5 * 4096, 4096);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].len, 5 * 4096);
    }

    #[test]
    fn plan_chunks_trims_a_4k_recorded_set_to_whole_huge_pages() {
        // `--hugepages` on a clone of a snapshot whose set was recorded by 4 KiB clones.
        // A partial huge page cannot be installed (EINVAL), so only the whole ones are
        // replayed and the ragged edges fault on demand.
        const HUGE: u64 = 2 * 1024 * 1024;
        let mappings = [mapping(0x4000_0000, 8 * HUGE as usize, 0)];
        let runs = [
            // Straddles two huge pages, covering neither completely.
            Run {
                offset: HUGE - 4096,
                len: 2 * 4096,
            },
            // Covers huge pages 2 and 3 completely, plus a ragged page either side.
            Run {
                offset: 2 * HUGE - 4096,
                len: 2 * HUGE + 2 * 4096,
            },
        ];
        let chunks = plan_chunks(runs.into_iter(), &mappings, 8 * HUGE, HUGE);
        assert_eq!(
            chunks,
            vec![PrefetchChunk {
                file_offset: 2 * HUGE as usize,
                host_addr: 0x4000_0000 + 2 * HUGE as usize,
                len: 2 * HUGE as usize,
            }]
        );
    }

    // =========================================================================
    // Record -> persist -> replay, against a real userfaultfd.
    //
    // The clones here are what Firecracker is to this server: a mapping, a userfaultfd
    // registered over it, and the handshake. That makes these tests exercise the real
    // handshake, the real replay ioctls and the real fault handler — no VM required.
    // =========================================================================

    const PAGE: usize = 4096;
    /// 4 MiB of guest memory, of which 3 MiB is recorded — both bigger than
    /// [`REPLAY_CHUNK_BYTES`], so replay has to span several ioctls per run.
    const CLONE_PAGES: usize = 1024;
    const RECORDED_PAGES: usize = 768;

    /// Stamp identifying one page of the fake snapshot. Distinct per page so a chunk placed
    /// at the wrong address is caught, and never 0 so a zero-filled page cannot pass.
    fn page_stamp(page: usize) -> u32 {
        page as u32 | 0x8000_0000
    }

    struct TestSnapshot {
        dir: PathBuf,
        mem: PathBuf,
        socket: PathBuf,
    }

    impl TestSnapshot {
        fn new(tag: &str) -> Self {
            static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "fcvm-ws-e2e-{tag}-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let mem = dir.join("memory.bin");
            let mut image = vec![0xA5u8; CLONE_PAGES * PAGE];
            for page in 0..CLONE_PAGES {
                image[page * PAGE..page * PAGE + 4]
                    .copy_from_slice(&page_stamp(page).to_le_bytes());
            }
            std::fs::write(&mem, &image).unwrap();
            Self {
                dir: dir.clone(),
                mem,
                socket: dir.join("uffd.sock"),
            }
        }

        fn image_digest(&self) -> Vec<u8> {
            use sha2::{Digest, Sha256};
            let mut h = Sha256::new();
            h.update(std::fs::read(&self.mem).unwrap());
            h.finalize().to_vec()
        }

        /// Record `pages` as this snapshot's working set through the production writer.
        fn record_working_set(&self, pages: std::ops::Range<usize>) {
            let store = WorkingSetStore::open(&self.mem, (CLONE_PAGES * PAGE) as u64).unwrap();
            let mut observed = store.recorder();
            observed.insert_range(
                (pages.start * PAGE) as u64,
                ((pages.end - pages.start) * PAGE) as u64,
            );
            assert!(store.merge_and_persist(&observed).unwrap().persisted);
        }

        async fn serve(&self, backing: UffdBacking) -> Arc<UffdStats> {
            let server =
                UffdServer::new_with_path("e2e".to_string(), &self.mem, &self.socket, backing)
                    .await
                    .unwrap();
            let stats = server.stats();
            let cancel = tokio_util::sync::CancellationToken::new();
            // Deliberately not cancelled at the end of the test: the clones below live in
            // this process, so their mm never dies and their handlers never see the uffd
            // close. Dropping the runtime tears the tasks down instead.
            tokio::spawn(async move { server.run(cancel).await });
            stats
        }
    }

    impl Drop for TestSnapshot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// A stand-in for a restoring Firecracker: guest memory mapped the way the backing
    /// requires, a userfaultfd registered over it, and the real handshake.
    struct FakeClone {
        map: memmap2::MmapMut,
        /// Kept open for the clone's life, exactly as Firecracker keeps its own copy.
        _uffd: Uffd,
        _stream: std::os::unix::net::UnixStream,
        /// MINOR only: the shared backing the server handed over.
        backing_fd: Option<File>,
    }

    impl FakeClone {
        fn connect(socket: &Path, mem_len: usize, backing: UffdBacking) -> Self {
            let stream = Self::connect_socket(socket);

            // MINOR mode: the server speaks first, handing over the shared memfd that this
            // clone maps MAP_PRIVATE.
            let (mut map, backing_fd) = match backing {
                UffdBacking::Copy => (MmapOptions::new().len(mem_len).map_anon().unwrap(), None),
                UffdBacking::Minor { .. } => {
                    let mut hello = [0u8; 64];
                    let (_, fd) = stream.recv_with_fd(&mut hello).unwrap();
                    let fd = fd.expect("server must send the backing memfd first");
                    let map = unsafe { MmapOptions::new().len(mem_len).map_copy(&fd).unwrap() };
                    (map, Some(fd))
                }
            };

            let base = map.as_mut_ptr() as usize;
            let uffd = userfaultfd::UffdBuilder::new()
                .close_on_exec(true)
                .non_blocking(false)
                .user_mode_only(true)
                .create()
                .unwrap();
            let mode = match backing {
                UffdBacking::Copy => userfaultfd::RegisterMode::MISSING,
                UffdBacking::Minor { .. } => userfaultfd::RegisterMode::MINOR,
            };
            uffd.register_with_mode(base as *mut std::ffi::c_void, mem_len, mode)
                .unwrap();

            let json = format!(
                "[{{\"base_host_virt_addr\":{base},\"size\":{mem_len},\"offset\":0,\
                  \"page_size\":{PAGE}}}]"
            );
            stream
                .send_with_fd(json.as_bytes(), uffd.as_raw_fd())
                .unwrap();

            Self {
                map,
                _uffd: uffd,
                _stream: stream,
                backing_fd,
            }
        }

        /// Retry until the server has bound its socket. Bounded by a deadline and driven by
        /// the connect result itself, so it ends the moment the server is up.
        fn connect_socket(socket: &Path) -> std::os::unix::net::UnixStream {
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            loop {
                match std::os::unix::net::UnixStream::connect(socket) {
                    Ok(s) => return s,
                    Err(e) if std::time::Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(1));
                        let _ = e;
                    }
                    Err(e) => panic!("server never bound {}: {e}", socket.display()),
                }
            }
        }

        /// Read a page's stamp — this is what raises a fault when the page is not already
        /// present.
        fn read_page(&self, page: usize) -> u32 {
            unsafe { std::ptr::read_volatile(self.map.as_ptr().add(page * PAGE) as *const u32) }
        }

        fn write_page(&mut self, page: usize, stamp: u32) {
            unsafe {
                std::ptr::write_volatile(self.map.as_mut_ptr().add(page * PAGE) as *mut u32, stamp)
            }
        }

        /// The pristine snapshot stamp behind a MINOR clone's private mapping.
        fn backing_page(&self, page: usize) -> u32 {
            let fd = self.backing_fd.as_ref().expect("MINOR clones only");
            let map = unsafe { MmapOptions::new().map_copy_read_only(fd).unwrap() };
            u32::from_le_bytes(map[page * PAGE..page * PAGE + 4].try_into().unwrap())
        }
    }

    /// Poll an observable condition to a deadline. Not a fixed delay: it returns as soon as
    /// the condition holds, and fails loudly instead of hanging if it never does.
    async fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while !cond() {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {what}"
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    /// The core claim, on the default (COPY) backing: a recorded set is replayed in bulk
    /// before the clone touches anything, those pages then cost ZERO faults, pages outside
    /// the set still fault and are served correctly, and one clone's writes are invisible to
    /// the other and to the snapshot file.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn working_set_replay_serves_pages_without_faults_and_keeps_clones_isolated() {
        let snap = TestSnapshot::new("copy");
        let digest_before = snap.image_digest();
        snap.record_working_set(0..RECORDED_PAGES);

        let stats = snap.serve(UffdBacking::Copy).await;
        let mem_len = CLONE_PAGES * PAGE;

        let first = FakeClone::connect(&snap.socket, mem_len, UffdBacking::Copy);
        wait_for("first clone's replay", || {
            stats.prefetched_bytes() >= (RECORDED_PAGES * PAGE) as u64
        })
        .await;
        assert_eq!(
            stats.faults(),
            0,
            "replay must not go through the fault path"
        );

        let second = FakeClone::connect(&snap.socket, mem_len, UffdBacking::Copy);
        wait_for("second clone's replay", || {
            stats.prefetched_bytes() >= (2 * RECORDED_PAGES * PAGE) as u64
        })
        .await;

        // Every replayed page is readable with the right content and no fault.
        let (first, second) = tokio::task::spawn_blocking(move || {
            for page in 0..RECORDED_PAGES {
                assert_eq!(
                    first.read_page(page),
                    page_stamp(page),
                    "replayed page {page} has the wrong content"
                );
                assert_eq!(
                    second.read_page(page),
                    page_stamp(page),
                    "replayed page {page} has the wrong content in the second clone"
                );
            }
            (first, second)
        })
        .await
        .unwrap();
        assert_eq!(
            stats.faults(),
            0,
            "{} pages were replayed, so reading them must raise no faults",
            RECORDED_PAGES
        );

        // A page outside the recorded set still faults and is still served correctly —
        // a partial list degrades to on-demand faulting, it does not break the clone.
        let first = tokio::task::spawn_blocking(move || {
            let unrecorded = CLONE_PAGES - 1;
            assert_eq!(first.read_page(unrecorded), page_stamp(unrecorded));
            first
        })
        .await
        .unwrap();
        assert_eq!(
            stats.faults(),
            1,
            "exactly the one unrecorded page should have faulted"
        );

        // Isolation: one clone writes into a replayed page; the other must not see it.
        let (first, second) = tokio::task::spawn_blocking(move || {
            let mut first = first;
            first.write_page(5, 0x1111_1111);
            assert_eq!(first.read_page(5), 0x1111_1111);
            assert_eq!(
                second.read_page(5),
                page_stamp(5),
                "one clone's write leaked into another clone"
            );
            (first, second)
        })
        .await
        .unwrap();
        drop((first, second));

        assert_eq!(
            snap.image_digest(),
            digest_before,
            "the snapshot memory image must be byte-identical after replay + a clone write"
        );
    }

    /// Same claim on the MINOR backing, where it is not free: replay installs PTEs onto
    /// folios SHARED by every clone, so a guest write must copy-on-write instead of
    /// modifying the shared page. (This is the mode where a MAP_SHARED mistake once
    /// corrupted a golden snapshot.)
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn working_set_replay_on_shared_backing_still_isolates_clones() {
        let snap = TestSnapshot::new("minor");
        let digest_before = snap.image_digest();
        snap.record_working_set(0..RECORDED_PAGES);

        let backing = UffdBacking::Minor { hugepages: false };
        let stats = snap.serve(backing).await;
        let mem_len = CLONE_PAGES * PAGE;

        let first = FakeClone::connect(&snap.socket, mem_len, backing);
        wait_for("first clone's replay", || {
            stats.prefetched_bytes() >= (RECORDED_PAGES * PAGE) as u64
        })
        .await;
        let second = FakeClone::connect(&snap.socket, mem_len, backing);
        wait_for("second clone's replay", || {
            stats.prefetched_bytes() >= (2 * RECORDED_PAGES * PAGE) as u64
        })
        .await;
        assert_eq!(
            stats.faults(),
            0,
            "replay must not go through the fault path"
        );

        let (first, second) = tokio::task::spawn_blocking(move || {
            let mut first = first;
            for page in 0..RECORDED_PAGES {
                assert_eq!(first.read_page(page), page_stamp(page));
                assert_eq!(second.read_page(page), page_stamp(page));
            }
            first.write_page(7, 0x2222_2222);
            assert_eq!(first.read_page(7), 0x2222_2222);
            assert_eq!(
                second.read_page(7),
                page_stamp(7),
                "a write to a CONTINUE-mapped page leaked into another clone"
            );
            assert_eq!(
                first.backing_page(7),
                page_stamp(7),
                "a write to a CONTINUE-mapped page reached the shared backing"
            );
            (first, second)
        })
        .await
        .unwrap();
        assert_eq!(stats.faults(), 0);
        drop((first, second));

        assert_eq!(
            snap.image_digest(),
            digest_before,
            "the snapshot memory image must be byte-identical"
        );
    }

    /// A corrupt list must cost nothing but the prefetch: the clone restores on demand,
    /// with the right bytes, and never hangs.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_corrupt_working_set_falls_back_to_on_demand_faulting() {
        let snap = TestSnapshot::new("corrupt");
        std::fs::write(
            WorkingSetStore::path_for(&snap.mem),
            b"FCVMWSET this is not a working set",
        )
        .unwrap();

        let stats = snap.serve(UffdBacking::Copy).await;
        let clone = FakeClone::connect(&snap.socket, CLONE_PAGES * PAGE, UffdBacking::Copy);

        tokio::task::spawn_blocking(move || {
            for page in 0..CLONE_PAGES {
                assert_eq!(clone.read_page(page), page_stamp(page));
            }
            clone
        })
        .await
        .unwrap();

        assert_eq!(stats.prefetched_bytes(), 0, "nothing should be replayed");
        assert_eq!(
            stats.faults(),
            CLONE_PAGES as u64,
            "every page must have been served on demand"
        );
    }

    /// The recorder publishes on drop, from whatever exit path the handler took.
    #[test]
    fn fault_recorder_publishes_the_observed_set_on_drop() {
        let snap = TestSnapshot::new("recorder");
        let mem_len = (CLONE_PAGES * PAGE) as u64;
        let store = Arc::new(WorkingSetStore::open(&snap.mem, mem_len).unwrap());
        assert!(store.to_prefetch().is_empty());

        let mut recorder =
            FaultRecorder::new(Some(Arc::clone(&store)), "vm-0", std::time::Instant::now());
        record_fault(&mut recorder, 0, PAGE, 0);
        record_fault(&mut recorder, 9 * PAGE, PAGE, 0);
        assert!(
            store.to_prefetch().is_empty(),
            "nothing is published before the handler exits"
        );
        drop(recorder);

        // Published in memory for the clones this server is still hosting...
        assert_eq!(store.to_prefetch().len(), 2);
        // ...and on disk for the next server.
        let reopened = WorkingSetStore::open(&snap.mem, mem_len).unwrap();
        assert_eq!(
            reopened.to_prefetch().runs().collect::<Vec<_>>(),
            vec![
                Run {
                    offset: 0,
                    len: PAGE as u64
                },
                Run {
                    offset: 9 * PAGE as u64,
                    len: PAGE as u64
                },
            ]
        );
    }

    /// With recording disabled there is nothing to publish and nothing to replay.
    #[test]
    fn fault_recorder_without_a_store_records_nothing() {
        let mut recorder = FaultRecorder::new(None, "vm-0", std::time::Instant::now());
        record_fault(&mut recorder, 0, PAGE, 0);
        assert!(recorder.observed.is_empty());
    }
}

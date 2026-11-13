//! Prefilled hugepage guest memory for GPU restores, and the pool of zeroed spares it comes from.
//!
//! A passthrough device pins all guest memory for DMA when the snapshot load attaches it. On
//! the File backend every page is a 4 KiB copy-on-write of the memory file, faulted and pinned
//! on one thread: about 0.65 s per GiB on a g4dn.metal, 3.5 s of a 6 GiB phone VM's 5.6 s
//! restore. Memory made of 2 MiB hugetlbfs pages that are already allocated pins in
//! milliseconds, so a GPU restore here gives Firecracker a hugetlbfs file that already holds
//! the snapshot's memory ([`MemBackend::Prefilled`](crate::firecracker::api::MemBackend)).
//!
//! The file comes from a pool of spares: files on a hugetlbfs mount whose pages were allocated,
//! and so zeroed by the kernel, before any restore asked for one. A restore claims a spare,
//! copies the memory file's data into it on several threads (a hole in the memory file is left
//! as the spare's zeros), and passes it to Firecracker. With no spare it allocates the file
//! itself, which adds the kernel's zeroing to the restore.
//!
//! Lifecycle of one file, and why no step can leak memory or hand one tenant's memory to
//! another:
//!
//! | step | name in the pool directory | who frees it if its process is killed |
//! |---|---|---|
//! | creation: `O_EXCL` create, `flock`, allocate | `.tmp-<uuid>` | the next fill: an unlocked `.tmp-` file belongs to a dead creator and is unlinked |
//! | ready | `spare-<uuid>`, pristine zeros | nobody needs to: it is a spare |
//! | claim: open, `flock` without waiting, unlink | none once unlinked | the kernel, when the last descriptor or mapping goes |
//! | filled with the snapshot, mapped by Firecracker | none | the kernel, when Firecracker exits |
//!
//! A spare is renamed into place only once all of its pages are allocated, and a claimer
//! writes to it only after it has unlinked it under the file's lock, so a file that still has
//! a `spare-` name has never held guest memory. Trimming takes the same lock before it unlinks.
//!
//! The pool holds memory nothing else can use, so it is bounded twice: `fill` creates a spare
//! only while `MemAvailable` stays above the floor after it, and `trim` releases spares while
//! `MemAvailable` is below the floor. Spares are kept per NUMA node, the node of the GPU the
//! restore attaches, so guest memory sits beside the device that reads it.
//!
//! The kernel must be able to hand out hugepages: either a pool (`vm.nr_hugepages`, or per node)
//! or surplus pages (`vm.nr_overcommit_hugepages`), which return to the host's allocator when
//! the file holding them is freed. A host that can give neither fails the allocation cleanly,
//! and the restore falls back to the File backend.

use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use anyhow::{bail, Context, Result};
use tracing::{debug, info, warn};

/// The hugetlbfs page size the pool uses.
pub const HUGE_PAGE: u64 = 2 << 20;
const HUGETLBFS_MAGIC: i64 = 0x9584_58f6;
const SPARE_PREFIX: &str = "spare-";
const TMP_PREFIX: &str = ".tmp-";
/// Environment overrides.
pub const DIR_ENV: &str = "FCVM_HUGEPAGE_SPARES_DIR";
pub const FLOOR_ENV: &str = "FCVM_HUGEPAGE_SPARES_FLOOR_MIB";
pub const COUNT_ENV: &str = "FCVM_HUGEPAGE_SPARES";
/// The threads that copy a memory file into a spare, and that allocate a file on the restore
/// path. Each copies at a few GB/s from the page cache; eight cover a 6 GiB guest in about
/// 0.3 s.
pub const COPY_THREADS: usize = 8;

/// The pool's directory: `FCVM_HUGEPAGE_SPARES_DIR`, else `/dev/hugepages/fcvm-<uid>`.
pub fn default_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os(DIR_ENV) {
        return PathBuf::from(dir);
    }
    PathBuf::from(format!("/dev/hugepages/fcvm-{}", nix::unistd::geteuid()))
}

/// The memory a fill leaves to the host: `FCVM_HUGEPAGE_SPARES_FLOOR_MIB`, else the larger of
/// 8 GiB and a tenth of the host's memory.
pub fn default_floor_mib() -> Result<u64> {
    if let Ok(v) = std::env::var(FLOOR_ENV) {
        return v
            .parse()
            .with_context(|| format!("{FLOOR_ENV}={v:?} is not a number of MiB"));
    }
    Ok((meminfo_kib("MemTotal")? / 1024 / 10).max(8192))
}

/// Spares to keep for each node and size a restore used: `FCVM_HUGEPAGE_SPARES`, else 1.
pub fn default_count() -> Result<usize> {
    match std::env::var(COUNT_ENV) {
        Ok(v) => v
            .parse()
            .with_context(|| format!("{COUNT_ENV}={v:?} is not a count")),
        Err(_) => Ok(1),
    }
}

/// A field of `/proc/meminfo`, in KiB.
fn meminfo_kib(field: &str) -> Result<u64> {
    parse_meminfo(&std::fs::read_to_string("/proc/meminfo")?, field)
}

fn parse_meminfo(text: &str, field: &str) -> Result<u64> {
    text.lines()
        .find_map(|line| {
            let rest = line.strip_prefix(field)?.strip_prefix(':')?;
            rest.split_whitespace().next()?.parse().ok()
        })
        .with_context(|| format!("/proc/meminfo has no {field}"))
}

/// The NUMA node of a PCI device, or `None` when the host does not say (`-1`).
pub fn pci_numa_node(sbdf: &str) -> Option<u32> {
    let text = std::fs::read_to_string(format!("/sys/bus/pci/devices/{sbdf}/numa_node")).ok()?;
    let node: i64 = text.trim().parse().ok()?;
    u32::try_from(node).ok()
}

/// The pool: one directory on a 2 MiB hugetlbfs mount, a subdirectory per node and size.
pub struct Spares {
    dir: PathBuf,
}

impl Spares {
    /// The pool at `dir`, created if missing. Refuses a directory that is not on a hugetlbfs
    /// mount with 2 MiB pages.
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating the hugepage spare directory {}", dir.display()))?;
        let stat =
            nix::sys::statfs::statfs(dir).with_context(|| format!("statfs {}", dir.display()))?;
        if stat.filesystem_type().0 as i64 != HUGETLBFS_MAGIC {
            bail!("{} is not on a hugetlbfs mount", dir.display());
        }
        if stat.block_size() as u64 != HUGE_PAGE {
            bail!(
                "{} is on hugetlbfs with {}-byte pages, not 2 MiB",
                dir.display(),
                stat.block_size()
            );
        }
        Ok(Self {
            dir: dir.to_path_buf(),
        })
    }

    fn pool_dir(&self, node: Option<u32>, mib: u64) -> PathBuf {
        let node = node.map_or_else(|| "any".to_string(), |n| n.to_string());
        self.dir.join(format!("node-{node}-{mib}m"))
    }

    /// Spares ready for `node` and `mib`.
    pub fn count(&self, node: Option<u32>, mib: u64) -> usize {
        std::fs::read_dir(self.pool_dir(node, mib))
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .filter(|e| e.file_name().to_string_lossy().starts_with(SPARE_PREFIX))
                    .count()
            })
            .unwrap_or(0)
    }

    /// Take one spare for `node` and `mib`: a file of `mib` MiB of allocated, zeroed hugepages
    /// that no longer has a name, so only the returned descriptor (and whatever maps it) holds
    /// it. `None` when the pool has none.
    pub fn claim(&self, node: Option<u32>, mib: u64) -> Result<Option<File>> {
        let dir = self.pool_dir(node, mib);
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("listing {}", dir.display())),
        };
        for entry in entries.filter_map(|e| e.ok()) {
            if !entry
                .file_name()
                .to_string_lossy()
                .starts_with(SPARE_PREFIX)
            {
                continue;
            }
            if let Some(file) = take_locked(&entry.path(), mib << 20)? {
                return Ok(Some(file));
            }
        }
        Ok(None)
    }

    /// Create spares for `node` and `mib` until there are `count`, as long as each leaves the
    /// host at least `floor_mib` of `MemAvailable`. Holds the pool's lock, so concurrent fills
    /// add up to `count`, not one each. Returns the spares created.
    pub fn fill(&self, node: Option<u32>, mib: u64, count: usize, floor_mib: u64) -> Result<usize> {
        let _lock = self.lock()?;
        self.reap_stale()?;
        let mut created = 0;
        while self.count(node, mib) < count {
            let available = meminfo_kib("MemAvailable")? / 1024;
            if available < mib + floor_mib {
                info!(
                    node = ?node,
                    mib,
                    available_mib = available,
                    floor_mib,
                    "not creating a hugepage spare: it would leave less than the floor"
                );
                break;
            }
            let started = Instant::now();
            self.create(node, mib)?;
            created += 1;
            info!(
                node = ?node,
                mib,
                ms = started.elapsed().as_millis() as u64,
                "created a hugepage spare"
            );
        }
        Ok(created)
    }

    /// Release spares, largest first, while `MemAvailable` is below `floor_mib`. Returns the
    /// MiB released.
    pub fn trim(&self, floor_mib: u64) -> Result<u64> {
        let _lock = self.lock()?;
        self.reap_stale()?;
        let mut released = 0;
        while meminfo_kib("MemAvailable")? / 1024 < floor_mib {
            let Some((path, mib)) = self.largest_spare()? else {
                break;
            };
            // Unlinked under its lock, so a claimer cannot be holding it.
            if take_locked(&path, mib << 20)?.is_some() {
                released += mib;
                info!(spare = %path.display(), mib, "released a hugepage spare: below the floor");
            }
        }
        Ok(released)
    }

    /// Every spare, as `(node, mib, count)`.
    pub fn status(&self) -> Result<Vec<(String, u64, usize)>> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&self.dir)?.filter_map(|e| e.ok()) {
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some((node, mib)) = parse_pool_dir(&name) {
                let node_id = node.parse().ok();
                out.push((node, mib, self.count(node_id, mib)));
            }
        }
        out.sort();
        Ok(out)
    }

    fn largest_spare(&self) -> Result<Option<(PathBuf, u64)>> {
        let mut best: Option<(PathBuf, u64)> = None;
        for entry in std::fs::read_dir(&self.dir)?.filter_map(|e| e.ok()) {
            let name = entry.file_name().to_string_lossy().to_string();
            let Some((_, mib)) = parse_pool_dir(&name) else {
                continue;
            };
            if best.as_ref().is_some_and(|(_, b)| *b >= mib) {
                continue;
            }
            for spare in std::fs::read_dir(entry.path())?.filter_map(|e| e.ok()) {
                if spare
                    .file_name()
                    .to_string_lossy()
                    .starts_with(SPARE_PREFIX)
                {
                    best = Some((spare.path(), mib));
                    break;
                }
            }
        }
        Ok(best)
    }

    /// One new spare: allocated under a temporary name and renamed into place only once every
    /// page is allocated.
    fn create(&self, node: Option<u32>, mib: u64) -> Result<()> {
        let dir = self.pool_dir(node, mib);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let id = uuid::Uuid::new_v4().simple().to_string();
        let tmp = dir.join(format!("{TMP_PREFIX}{id}"));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        // Held until the rename: a fill that finds this file unlocked knows its creator died.
        lock_now(&file)
            .context("locking a new hugepage spare")?
            .then_some(())
            .context("a new hugepage spare was locked by someone else")?;
        if let Err(error) = allocate(&file, mib << 20, node, COPY_THREADS) {
            let _ = std::fs::remove_file(&tmp);
            return Err(error);
        }
        let ready = dir.join(format!("{SPARE_PREFIX}{id}"));
        std::fs::rename(&tmp, &ready)
            .with_context(|| format!("publishing hugepage spare {}", ready.display()))?;
        Ok(())
    }

    /// Unlink every `.tmp-` file whose creator is gone (its lock is free).
    fn reap_stale(&self) -> Result<()> {
        for entry in std::fs::read_dir(&self.dir)?.filter_map(|e| e.ok()) {
            if parse_pool_dir(&entry.file_name().to_string_lossy()).is_none() {
                continue;
            }
            for tmp in std::fs::read_dir(entry.path())?.filter_map(|e| e.ok()) {
                if !tmp.file_name().to_string_lossy().starts_with(TMP_PREFIX) {
                    continue;
                }
                if take_locked(&tmp.path(), 0)?.is_some() {
                    warn!(file = %tmp.path().display(), "removed a hugepage spare its creator left unfinished");
                }
            }
        }
        Ok(())
    }

    fn lock(&self) -> Result<nix::fcntl::Flock<File>> {
        let path = self.dir.join(".lock");
        // hugetlbfs cannot hold a regular file's data, but an empty file locks like any other.
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusive)
            .map_err(|(_, e)| e)
            .context("locking the hugepage spare pool")
    }
}

/// Take `file`'s exclusive `flock` without waiting: `false` when someone else holds it. The
/// lock belongs to the open file, so it lasts as long as the descriptor (and its duplicates,
/// a VMM's included) stays open.
fn lock_now(file: &File) -> std::io::Result<bool> {
    // SAFETY: flock on a descriptor `file` owns.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
        Ok(false)
    } else {
        Err(error)
    }
}

/// `node-<node>-<mib>m` as `(node, mib)`.
fn parse_pool_dir(name: &str) -> Option<(String, u64)> {
    let rest = name.strip_prefix("node-")?;
    let (node, size) = rest.rsplit_once('-')?;
    let mib = size.strip_suffix('m')?.parse().ok()?;
    Some((node.to_string(), mib))
}

/// Open `path`, take its lock without waiting and unlink it. `None` when someone else holds
/// the lock (a claimer or a creator) or it is already gone. With `size` non-zero, a file of
/// another size is refused and left in place.
fn take_locked(path: &Path, size: u64) -> Result<Option<File>> {
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("opening {}", path.display())),
    };
    if !lock_now(&file).with_context(|| format!("locking {}", path.display()))? {
        return Ok(None);
    }
    // Under the lock the name still names this inode: everyone unlinks only under it.
    let named = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("stat {}", path.display())),
    };
    let held = file.metadata()?;
    if named.ino() != held.ino() || named.dev() != held.dev() {
        return Ok(None);
    }
    if size != 0 && held.len() != size {
        warn!(file = %path.display(), len = held.len(), size, "a hugepage spare has the wrong size; leaving it");
        return Ok(None);
    }
    std::fs::remove_file(path).with_context(|| format!("unlinking {}", path.display()))?;
    Ok(Some(file))
}

/// A file of `size` bytes of allocated (and so zeroed) hugepages on `node`, with no name. The
/// restore's fallback when the pool has no spare: the allocation and the kernel's zeroing then
/// run here, on `threads` threads.
pub fn allocate_unnamed(dir: &Path, size: u64, node: Option<u32>, threads: usize) -> Result<File> {
    let spares = Spares::open(dir)?;
    let path = spares
        .dir
        .join(format!("{TMP_PREFIX}{}", uuid::Uuid::new_v4().simple()));
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&path)
        .with_context(|| format!("creating {}", path.display()))?;
    // Unnamed before any page exists, so nothing can leak.
    std::fs::remove_file(&path).with_context(|| format!("unlinking {}", path.display()))?;
    allocate(&file, size, node, threads)?;
    Ok(file)
}

/// Size `file` to `size` and allocate every page, on `threads` threads bound to `node`.
///
/// The pages are faulted in through one shared mapping, a piece per thread
/// (`MADV_POPULATE_WRITE`), so the kernel allocates and zeroes them in parallel: `fallocate`
/// on hugetlbfs holds the inode's lock and zeroed a 6 GiB file on one thread in about 1.75 s.
/// The mapping is `MAP_NORESERVE`, so nothing is reserved up front and a host that runs out of
/// hugepages fails the populate with an error instead of signalling a later access.
fn allocate(file: &File, size: u64, node: Option<u32>, threads: usize) -> Result<()> {
    if size == 0 || !size.is_multiple_of(HUGE_PAGE) {
        bail!("a hugepage file must be a positive multiple of 2 MiB, not {size} bytes");
    }
    file.set_len(size).context("sizing a hugepage file")?;
    // SAFETY: a shared mapping of `file`, unmapped before returning.
    let map = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            size as usize,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED | libc::MAP_NORESERVE,
            file.as_raw_fd(),
            0,
        )
    };
    if map == libc::MAP_FAILED {
        return Err(std::io::Error::last_os_error()).context("mapping a hugepage file");
    }
    let base = map as usize;
    let pieces = split(0, size, threads.max(1));
    let result = std::thread::scope(|scope| -> Result<()> {
        let workers: Vec<_> = pieces
            .iter()
            .map(|&(offset, len)| {
                scope.spawn(move || -> Result<()> {
                    if let Some(node) = node {
                        bind_thread_memory(node)?;
                    }
                    // SAFETY: `offset..offset + len` is inside the mapping made above.
                    let ret = unsafe {
                        libc::madvise(
                            (base + offset as usize) as *mut libc::c_void,
                            len as usize,
                            libc::MADV_POPULATE_WRITE,
                        )
                    };
                    if ret != 0 {
                        return Err(std::io::Error::last_os_error()).with_context(|| {
                            format!(
                                "allocating {} MiB of hugepages{}",
                                len >> 20,
                                node.map_or_else(String::new, |n| format!(" on node {n}"))
                            )
                        });
                    }
                    Ok(())
                })
            })
            .collect();
        for worker in workers {
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("a hugepage allocation thread panicked"))??;
        }
        Ok(())
    });
    // SAFETY: unmapping the mapping made above.
    unsafe { libc::munmap(map, size as usize) };
    result
}

/// Allocate this thread's memory on `node` only.
fn bind_thread_memory(node: u32) -> Result<()> {
    const MPOL_BIND: libc::c_long = 2;
    let mut mask = [0u64; 16];
    let word = (node / 64) as usize;
    if word >= mask.len() {
        bail!("NUMA node {node} is out of range");
    }
    mask[word] = 1 << (node % 64);
    // SAFETY: set_mempolicy reads `mask` for the given number of bits.
    let ret = unsafe {
        libc::syscall(
            libc::SYS_set_mempolicy,
            MPOL_BIND,
            mask.as_ptr(),
            (mask.len() * 64 + 1) as libc::c_ulong,
        )
    };
    if ret != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("binding memory to NUMA node {node}"));
    }
    Ok(())
}

/// `[start, end)` cut into at most `parts` pieces, each a multiple of 2 MiB but the last.
fn split(start: u64, end: u64, parts: usize) -> Vec<(u64, u64)> {
    let len = end - start;
    let per = len.div_ceil(parts as u64).div_ceil(HUGE_PAGE) * HUGE_PAGE;
    let mut out = Vec::new();
    let mut at = start;
    while at < end {
        let n = per.min(end - at);
        out.push((at, n));
        at += n;
    }
    out
}

/// What [`fill_from`] copied.
#[derive(Debug, Clone, Copy, Default)]
pub struct FillStats {
    pub data_mib: u64,
    pub hole_mib: u64,
    pub ms: u64,
}

/// Copy `source`'s data into `dest`, a zeroed file at least as large, on `threads` threads.
/// A hole in `source` is skipped and reads as the destination's zeros.
pub fn fill_from(dest: &File, source: &Path, threads: usize) -> Result<FillStats> {
    let started = Instant::now();
    let src = File::open(source).with_context(|| format!("opening {}", source.display()))?;
    let size = src.metadata()?.len();
    if dest.metadata()?.len() < size {
        bail!("the hugepage file is smaller than {}", source.display());
    }
    let extents = data_extents(&src, size)?;
    let data: u64 = extents.iter().map(|(_, len)| len).sum();
    // 32 MiB pieces, taken in turn by the threads.
    const PIECE: u64 = 32 << 20;
    let pieces: Vec<(u64, u64)> = extents
        .iter()
        .flat_map(|&(offset, len)| {
            (0..len.div_ceil(PIECE)).map(move |i| {
                let at = offset + i * PIECE;
                (at, PIECE.min(offset + len - at))
            })
        })
        .collect();
    // SAFETY: a shared mapping of `dest`, unmapped before returning.
    let map = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            size as usize,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            dest.as_raw_fd(),
            0,
        )
    };
    if map == libc::MAP_FAILED {
        return Err(std::io::Error::last_os_error()).context("mapping the hugepage file");
    }
    let base = map as usize;
    let next = AtomicUsize::new(0);
    let result = std::thread::scope(|scope| -> Result<()> {
        let workers: Vec<_> = (0..threads.max(1))
            .map(|_| {
                let (src, pieces, next) = (&src, &pieces, &next);
                scope.spawn(move || -> Result<()> {
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(&(offset, len)) = pieces.get(i) else {
                            return Ok(());
                        };
                        // SAFETY: `offset..offset + len` is inside the mapping, and pieces do
                        // not overlap.
                        let buf = unsafe {
                            std::slice::from_raw_parts_mut(
                                (base + offset as usize) as *mut u8,
                                len as usize,
                            )
                        };
                        src.read_exact_at(buf, offset)
                            .with_context(|| format!("reading the memory file at {offset:#x}"))?;
                    }
                })
            })
            .collect();
        for worker in workers {
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("a hugepage fill thread panicked"))??;
        }
        Ok(())
    });
    // SAFETY: unmapping the mapping made above.
    unsafe { libc::munmap(map, size as usize) };
    result?;
    let stats = FillStats {
        data_mib: data >> 20,
        hole_mib: (size - data) >> 20,
        ms: started.elapsed().as_millis() as u64,
    };
    debug!(?stats, "filled hugepage guest memory from the memory file");
    Ok(stats)
}

/// `(offset, len)` of every data extent of `file` (`SEEK_DATA`/`SEEK_HOLE`), the whole file
/// when the filesystem does not report holes.
fn data_extents(file: &File, size: u64) -> Result<Vec<(u64, u64)>> {
    let fd = file.as_raw_fd();
    let mut out = Vec::new();
    let mut at: i64 = 0;
    while (at as u64) < size {
        // SAFETY: lseek on a descriptor this function borrows.
        let data = unsafe { libc::lseek(fd, at, libc::SEEK_DATA) };
        if data < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ENXIO) {
                break;
            }
            return Err(error).context("SEEK_DATA on the memory file");
        }
        // SAFETY: as above.
        let hole = unsafe { libc::lseek(fd, data, libc::SEEK_HOLE) };
        if hole < 0 {
            return Err(std::io::Error::last_os_error()).context("SEEK_HOLE on the memory file");
        }
        let end = (hole as u64).min(size);
        if end > data as u64 {
            out.push((data as u64, end - data as u64));
        }
        at = hole;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meminfo_fields_parse() {
        let text = "MemTotal:       395842220 kB\nMemFree:        311000000 kB\nMemAvailable:   377000000 kB\n";
        assert_eq!(parse_meminfo(text, "MemAvailable").unwrap(), 377_000_000);
        assert_eq!(parse_meminfo(text, "MemTotal").unwrap(), 395_842_220);
        assert!(parse_meminfo(text, "Mem").is_err());
    }

    #[test]
    fn pool_directory_names_round_trip() {
        assert_eq!(parse_pool_dir("node-0-6144m"), Some(("0".into(), 6144)));
        assert_eq!(parse_pool_dir("node-any-2048m"), Some(("any".into(), 2048)));
        assert_eq!(parse_pool_dir(".lock"), None);
        assert_eq!(parse_pool_dir("node-1-6144"), None);
    }

    #[test]
    fn split_covers_the_range_in_huge_pages() {
        for (len, parts) in [(6144u64 << 20, 8usize), (2 << 20, 8), (10 << 20, 3)] {
            let pieces = split(0, len, parts);
            assert!(pieces.len() <= parts);
            let mut at = 0;
            for (i, &(off, n)) in pieces.iter().enumerate() {
                assert_eq!(off, at);
                if i + 1 < pieces.len() {
                    assert_eq!(n % HUGE_PAGE, 0);
                }
                at += n;
            }
            assert_eq!(at, len);
        }
    }

    /// The claim protocol on an ordinary directory (no hugetlbfs needed): a locked file is
    /// never taken, a free one is taken exactly once and loses its name, and a file of the
    /// wrong size is left alone.
    #[test]
    fn a_spare_is_taken_once_and_never_while_locked() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("spare-x");
        let file = File::create(&path).unwrap();
        file.set_len(4096).unwrap();
        assert!(lock_now(&file).unwrap());
        assert!(
            take_locked(&path, 4096).unwrap().is_none(),
            "took a locked spare"
        );
        assert!(path.exists());
        drop(file);
        assert!(
            take_locked(&path, 8192).unwrap().is_none(),
            "took a spare of the wrong size"
        );
        let taken = take_locked(&path, 4096)
            .unwrap()
            .expect("a free spare was not taken");
        assert!(!path.exists(), "a taken spare kept its name");
        assert!(take_locked(&path, 4096).unwrap().is_none());
        // The taker holds the lock for as long as it holds the file.
        let again = File::open(dir.path()).unwrap();
        drop(again);
        drop(taken);
    }

    /// A fill copies the data and leaves the source's holes as the destination's zeros.
    #[test]
    fn fill_copies_data_and_leaves_holes_zero() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("memory.bin");
        let src = File::create(&source).unwrap();
        let size = 16u64 << 20;
        src.set_len(size).unwrap();
        src.write_all_at(&[0x5a; 4096], 0).unwrap();
        src.write_all_at(&vec![0xa5; 3 << 20], 9 << 20).unwrap();
        drop(src);
        let dest = tempfile::tempfile_in(dir.path()).unwrap();
        dest.set_len(size).unwrap();
        // A non-zero destination proves the hole was not written.
        dest.write_all_at(&vec![0x11; size as usize], 0).unwrap();
        let stats = fill_from(&dest, &source, 4).unwrap();
        let mut got = vec![0u8; size as usize];
        dest.read_exact_at(&mut got, 0).unwrap();
        assert!(got[..4096].iter().all(|&b| b == 0x5a));
        assert!(got[9 << 20..12 << 20].iter().all(|&b| b == 0xa5));
        if stats.hole_mib > 0 {
            // tmpfs and most filesystems report the holes; the hole stayed as the file had it.
            assert!(got[1 << 20..8 << 20].iter().all(|&b| b == 0x11));
        }
    }
}

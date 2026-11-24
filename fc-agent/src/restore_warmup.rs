//! Read, before a snapshot is taken, files a clone's restore path executes and maps, so they
//! are in the guest's page cache when its memory is captured.
//!
//! A restore restarts journald and chronyd and runs `systemctl` and `chronyc`. The snapshot's
//! guest has usually never run them, so every clone read them, their libraries and the
//! journal from its own disk: the same bytes in every clone, and each clone's disk is its own
//! file on the host. Read here, they come back with the snapshot's memory instead: through
//! `memory.bin` on the File backend, and on the UFFD backend through the pages a clone faults,
//! which the recorded working set then replays.
//!
//! This only reads, and it reads a bounded amount: at most [`WARMUP_MAX_BYTES`], within
//! [`WARMUP_DEADLINE`], which planning and reading both check. It reads only into free memory:
//! its headroom is the guest's free memory above the kernel's high watermarks (`MemFree` less
//! the zones' `high` in `/proc/zoneinfo`), below which page reclaim, and so eviction of the
//! workload's own cache, begins. Under [`PLANNING_MAX_BYTES`] of headroom it reads nothing at
//! all, not even to plan; planning reads ELF headers, unit and chrony files and journal
//! headers (at most about 80 KiB per executable or library, 160 bytes per journal); and the
//! reads proper happen only when the headroom holds the planned total, so they never push the
//! guest into reclaim. The rule scales with the guest rather than being a fixed margin, which
//! a small guest could never pass. An overlay root, or any
//! root whose filesystem is not in [`LOCAL_FS_TYPES`], reads nothing either. It starts,
//! stops, flushes and writes nothing. A page-cache read is what any process in the guest
//! does, the kernel can drop the pages again, and a file that cannot be read is skipped.
//!
//! What it reads, in this order, until a bound stops it:
//! 1. each program in [`RESTORE_PROGRAMS`], resolved the way the spawn resolves it (an
//!    absolute name as is, a bare name through this process's `PATH`, which the agent's exec
//!    server hands on from the agent's own environment), `systemd-executor`, the `ExecStart=`
//!    executables of the unit the restore restarts, and the dynamic-library closure of each,
//!    read from its ELF headers;
//! 2. chronyd's configuration and the files it names;
//! 3. the journal files: of the live ones the parts a journald that starts reads
//!    ([`journal_ranges`]), of each archive its header.
//!
//! Every path is resolved one component at a time and read only when each component lies on a
//! local filesystem by `/proc/self/mountinfo` ([`LOCAL_FS_TYPES`]): a read on a wedged NFS or
//! FUSE mount sleeps uninterruptibly, and this runs before the snapshot's pause.
//!
//! [`RESTORE_PROGRAMS`] is kept by hand. What checks it is a scan of every `Command::new` in
//! the non-test code of fc-agent: every source file that spawns is either pinned by a test of
//! its own (`restore.rs`, `snapshot_network.rs`, `mounts.rs`, `network.rs`, `proxy.rs`,
//! `retired_address.rs`, `agent.rs`, `gpu.rs`, `gpu_park.rs`), which fixes that module's exact
//! set of spawn expressions, or named, with its reason, as off the restore path
//! (`container.rs`, `tty.rs`, `system.rs`). A literal program must be in the registry, and a
//! spawn whose program is a variable lists the programs fed to it, each of which must appear
//! in that module. A new spawn site, in any file, fails a test until it is registered or
//! excluded. A program started inside a `sh -c` script is not seen by the scan.

use std::ffi::OsString;
use std::os::unix::fs::FileExt;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

/// A program the restore path may spawn, and what spawns it.
pub struct Program {
    /// The name the spawn passes: absolute, or bare and resolved through `PATH`.
    pub name: &'static str,
    pub spawned_by: &'static str,
    pub warm: Warm,
}

/// Whether the warm-up reads a program.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Warm {
    Always,
    /// Only when the guest has an NFS mount when the snapshot is taken: the restore remounts
    /// only the shares the guest had.
    IfNfs,
    /// Only when the guest has the GPU driver image mounted at `gpu::DRIVER_ROOT`: only such
    /// a guest's clones attach GPUs.
    IfGpu,
    /// Never: see the entry.
    Never,
}

/// Every program the restore path may spawn, in the order the warm-up reads them: the ones
/// every restore runs first.
pub const RESTORE_PROGRAMS: &[Program] = &[
    Program {
        name: "systemctl",
        spawned_by: "restore::restart_journald, agent::stop_running_chronyd",
        warm: Warm::Always,
    },
    Program {
        name: "/usr/sbin/chronyd",
        spawned_by: "agent::spawn_chronyd",
        warm: Warm::Always,
    },
    Program {
        name: "/usr/bin/chronyc",
        spawned_by: "agent::chronyc_sources, snapshot_network::chronyc",
        warm: Warm::Always,
    },
    Program {
        name: "ip",
        spawned_by: "network.rs, retired_address.rs",
        warm: Warm::Always,
    },
    Program {
        name: "sysctl",
        spawned_by: "network.rs",
        warm: Warm::Always,
    },
    Program {
        name: "iptables",
        spawned_by: "network.rs, proxy.rs, snapshot_network's gate runner",
        warm: Warm::Always,
    },
    Program {
        name: "ip6tables",
        spawned_by: "proxy.rs, snapshot_network's gate runner, retired_address.rs",
        warm: Warm::Always,
    },
    Program {
        name: "iptables-restore",
        spawned_by: "snapshot_network's gate runner",
        warm: Warm::Always,
    },
    Program {
        name: "ip6tables-restore",
        spawned_by: "snapshot_network's gate runner, retired_address.rs",
        warm: Warm::Always,
    },
    Program {
        name: "conntrack",
        spawned_by: "snapshot_network's gate runner",
        warm: Warm::Always,
    },
    Program {
        name: "umount",
        spawned_by: "restore.rs and mounts.rs, for NFS shares",
        warm: Warm::IfNfs,
    },
    Program {
        name: "mount",
        spawned_by: "mounts.rs, for NFS shares",
        warm: Warm::IfNfs,
    },
    Program {
        name: "/sbin/mount.nfs",
        spawned_by: "`mount -t nfs`, which execs it",
        warm: Warm::IfNfs,
    },
    // The restore hook's runner (snapshot_network::run_bounded). `timeout`, on the hook's
    // argv, is not here: `podman exec` runs the container image's copy, not the guest's.
    // podman is not read: it is 34 MiB in the guest image, more than everything else here
    // together, a hook is chosen at restore time and most clones run none, and the pages
    // its run of the container touched at boot are cached already.
    Program {
        name: "env",
        spawned_by: "the restore hook, rootless prefix (container::run_as_user_prefix)",
        warm: Warm::Always,
    },
    Program {
        name: "runuser",
        spawned_by: "the restore hook, rootless prefix",
        warm: Warm::Always,
    },
    Program {
        name: "podman",
        spawned_by: "the restore hook (snapshot_network::run_bounded)",
        warm: Warm::Never,
    },
    Program {
        name: "conmon",
        spawned_by: "`podman exec`",
        warm: Warm::Always,
    },
    Program {
        name: "crun",
        spawned_by: "`podman exec`",
        warm: Warm::Always,
    }, // GPU attach (gpu.rs, and gpu_park.rs, which it calls to unpark), on a clone given GPUs.
    Program {
        name: "dmesg",
        spawned_by: "gpu::attach",
        warm: Warm::IfGpu,
    },
    Program {
        name: "insmod",
        spawned_by: "gpu::attach",
        warm: Warm::IfGpu,
    },
    Program {
        name: "/run/fcvm-gpu/driver/usr/bin/nvidia-smi",
        spawned_by: "gpu::attach, gpu_park",
        warm: Warm::IfGpu,
    },
    Program {
        name: "/run/fcvm-gpu/driver/usr/bin/nvidia-ctk",
        spawned_by: "gpu::attach",
        warm: Warm::IfGpu,
    },
    Program {
        name: "/run/fcvm-gpu/driver/usr/bin/fcvm-gpu-scrub",
        spawned_by: "gpu::attach (gpu::SCRUB_TOOL)",
        warm: Warm::IfGpu,
    },
    Program {
        name: "/run/fcvm-gpu/driver/usr/bin/nvidia-persistenced",
        spawned_by: "gpu::attach",
        warm: Warm::IfGpu,
    },
    Program {
        name: "/run/fcvm-gpu/driver/usr/bin/cuda-checkpoint",
        spawned_by: "gpu_park::unpark",
        warm: Warm::IfGpu,
    },
];

/// The unit the restore restarts: its `ExecStart=` executables run on every restore.
pub const JOURNALD_UNIT: &str = "systemd-journald";

/// The whole warm-up reads at most this much. In the guest image (Ubuntu 24.04, arm64) it
/// read 41.8 MiB in 64 files on a fresh guest, and 42.8 MiB in 67 files with a 40 MiB live
/// journal and three archives. Most of it is libraries of the registered programs
/// (libcrypto 4.4 MiB, libsystemd-shared 3.8 MiB, libgnutls 2.1 MiB, ...).
pub const WARMUP_MAX_BYTES: u64 = 64 << 20;
/// The warm-up stops planning and reading at this deadline. Those reads took 24 to 96 ms
/// from a cold guest page cache. The host's exec of the prepare one-shot has no timeout of
/// its own, so this is what bounds the time the warm-up adds to `snapshot create`.
pub const WARMUP_DEADLINE: Duration = Duration::from_secs(2);
/// The warm-up plans nothing unless the guest's headroom ([`headroom`]) is at least this, a
/// bound on what planning itself reads.
pub const PLANNING_MAX_BYTES: u64 = 8 << 20;

/// A live journal whose used part is at most this size is read whole: journald allocates its
/// files 8 MiB at a time, and a fresh guest's `system.journal` is one such file.
pub const JOURNAL_WHOLE_FILE_BYTES: u64 = 8 << 20;
/// Of a larger live journal, the last this many bytes of its used part are read, besides
/// its header and hash tables. A cold start of journald on a 40 MiB journal read 7.0 MiB of
/// it: the header and hash tables (3.6 MiB of it), 0.25 MiB in the last 1 MiB, and the rest
/// in 128 KiB pieces across the file that only parsing its objects would find.
pub const JOURNAL_TAIL_BYTES: u64 = 1 << 20;
/// Of each archived journal, the first this many bytes: the same start read 16 KiB of each.
pub const JOURNAL_ARCHIVE_HEAD_BYTES: u64 = 16 << 10;
/// At most this many archives are read: journald's default `SystemMaxFiles`.
pub const JOURNAL_MAX_ARCHIVES: usize = 100;
/// At most this many live journals are read, the most recently modified first. The traced
/// guest had one (`system.journal`); a guest has one more per user who has logged.
pub const JOURNAL_MAX_LIVE: usize = 16;
/// At most this many directory entries under the journal directory are looked at.
pub const JOURNAL_MAX_ENTRIES: usize = 4096;

/// Filesystems a read cannot park on: local block filesystems and memory.
pub const LOCAL_FS_TYPES: &[&str] = &["ext4", "btrfs", "xfs", "tmpfs", "ramfs", "rootfs"];

/// What systemd runs for a unit it starts, besides the unit's own executable.
const SYSTEMD_EXECUTOR: &str = "/usr/lib/systemd/systemd-executor";
const UNIT_DIRS: &[&str] = &[
    "/etc/systemd/system",
    "/usr/lib/systemd/system",
    "/lib/systemd/system",
];
const JOURNAL_DIR: &str = "/var/log/journal";
/// The configuration chronyd is started with (`agent::CHRONY_CONF`).
const CHRONY_CONF: &str = "/etc/chrony.conf";
/// What `execvp` searches when `PATH` is unset.
const DEFAULT_PATH: &str = "/bin:/usr/bin";
const LIBRARY_DIRS: &[&str] = &[
    "/usr/lib/aarch64-linux-gnu",
    "/lib/aarch64-linux-gnu",
    "/usr/lib/x86_64-linux-gnu",
    "/lib/x86_64-linux-gnu",
    "/usr/lib",
    "/lib",
    "/lib64",
];
/// The kernel's own bound on symlinks followed in one lookup.
const MAX_SYMLINKS: usize = 40;
/// ELF parsing reads at most this much of a dynamic section, this much of one string, and
/// this many program headers, so the bytes parsing reads from one file are bounded by
/// 64 + 64 * 56 + 4 KiB + 64 KiB + 4 KiB per name it needs.
const ELF_MAX_DYNAMIC: usize = 64 << 10;
const ELF_MAX_STRING: usize = 4 << 10;
const ELF_MAX_PHNUM: usize = 64;
/// Every read goes through one buffer of this size.
const READ_CHUNK: usize = 1 << 20;

/// The registered programs the warm-up reads, given whether the guest has an NFS mount.
fn programs_to_warm(nfs: bool, gpu: bool) -> impl Iterator<Item = &'static Program> {
    RESTORE_PROGRAMS.iter().filter(move |p| match p.warm {
        Warm::Always => true,
        Warm::IfNfs => nfs,
        Warm::IfGpu => gpu,
        Warm::Never => false,
    })
}

/// Add all of `path` to `plan` unless it is there already or is not a regular file.
fn add_whole(plan: &mut Vec<FileRead>, path: PathBuf) {
    if plan.iter().any(|r| r.path == path) {
        return;
    }
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.is_file() {
            plan.push(FileRead {
                ranges: vec![(0, meta.len())],
                path,
            });
        }
    }
}

/// What a warm-up did, for its one log line.
#[derive(Debug, Default, PartialEq)]
pub struct Report {
    pub files: usize,
    pub bytes: u64,
    pub elapsed: Duration,
    /// Programs that did not resolve to a file, each with what spawns it.
    pub not_found: Vec<String>,
    /// Paths that were not read because a component is not on a local filesystem.
    pub not_local: Vec<PathBuf>,
    /// Set when nothing was read because memory was short, or when a bound stopped the reads.
    pub stopped: Option<String>,
    /// `MemFree` and the zones' summed high watermarks when it started, in bytes.
    pub memory: Option<(u64, u64)>,
}

impl std::fmt::Display for Report {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "read {} files ({} KiB) a restore runs into the page cache in {} ms",
            self.files,
            self.bytes / 1024,
            self.elapsed.as_millis()
        )?;
        if !self.not_found.is_empty() {
            write!(f, "; not found: {}", self.not_found.join(", "))?;
        }
        if !self.not_local.is_empty() {
            let shown: Vec<_> = self
                .not_local
                .iter()
                .map(|p| p.display().to_string())
                .collect();
            write!(f, "; not on a local filesystem: {}", shown.join(", "))?;
        }
        if let Some(why) = &self.stopped {
            write!(f, "; {why}")?;
        }
        if let Some((free, high)) = self.memory {
            write!(
                f,
                "; MemFree {} KiB, high watermarks {} KiB",
                free / 1024,
                high / 1024
            )?;
        }
        Ok(())
    }
}

/// Read what a restore executes and maps into the page cache, within the bounds above.
pub fn warm() -> Report {
    let started = Instant::now();
    let Some(mounts) = std::fs::read_to_string("/proc/self/mountinfo")
        .ok()
        .map(|text| Mounts::parse(&text))
    else {
        return Report {
            stopped: Some("skipped: /proc/self/mountinfo is unreadable".into()),
            ..Default::default()
        };
    };
    let env = Env {
        mounts: &mounts,
        path: std::env::var_os("PATH").unwrap_or_else(|| DEFAULT_PATH.into()),
        journal_dir: Path::new(JOURNAL_DIR),
        chrony_conf: Path::new(CHRONY_CONF),
        deadline: started + WARMUP_DEADLINE,
    };
    let memory = memory();
    let mut report = warm_in(
        &env,
        memory.map(|(free, high)| free.saturating_sub(high)),
        started,
    );
    report.memory = memory;
    report
}

/// [`warm`] in `env`, given the guest's memory headroom. It is checked before anything is
/// planned, so a guest without room for planning reads nothing at all, and again on the
/// planned total.
fn warm_in(env: &Env, headroom: Option<u64>, started: Instant) -> Report {
    if headroom.is_none_or(|room| room < PLANNING_MAX_BYTES) {
        return Report {
            stopped: Some(format!(
                "skipped before planning: {} KiB free above the high watermarks, under {} KiB",
                headroom.map_or("unknown".into(), |a| (a / 1024).to_string()),
                PLANNING_MAX_BYTES / 1024
            )),
            elapsed: started.elapsed(),
            ..Default::default()
        };
    }
    let mut report = Report::default();
    let plan = plan(env, &mut report);
    let mut report = execute(
        &plan,
        report,
        Bounds {
            max_bytes: WARMUP_MAX_BYTES,
            deadline: env.deadline,
            headroom,
        },
    );
    report.elapsed = started.elapsed();
    report
}

/// Where a plan is built from; tests point it at a directory of their own.
struct Env<'a> {
    mounts: &'a Mounts,
    path: OsString,
    journal_dir: &'a Path,
    chrony_conf: &'a Path,
    /// Planning stops adding files at this instant too.
    deadline: Instant,
}

/// One file to read, and the byte ranges of it.
#[derive(Debug, PartialEq)]
struct FileRead {
    path: PathBuf,
    ranges: Vec<(u64, u64)>,
}

impl FileRead {
    fn bytes(&self) -> u64 {
        self.ranges.iter().map(|(start, end)| end - start).sum()
    }
}

fn plan(env: &Env, report: &mut Report) -> Vec<FileRead> {
    let mut plan: Vec<FileRead> = Vec::new();
    let mut executables = Vec::new();
    let gpu = env.mounts.has_mount_at(Path::new(crate::gpu::DRIVER_ROOT));
    for program in programs_to_warm(env.mounts.has_nfs(), gpu) {
        match find_program(program.name, &env.path, env.mounts, &mut report.not_local) {
            Some(path) => executables.push(path),
            None => report
                .not_found
                .push(format!("{} ({})", program.name, program.spawned_by)),
        }
    }
    let mut extra: Vec<PathBuf> = vec![PathBuf::from(SYSTEMD_EXECUTOR)];
    extra.extend(unit_executables(JOURNALD_UNIT, env.mounts));
    if let Ok(me) = std::fs::read_link("/proc/self/exe") {
        extra.push(me);
    }
    for path in extra {
        match resolve(&path, env.mounts) {
            Ok(path) => executables.push(path),
            Err(Unresolved::NotLocal(p)) => report.not_local.push(p),
            Err(Unresolved::Missing) => {}
        }
    }
    for exe in executables {
        add_whole(&mut plan, exe.clone());
        for lib in library_closure(&exe, env.mounts, env.deadline) {
            add_whole(&mut plan, lib);
        }
    }
    for path in chrony_files(env.chrony_conf, env.mounts) {
        add_whole(&mut plan, path);
    }
    plan.extend(journal_reads(env.journal_dir, env.mounts, env.deadline));
    if Instant::now() >= env.deadline {
        report.stopped = Some("planning reached the deadline".into());
    }
    plan
}

struct Bounds {
    max_bytes: u64,
    deadline: Instant,
    headroom: Option<u64>,
}

fn execute(plan: &[FileRead], mut report: Report, bounds: Bounds) -> Report {
    let planned: u64 = plan
        .iter()
        .map(FileRead::bytes)
        .sum::<u64>()
        .min(bounds.max_bytes);
    match bounds.headroom {
        Some(room) if room >= planned => {}
        room => {
            report.stopped = Some(format!(
                "skipped: {} KiB free above the high watermarks, under the {} KiB to read",
                room.map_or("unknown".into(), |a| (a / 1024).to_string()),
                planned / 1024
            ));
            return report;
        }
    }
    let mut buf = vec![0u8; READ_CHUNK];
    'files: for read in plan {
        let Ok(file) = std::fs::File::open(&read.path) else {
            continue;
        };
        report.files += 1;
        for &(start, end) in &read.ranges {
            let mut at = start;
            while at < end {
                if report.bytes >= bounds.max_bytes {
                    report.stopped = Some(format!("stopped at {} KiB", bounds.max_bytes / 1024));
                    break 'files;
                }
                if Instant::now() >= bounds.deadline {
                    report.stopped = Some(format!(
                        "stopped at the deadline, {} KiB read",
                        report.bytes / 1024
                    ));
                    break 'files;
                }
                let want = (end - at)
                    .min(READ_CHUNK as u64)
                    .min(bounds.max_bytes - report.bytes) as usize;
                match file.read_at(&mut buf[..want], at) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        at += n as u64;
                        report.bytes += n as u64;
                    }
                }
            }
        }
    }
    report
}

/// The guest's free memory and the sum of its zones' high watermarks, in bytes. The free
/// memory above the watermarks is the headroom: what can be filled without waking reclaim.
/// None when either file cannot be read.
fn memory() -> Option<(u64, u64)> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let zoneinfo = std::fs::read_to_string("/proc/zoneinfo").ok()?;
    // SAFETY: sysconf takes a constant and reads no memory of ours.
    let page = u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).ok()?;
    memory_from(&meminfo, &zoneinfo, page)
}

fn memory_from(meminfo: &str, zoneinfo: &str, page: u64) -> Option<(u64, u64)> {
    let line = meminfo.lines().find(|l| l.starts_with("MemFree:"))?;
    let free = line.split_whitespace().nth(1)?.parse::<u64>().ok()? * 1024;
    let mut high_pages = 0u64;
    for line in zoneinfo.lines() {
        let mut words = line.split_whitespace();
        if words.next() == Some("high") {
            high_pages += words.next()?.parse::<u64>().ok()?;
        }
    }
    Some((free, high_pages * page))
}

/// The mounts of `/proc/self/mountinfo`: mount point and filesystem type, in mount order.
struct Mounts(Vec<(PathBuf, String)>);

impl Mounts {
    fn parse(text: &str) -> Self {
        let mounts = text
            .lines()
            .filter_map(|line| {
                let (before, after) = line.split_once(" - ")?;
                let point = before.split(' ').nth(4)?;
                let fstype = after.split(' ').next()?;
                Some((PathBuf::from(unescape(point)), fstype.to_string()))
            })
            .collect();
        Mounts(mounts)
    }

    /// Whether something is mounted at `point`.
    fn has_mount_at(&self, point: &Path) -> bool {
        self.0.iter().any(|(mounted, _)| mounted == point)
    }

    /// Whether any mount is NFS.
    fn has_nfs(&self) -> bool {
        self.0
            .iter()
            .any(|(_, fstype)| fstype == "nfs" || fstype == "nfs4")
    }

    /// Whether `path` (absolute, without symlinks in it) lies on a local filesystem: the type
    /// of the last-mounted of the longest mount points that contain it.
    fn is_local(&self, path: &Path) -> bool {
        let mut best: Option<&(PathBuf, String)> = None;
        for mount in &self.0 {
            if path.starts_with(&mount.0)
                && best.is_none_or(|b| mount.0.components().count() >= b.0.components().count())
            {
                best = Some(mount);
            }
        }
        best.is_some_and(|(_, fstype)| LOCAL_FS_TYPES.contains(&fstype.as_str()))
    }
}

/// Undo mountinfo's octal escapes (`\040` for a space).
fn unescape(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && i + 4 <= bytes.len()
            && bytes[i + 1..i + 4].iter().all(u8::is_ascii_digit)
        {
            let code = u8::from_str_radix(&field[i + 1..i + 4], 8).unwrap_or(b'?');
            out.push(code);
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[derive(Debug, PartialEq)]
enum Unresolved {
    Missing,
    /// The first path, symlinks followed so far, that is not on a local filesystem.
    NotLocal(PathBuf),
}

/// `path` with every symlink followed, looking up each component only once the path up to it
/// is known to be on a local filesystem, so no lookup reaches a remote or FUSE mount.
fn resolve(path: &Path, mounts: &Mounts) -> Result<PathBuf, Unresolved> {
    if !path.is_absolute() {
        return Err(Unresolved::Missing);
    }
    let mut done = PathBuf::from("/");
    if !mounts.is_local(&done) {
        return Err(Unresolved::NotLocal(done));
    }
    let mut pending: Vec<OsString> = Vec::new();
    push_components(&mut pending, path);
    let mut links = 0;
    while let Some(part) = pending.pop() {
        match part.to_str() {
            Some(".") => continue,
            Some("..") => {
                done.pop();
                continue;
            }
            _ => {}
        }
        let next = done.join(&part);
        if !mounts.is_local(&next) {
            return Err(Unresolved::NotLocal(next));
        }
        let meta = std::fs::symlink_metadata(&next).map_err(|_| Unresolved::Missing)?;
        if meta.file_type().is_symlink() {
            links += 1;
            if links > MAX_SYMLINKS {
                return Err(Unresolved::Missing);
            }
            let target = std::fs::read_link(&next).map_err(|_| Unresolved::Missing)?;
            if target.is_absolute() {
                done = PathBuf::from("/");
            }
            push_components(&mut pending, &target);
        } else {
            done = next;
        }
    }
    Ok(done)
}

/// Push `path`'s components onto `stack` so the first is popped first.
fn push_components(stack: &mut Vec<OsString>, path: &Path) {
    let parts: Vec<OsString> = path
        .components()
        .filter_map(|c| match c {
            Component::Normal(p) => Some(p.to_os_string()),
            Component::ParentDir => Some("..".into()),
            Component::CurDir => Some(".".into()),
            Component::RootDir | Component::Prefix(_) => None,
        })
        .collect();
    stack.extend(parts.into_iter().rev());
}

/// Resolve `name` the way the spawn does: an absolute name as is, a bare one through `path`.
fn find_program(
    name: &str,
    path: &std::ffi::OsStr,
    mounts: &Mounts,
    not_local: &mut Vec<PathBuf>,
) -> Option<PathBuf> {
    let candidates: Vec<PathBuf> = if name.contains('/') {
        vec![PathBuf::from(name)]
    } else {
        std::env::split_paths(path)
            .filter(|dir| dir.is_absolute())
            .map(|dir| dir.join(name))
            .collect()
    };
    for candidate in candidates {
        match resolve(&candidate, mounts) {
            Ok(found) if std::fs::metadata(&found).is_ok_and(|m| m.is_file()) => {
                return Some(found)
            }
            Ok(_) | Err(Unresolved::Missing) => {}
            Err(Unresolved::NotLocal(p)) => {
                not_local.push(p);
            }
        }
    }
    None
}

/// The executables a unit's `ExecStart=` lines name.
fn unit_executables(unit: &str, mounts: &Mounts) -> Vec<PathBuf> {
    UNIT_DIRS
        .iter()
        .map(|dir| Path::new(dir).join(format!("{unit}.service")))
        .filter_map(|path| resolve(&path, mounts).ok())
        .find_map(|path| std::fs::read_to_string(path).ok())
        .map(|text| exec_start_paths(&text))
        .unwrap_or_default()
}

fn exec_start_paths(unit_text: &str) -> Vec<PathBuf> {
    unit_text
        .lines()
        .filter_map(|line| line.trim().strip_prefix("ExecStart="))
        .filter_map(|cmd| {
            cmd.trim_start_matches(['@', '-', ':', '+', '!'])
                .split_whitespace()
                .next()
                .map(PathBuf::from)
        })
        .filter(|path| path.is_absolute())
        .collect()
}

/// chronyd's configuration and the files its `driftfile`, `keyfile` and `include`
/// directives name.
fn chrony_files(conf: &Path, mounts: &Mounts) -> Vec<PathBuf> {
    let Ok(conf) = resolve(conf, mounts) else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(&conf) else {
        return Vec::new();
    };
    let mut out = vec![conf];
    for line in text.lines() {
        let mut words = line.split_whitespace();
        if let (Some("driftfile" | "keyfile" | "include"), Some(file)) =
            (words.next(), words.next())
        {
            if let Ok(path) = resolve(Path::new(file), mounts) {
                out.push(path);
            }
        }
    }
    out
}

/// The journal files under `dir` and its machine-id subdirectories: of each live file the
/// parts [`journal_ranges`] gives, of each archive (`system@….journal`, `….journal~`) its
/// first [`JOURNAL_ARCHIVE_HEAD_BYTES`], for at most [`JOURNAL_MAX_ARCHIVES`] archives.
fn journal_reads(dir: &Path, mounts: &Mounts, deadline: Instant) -> Vec<FileRead> {
    let mut out = Vec::new();
    let Ok(dir) = resolve(dir, mounts) else {
        return out;
    };
    // (modified time, live, path) of the journal files, newest first once sorted.
    let mut found: Vec<(std::time::SystemTime, bool, PathBuf)> = Vec::new();
    let mut dirs = vec![dir];
    let mut seen = 0;
    'scan: while let Some(dir) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            seen += 1;
            if seen > JOURNAL_MAX_ENTRIES || Instant::now() >= deadline {
                break 'scan;
            }
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if meta.is_dir() && dirs.len() < 64 {
                if let Ok(sub) = resolve(&entry.path(), mounts) {
                    dirs.push(sub);
                }
            } else if meta.is_file() && name.contains(".journal") {
                let live = name.ends_with(".journal") && !name.contains('@');
                let modified = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
                found.push((modified, live, entry.path()));
            }
        }
    }
    found.sort_by(|a, b| b.0.cmp(&a.0));
    let (mut live, mut archives) = (0, 0);
    for (_, is_live, path) in found {
        if Instant::now() >= deadline {
            break;
        }
        let Ok(file) = std::fs::File::open(&path) else {
            continue;
        };
        if is_live && live < JOURNAL_MAX_LIVE {
            live += 1;
            if let Some(ranges) = journal_ranges(&file) {
                out.push(FileRead { path, ranges });
            }
        } else if !is_live && archives < JOURNAL_MAX_ARCHIVES {
            archives += 1;
            let len = file.metadata().map_or(0, |m| m.len());
            out.push(FileRead {
                path,
                ranges: vec![(0, len.min(JOURNAL_ARCHIVE_HEAD_BYTES))],
            });
        }
    }
    out
}

/// The parts of a live journal file a journald that starts reads: all of the used part of a
/// small one; of a larger one its header, its two hash tables and the tail of its used part.
/// The used part ends at the end of the tail object (header field `tail_object_offset`).
fn journal_ranges(file: &std::fs::File) -> Option<Vec<(u64, u64)>> {
    let len = file.metadata().ok()?.len();
    let mut header = [0u8; 144];
    file.read_exact_at(&mut header, 0).ok()?;
    if &header[..8] != b"LPKSHHRH" {
        return None;
    }
    let u64_at = |o: usize| u64::from_le_bytes(header[o..o + 8].try_into().unwrap());
    let header_size = u64_at(88);
    let tail = u64_at(136);
    let mut object = [0u8; 16];
    let used = if tail >= header_size && file.read_exact_at(&mut object, tail).is_ok() {
        let size = u64::from_le_bytes(object[8..16].try_into().unwrap());
        tail.saturating_add(size)
    } else {
        header_size.saturating_add(u64_at(96))
    }
    .min(len);
    if used <= JOURNAL_WHOLE_FILE_BYTES {
        return Some(vec![(0, used)]);
    }
    // A hash table's offset is that of its payload, after a 16-byte object header.
    let table = |offset: u64, size: u64| {
        let start = offset.saturating_sub(16).min(used);
        (start, offset.saturating_add(size).min(used))
    };
    let mut ranges = vec![
        (0, header_size.min(used)),
        table(u64_at(104), u64_at(112)),
        table(u64_at(120), u64_at(128)),
        (used.saturating_sub(JOURNAL_TAIL_BYTES), used),
    ];
    ranges.sort_unstable();
    let mut merged: Vec<(u64, u64)> = Vec::new();
    for (start, end) in ranges.into_iter().filter(|(s, e)| s < e) {
        match merged.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    Some(merged)
}

/// The shared libraries `exe` needs, its interpreter included, followed transitively.
fn library_closure(exe: &Path, mounts: &Mounts, deadline: Instant) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut queue = vec![exe.to_path_buf()];
    while let Some(next) = queue.pop() {
        if Instant::now() >= deadline {
            break;
        }
        let Ok(file) = std::fs::File::open(&next) else {
            continue;
        };
        let Some(dynamic) = Dynamic::parse_with(|off, len| read_up_to(&file, off, len)) else {
            continue;
        };
        let origin = next.parent().map(Path::to_path_buf).unwrap_or_default();
        let mut found = Vec::new();
        if let Some(interp) = dynamic.interp {
            if let Ok(path) = resolve(Path::new(&interp), mounts) {
                found.push(path);
            }
        }
        let runpath: Vec<PathBuf> = dynamic
            .runpath
            .iter()
            .map(|dir| PathBuf::from(expand_origin(dir, &origin)))
            .collect();
        for name in &dynamic.needed {
            let dirs = runpath
                .iter()
                .cloned()
                .chain(LIBRARY_DIRS.iter().map(PathBuf::from));
            let lib = dirs
                .filter(|dir| dir.is_absolute())
                .filter_map(|dir| resolve(&dir.join(name), mounts).ok())
                .find(|p| std::fs::metadata(p).is_ok_and(|m| m.is_file()));
            found.extend(lib);
        }
        for path in found {
            if !out.contains(&path) {
                out.push(path.clone());
                queue.push(path);
            }
        }
    }
    out
}

/// `$ORIGIN` and `${ORIGIN}` in a run path, replaced by the object's directory.
fn expand_origin(dir: &str, origin: &Path) -> String {
    let origin = origin.to_string_lossy();
    dir.replace("${ORIGIN}", &origin)
        .replace("$ORIGIN", &origin)
}

/// Up to `len` bytes of `file` at `offset`; fewer at its end.
fn read_up_to(file: &std::fs::File, offset: u64, len: usize) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; len];
    let mut filled = 0;
    while filled < len {
        match file.read_at(&mut buf[filled..], offset + filled as u64) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(_) => return None,
        }
    }
    buf.truncate(filled);
    Some(buf)
}

/// What the dynamic linker reads from a 64-bit little-endian ELF file.
#[derive(Debug, Default, PartialEq)]
struct Dynamic {
    interp: Option<String>,
    needed: Vec<String>,
    runpath: Vec<String>,
}

impl Dynamic {
    #[cfg(test)]
    fn parse(b: &[u8]) -> Option<Self> {
        Self::parse_with(|off, len| {
            let start = usize::try_from(off).ok()?.min(b.len());
            Some(b[start..start.saturating_add(len).min(b.len())].to_vec())
        })
    }

    /// Parse through `at(offset, len)`, which returns up to `len` bytes at `offset`. Every
    /// read is bounded (`ELF_MAX_*`), so a large file is never read whole.
    fn parse_with(at: impl Fn(u64, usize) -> Option<Vec<u8>>) -> Option<Self> {
        const PT_LOAD: u32 = 1;
        const PT_DYNAMIC: u32 = 2;
        const PT_INTERP: u32 = 3;
        const DT_NEEDED: u64 = 1;
        const DT_STRTAB: u64 = 5;
        const DT_RPATH: u64 = 15;
        const DT_RUNPATH: u64 = 29;
        let u16_at =
            |b: &[u8], o: usize| Some(u16::from_le_bytes(b.get(o..o + 2)?.try_into().ok()?));
        let u32_at =
            |b: &[u8], o: usize| Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?));
        let u64_at =
            |b: &[u8], o: usize| Some(u64::from_le_bytes(b.get(o..o + 8)?.try_into().ok()?));
        let c_str = |off: u64| -> Option<String> {
            let rest = at(off, ELF_MAX_STRING)?;
            let end = rest.iter().position(|&c| c == 0)?;
            Some(String::from_utf8_lossy(&rest[..end]).into_owned())
        };
        let head = at(0, 64)?;
        if head.get(0..4)? != b"\x7fELF" || *head.get(4)? != 2 || *head.get(5)? != 1 {
            return None;
        }
        let phoff = u64_at(&head, 0x20)?;
        let phentsize = usize::from(u16_at(&head, 0x36)?);
        let phnum = usize::from(u16_at(&head, 0x38)?);
        if phentsize < 56 || phnum > ELF_MAX_PHNUM {
            return None;
        }
        let phs = at(phoff, phnum * phentsize)?;
        let mut loads = Vec::new();
        let (mut dynamic, mut interp) = (None, None);
        for i in 0..phnum {
            let ph = i * phentsize;
            let (kind, offset, vaddr, filesz) = (
                u32_at(&phs, ph)?,
                u64_at(&phs, ph + 8)?,
                u64_at(&phs, ph + 16)?,
                u64_at(&phs, ph + 32)?,
            );
            match kind {
                PT_LOAD => loads.push((vaddr, offset, filesz)),
                PT_DYNAMIC => dynamic = Some((offset, filesz)),
                PT_INTERP => interp = Some(offset),
                _ => {}
            }
        }
        let mut out = Dynamic {
            interp: interp.and_then(c_str),
            ..Default::default()
        };
        let Some((dyn_off, dyn_size)) = dynamic else {
            return Some(out);
        };
        let section = at(
            dyn_off,
            usize::try_from(dyn_size).ok()?.min(ELF_MAX_DYNAMIC),
        )?;
        let mut entries = Vec::new();
        let mut strtab = None;
        for entry in section.chunks_exact(16) {
            let (tag, val) = (u64_at(entry, 0)?, u64_at(entry, 8)?);
            if tag == 0 {
                break;
            }
            if tag == DT_STRTAB {
                strtab = Some(val);
            }
            entries.push((tag, val));
        }
        let strtab_vaddr = strtab?;
        let strtab_off = loads.iter().find_map(|&(vaddr, offset, filesz)| {
            (strtab_vaddr >= vaddr && strtab_vaddr < vaddr.saturating_add(filesz))
                .then(|| strtab_vaddr - vaddr + offset)
        })?;
        let name = |val: u64| c_str(strtab_off.checked_add(val)?);
        for (tag, val) in entries {
            match tag {
                DT_NEEDED => out.needed.extend(name(val)),
                DT_RPATH | DT_RUNPATH => {
                    if let Some(paths) = name(val) {
                        out.runpath.extend(paths.split(':').map(String::from));
                    }
                }
                _ => {}
            }
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `source` without its `#[cfg(test)] mod … { … }` blocks, so a scan sees what ships.
    fn shipped(source: &str) -> String {
        let mut out = String::new();
        let mut rest = source;
        while let Some(at) = rest.find("#[cfg(test)]\nmod ") {
            out.push_str(&rest[..at]);
            let block = &rest[at..];
            let open = block.find('{').expect("a test module has a body");
            let end = open + matching(&block[open..]);
            rest = &block[end..];
        }
        out.push_str(rest);
        out
    }

    /// The length of the bracketed group `text` starts with (`(` or `{`), skipping string,
    /// char and comment contents.
    fn matching(text: &str) -> usize {
        let b = text.as_bytes();
        let (open, close) = (b[0], if b[0] == b'(' { b')' } else { b'}' });
        let mut depth = 0;
        let mut i = 0;
        while i < b.len() {
            match b[i] {
                b'"' => {
                    i += 1;
                    while b[i] != b'"' {
                        i += if b[i] == b'\\' { 2 } else { 1 };
                    }
                }
                b'r' if b.get(i + 1) == Some(&b'#')
                    || (b.get(i + 1) == Some(&b'"') && !b[i - 1].is_ascii_alphanumeric()) =>
                {
                    let hashes = b[i + 1..].iter().take_while(|&&c| c == b'#').count();
                    let end = format!("\"{}", "#".repeat(hashes));
                    let start = i + 1 + hashes + 1;
                    i = start + text[start..].find(&end).unwrap() + end.len() - 1;
                }
                b'\'' if b.get(i + 2) == Some(&b'\'') => i += 2,
                b'\'' if b.get(i + 1) == Some(&b'\\') => {
                    i += 2 + b[i + 2..].iter().position(|&c| c == b'\'').unwrap();
                }
                b'/' if b.get(i + 1) == Some(&b'/') => {
                    i += b[i..]
                        .iter()
                        .position(|&c| c == b'\n')
                        .unwrap_or(b.len() - i);
                }
                c if c == open => depth += 1,
                c if c == close => {
                    depth -= 1;
                    if depth == 0 {
                        return i + 1;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        panic!("unbalanced source");
    }

    /// The program expression of every `Command::new(…)` in `source`, whitespace squeezed.
    fn spawns(source: &str) -> Vec<String> {
        let mut out: Vec<String> = source
            .match_indices("Command::new(")
            .map(|(i, m)| {
                let group = &source[i + m.len() - 1..];
                let arg = &group[1..matching(group) - 1];
                arg.split_whitespace().collect::<String>()
            })
            .collect();
        out.sort();
        out
    }

    /// The free function `name` in `source`, to its closing brace.
    fn function<'a>(source: &'a str, name: &str) -> &'a str {
        let start = source
            .find(&format!("fn {name}("))
            .unwrap_or_else(|| panic!("no fn {name}"));
        let rest = &source[start..];
        &rest[..rest.find("\n}\n").unwrap()]
    }

    fn registered(program: &str) -> bool {
        RESTORE_PROGRAMS.iter().any(|p| p.name == program)
    }

    /// Pin a module's spawns: `expected` is its exact sorted set of spawn expressions, every
    /// literal among them is registered, and each program `fed` to a variable spawn is
    /// registered and appears as a literal in the module.
    fn check_module(module: &str, source: &str, expected: &[&str], fed: &[&str]) {
        let mut expected: Vec<String> = expected.iter().map(|s| s.to_string()).collect();
        expected.sort();
        assert_eq!(
            spawns(source),
            expected,
            "{module} spawns a different set of programs: register any new one in RESTORE_PROGRAMS \
             and update this test"
        );
        for spawn in &expected {
            if let Some(literal) = spawn.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
                assert!(
                    registered(literal),
                    "{module} spawns {literal}, which RESTORE_PROGRAMS does not list"
                );
            }
        }
        for program in fed {
            assert!(
                registered(program),
                "{program}, fed to a spawn in {module}, is not registered"
            );
            assert!(
                source.contains(&format!("\"{program}\"")),
                "{module} no longer names {program}"
            );
        }
    }

    #[test]
    fn restore_rs_spawns_are_registered() {
        let source = shipped(include_str!("restore.rs"));
        check_module("restore.rs", &source, &["\"umount\"", "\"systemctl\""], &[]);
        assert!(
            function(&source, "restart_journald").contains(&format!("\"{JOURNALD_UNIT}\"")),
            "the restore restarts a unit other than {JOURNALD_UNIT}"
        );
    }

    #[test]
    fn snapshot_network_spawns_are_registered() {
        let source = shipped(include_str!("snapshot_network.rs"));
        check_module(
            "snapshot_network.rs",
            &source,
            // The gate's runner twice, the scratch-namespace runner, chronyc, the hook runner.
            &[
                "program",
                "program",
                "program",
                "\"/usr/bin/chronyc\"",
                "&argv[0]",
            ],
            &["iptables", "ip6tables", "conntrack"],
        );
        // The runner's `-restore` programs are built from the family's name.
        assert!(source.contains("let restore = format!(\"{program}-restore\");"));
        for program in ["iptables-restore", "ip6tables-restore"] {
            assert!(registered(program));
        }
        // The hook's argv: the rootless prefix, then podman.
        let argv = function(&source, "argv");
        assert!(
            argv.contains("crate::container::podman_cmd_prefix()") && argv.contains("\"podman\",")
        );
        let prefix = function(include_str!("container.rs"), "run_as_user_prefix");
        for program in ["env", "runuser"] {
            assert!(prefix.contains(&format!("\"{program}\"")) && registered(program));
        }
    }

    #[test]
    fn mounts_spawns_are_registered() {
        let source = shipped(include_str!("mounts.rs"));
        check_module(
            "mounts.rs",
            &source,
            &[
                "\"umount\"",
                "\"mount\"",
                "\"mount\"",
                "\"mount\"",
                "\"umount\"",
                "\"umount\"",
            ],
            &[],
        );
        // `mount -t nfs` execs the NFS mount helper.
        assert!(function(&source, "mount_nfs_shares").contains("\"nfs\""));
        assert!(registered("/sbin/mount.nfs"));
    }

    #[test]
    fn network_spawns_are_registered() {
        let source = shipped(include_str!("network.rs"));
        check_module(
            "network.rs",
            &source,
            &["\"ip\"", "\"ip\"", "\"ip\"", "\"iptables\"", "\"sysctl\""],
            &[],
        );
    }

    #[test]
    fn proxy_spawns_are_registered() {
        let source = shipped(include_str!("proxy.rs"));
        check_module("proxy.rs", &source, &["\"iptables\"", "\"ip6tables\""], &[]);
    }

    #[test]
    fn retired_address_spawns_are_registered() {
        let source = shipped(include_str!("retired_address.rs"));
        check_module(
            "retired_address.rs",
            &source,
            &["program"],
            &["ip", "ip6tables", "ip6tables-restore"],
        );
    }

    /// The whole of agent.rs: the chronyd functions run on every restore, and `hostname` sets
    /// the hostname at boot only, so it is not registered.
    #[test]
    fn agent_spawns_are_registered_or_boot_only() {
        let source = shipped(include_str!("agent.rs"));
        let mut expected = vec![
            "\"/usr/sbin/chronyd\"",
            "\"/usr/bin/chronyc\"",
            "\"systemctl\"",
            "\"sysctl\"",
            "\"hostname\"",
        ];
        expected.sort();
        assert_eq!(spawns(&source), expected);
        assert!(registered("sysctl") && !registered("hostname"));
    }

    #[test]
    fn gpu_spawns_are_registered() {
        let source = shipped(include_str!("gpu.rs"));
        check_module(
            "gpu.rs",
            &source,
            &[
                "\"dmesg\"",
                "\"insmod\"",
                "driver_root.join(\"usr/bin/nvidia-smi\")",
                "driver_root.join(\"usr/bin/nvidia-smi\")",
                "driver_root.join(\"usr/bin/nvidia-ctk\")",
                "driver_root.join(SCRUB_TOOL)",
                "driver_root.join(\"usr/bin/nvidia-persistenced\")",
            ],
            &[],
        );
        let tools = [
            "usr/bin/nvidia-smi",
            "usr/bin/nvidia-ctk",
            crate::gpu::SCRUB_TOOL,
            "usr/bin/nvidia-persistenced",
        ];
        for tool in tools {
            let path = format!("{}/{tool}", crate::gpu::DRIVER_ROOT);
            assert!(registered(&path), "{path}");
        }
    }

    #[test]
    fn gpu_park_spawns_are_registered() {
        let source = shipped(include_str!("gpu_park.rs"));
        check_module(
            "gpu_park.rs",
            &source,
            &[
                "&argv[0]",
                "root.join(\"usr/bin/cuda-checkpoint\")",
                "root.join(\"usr/bin/nvidia-smi\")",
            ],
            &["podman"],
        );
        for tool in ["usr/bin/cuda-checkpoint", "usr/bin/nvidia-smi"] {
            let path = format!("{}/{tool}", crate::gpu::DRIVER_ROOT);
            assert!(registered(&path), "{path}");
        }
    }

    /// Modules that spawn but are not on the restore path, and why.
    const OFF_RESTORE_PATH: &[(&str, &str)] = &[
        (
            "container.rs",
            "boot: starts the container and prepares its storage and user; the restore \
             hook's prefix comes from it but snapshot_network spawns the hook",
        ),
        (
            "tty.rs",
            "the exec server: spawns the argv an exec request names, not a restore step",
        ),
        (
            "system.rs",
            "shutdown (sync, poweroff, reboot), which ends a clone rather than restoring it",
        ),
    ];

    /// Modules a test above pins.
    const PINNED: &[&str] = &[
        "restore.rs",
        "snapshot_network.rs",
        "mounts.rs",
        "network.rs",
        "proxy.rs",
        "retired_address.rs",
        "agent.rs",
        "gpu.rs",
        "gpu_park.rs",
    ];

    /// Every fc-agent source file whose shipped code spawns a process is pinned by a test
    /// above or named as off the restore path, so a spawn in a new module fails here.
    #[test]
    fn every_spawning_module_is_pinned_or_excluded() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut dirs = vec![src.clone()];
        let mut spawning = Vec::new();
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(&dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    dirs.push(path);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    let source = std::fs::read_to_string(&path).unwrap();
                    if !spawns(&shipped(&source)).is_empty() {
                        let name = path.strip_prefix(&src).unwrap();
                        spawning.push(name.to_string_lossy().into_owned());
                    }
                }
            }
        }
        spawning.sort();
        let mut known: Vec<String> = PINNED
            .iter()
            .chain(OFF_RESTORE_PATH.iter().map(|(module, _)| module))
            .map(|module| module.to_string())
            .collect();
        known.sort();
        assert_eq!(
            spawning, known,
            "a module that spawns is neither pinned nor named off the restore path"
        );
    }

    /// The memory figures are `MemFree` and every zone's high watermark, in pages.
    #[test]
    fn memory_is_free_memory_and_the_high_watermarks() {
        let meminfo = "MemTotal:  262144 kB\nMemFree:   102400 kB\nMemAvailable: 150000 kB\n";
        let zoneinfo = "Node 0, zone      DMA\n  pages free     25600\n        min      512\n        low      640\n        high     768\n        spanned  65536\nNode 0, zone   Normal\n  pages free     0\n        min      0\n        low      0\n        high     256\n";
        assert_eq!(
            memory_from(meminfo, zoneinfo, 4096),
            Some((100 << 20, 1024 * 4096))
        );
        assert_eq!(memory_from("", zoneinfo, 4096), None);
    }

    /// Under the planning bound nothing is planned: planning would have looked every program up
    /// and reported the ones an empty `PATH` cannot find.
    #[test]
    fn short_memory_skips_planning_too() {
        let dir = scratch("short-memory");
        let mounts = local_root();
        let conf = dir.join("chrony.conf");
        let env = Env {
            mounts: &mounts,
            path: dir.clone().into_os_string(),
            journal_dir: &dir,
            chrony_conf: &conf,
            deadline: far(),
        };
        let short = warm_in(&env, Some(PLANNING_MAX_BYTES - 1), Instant::now());
        assert!(short.not_found.is_empty(), "{short}");
        assert!(short
            .stopped
            .as_deref()
            .unwrap()
            .starts_with("skipped before planning"));
        assert!(warm_in(&env, None, Instant::now()).not_found.is_empty());
        let enough = warm_in(&env, Some(u64::MAX), Instant::now());
        assert!(enough.not_found.iter().any(|n| n.starts_with("systemctl")));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A deadline already past stops library and journal planning, and journal planning
    /// considers at most the newest live journals and directory entries its caps allow.
    #[test]
    fn planning_stops_at_the_deadline_and_the_journal_caps() {
        let me = std::env::current_exe().unwrap();
        assert!(library_closure(&me, &local_root(), Instant::now()).is_empty());
        let dir = scratch("many-journals");
        for i in 0..(JOURNAL_MAX_LIVE + 4) {
            fake_journal(&dir.join(format!("user-{i}.journal")), 1 << 20, 1 << 20);
        }
        assert!(journal_reads(&dir, &local_root(), Instant::now()).is_empty());
        let reads = journal_reads(&dir, &local_root(), far());
        assert_eq!(reads.len(), JOURNAL_MAX_LIVE);
        for i in 0..JOURNAL_MAX_ENTRIES {
            std::fs::write(dir.join(format!("other-{i}")), b"").unwrap();
        }
        assert!(journal_reads(&dir, &local_root(), far()).len() <= JOURNAL_MAX_LIVE);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn agent_chronyd_spawns_are_registered() {
        let agent = include_str!("agent.rs");
        let source = ["spawn_chronyd", "chronyc_sources", "stop_running_chronyd"]
            .map(|f| function(agent, f))
            .join("\n");
        check_module(
            "agent.rs",
            &source,
            &[
                "\"/usr/sbin/chronyd\"",
                "\"/usr/bin/chronyc\"",
                "\"systemctl\"",
            ],
            &[],
        );
        assert!(agent.contains(&format!("const CHRONY_CONF: &str = \"{CHRONY_CONF}\";")));
    }

    /// A temporary directory. Whether a path is local is decided by the fake mount tables
    /// the tests pass, not by the filesystem the directory is on.
    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("fc-agent-warmup-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn far() -> Instant {
        Instant::now() + Duration::from_secs(600)
    }

    fn local_root() -> Mounts {
        Mounts::parse("22 1 259:1 / / rw,relatime shared:1 - ext4 /dev/vda rw\n")
    }

    /// A sparse journal file: a valid header, its tail object ending at `used`, `len` long.
    fn fake_journal(path: &Path, used: u64, len: u64) {
        let file = std::fs::File::create(path).unwrap();
        file.set_len(len).unwrap();
        let mut header = [0u8; 272];
        header[..8].copy_from_slice(b"LPKSHHRH");
        let mut put = |o: usize, v: u64| header[o..o + 8].copy_from_slice(&v.to_le_bytes());
        put(88, 272);
        put(96, len - 272);
        put(104, 272 + 16);
        put(112, 4 << 20);
        put(120, 272 + 16 + (4 << 20) + 16);
        put(128, 64 << 10);
        put(136, used - 64);
        file.write_all_at(&header, 0).unwrap();
        let mut object = [0u8; 16];
        object[8..].copy_from_slice(&64u64.to_le_bytes());
        file.write_all_at(&object, used - 64).unwrap();
    }

    /// A journal directory of about 1.2 GiB: a live system journal of 128 MiB used, a small
    /// live user journal, and 109 archives. Of the live system journal only its header, hash
    /// tables and tail are read, of the small one its used part, of 100 archives their heads.
    #[test]
    fn a_large_journal_directory_is_read_within_the_caps() {
        let dir = scratch("journal");
        let machine = dir.join("0123456789abcdef0123456789abcdef");
        std::fs::create_dir(&machine).unwrap();
        fake_journal(&machine.join("system.journal"), 128 << 20, 136 << 20);
        for i in 0..8 {
            let name = format!("system@{i:032x}-0000000000000001-0000000000000001.journal");
            fake_journal(&machine.join(name), 64 << 20, 64 << 20);
        }
        for i in 0..101 {
            fake_journal(
                &machine.join(format!("user-1000@{i:04}.journal~")),
                8 << 20,
                8 << 20,
            );
        }
        fake_journal(&machine.join("user-1000.journal"), 2 << 20, 8 << 20);
        let reads = journal_reads(&dir, &local_root(), far());
        let live: Vec<_> = reads
            .iter()
            .filter(|r| {
                r.path.extension().is_some_and(|e| e == "journal")
                    && !r.path.to_string_lossy().contains('@')
            })
            .collect();
        assert_eq!(live.len(), 2);
        let system = live
            .iter()
            .find(|r| r.path.ends_with("system.journal"))
            .unwrap();
        let used = 128u64 << 20;
        assert_eq!(
            system.ranges,
            vec![
                (0, 272 + 16 + (4 << 20) + 16 + (64 << 10)),
                (used - JOURNAL_TAIL_BYTES, used)
            ]
        );
        let user = live
            .iter()
            .find(|r| r.path.ends_with("user-1000.journal"))
            .unwrap();
        assert_eq!(
            user.ranges,
            vec![(0, 2 << 20)],
            "a small live journal is read to its used end"
        );
        let archives: Vec<_> = reads
            .iter()
            .filter(|r| r.path.to_string_lossy().contains('@'))
            .collect();
        assert_eq!(archives.len(), JOURNAL_MAX_ARCHIVES);
        assert!(archives
            .iter()
            .all(|r| r.ranges == [(0, JOURNAL_ARCHIVE_HEAD_BYTES)]));
        let total: u64 = reads.iter().map(FileRead::bytes).sum();
        assert!(
            total <= 9 << 20,
            "{total} bytes planned from a 1.2 GiB directory"
        );

        // The whole warm-up stops at its byte cap.
        let bounds = |max_bytes, deadline, headroom| Bounds {
            max_bytes,
            deadline,
            headroom,
        };
        let far = Instant::now() + Duration::from_secs(60);
        let report = execute(
            &reads,
            Report::default(),
            bounds(1 << 20, far, Some(u64::MAX)),
        );
        assert_eq!(report.bytes, 1 << 20);
        assert!(report
            .stopped
            .as_deref()
            .unwrap()
            .starts_with("stopped at 1024 KiB"));
        // And at its deadline.
        let report = execute(
            &reads,
            Report::default(),
            bounds(u64::MAX, Instant::now(), Some(u64::MAX)),
        );
        assert_eq!(report.bytes, 0);
        assert!(report
            .stopped
            .as_deref()
            .unwrap()
            .starts_with("stopped at the deadline"));
        // And reads nothing when the free memory above the watermarks cannot hold it.
        let short = total - 1;
        let report = execute(
            &reads,
            Report::default(),
            bounds(u64::MAX, far, Some(short)),
        );
        assert_eq!((report.files, report.bytes), (0, 0));
        assert!(report.stopped.as_deref().unwrap().starts_with("skipped: "));
        let report = execute(&reads, Report::default(), bounds(u64::MAX, far, None));
        assert_eq!(report.bytes, 0, "unreadable memory figures read nothing");
        let report = execute(
            &reads,
            Report::default(),
            bounds(u64::MAX, far, Some(short + 1)),
        );
        assert_eq!((report.files, report.bytes), (reads.len(), total));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The NFS programs are read only when the guest has an NFS mount, and podman never.
    #[test]
    fn the_nfs_programs_are_read_only_with_nfs_and_podman_never() {
        let names = |nfs, gpu| {
            programs_to_warm(nfs, gpu)
                .map(|p| p.name)
                .collect::<Vec<_>>()
        };
        let nfs_only = ["umount", "mount", "/sbin/mount.nfs"];
        let without = names(false, false);
        let with = names(true, false);
        assert!(nfs_only
            .iter()
            .all(|n| !without.contains(n) && with.contains(n)));
        assert!(!with.contains(&"podman"));
        let gpu_only: Vec<_> = RESTORE_PROGRAMS
            .iter()
            .filter(|p| p.warm == Warm::IfGpu)
            .map(|p| p.name)
            .collect();
        assert!(gpu_only.contains(&"insmod"));
        assert!(gpu_only
            .iter()
            .all(|n| !with.contains(n) && names(false, true).contains(n)));
        assert_eq!(names(true, true).len(), RESTORE_PROGRAMS.len() - 1);
        let driver = Mounts::parse(&format!(
            "40 22 254:16 / {} ro - ext4 /dev/vdb ro\n",
            crate::gpu::DRIVER_ROOT
        ));
        assert!(driver.has_mount_at(Path::new(crate::gpu::DRIVER_ROOT)));
        assert!(!local_root().has_mount_at(Path::new(crate::gpu::DRIVER_ROOT)));
        assert!(Mounts::parse("30 22 0:5 / /mnt rw - nfs4 host:/ rw\n").has_nfs());
        assert!(!local_root().has_nfs());
    }

    /// A symlink onto a FUSE mount is never followed into it, and a `PATH` entry on one is
    /// never searched: both are reported instead.
    #[test]
    fn a_path_through_a_fuse_mount_is_not_read() {
        let dir = scratch("fuse");
        let fuse = dir.join("fuse");
        std::fs::create_dir(&fuse).unwrap();
        std::fs::write(fuse.join("tool"), b"x").unwrap();
        std::os::unix::fs::symlink(fuse.join("tool"), dir.join("link")).unwrap();
        let mounts = Mounts::parse(&format!(
            "22 1 259:1 / / rw shared:1 - ext4 /dev/vda rw\n\
             90 22 0:77 / {} rw,nosuid shared:40 - fuse.sshfs host:/x rw\n",
            fuse.display()
        ));
        assert!(mounts.is_local(&dir));
        assert!(!mounts.is_local(&fuse));
        assert_eq!(
            resolve(&dir.join("link"), &mounts),
            Err(Unresolved::NotLocal(fuse.clone()))
        );
        let mut not_local = Vec::new();
        assert_eq!(
            find_program("tool", fuse.as_os_str(), &mounts, &mut not_local),
            None
        );
        assert_eq!(not_local, std::slice::from_ref(&fuse));
        // On a local mount the same link resolves to its target.
        assert_eq!(
            resolve(&dir.join("link"), &local_root()),
            Ok(fuse.join("tool"))
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_last_and_longest_mount_decides() {
        let mounts = Mounts::parse(
            "22 1 259:1 / / rw - ext4 /dev/vda rw\n\
             30 22 0:5 / /mnt rw - nfs4 host:/ rw\n\
             31 30 0:6 / /mnt/local\\040dir rw - tmpfs tmpfs rw\n\
             32 22 0:7 / /srv rw - xfs /dev/vdb rw\n\
             33 32 0:8 / /srv rw - fuse fuse rw\n",
        );
        assert!(mounts.is_local(Path::new("/usr/bin/ls")));
        assert!(!mounts.is_local(Path::new("/mnt/x")));
        assert!(mounts.is_local(Path::new("/mnt/local dir/x")));
        assert!(
            !mounts.is_local(Path::new("/srv/x")),
            "a mount stacked on /srv hides the xfs one"
        );
        assert!(!Mounts::parse("").is_local(Path::new("/")));
    }

    #[test]
    fn a_bare_name_is_found_through_path_and_an_absolute_one_as_is() {
        let dir = scratch("path");
        for sub in ["a", "b"] {
            std::fs::create_dir(dir.join(sub)).unwrap();
        }
        std::fs::write(dir.join("b/tool"), b"x").unwrap();
        let path = std::env::join_paths([dir.join("a"), dir.join("b")]).unwrap();
        let mut not_local = Vec::new();
        assert_eq!(
            find_program("tool", &path, &local_root(), &mut not_local),
            Some(dir.join("b/tool"))
        );
        let absolute = dir.join("b/tool");
        let absolute = absolute.to_str().unwrap();
        assert_eq!(
            find_program(absolute, "".as_ref(), &local_root(), &mut not_local),
            Some(dir.join("b/tool"))
        );
        assert_eq!(
            find_program("absent", &path, &local_root(), &mut not_local),
            None
        );
        assert!(not_local.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn origin_in_a_run_path_is_the_objects_directory() {
        let origin = Path::new("/opt/app/bin");
        assert_eq!(
            expand_origin("$ORIGIN/../lib", origin),
            "/opt/app/bin/../lib"
        );
        assert_eq!(expand_origin("${ORIGIN}/lib", origin), "/opt/app/bin/lib");
        assert_eq!(expand_origin("/usr/lib", origin), "/usr/lib");
    }

    #[test]
    fn chrony_files_follow_its_configuration() {
        let dir = scratch("chrony");
        let conf = dir.join("chrony.conf");
        let drift = dir.join("drift");
        std::fs::write(&drift, b"0.0 0.0").unwrap();
        std::fs::write(
            &conf,
            format!(
                "server 10.0.2.2 iburst\nmakestep 1 -1\ndriftfile {}\nkeyfile /nonexistent\n",
                drift.display()
            ),
        )
        .unwrap();
        assert_eq!(
            chrony_files(&conf, &local_root()),
            vec![conf.clone(), drift]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn exec_start_lines_give_their_executables() {
        let unit = "[Service]\nExecStart=/usr/lib/systemd/systemd-journald\nExecStart=-@/bin/x y\nExecStart=relative\n";
        assert_eq!(
            exec_start_paths(unit),
            vec![
                PathBuf::from("/usr/lib/systemd/systemd-journald"),
                PathBuf::from("/bin/x")
            ]
        );
    }

    /// The closure of this test binary holds its C library, found through its ELF headers,
    /// and parsing it reads a bounded part of each file, never the whole.
    #[test]
    fn a_dynamic_executable_brings_its_libraries() {
        let me = std::env::current_exe().unwrap();
        let file = std::fs::File::open(&me).unwrap();
        let asked = std::cell::RefCell::new(Vec::new());
        let dynamic = Dynamic::parse_with(|off, len| {
            asked.borrow_mut().push(len);
            read_up_to(&file, off, len)
        })
        .unwrap();
        let asked = asked.into_inner();
        let bound = 64
            + ELF_MAX_PHNUM * 56
            + ELF_MAX_DYNAMIC
            + ELF_MAX_STRING * (1 + dynamic.needed.len() + 1);
        assert!(asked.iter().all(|&len| len <= ELF_MAX_DYNAMIC), "{asked:?}");
        assert!(asked.iter().sum::<usize>() <= bound, "{asked:?}");
        assert!((asked.iter().sum::<usize>() as u64) < file.metadata().unwrap().len());
        if dynamic.needed.is_empty() {
            // A static build (musl) needs nothing: its closure is empty.
            assert!(library_closure(&me, &local_root(), far()).is_empty());
            return;
        }
        assert!(
            dynamic.needed.iter().any(|n| n.starts_with("libc.so")),
            "{dynamic:?}"
        );
        let closure = library_closure(&me, &local_root(), far());
        assert!(
            closure.iter().any(|p| p
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("libc.so"))),
            "{closure:?}"
        );
        assert_eq!(Dynamic::parse(&std::fs::read(&me).unwrap()), Some(dynamic));
    }

    #[test]
    fn a_file_that_is_not_elf_has_no_dynamic_section() {
        assert_eq!(Dynamic::parse(b"#!/bin/sh\n"), None);
        assert_eq!(Dynamic::parse(b""), None);
    }

    #[test]
    fn the_report_names_what_it_could_not_read() {
        let report = Report {
            files: 3,
            bytes: 4096,
            elapsed: Duration::from_millis(7),
            not_found: vec!["conntrack (the gate)".into()],
            not_local: vec![PathBuf::from("/mnt/nfs")],
            stopped: None,
            memory: Some((100 << 20, 4 << 20)),
        };
        assert_eq!(
            report.to_string(),
            "read 3 files (4 KiB) a restore runs into the page cache in 7 ms; not found: conntrack (the gate); \
             not on a local filesystem: /mnt/nfs; MemFree 102400 KiB, high watermarks 4096 KiB"
        );
    }
}

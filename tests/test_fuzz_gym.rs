//! Fuzz gym: cross the ways a VM is created, restored and used (#1009).
//!
//! Every bug found while investigating the www VM restore of 2026-09-25 sat at a
//! crossing of how a VM was created, how it was restored, and what the guest held
//! across the snapshot, and no single-path test reached them:
//!
//! 1. `fcvm snapshot create` wrote no inode tables for `--portable-volumes`, so a
//!    clone answered `unknown inode in request` for every inode the guest held
//!    (058a552f).
//! 2. A restored volume server numbered handles from 1 again, so a new handle could
//!    alias one the guest held from before the snapshot (d6ab3c3a).
//! 3. Reopening a stale handle asked for `O_RDWR`, so a held handle on a file the
//!    server may only read failed after a restore.
//! 4. perf with hardware counters in a restored x86_64 guest killed the guest: an
//!    async page fault taken inside the perf NMI. Fixed by the kernel (upstream
//!    81d99abe2bd8, in 6.18.59); `test_async_pf_inside_nmi_does_not_panic_a_restored_guest`
//!    pins it.
//!
//! A scenario takes one value from each of five dimensions: [`Origin`], [`Restore`],
//! [`Storage`], [`Network`] and [`After`]. In every scenario the guest holds, at
//! once, every resource the storage allows ([`holds`]): each is a `gymhold` process
//! (tests/gym/gymhold.c, built static on the host and shipped on the volume) that
//! checks its resource every 200 ms and logs each check. The host also holds a TCP
//! connection through the VM's published port. The gym snapshots, restores the listed
//! way, makes the post-restore changes every scenario makes ([`common_afters`]) and
//! then the one its [`After`] names, and checks the oracles listed on [`run_scenario`].
//!
//! What a clone keeps is per hold ([`Expect`]): everything works, except a file the
//! host renamed or deleted before the restore, and files held open on a `--map`
//! without `--portable-volumes`; those must read correct content or fail, never read
//! wrong content.
//!
//! What the gym found when it first ran (each now a deterministic test):
//! - `--pmem` on the 6.18.50 guest kernel: a page fault on a DAX mapping (running a
//!   program stored on the image) was a kernel Oops in dax_disassociate_entry, fixed
//!   by 6.18.60 (`test_pmem_dax_serves_a_page_fault_on_a_mapped_program`).
//! - `--nfs` with rootless or routed networking booted and then failed to mount;
//!   fcvm now refuses it before boot
//!   (`nfs_without_bridged_networking_is_refused_before_boot`).
//! - A restore of a VM with a `--map` without `--portable-volumes` answers EBADF for
//!   every inode the guest took before the snapshot, directories included (#1684).
//!   The gym holds those to "correct or error" until that is fixed.
//!
//! Per PR: `gym_pairs_NN`, one test per scenario of a deterministic all-pairs covering
//! set ([`all_pairs`]), and `gym_hugepage_pairs_NN` for its hugepage scenarios, which
//! share the host's hugepage pool with the other hugepage tests. Nightly (weekly.yml,
//! `make fuzz-gym`): `gym_full_cross_product`, every valid combination, optionally
//! sharded by `FCVM_GYM_SHARD=i/n`, followed by `FCVM_GYM_SAMPLES` seeded random
//! repeats. A failure prints the scenario and its index; `FCVM_GYM_SCENARIO=<index>`
//! with `gym_one` replays it.

#![cfg(feature = "privileged-tests")]

mod common;

use anyhow::{bail, ensure, Context, Result};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// How the VM that is snapshotted came to exist, and what the snapshot is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    /// `podman run`, then `snapshot create`.
    Fresh,
    /// A second `podman run` that restores the first one's startup snapshot.
    CacheHit,
    /// A clone restored from a snapshot of a fresh VM, then `snapshot create` of it.
    Clone,
    /// The same, two clone generations deep.
    CloneOfClone,
    /// `snapshot create --disk-only`: the clone boots from the captured disk.
    DiskOnly,
}

/// How the clone is restored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Restore {
    ServeCopy,
    ServeMinor,
    ServeNoPrefetch,
    /// `snapshot serve --uffd-fault-around 65536`.
    ServeFaultAround,
    /// `snapshot run --snapshot`, Firecracker's file backend.
    DirectFile,
    /// A VM on 2 MiB hugepages, restored through `snapshot serve`.
    Hugepages,
    /// A Cloud Hypervisor VM, restored by `snapshot run --snapshot` (CH `--restore`).
    CloudHypervisor,
}

/// Where the guest's held files live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Storage {
    /// The container's own root filesystem: no volume.
    Rootfs,
    /// `--map`, inodes named by host inode numbers.
    Map,
    /// `--map --portable-volumes`, read-write.
    PortableRw,
    /// `--map --portable-volumes`, read-only.
    PortableRo,
    /// `--nfs`.
    Nfs,
    /// `--disk-dir ...:ro`, an ext4 image fcvm builds from the directory.
    DiskDir,
    /// `--disk ...:ro`, an ext4 image the test builds.
    RoDisk,
    /// `--pmem ...:ro`, the same image over virtio-pmem with DAX.
    Pmem,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Network {
    Rootless,
    Bridged,
    Routed,
}

/// What the clone does after the changes every scenario makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum After {
    Nothing,
    /// Snapshot the clone and restore a second generation from it.
    SnapshotOfTheClone,
    /// Restore two clones of the snapshot at once.
    TwoClones,
    /// Restore 90 s after the snapshot, so the guest clock steps on resume.
    LateRestore,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Scenario {
    origin: Origin,
    restore: Restore,
    storage: Storage,
    network: Network,
    after: After,
}

const ORIGINS: [Origin; 5] = [
    Origin::Fresh,
    Origin::CacheHit,
    Origin::Clone,
    Origin::CloneOfClone,
    Origin::DiskOnly,
];
const RESTORES: [Restore; 7] = [
    Restore::ServeCopy,
    Restore::ServeMinor,
    Restore::ServeNoPrefetch,
    Restore::ServeFaultAround,
    Restore::DirectFile,
    Restore::Hugepages,
    Restore::CloudHypervisor,
];
const STORAGES: [Storage; 8] = [
    Storage::Rootfs,
    Storage::Map,
    Storage::PortableRw,
    Storage::PortableRo,
    Storage::Nfs,
    Storage::DiskDir,
    Storage::RoDisk,
    Storage::Pmem,
];
const NETWORKS: [Network; 3] = [Network::Rootless, Network::Bridged, Network::Routed];
const AFTERS: [After; 4] = [
    After::Nothing,
    After::SnapshotOfTheClone,
    After::TwoClones,
    After::LateRestore,
];
const DIMENSIONS: usize = 5;

impl Storage {
    fn writable(self) -> bool {
        matches!(
            self,
            Storage::Rootfs | Storage::Map | Storage::PortableRw | Storage::Nfs
        )
    }

    /// The host sees the guest's files, and the guest the host's, while it runs.
    fn shared_with_host(self) -> bool {
        matches!(
            self,
            Storage::Map | Storage::PortableRw | Storage::PortableRo | Storage::Nfs
        )
    }

    fn guest_dir(self) -> &'static str {
        match self {
            Storage::Rootfs => "/gymdata",
            _ => "/mnt/gym",
        }
    }
}

/// Why a combination is not run, or None to run it. Every reason is a combination
/// fcvm refuses or that has no meaning, not one the gym finds inconvenient.
fn skip_reason(s: &Scenario) -> Option<&'static str> {
    if s.origin == Origin::DiskOnly && s.restore != Restore::DirectFile {
        return Some(
            "a disk-only snapshot has no memory: its clone boots, by `snapshot run --snapshot`",
        );
    }
    if s.origin == Origin::DiskOnly && s.after == After::LateRestore {
        return Some("a disk-only clone boots, so there is no guest clock to step");
    }
    if s.restore == Restore::CloudHypervisor && !cfg!(target_arch = "aarch64") {
        return Some("fcvm runs Cloud Hypervisor on arm64 only");
    }
    if s.restore == Restore::CloudHypervisor && matches!(s.storage, Storage::Nfs | Storage::Pmem) {
        return Some("--nfs needs the nested kernel profile and --pmem is Firecracker only; neither runs on Cloud Hypervisor");
    }
    if s.restore == Restore::CloudHypervisor && s.origin == Origin::CacheHit {
        return Some("podman run's startup snapshot cache is Firecracker only");
    }
    if s.storage == Storage::Nfs && s.network != Network::Bridged {
        return Some("fcvm refuses --nfs without bridged networking (validate_nfs_network)");
    }
    if s.origin == Origin::DiskOnly && matches!(s.storage, Storage::RoDisk | Storage::DiskDir) {
        return Some("snapshot create --disk-only refuses a VM with extra disks");
    }
    None
}

/// Every valid combination, in a fixed order.
fn cross_product() -> Vec<Scenario> {
    let mut all = Vec::new();
    for origin in ORIGINS {
        for restore in RESTORES {
            for storage in STORAGES {
                for network in NETWORKS {
                    for after in AFTERS {
                        let s = Scenario {
                            origin,
                            restore,
                            storage,
                            network,
                            after,
                        };
                        if skip_reason(&s).is_none() {
                            all.push(s);
                        }
                    }
                }
            }
        }
    }
    all
}

/// The values of each dimension of `s`, as indices, for pair coverage.
fn coordinates(s: &Scenario) -> [usize; DIMENSIONS] {
    [
        ORIGINS.iter().position(|v| *v == s.origin).unwrap(),
        RESTORES.iter().position(|v| *v == s.restore).unwrap(),
        STORAGES.iter().position(|v| *v == s.storage).unwrap(),
        NETWORKS.iter().position(|v| *v == s.network).unwrap(),
        AFTERS.iter().position(|v| *v == s.after).unwrap(),
    ]
}

type Pair = (usize, usize, usize, usize);

fn pairs_of(s: &Scenario) -> Vec<Pair> {
    let c = coordinates(s);
    let mut pairs = Vec::new();
    for a in 0..DIMENSIONS {
        for b in a + 1..DIMENSIONS {
            pairs.push((a, c[a], b, c[b]));
        }
    }
    pairs
}

/// Every pair of values of two dimensions that some valid scenario holds.
fn coverable_pairs(all: &[Scenario]) -> std::collections::BTreeSet<Pair> {
    all.iter().flat_map(pairs_of).collect()
}

/// A deterministic all-pairs covering set: greedily the scenario that covers the
/// most pairs not yet covered, first in cross-product order on a tie.
fn all_pairs() -> Vec<Scenario> {
    let all = cross_product();
    let mut uncovered = coverable_pairs(&all);
    let mut chosen = Vec::new();
    while !uncovered.is_empty() {
        let mut best: Option<(usize, &Scenario)> = None;
        for s in &all {
            let gain = pairs_of(s).iter().filter(|p| uncovered.contains(p)).count();
            if best.is_none_or(|(g, _)| gain > g) {
                best = Some((gain, s));
            }
        }
        let (_, s) = best.expect("a scenario covers each coverable pair");
        for p in pairs_of(s) {
            uncovered.remove(&p);
        }
        chosen.push(*s);
    }
    chosen
}

/// The all-pairs set split into the scenarios that need the hugepage pool and the rest.
fn pairs_sets() -> (Vec<Scenario>, Vec<Scenario>) {
    all_pairs()
        .into_iter()
        .partition(|s| s.restore != Restore::Hugepages)
}

/// The number of `gym_pairs_NN` and `gym_hugepage_pairs_NN` tests below.
/// `the_pairs_tests_cover_every_scenario_of_the_set` fails when a set outgrows them.
const PAIRS_TESTS: usize = 64;
const HUGEPAGE_PAIRS_TESTS: usize = 12;

#[test]
fn the_pairs_tests_cover_every_scenario_of_the_set() {
    let (plain, huge) = pairs_sets();
    assert!(
        plain.len() <= PAIRS_TESTS,
        "the all-pairs set has {} scenarios off hugepages and only {PAIRS_TESTS} gym_pairs_NN tests run them; add more",
        plain.len()
    );
    assert!(
        huge.len() <= HUGEPAGE_PAIRS_TESTS,
        "the all-pairs set has {} hugepage scenarios and only {HUGEPAGE_PAIRS_TESTS} gym_hugepage_pairs_NN tests run them; add more",
        huge.len()
    );
    let covered: std::collections::BTreeSet<Pair> =
        plain.iter().chain(&huge).flat_map(pairs_of).collect();
    assert_eq!(covered, coverable_pairs(&cross_product()));
}

#[test]
fn the_cross_product_skips_only_what_cannot_run() {
    let all = cross_product();
    let total = ORIGINS.len() * RESTORES.len() * STORAGES.len() * NETWORKS.len() * AFTERS.len();
    let mut reasons = std::collections::BTreeMap::new();
    for origin in ORIGINS {
        for restore in RESTORES {
            for storage in STORAGES {
                for network in NETWORKS {
                    for after in AFTERS {
                        let s = Scenario {
                            origin,
                            restore,
                            storage,
                            network,
                            after,
                        };
                        if let Some(reason) = skip_reason(&s) {
                            *reasons.entry(reason).or_insert(0usize) += 1;
                        }
                    }
                }
            }
        }
    }
    assert_eq!(total - all.len(), reasons.values().sum::<usize>());
    // Every value of every dimension is in some scenario that runs.
    for (dim, len) in [
        ORIGINS.len(),
        RESTORES.len(),
        STORAGES.len(),
        NETWORKS.len(),
        AFTERS.len(),
    ]
    .into_iter()
    .enumerate()
    {
        for value in 0..len {
            assert!(
                all.iter().any(|s| coordinates(s)[dim] == value),
                "dimension {dim} value {value} is in no scenario that runs"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// What the guest holds
// ---------------------------------------------------------------------------

/// One resource the guest holds across the snapshot: a `gymhold <log> <kind> <arg>`
/// process run with its working directory at `cwd`.
#[derive(Debug, Clone)]
struct Hold {
    /// Names the log, /tmp/gym-<label>.log in the container (or the guest).
    label: &'static str,
    kind: &'static str,
    arg: String,
    cwd: String,
    /// The host renames or deletes the held file before the restore.
    host_change: Option<HostChange>,
    /// Runs in the guest OS, not the container: the container's seccomp
    /// profile refuses perf_event_open, and bug 4 was perf in the guest.
    in_vm: bool,
    /// What it must do in a clone.
    expect: Expect,
}

/// What a hold must do after a restore.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expect {
    /// Every check passes.
    Works,
    /// Each check either passes or fails with an error, and none reads wrong content:
    /// what a clone cannot keep, it refuses. For a file the host renamed or deleted
    /// before the restore (a path-named volume cannot find it again, and the guest may
    /// still serve pages it cached), and for a file held open on a `--map` without
    /// `--portable-volumes`, whose handles the restored volume server never issued.
    CorrectOrError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostChange {
    Renamed,
    Deleted,
}

/// Everything the guest holds in a scenario on `storage`: every hold that storage
/// supports, all at once.
fn holds(storage: Storage, hardware_perf: bool) -> Vec<Hold> {
    let g = storage.guest_dir();
    let hold = |label, kind, arg: String| Hold {
        label,
        kind,
        arg,
        cwd: g.to_string(),
        host_change: None,
        in_vm: false,
        expect: Expect::Works,
    };
    let vm_hold = |label, kind| Hold {
        cwd: "/".to_string(),
        in_vm: true,
        ..hold(label, kind, "-".to_string())
    };
    let mut all = vec![
        hold("read", "read", format!("{g}/held")),
        hold("mmap", "mmap", format!("{g}/mapped")),
        hold("flock", "flock", format!("{g}/lock")),
        hold("exec", "exec", format!("{g}/bin/gymhold")),
        Hold {
            cwd: format!("{g}/dir"),
            ..hold("cwd", "cwd", "inside".to_string())
        },
        hold("dirfd", "dirfd", format!("{g}/dir")),
        hold("listen", "listen", "7070".to_string()),
        vm_hold("swperf", "swperf"),
    ];
    if hardware_perf {
        all.push(vm_hold("hwperf", "hwperf"));
    }
    if storage.writable() {
        all.push(hold("rw", "rw", format!("{g}/rw")));
        all.push(hold("append", "append", format!("{g}/appended")));
        all.push(hold("notify", "notify", format!("{g}/notify")));
    }
    if storage.shared_with_host() {
        all.push(Hold {
            host_change: Some(HostChange::Renamed),
            expect: Expect::CorrectOrError,
            ..hold("renamed", "read", format!("{g}/held-renamed"))
        });
        all.push(Hold {
            host_change: Some(HostChange::Deleted),
            expect: Expect::CorrectOrError,
            ..hold("deleted", "read", format!("{g}/held-deleted"))
        });
    }
    if storage == Storage::Nfs {
        // A restore remounts every NFS share under a new client, in the guest and in the
        // container (fc-agent restore.rs), because the snapshot's client carries the
        // snapshot's NFSv4 identity, which twin clones share. An inotify watch taken
        // before the snapshot stays on the old mount's directory, so files made through
        // the share afterwards raise nothing on it.
        for h in &mut all {
            if h.kind == "notify" {
                h.expect = Expect::CorrectOrError;
            }
        }
    }
    if storage == Storage::Map {
        // A --map without --portable-volumes names inodes by host inode number and
        // keeps no table of them, so a restored volume server answers EBADF for every
        // inode and handle the guest took before the snapshot, directories included:
        // a fresh exec or create under a directory the guest had looked up fails too.
        // Only the flock (local to the guest), the socket and the counter keep working.
        for h in &mut all {
            if !matches!(h.kind, "flock" | "listen" | "swperf" | "hwperf") {
                h.expect = Expect::CorrectOrError;
            }
        }
    }
    all
}

#[test]
fn every_storage_holds_what_it_supports() {
    for storage in STORAGES {
        let labels: Vec<_> = holds(storage, true).iter().map(|h| h.label).collect();
        for always in [
            "read", "mmap", "flock", "exec", "cwd", "dirfd", "listen", "swperf", "hwperf",
        ] {
            assert!(labels.contains(&always), "{storage:?} holds no {always}");
        }
        for write in ["rw", "append", "notify"] {
            assert_eq!(
                labels.contains(&write),
                storage.writable(),
                "{storage:?} {write}"
            );
        }
        for host in ["renamed", "deleted"] {
            assert_eq!(
                labels.contains(&host),
                storage.shared_with_host(),
                "{storage:?} {host}"
            );
        }
    }
}

const BLOCKS: usize = 256;

/// The patterned files and their salts. Each file's pattern is offset by its salt,
/// so a hold that reads another file's blocks (a handle aliased to a newer one, bug
/// 2) sees the wrong letter. gymhold.c computes the same.
const PATTERNED: [(&str, usize); 6] = [
    ("held", 1),
    ("mapped", 2),
    ("rw", 3),
    ("held-renamed", 4),
    ("held-deleted", 5),
    ("other", 13),
];

fn salt_of(name: &str) -> usize {
    PATTERNED
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, s)| *s)
        .unwrap_or(0)
}

/// The byte block `n` of the patterned file with `salt` holds.
fn block_byte(n: usize, salt: usize) -> u8 {
    b'a' + ((n + salt) % 26) as u8
}

fn patterned(salt: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(BLOCKS * 4096);
    for n in 0..BLOCKS {
        data.extend(std::iter::repeat_n(block_byte(n, salt), 4096));
    }
    data
}

/// Build gymhold for this host, once per test process.
async fn gymhold_binary() -> Result<PathBuf> {
    static BUILT: tokio::sync::OnceCell<PathBuf> = tokio::sync::OnceCell::const_new();
    BUILT
        .get_or_try_init(|| async {
            let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/gym/gymhold.c");
            let out = std::env::temp_dir().join(format!("fcvm-gymhold-{}", std::process::id()));
            let status = tokio::process::Command::new("cc")
                .args(["-static", "-Os", "-s", "-o"])
                .arg(&out)
                .arg(&source)
                .status()
                .await
                .context("running cc for gymhold (build-essential is a quickstart package)")?;
            ensure!(status.success(), "cc gymhold.c: {status}");
            Ok(out)
        })
        .await
        .cloned()
}

/// Lay out the files every hold uses under `dir`.
async fn populate(dir: &Path, gymhold: &Path) -> Result<()> {
    for sub in ["dir", "bin", "notify"] {
        tokio::fs::create_dir_all(dir.join(sub)).await?;
    }
    for (name, salt) in PATTERNED {
        tokio::fs::write(dir.join(name), patterned(salt)).await?;
    }
    tokio::fs::write(dir.join("dir/inside"), "inside\n").await?;
    tokio::fs::write(dir.join("appended"), "").await?;
    tokio::fs::write(dir.join("lock"), "").await?;
    tokio::fs::write(dir.join("from-host"), "before\n").await?;
    tokio::fs::copy(gymhold, dir.join("bin/gymhold")).await?;
    // The guest's root reaches the volume as other users do.
    let status = tokio::process::Command::new("chmod")
        .args(["-R", "a+rwX"])
        .arg(dir)
        .status()
        .await?;
    ensure!(status.success(), "chmod -R a+rwX {}", dir.display());
    Ok(())
}

/// An ext4 image of `dir` with 4 KiB blocks, its length a multiple of 2 MiB (what
/// `--pmem` requires; `--disk` takes it too).
async fn ext4_image(dir: &Path, image: &Path) -> Result<()> {
    let status = tokio::process::Command::new("truncate")
        .args(["-s", "64M"])
        .arg(image)
        .status()
        .await?;
    ensure!(status.success(), "truncate {}", image.display());
    let out = tokio::process::Command::new("mkfs.ext4")
        .args(["-q", "-F", "-b", "4096", "-d"])
        .arg(dir)
        .arg(image)
        .output()
        .await
        .context("running mkfs.ext4")?;
    ensure!(
        out.status.success(),
        "mkfs.ext4 -d {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Processes
// ---------------------------------------------------------------------------

/// Every fcvm process the scenario started, and the helpers under each, so teardown
/// can be checked.
#[derive(Default)]
struct Started {
    pids: Vec<u32>,
    names: Vec<String>,
    children: Vec<tokio::process::Child>,
    /// (pid, start time, what) of every firecracker, cloud-hypervisor, pasta and
    /// namespace holder under a started fcvm.
    helpers: Vec<(u32, u64, String)>,
}

impl Started {
    fn keep(&mut self, (child, pid): (tokio::process::Child, u32), name: &str) -> u32 {
        self.children.push(child);
        self.pids.push(pid);
        self.names.push(name.to_string());
        pid
    }

    /// Record the helpers now under `pid`, for the teardown oracle.
    fn track(&mut self, pid: u32) {
        let mut stack = vec![pid];
        while let Some(p) = stack.pop() {
            let Ok(tasks) = std::fs::read_dir(format!("/proc/{p}/task")) else {
                continue;
            };
            for task in tasks.flatten() {
                let children =
                    std::fs::read_to_string(task.path().join("children")).unwrap_or_default();
                for child in children
                    .split_whitespace()
                    .filter_map(|c| c.parse::<u32>().ok())
                {
                    let comm =
                        std::fs::read_to_string(format!("/proc/{child}/comm")).unwrap_or_default();
                    let comm = comm.trim();
                    if [
                        "firecracker",
                        "cloud-hyperviso",
                        "pasta",
                        "sleep",
                        "unshare",
                    ]
                    .iter()
                    .any(|c| comm.starts_with(c))
                    {
                        if let Some(start) = fcvm::utils::process_start_time(child) {
                            self.helpers.push((child, start, comm.to_string()));
                        }
                    }
                    stack.push(child);
                }
            }
        }
    }

    async fn kill(&mut self, pid: u32) {
        common::kill_process(pid).await;
        self.pids.retain(|p| *p != pid);
    }

    async fn kill_all(&mut self) {
        for pid in self.pids.drain(..).rev() {
            common::kill_process(pid).await;
        }
        for mut child in self.children.drain(..) {
            let _ = child.wait().await;
        }
    }

    /// The teardown oracle: no helper of a started fcvm and no state file of a
    /// started VM outlives it.
    async fn check_teardown(&self) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(15);
        let alive = |h: &(u32, u64, String)| fcvm::utils::process_start_time(h.0) == Some(h.1);
        while self.helpers.iter().any(alive) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let leaked: Vec<_> = self.helpers.iter().filter(|h| alive(h)).collect();
        ensure!(
            leaked.is_empty(),
            "still running after teardown: {leaked:?}"
        );
        let state_dir = fcvm::paths::state_dir();
        let mut left = Vec::new();
        for entry in std::fs::read_dir(&state_dir)
            .into_iter()
            .flatten()
            .flatten()
        {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let name = std::fs::read_to_string(&path)
                .ok()
                .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                .and_then(|v| v.get("name").and_then(|n| n.as_str()).map(String::from));
            if name.is_some_and(|n| self.names.contains(&n)) {
                left.push(path);
            }
        }
        ensure!(left.is_empty(), "state files left after teardown: {left:?}");
        Ok(())
    }
}

/// Copy `source` into VM `pid` at `dest`, through exec's stdin: into the
/// container, or with `vm` into the guest OS.
async fn copy_into(pid: u32, source: &Path, dest: &str, vm: bool) -> Result<()> {
    let fcvm = common::find_fcvm_binary()?;
    let mut command = tokio::process::Command::new(&fcvm);
    command.args(["exec", "-i", "--pid", &pid.to_string()]);
    if vm {
        command.arg("--vm");
    }
    let mut child = command
        .args(["--", "sh", "-c"])
        .arg(format!("cat > {dest} && chmod 755 {dest}"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("spawning fcvm exec -i")?;
    let bytes = tokio::fs::read(source).await?;
    let mut stdin = child.stdin.take().context("exec stdin")?;
    stdin.write_all(&bytes).await?;
    drop(stdin);
    let out = tokio::time::timeout(Duration::from_secs(60), child.wait_with_output())
        .await
        .context("copying into the container timed out")??;
    ensure!(
        out.status.success(),
        "copying {} into the container: {}",
        source.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Logs and checks
// ---------------------------------------------------------------------------

/// Every hold's log lines in VM `pid`, as label -> [(n, line)].
async fn hold_logs(
    pid: u32,
    holds: &[Hold],
) -> Result<std::collections::BTreeMap<&'static str, Vec<(usize, String)>>> {
    let script = |in_vm: bool| {
        holds
            .iter()
            .filter(|h| h.in_vm == in_vm)
            .map(|h| {
                format!(
                    "echo '== {}'; cat /tmp/gym-{}.log 2>/dev/null",
                    h.label, h.label
                )
            })
            .collect::<Vec<_>>()
            .join("; ")
    };
    let mut out = common::exec_in_container(pid, &[&script(false)]).await?;
    if holds.iter().any(|h| h.in_vm) {
        out.push('\n');
        out.push_str(&common::exec_in_vm(pid, &[&script(true)]).await?);
    }
    let mut logs = std::collections::BTreeMap::new();
    let mut current: Option<&'static str> = None;
    for line in out.lines() {
        if let Some(label) = line.strip_prefix("== ") {
            current = holds.iter().find(|h| h.label == label).map(|h| h.label);
            if let Some(l) = current {
                logs.entry(l).or_insert_with(Vec::new);
            }
            continue;
        }
        let (Some(label), Some(n)) = (
            current,
            line.split_whitespace().next().and_then(|n| n.parse().ok()),
        ) else {
            continue;
        };
        logs.entry(label)
            .or_insert_with(Vec::new)
            .push((n, line.to_string()));
    }
    Ok(logs)
}

/// The number of lines each hold has logged.
async fn log_marks(
    pid: u32,
    holds: &[Hold],
) -> Result<std::collections::BTreeMap<&'static str, usize>> {
    Ok(hold_logs(pid, holds)
        .await?
        .into_iter()
        .map(|(label, lines)| (label, lines.len()))
        .collect())
}

/// Wait until every hold has logged `count` checks past its mark, and hold each to its
/// [`Expect`]: every check passed, or (for `CorrectOrError`, after a restore) no check
/// read wrong content. `restored` is false before the first snapshot, where every hold
/// must work.
async fn holds_keep_working(
    pid: u32,
    holds: &[Hold],
    marks: &std::collections::BTreeMap<&'static str, usize>,
    count: usize,
    what: &str,
    restored: bool,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let logs = hold_logs(pid, holds).await?;
        let mut waiting = Vec::new();
        for h in holds {
            let from = marks.get(h.label).copied().unwrap_or(0);
            let lines = logs.get(h.label).map(Vec::as_slice).unwrap_or(&[]);
            let past = lines.get(from..).unwrap_or(&[]);
            let expect = if restored { h.expect } else { Expect::Works };
            let failed: Vec<_> = past
                .iter()
                .filter(|(_, l)| match expect {
                    Expect::Works => !l.ends_with(" ok"),
                    Expect::CorrectOrError => l.contains(" WRONG "),
                })
                .collect();
            ensure!(
                failed.is_empty(),
                "{what}: hold {} ({} {}, expected {expect:?}) failed: {:?}",
                h.label,
                h.kind,
                h.arg,
                &failed[..failed.len().min(3)]
            );
            // A hold that may answer with an error (CorrectOrError, after a restore) can
            // take its whole timeout per check (a notify on NFS waits 5 s for an event
            // that does not come), so one check past the mark is enough of it.
            let needed = if expect == Expect::CorrectOrError {
                1
            } else {
                count
            };
            if past.len() < needed {
                waiting.push(format!("{} {}/{needed}", h.label, past.len()));
            }
        }
        if waiting.is_empty() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            // What the stuck holds are doing, read in the guest: the kernel stack and
            // state of each hold, and the NFS clients and mounts it holds.
            let state = common::exec_in_vm(
                pid,
                &[
                    "sh",
                    "-c",
                    "cat /proc/fs/nfsfs/servers /proc/fs/nfsfs/volumes 2>/dev/null; \
                     for p in $(pgrep gymhold); do \
                       echo \"== $p $(cut -d' ' -f3 /proc/$p/stat)\"; cat /proc/$p/stack; done",
                ],
            )
            .await
            .unwrap_or_else(|error| format!("could not read the holds' state: {error:#}"));
            bail!(
                "{what}: holds still short of {count} checks after 90 s: {}\n{state}",
                waiting.join(", ")
            );
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// The address the host reaches VM `pid`'s published ports on.
async fn published_ip(pid: u32, network: Network) -> Result<String> {
    match network {
        Network::Bridged => common::get_network_field(pid, "host_ip").await,
        Network::Rootless | Network::Routed => common::get_loopback_ip(pid).await,
    }
}

const PUBLISHED: u16 = 8080;

/// A TCP connection from the host to nginx through the VM's published port, after one
/// keep-alive request and its response.
async fn hold_tcp(ip: &str) -> Result<tokio::net::TcpStream> {
    let mut stream = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::net::TcpStream::connect((ip, PUBLISHED)),
    )
    .await
    .context("connecting to the published port timed out")??;
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: gym\r\nConnection: keep-alive\r\n\r\n")
        .await?;
    let mut buf = vec![0u8; 4096];
    let n = tokio::time::timeout(Duration::from_secs(10), stream.read(&mut buf))
        .await
        .context("no response on the held connection")??;
    ensure!(
        buf[..n].starts_with(b"HTTP/1.1 200"),
        "the held connection answered {:?}",
        String::from_utf8_lossy(&buf[..n.min(80)])
    );
    Ok(stream)
}

/// The guest side of every TCP connection to nginx (port 80) that is established now,
/// as (local, peer) from the container's `netstat -tn`.
async fn established_to_nginx(pid: u32) -> Result<std::collections::BTreeSet<(String, String)>> {
    let out = common::exec_in_container(pid, &["netstat -tn"]).await?;
    Ok(out
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            (f.len() >= 6 && f[5] == "ESTABLISHED" && f[3].ends_with(":80"))
                .then(|| (f[3].to_string(), f[4].to_string()))
        })
        .collect())
}

/// The guest side of the connection the host holds: established in two reads a
/// second apart, so not a health check passing through.
async fn held_connection_in_guest(
    pid: u32,
) -> Result<std::collections::BTreeSet<(String, String)>> {
    let first = established_to_nginx(pid).await?;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let second = established_to_nginx(pid).await?;
    let held: std::collections::BTreeSet<_> = first.intersection(&second).cloned().collect();
    ensure!(
        !held.is_empty(),
        "the guest shows no connection to nginx while the host holds one; the TCP oracle would check nothing"
    );
    Ok(held)
}

/// The TCP oracle in a clone: the connection the host held across the snapshot is not
/// established in the clone (the restore retires every socket of the snapshot), and a
/// new connection through the clone's published port is served.
async fn tcp_after_restore(
    clone: u32,
    network: Network,
    held: &std::collections::BTreeSet<(String, String)>,
) -> Result<()> {
    let now = established_to_nginx(clone).await?;
    let survived: Vec<_> = held.intersection(&now).collect();
    ensure!(
        survived.is_empty(),
        "the connection the host held across the snapshot is still established in the clone: {survived:?}"
    );
    let ip = published_ip(clone, network).await?;
    let result = common::curl_check(&ip, PUBLISHED, 10).await;
    ensure!(
        result.success && result.body_len > 0,
        "a new connection to the clone's published port {ip}:{PUBLISHED} failed: {}",
        result.error
    );
    Ok(())
}

/// No fcvm log of this scenario says `unknown inode in request`, and no guest kernel
/// line of it reports a crash.
fn scan_logs(vm_name: &str) -> Result<()> {
    // unique_names gives the base, clones and serves one `<pid>-<n>` id.
    let stem = format!("-{}-", vm_name.trim_start_matches("gym-base-"));
    for entry in std::fs::read_dir("/tmp/fcvm-test-logs")
        .into_iter()
        .flatten()
        .flatten()
    {
        let path = entry.path();
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        if !name.starts_with("gym") || !name.contains(&stem) {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        for needle in [
            "unknown inode in request",
            "Unexpected exit reason on vcpu run",
            "Kernel panic",
            "Bus error",
            "Unable to handle kernel paging request",
            "Internal error: Oops",
        ] {
            if let Some(line) = text.lines().find(|l| l.contains(needle)) {
                bail!("{} says `{needle}`: {line}", path.display());
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The scenario run
// ---------------------------------------------------------------------------

struct Layout {
    /// The host directory the volume serves, or the source of the image.
    dir: PathBuf,
    image: PathBuf,
}

fn storage_args(storage: Storage, layout: &Layout) -> Vec<String> {
    let dir = layout.dir.display();
    let image = layout.image.display();
    match storage {
        Storage::Rootfs => vec![],
        Storage::Map => vec!["--map".into(), format!("{dir}:/mnt/gym")],
        Storage::PortableRw => vec![
            "--portable-volumes".into(),
            "--map".into(),
            format!("{dir}:/mnt/gym"),
        ],
        Storage::PortableRo => vec![
            "--portable-volumes".into(),
            "--map".into(),
            format!("{dir}:/mnt/gym:ro"),
        ],
        Storage::Nfs => vec![
            "--kernel-profile".into(),
            "nested".into(),
            "--nfs".into(),
            format!("{dir}:/mnt/gym"),
        ],
        Storage::DiskDir => vec!["--disk-dir".into(), format!("{dir}:/mnt/gym:ro")],
        Storage::RoDisk => vec!["--disk".into(), format!("{image}:/mnt/gym:ro")],
        Storage::Pmem => vec!["--pmem".into(), format!("{image}:/mnt/gym:ro")],
    }
}

fn network_arg(network: Network) -> &'static str {
    match network {
        Network::Rootless => "rootless",
        Network::Bridged => "bridged",
        Network::Routed => "routed",
    }
}

/// The `podman run` that starts the VM the scenario snapshots.
fn base_args(s: &Scenario, name: &str, layout: &Layout) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "podman".into(),
        "run".into(),
        "--name".into(),
        name.into(),
        "--network".into(),
        network_arg(s.network).into(),
        "--publish".into(),
        format!("{PUBLISHED}:80"),
    ];
    if s.origin == Origin::CacheHit {
        // podman run takes a startup snapshot only for a VM with a health check.
        args.extend(["--health-check".into(), "http://localhost/".into()]);
    }
    match s.restore {
        // Half the default guest size: the hugepage tests share a pool of 512 pages (1 GiB,
        // HUGEPAGE_POOL_TESTS), and a scenario runs two restored VMs at once (the clone
        // and its twin, or the clone and the restore of its snapshot).
        Restore::Hugepages => args.extend(["--hugepages".into(), "--mem".into(), "512".into()]),
        Restore::CloudHypervisor => args.extend(["--hypervisor".into(), "cloud-hypervisor".into()]),
        _ => {}
    }
    args.extend(storage_args(s.storage, layout));
    args.push(common::TEST_IMAGE.into());
    args
}

async fn spawn(
    started: &mut Started,
    args: &[String],
    name: &str,
    snapshots: bool,
) -> Result<(u32, PathBuf)> {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    // Only the CacheHit origin uses podman run's snapshot cache. Every other run turns
    // it off, so a startup snapshot taken in the background cannot race the gym's own.
    let (child, pid, log) = if snapshots {
        common::spawn_fcvm_snapshots_enabled_with_env_and_log_path(&args, &[]).await?
    } else {
        common::spawn_fcvm_with_env_and_log_path(&args, &[("FCVM_NO_SNAPSHOT", "1")]).await?
    };
    started.keep((child, pid), name);
    Ok((pid, log))
}

async fn restore(started: &mut Started, s: &Scenario, tag: &str, name: &str) -> Result<u32> {
    let pid = match s.restore {
        Restore::DirectFile | Restore::CloudHypervisor => {
            let args = ["snapshot", "run", "--snapshot", tag, "--name", name].map(String::from);
            spawn(started, &args, name, false).await?.0
        }
        serve => {
            let mut args = vec!["snapshot".to_string(), "serve".into(), tag.into()];
            match serve {
                Restore::ServeMinor => args.extend(["--uffd-mode".into(), "minor".into()]),
                Restore::ServeNoPrefetch => args.extend(["--uffd-prefetch".into(), "off".into()]),
                Restore::ServeFaultAround => {
                    args.extend(["--uffd-fault-around".into(), "65536".into()])
                }
                _ => {}
            }
            let serve_name = format!("{name}-serve");
            let (serve_pid, _) = spawn(started, &args, &serve_name, false).await?;
            common::poll_serve_ready(tag, serve_pid, 60).await?;
            let args = [
                "snapshot".to_string(),
                "run".into(),
                "--pid".into(),
                serve_pid.to_string(),
                "--name".into(),
                name.into(),
            ];
            spawn(started, &args, name, false).await?.0
        }
    };
    common::poll_health_by_pid(pid, 240)
        .await
        .with_context(|| format!("restored {name} never became healthy"))?;
    started.track(pid);
    Ok(pid)
}

/// Start every hold in VM `pid`. Each runs the gymhold on the storage under test, so
/// its text pages come through that storage too.
async fn start_holds(pid: u32, storage: Storage, holds: &[Hold]) -> Result<()> {
    let g = storage.guest_dir();
    let script = |in_vm: bool, bin: &str| {
        holds
            .iter()
            .filter(|h| h.in_vm == in_vm)
            .map(|h| {
                format!(
                    "(cd {} && nohup {bin} /tmp/gym-{}.log {} {} {} >/dev/null 2>&1 &)",
                    h.cwd,
                    h.label,
                    h.kind,
                    h.arg,
                    salt_of(h.arg.rsplit('/').next().unwrap_or_default())
                )
            })
            .collect::<Vec<_>>()
            .join("; ")
    };
    // On a --map without --portable-volumes the clone cannot serve pages of a file the
    // guest held open, gymhold's own text among them, so there it runs from a copy in
    // the container: its checks can then report what the volume does.
    let bin = if storage == Storage::Map {
        copy_into(pid, &gymhold_binary().await?, CONTAINER_GYMHOLD, false).await?;
        CONTAINER_GYMHOLD.to_string()
    } else {
        format!("{g}/bin/gymhold")
    };
    common::exec_in_container(pid, &[&script(false, &bin)]).await?;
    if holds.iter().any(|h| h.in_vm) {
        copy_into(pid, &gymhold_binary().await?, GUEST_GYMHOLD, true).await?;
        common::exec_in_vm(pid, &[&script(true, GUEST_GYMHOLD)]).await?;
    }
    Ok(())
}

/// Where the guest-OS holds run gymhold from: copied in over exec.
const GUEST_GYMHOLD: &str = "/tmp/gymhold";
/// Where the container's holds run gymhold from on a `--map` without portable volumes.
const CONTAINER_GYMHOLD: &str = "/tmp/gymhold";

/// Whether a Firecracker guest on this host gets a hardware PMU: only on an AMD
/// x86_64 host (bug 4 above). In an arm64 guest perf_event_open of
/// PERF_COUNT_HW_CPU_CYCLES fails with ENOENT (measured on a c8g.metal-48xl).
fn guest_has_hardware_perf() -> bool {
    cfg!(target_arch = "x86_64")
        && std::fs::read_to_string("/proc/cpuinfo").is_ok_and(|c| c.contains("AuthenticAMD"))
}

/// Run one scenario end to end. The oracles:
/// - every hold keeps working with the right content through the snapshot, the
///   restore and every post-restore change, and logs at least 10 checks in the clone;
///   a read hold on a file the host deleted before the restore fails, never reads;
/// - the TCP connection the host held is retired in the clone and the clone's
///   published port serves a new one;
/// - the post-restore changes ([`common_afters`] and the scenario's [`After`]) work;
/// - no log says `unknown inode in request`, no VMM exits, no guest panics;
/// - teardown leaves no helper process and no state file.
async fn run_scenario(s: Scenario) -> Result<()> {
    println!("GYM scenario {s:?}");
    let (vm_name, clone_name, tag, _) = common::unique_names("gym");
    let root = fcvm::paths::data_dir().join("gym").join(&vm_name);
    let layout = Layout {
        dir: root.join("dir"),
        image: root.join("gym.ext4"),
    };
    let mut started = Started::default();
    let result = scenario_body(&s, &mut started, &layout, &vm_name, &clone_name, &tag).await;
    started.kill_all().await;
    let teardown = started.check_teardown().await;
    let scanned = scan_logs(&vm_name);
    let _ = tokio::fs::remove_dir_all(&root).await;
    result
        .and(scanned)
        .and(teardown)
        .with_context(|| format!("gym scenario {s:?}"))
}

async fn scenario_body(
    s: &Scenario,
    started: &mut Started,
    layout: &Layout,
    vm_name: &str,
    clone_name: &str,
    tag: &str,
) -> Result<()> {
    let gymhold = gymhold_binary().await?;
    tokio::fs::create_dir_all(&layout.dir).await?;
    populate(&layout.dir, &gymhold).await?;
    if matches!(s.storage, Storage::RoDisk | Storage::Pmem) {
        ext4_image(&layout.dir, &layout.image).await?;
    }
    let holds = holds(s.storage, guest_has_hardware_perf());
    let args = base_args(s, vm_name, layout);

    // -- The VM that is snapshotted ----------------------------------------
    let base = if s.origin == Origin::CacheHit {
        let first_name = format!("{vm_name}-first");
        let mut first_args = args.clone();
        first_args[3] = first_name.clone();
        let (first, first_log) = spawn(started, &first_args, &first_name, true).await?;
        common::poll_health_by_pid(first, 240).await?;
        wait_for_startup_snapshot(&first_log).await?;
        started.kill(first).await;
        let (base, log) = spawn(started, &args, vm_name, true).await?;
        common::poll_health_by_pid(base, 240).await?;
        let text = tokio::fs::read_to_string(&log).await.unwrap_or_default();
        ensure!(
            text.contains("Startup snapshot hit!"),
            "the second podman run did not restore the first one's startup snapshot ({})",
            log.display()
        );
        base
    } else {
        let (base, _) = spawn(started, &args, vm_name, false).await?;
        common::poll_health_by_pid(base, 240).await?;
        base
    };
    started.track(base);
    let g = s.storage.guest_dir();
    if s.storage == Storage::Rootfs {
        common::exec_in_container(base, &[&format!("mkdir -p {g}/bin {g}/dir {g}/notify")]).await?;
        copy_into(base, &gymhold, &format!("{g}/bin/gymhold"), false).await?;
        common::exec_in_container(
            base,
            &[&format!(
                "for fs in held:1 mapped:2 rw:3 other:13; do {g}/bin/gymhold - make {g}/${{fs%:*}} ${{fs#*:}} || exit 1; done; \
                 echo inside > {g}/dir/inside; : > {g}/appended; : > {g}/lock"
            )],
        )
        .await?;
    }
    start_holds(base, s.storage, &holds).await?;
    let zero = std::collections::BTreeMap::new();
    holds_keep_working(base, &holds, &zero, 5, "the VM before the snapshot", false).await?;
    let held_tcp = hold_tcp(&published_ip(base, s.network).await?).await?;
    let held_in_guest = held_connection_in_guest(base).await?;
    common::exec_in_container(base, &["echo before-snapshot > /root/gym-marker"]).await?;

    // -- The snapshot -------------------------------------------------------
    let mut snapshot = tag.to_string();
    if s.origin == Origin::DiskOnly {
        create_snapshot(base, &snapshot, true).await?;
    } else {
        create_snapshot(base, &snapshot, false).await?;
    }
    let mut marks = log_marks(base, &holds).await?;
    started.kill(base).await;
    drop(held_tcp);

    let generations = match s.origin {
        Origin::Clone => 1,
        Origin::CloneOfClone => 2,
        _ => 0,
    };
    for generation in 1..=generations {
        let name = format!("{clone_name}-g{generation}");
        let clone = restore(started, s, &snapshot, &name).await?;
        holds_keep_working(clone, &holds, &marks, 5, &name, true).await?;
        let next = format!("{tag}-g{generation}");
        create_snapshot(clone, &next, false).await?;
        marks = log_marks(clone, &holds).await?;
        started.kill(clone).await;
        snapshot = next;
    }

    // -- Host-side changes before the restore -------------------------------
    for h in &holds {
        match h.host_change {
            Some(HostChange::Renamed) => {
                tokio::fs::rename(
                    layout.dir.join("held-renamed"),
                    layout.dir.join("held-renamed.moved"),
                )
                .await?
            }
            Some(HostChange::Deleted) => {
                tokio::fs::remove_file(layout.dir.join("held-deleted")).await?
            }
            None => {}
        }
    }
    if s.after == After::LateRestore {
        tokio::time::sleep(Duration::from_secs(90)).await;
    }

    // -- The restore ----------------------------------------------------------
    let clone = restore(started, s, &snapshot, clone_name).await?;
    // A disk-only clone boots: its holds start again, on fresh opens, except those of
    // files the host moved, which are no longer there to open.
    let restarted: Vec<Hold> = holds
        .iter()
        .filter(|h| h.host_change.is_none())
        .cloned()
        .map(|mut h| {
            h.expect = Expect::Works;
            h
        })
        .collect();
    let active = if s.origin == Origin::DiskOnly {
        &restarted
    } else {
        &holds
    };
    if s.origin == Origin::DiskOnly {
        // What the VM wrote to its root filesystem is there.
        let marker = common::exec_in_container(clone, &["cat /root/gym-marker"]).await?;
        ensure!(
            marker.trim() == "before-snapshot",
            "the disk-only clone's root filesystem lost the base's write: {marker:?}"
        );
        start_holds(clone, s.storage, active).await?;
        marks = std::collections::BTreeMap::new();
    } else {
        tcp_after_restore(clone, s.network, &held_in_guest).await?;
    }
    holds_keep_working(clone, active, &marks, 10, "the clone", true).await?;
    if s.after == After::LateRestore {
        let guest = common::exec_in_vm(clone, &["date +%s"]).await?;
        let guest: i64 = guest.trim().parse().context("guest date")?;
        let host = chrono::Utc::now().timestamp();
        ensure!(
            (guest - host).abs() <= 5,
            "the clone's clock is {}s off the host's after a late restore",
            guest - host
        );
    }

    common_afters(clone, s, layout).await?;

    match s.after {
        After::Nothing | After::LateRestore => {}
        After::SnapshotOfTheClone => {
            let third = format!("{tag}-g9");
            create_snapshot(clone, &third, s.origin == Origin::DiskOnly).await?;
            let at = log_marks(clone, active).await?;
            let again = restore(started, s, &third, &format!("{clone_name}-g9")).await?;
            if s.origin == Origin::DiskOnly {
                start_holds(again, s.storage, active).await?;
                holds_keep_working(
                    again,
                    active,
                    &zero,
                    10,
                    "a disk-only clone of the clone",
                    true,
                )
                .await?;
            } else {
                holds_keep_working(again, active, &at, 10, "a clone of the clone", true).await?;
            }
        }
        After::TwoClones => {
            let twin = restore(started, s, &snapshot, &format!("{clone_name}-twin")).await?;
            if s.origin == Origin::DiskOnly {
                start_holds(twin, s.storage, active).await?;
            }
            // Two clones of one NFS snapshot hold the snapshot's open files under the one
            // NFSv4 client identity they both inherited, and the server keeps one copy of
            // that client's open and lock state. Whichever twin it does not follow gets an
            // error on those files (EBADF on a write), never another file's data.
            let twin_holds: Vec<Hold> = active
                .iter()
                .cloned()
                .map(|mut h| {
                    if s.storage == Storage::Nfs
                        && !matches!(h.kind, "listen" | "swperf" | "hwperf")
                    {
                        h.expect = Expect::CorrectOrError;
                    }
                    h
                })
                .collect();
            holds_keep_working(
                twin,
                &twin_holds,
                &marks,
                10,
                "a second clone of the snapshot",
                true,
            )
            .await?;
        }
    }
    // The holds kept working through everything after the restore.
    let at = log_marks(clone, active).await?;
    holds_keep_working(
        clone,
        active,
        &at,
        5,
        "the clone after the post-restore changes",
        true,
    )
    .await?;
    Ok(())
}

async fn create_snapshot(pid: u32, tag: &str, disk_only: bool) -> Result<()> {
    let fcvm = common::find_fcvm_binary()?;
    let mut args = vec![
        "snapshot".to_string(),
        "create".into(),
        "--pid".into(),
        pid.to_string(),
    ];
    args.extend(["--tag".into(), tag.into()]);
    if disk_only {
        args.push("--disk-only".into());
    }
    let out = tokio::time::timeout(
        Duration::from_secs(300),
        tokio::process::Command::new(&fcvm)
            .args(&args)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("snapshot create timed out")??;
    let stderr = String::from_utf8_lossy(&out.stderr);
    ensure!(
        out.status.success(),
        "snapshot create {tag}: {}",
        stderr
            .lines()
            .rev()
            .find(|l| l.contains("Error"))
            .unwrap_or_else(|| stderr.lines().last().unwrap_or_default())
    );
    Ok(())
}

/// Wait until a podman run's log says it created its startup snapshot.
async fn wait_for_startup_snapshot(log: &Path) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(240);
    loop {
        let text = tokio::fs::read_to_string(log).await.unwrap_or_default();
        if text.contains("Startup snapshot created") || text.contains("startup snapshot created") {
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "{} logged no startup snapshot in 240 s",
            log.display()
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// The changes every scenario makes in the clone, each checked: new opens on the
/// volume, a fresh exec of a program on it, and where the storage allows them a guest
/// write, a guest rename, a write the host makes and a write the host reads.
async fn common_afters(clone: u32, s: &Scenario, layout: &Layout) -> Result<()> {
    let g = s.storage.guest_dir();
    for i in 0..6 {
        let block = 10 + i * 37;
        let got = common::exec_in_container(
            clone,
            &[&format!(
                "dd if={g}/other bs=4096 count=1 skip={block} 2>/dev/null | head -c 1"
            )],
        )
        .await?;
        let want = block_byte(block, salt_of("other")) as char;
        ensure!(
            got.trim() == want.to_string(),
            "a file opened after the restore read {got:?} at block {block}, want {want:?}"
        );
    }
    // bin/ is a directory the guest looked up before the snapshot, which a --map
    // without portable volumes no longer serves (see holds()).
    if s.storage != Storage::Map {
        let probe =
            common::exec_in_container(clone, &[&format!("{g}/bin/gymhold - probe - && echo ran")])
                .await?;
        ensure!(
            probe.trim().ends_with("ran"),
            "exec of {g}/bin/gymhold after the restore: {probe:?}"
        );
    }
    if s.storage.writable() {
        let out = common::exec_in_container(
            clone,
            &[&format!(
                "echo after-restore > {g}/written && mv {g}/written {g}/moved && cat {g}/moved"
            )],
        )
        .await?;
        ensure!(
            out.trim() == "after-restore",
            "write and rename in the clone: {out:?}"
        );
    }
    if s.storage.shared_with_host() && s.storage.writable() {
        let host = tokio::fs::read_to_string(layout.dir.join("moved")).await?;
        ensure!(
            host == "after-restore\n",
            "the host read {host:?} from the clone's write"
        );
    }
    if s.storage.shared_with_host() {
        tokio::fs::write(layout.dir.join("from-host"), "after\n").await?;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let got = common::exec_in_container(clone, &[&format!("cat {g}/from-host")]).await?;
            if got.trim() == "after" {
                break;
            }
            ensure!(
                Instant::now() < deadline,
                "the clone still reads {got:?} 10 s after the host rewrote from-host"
            );
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let moved =
            common::exec_in_container(clone, &[&format!("head -c 1 {g}/held-renamed.moved")])
                .await?;
        let want = block_byte(0, salt_of("held-renamed")) as char;
        ensure!(
            moved.trim() == want.to_string(),
            "the clone reads {moved:?} from the file the host renamed, want {want:?}"
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

async fn pairs(index: usize) -> Result<()> {
    match pairs_sets().0.get(index) {
        Some(s) => run_scenario(*s).await,
        None => {
            println!("GYM pairs index {index} is past the set");
            Ok(())
        }
    }
}

async fn hugepage_pairs(index: usize) -> Result<()> {
    match pairs_sets().1.get(index) {
        Some(s) => run_scenario(*s).await,
        None => {
            println!("GYM hugepage pairs index {index} is past the set");
            Ok(())
        }
    }
}

macro_rules! pairs_tests {
    ($run:ident: $($name:ident => $index:expr),* $(,)?) => {
        $(
            #[tokio::test]
            async fn $name() -> Result<()> {
                $run($index).await
            }
        )*
    };
}

pairs_tests! { pairs:
    gym_pairs_00 => 0, gym_pairs_01 => 1, gym_pairs_02 => 2, gym_pairs_03 => 3,
    gym_pairs_04 => 4, gym_pairs_05 => 5, gym_pairs_06 => 6, gym_pairs_07 => 7,
    gym_pairs_08 => 8, gym_pairs_09 => 9, gym_pairs_10 => 10, gym_pairs_11 => 11,
    gym_pairs_12 => 12, gym_pairs_13 => 13, gym_pairs_14 => 14, gym_pairs_15 => 15,
    gym_pairs_16 => 16, gym_pairs_17 => 17, gym_pairs_18 => 18, gym_pairs_19 => 19,
    gym_pairs_20 => 20, gym_pairs_21 => 21, gym_pairs_22 => 22, gym_pairs_23 => 23,
    gym_pairs_24 => 24, gym_pairs_25 => 25, gym_pairs_26 => 26, gym_pairs_27 => 27,
    gym_pairs_28 => 28, gym_pairs_29 => 29, gym_pairs_30 => 30, gym_pairs_31 => 31,
    gym_pairs_32 => 32, gym_pairs_33 => 33, gym_pairs_34 => 34, gym_pairs_35 => 35,
    gym_pairs_36 => 36, gym_pairs_37 => 37, gym_pairs_38 => 38, gym_pairs_39 => 39,
    gym_pairs_40 => 40, gym_pairs_41 => 41, gym_pairs_42 => 42, gym_pairs_43 => 43,
    gym_pairs_44 => 44, gym_pairs_45 => 45, gym_pairs_46 => 46, gym_pairs_47 => 47,
    gym_pairs_48 => 48, gym_pairs_49 => 49, gym_pairs_50 => 50, gym_pairs_51 => 51,
    gym_pairs_52 => 52, gym_pairs_53 => 53, gym_pairs_54 => 54, gym_pairs_55 => 55,
    gym_pairs_56 => 56, gym_pairs_57 => 57, gym_pairs_58 => 58, gym_pairs_59 => 59,
    gym_pairs_60 => 60, gym_pairs_61 => 61, gym_pairs_62 => 62, gym_pairs_63 => 63,
}

pairs_tests! { hugepage_pairs:
    gym_hugepage_pairs_00 => 0, gym_hugepage_pairs_01 => 1, gym_hugepage_pairs_02 => 2,
    gym_hugepage_pairs_03 => 3, gym_hugepage_pairs_04 => 4, gym_hugepage_pairs_05 => 5,
    gym_hugepage_pairs_06 => 6, gym_hugepage_pairs_07 => 7, gym_hugepage_pairs_08 => 8,
    gym_hugepage_pairs_09 => 9, gym_hugepage_pairs_10 => 10, gym_hugepage_pairs_11 => 11,
}

/// Nightly: every valid combination, `FCVM_GYM_PARALLEL` (default 8) at a time, then
/// `FCVM_GYM_SAMPLES` (default 0) more drawn at random from `FCVM_GYM_SEED`.
/// `FCVM_GYM_SHARD=i/n` takes every n-th scenario from i. Hugepage scenarios run one
/// at a time, after the rest, since they share the host's pool.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "the full cross product, run nightly by weekly.yml (make fuzz-gym)"]
async fn gym_full_cross_product() -> Result<()> {
    let (shard, shards) = match std::env::var("FCVM_GYM_SHARD") {
        Ok(spec) => {
            let (i, n) = spec.split_once('/').context("FCVM_GYM_SHARD is i/n")?;
            (i.parse::<usize>()?, n.parse::<usize>()?)
        }
        Err(_) => (0, 1),
    };
    ensure!(shard < shards, "FCVM_GYM_SHARD {shard}/{shards}");
    let parallel: usize = std::env::var("FCVM_GYM_PARALLEL")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8);
    let samples: usize = std::env::var("FCVM_GYM_SAMPLES")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(0);
    let seed: u64 = std::env::var("FCVM_GYM_SEED")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or_else(|| chrono::Utc::now().timestamp() as u64);
    let all = cross_product();
    let mut order: Vec<usize> = (0..all.len()).filter(|i| i % shards == shard).collect();
    let mut state = seed | 1;
    for _ in 0..samples {
        // xorshift64: a replayable draw, printed with the seed.
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        order.push((state % all.len() as u64) as usize);
    }
    println!(
        "GYM cross product: {} scenarios, {samples} samples from seed {seed}",
        order.len()
    );
    let (huge, plain): (Vec<usize>, Vec<usize>) = order
        .into_iter()
        .partition(|i| all[*i].restore == Restore::Hugepages);
    let mut failures = Vec::new();
    let mut ran = 0;
    for (batch, width) in [(plain, parallel.max(1)), (huge, 1)] {
        let gate = std::sync::Arc::new(tokio::sync::Semaphore::new(width));
        let mut runs = tokio::task::JoinSet::new();
        for index in batch {
            let gate = gate.clone();
            let s = all[index];
            runs.spawn(async move {
                let _slot = gate
                    .acquire_owned()
                    .await
                    .expect("the gate is never closed");
                (index, run_scenario(s).await)
            });
        }
        while let Some(joined) = runs.join_next().await {
            let (index, result) = joined.context("a scenario task panicked")?;
            ran += 1;
            if let Err(error) = result {
                println!("GYM FAIL index={index}: {error:#}");
                failures.push(format!("index {index}: {error:#}"));
            }
        }
    }
    ensure!(
        failures.is_empty(),
        "{} of {ran} scenarios failed (replay one with FCVM_GYM_SCENARIO=<index> and gym_one):\n{}",
        failures.len(),
        failures.join("\n")
    );
    Ok(())
}

/// Replay one scenario of the cross product: `FCVM_GYM_SCENARIO=<index>`.
#[tokio::test]
#[ignore = "replays the scenario FCVM_GYM_SCENARIO names"]
async fn gym_one() -> Result<()> {
    let index: usize = std::env::var("FCVM_GYM_SCENARIO")
        .context("FCVM_GYM_SCENARIO=<index into the cross product>")?
        .parse()?;
    let all = cross_product();
    let s = *all.get(index).context("index past the cross product")?;
    run_scenario(s).await
}

//! The container's cgroup, frozen across a snapshot boundary.
//!
//! A restored clone's memory is paged in on demand, and every fault is answered in the order
//! it arrived. A container that runs on every vCPU right after the restore (a workload that
//! keeps touching tens of GiB, allocating and freeing) puts its faults ahead of fc-agent's,
//! so fc-agent's restore, a few hundred milliseconds of its own work, waits behind the
//! workload's page-in. Measured on a 128 GiB, 32 vCPU guest whose 28 workers touch 40 GiB,
//! with the memory file read at 400 MB/s: the guest's restore phases took 0.3 s and its
//! acknowledgement came 91 to 109 s after the restore began (#986, #1069).
//!
//! So the boundary freezes the container's cgroup before the network gate closes
//! (`freeze`), the source thaws it when it resumes, and a restored clone thaws it right
//! before publication (`thaw`), after its own restore phases and before the restore hook,
//! which may exec into the container. Freezing is a reversible gate on the VM being
//! snapshotted: it changes no kernel state that a thaw does not restore.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Where podman, started by fc-agent's service, puts the container's cgroup.
const PAYLOAD_PARENT: &str = "/sys/fs/cgroup/system.slice/fc-agent.service";
const PAYLOAD_PREFIX: &str = "libpod-payload-";

/// How long a freeze may take to settle before the boundary goes on without it.
const FREEZE_SETTLE: Duration = Duration::from_secs(5);

/// The cgroups this boundary froze, one path a line. On /run, which is in guest memory, so a
/// restored clone reads the record its snapshot was taken with. A thaw thaws only these: a
/// container someone paused before the snapshot (`podman pause` freezes the same cgroup)
/// stays paused.
const FROZEN_RECORD: &str = "/run/fcvm-payload-frozen";

/// The container cgroups under `parent`.
fn payload_cgroups(parent: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut cgroups: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(PAYLOAD_PREFIX)
                && entry.path().join("cgroup.freeze").exists()
        })
        .map(|entry| entry.path())
        .collect();
    cgroups.sort();
    cgroups
}

/// Whether `cgroup`'s `cgroup.events` says it is frozen.
fn is_frozen(cgroup: &Path) -> bool {
    std::fs::read_to_string(cgroup.join("cgroup.events"))
        .is_ok_and(|events| events.lines().any(|line| line.trim() == "frozen 1"))
}

/// Write `value` to `cgroup`'s `cgroup.freeze`, and say whether it was written.
fn write_freeze(cgroup: &Path, value: &str) -> bool {
    match std::fs::write(cgroup.join("cgroup.freeze"), value) {
        Ok(()) => true,
        Err(error) => {
            eprintln!(
                "[fc-agent] WARNING: writing {value} to {}/cgroup.freeze failed: {error}",
                cgroup.display()
            );
            false
        }
    }
}

/// Freeze the container's cgroups under `parent` that are not frozen already, record them in
/// `record`, and wait, up to [`FREEZE_SETTLE`], for the kernel to report them frozen. Never
/// fails: a container that is not frozen in time is still snapshotted correctly, only without
/// the benefit.
fn freeze_in(parent: &Path, record: &Path) -> usize {
    let mut frozen = Vec::new();
    for cgroup in payload_cgroups(parent) {
        let paused = std::fs::read_to_string(cgroup.join("cgroup.freeze"))
            .is_ok_and(|value| value.trim() == "1");
        if !paused && write_freeze(&cgroup, "1") {
            frozen.push(cgroup);
        }
    }
    let listed: String = frozen
        .iter()
        .map(|cgroup| format!("{}\n", cgroup.display()))
        .collect();
    if let Err(error) = std::fs::write(record, listed) {
        // Without the record a thaw would leave these frozen, so undo the freeze now.
        eprintln!(
            "[fc-agent] WARNING: recording the frozen container cgroups in {} failed: {error}",
            record.display()
        );
        for cgroup in &frozen {
            write_freeze(cgroup, "0");
        }
        return 0;
    }
    let started = Instant::now();
    while !frozen.iter().all(|cgroup| is_frozen(cgroup)) {
        if started.elapsed() >= FREEZE_SETTLE {
            eprintln!(
                "[fc-agent] WARNING: the container's cgroup did not report frozen within {FREEZE_SETTLE:?}"
            );
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    frozen.len()
}

/// Thaw the cgroups `record` names, the ones a freeze froze, and remove it. No record, no
/// thaw.
fn thaw_in(record: &Path) -> usize {
    let Ok(listed) = std::fs::read_to_string(record) else {
        return 0;
    };
    let thawed = listed
        .lines()
        .filter(|line| !line.is_empty())
        .filter(|line| write_freeze(Path::new(line), "0"))
        .count();
    let _ = std::fs::remove_file(record);
    thawed
}

/// Freeze the container before the snapshot boundary closes.
///
/// Not when the guest's GPUs are parked for a warm GPU snapshot: a clone unparks them during
/// its restore, before publication, by restoring the checkpointed CUDA processes and running
/// the container's GPU hook, which must find the container running.
pub fn freeze() {
    if Path::new(crate::gpu_park::PARKED_RECORD).exists() {
        eprintln!("[fc-agent] the GPUs are parked; the container is not frozen for this snapshot");
        return;
    }
    let started = Instant::now();
    let n = freeze_in(Path::new(PAYLOAD_PARENT), Path::new(FROZEN_RECORD));
    if n > 0 {
        eprintln!(
            "[fc-agent] froze {n} container cgroup(s) for the snapshot boundary in {} ms",
            started.elapsed().as_millis()
        );
    }
}

/// Thaw the container: on the source when it resumes, on a clone before publication.
pub fn thaw() {
    let n = thaw_in(Path::new(FROZEN_RECORD));
    if n > 0 {
        eprintln!("[fc-agent] thawed {n} container cgroup(s)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake cgroup tree: `cgroup.freeze` files the functions write and `cgroup.events`
    /// files the test sets.
    fn fake_payload(parent: &Path, name: &str, frozen: bool) -> PathBuf {
        let cgroup = parent.join(name);
        std::fs::create_dir_all(&cgroup).unwrap();
        std::fs::write(cgroup.join("cgroup.freeze"), "0").unwrap();
        std::fs::write(
            cgroup.join("cgroup.events"),
            format!("populated 1\nfrozen {}\n", u8::from(frozen)),
        )
        .unwrap();
        cgroup
    }

    #[test]
    fn freeze_writes_every_container_cgroup_and_thaw_undoes_it() {
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("frozen");
        let a = fake_payload(dir.path(), "libpod-payload-aaa", true);
        let b = fake_payload(dir.path(), "libpod-payload-bbb", true);
        let other = fake_payload(dir.path(), "conmon", false);
        assert_eq!(freeze_in(dir.path(), &record), 2);
        for cgroup in [&a, &b] {
            assert_eq!(
                std::fs::read_to_string(cgroup.join("cgroup.freeze")).unwrap(),
                "1"
            );
        }
        assert_eq!(
            std::fs::read_to_string(other.join("cgroup.freeze")).unwrap(),
            "0"
        );
        assert_eq!(thaw_in(&record), 2);
        assert!(!record.exists(), "the thaw left its record");
        for cgroup in [&a, &b] {
            assert_eq!(
                std::fs::read_to_string(cgroup.join("cgroup.freeze")).unwrap(),
                "0"
            );
        }
    }

    #[test]
    fn no_container_is_nothing_to_freeze() {
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("frozen");
        assert_eq!(freeze_in(dir.path(), &record), 0);
        assert_eq!(freeze_in(&dir.path().join("missing"), &record), 0);
        assert_eq!(thaw_in(&dir.path().join("no-record")), 0);
    }

    /// A container paused before the snapshot (`podman pause` writes the same cgroup.freeze)
    /// is not the boundary's to thaw: it stays paused through the freeze and the thaw, and the
    /// boundary thaws only what it froze.
    #[test]
    fn a_container_paused_before_the_boundary_stays_paused() {
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("frozen");
        let paused = fake_payload(dir.path(), "libpod-payload-paused", true);
        std::fs::write(paused.join("cgroup.freeze"), "1").unwrap();
        let running = fake_payload(dir.path(), "libpod-payload-running", true);
        assert_eq!(freeze_in(dir.path(), &record), 1);
        assert_eq!(thaw_in(&record), 1);
        assert_eq!(
            std::fs::read_to_string(paused.join("cgroup.freeze")).unwrap(),
            "1",
            "the thaw resumed a container someone had paused"
        );
        assert_eq!(
            std::fs::read_to_string(running.join("cgroup.freeze")).unwrap(),
            "0"
        );
    }
}

//! Cgroups that hold a set of processes on exact host CPUs, under a delegated systemd unit.
//!
//! Two callers: the potato ring (`agent::ring_cpus`, one cgroup on the unit's last K CPUs)
//! and pages mode (`renders`, one cgroup per side and one for the harness). Both need the
//! same four things: the process runs in a subgroup of a delegated unit (systemd
//! `Delegate=yes` with `DelegateSubgroup=`), so the unit's own cgroup holds no process and
//! its children can be given the cpuset controller; a cgroup's `cpuset.cpus` is written and
//! then read back from `cpuset.cpus.effective`, because the claim is what the kernel made of
//! the request; a process joins before it executes, so nothing of it runs elsewhere; and
//! `cgroup.kill` ends whatever is left.

use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Parse a cpuset list (`0-3,8,10-11`).
pub fn parse_cpu_list(list: &str) -> Option<Vec<u32>> {
    let mut cpus = Vec::new();
    for part in list.trim().split(',').filter(|p| !p.is_empty()) {
        match part.split_once('-') {
            Some((a, b)) => {
                let (a, b): (u32, u32) = (a.trim().parse().ok()?, b.trim().parse().ok()?);
                if b < a {
                    return None;
                }
                cpus.extend(a..=b);
            }
            None => cpus.push(part.trim().parse().ok()?),
        }
    }
    cpus.sort_unstable();
    cpus.dedup();
    Some(cpus)
}

pub fn cpu_list(cpus: &[u32]) -> String {
    cpus.iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

/// The cgroup a process is in, below the cgroup2 root, from its `/proc/<pid>/cgroup`.
pub fn own_cgroup(proc_cgroup: &str) -> Option<PathBuf> {
    let path = proc_cgroup.lines().find_map(|l| l.strip_prefix("0::"))?;
    Some(PathBuf::from(path.trim().trim_start_matches('/')))
}

/// Whether systemd delegated `dir` (`Delegate=`): it marks such a cgroup with the
/// `trusted.delegate` or `user.delegate` attribute. Anything else in the tree is systemd's
/// to write, whatever the process's uid allows.
pub fn systemd_delegated(dir: &Path) -> bool {
    let Ok(path) = CString::new(dir.as_os_str().as_encoded_bytes()) else {
        return false;
    };
    [c"trusted.delegate", c"user.delegate"].iter().any(|name| {
        let mut value = [0u8; 8];
        // SAFETY: NUL-terminated path and name, and a buffer of the length given.
        let n = unsafe {
            libc::getxattr(
                path.as_ptr(),
                name.as_ptr(),
                value.as_mut_ptr().cast(),
                value.len(),
            )
        };
        n == 1 && value[0] == b'1'
    })
}

fn read(p: &Path) -> Result<String, String> {
    std::fs::read_to_string(p).map_err(|e| format!("reading {}: {e}", p.display()))
}

fn write(p: &Path, v: &str) -> Result<(), String> {
    std::fs::write(p, v).map_err(|e| format!("writing {v:?} to {}: {e}", p.display()))
}

fn mkdir(p: &Path) -> Result<(), String> {
    match std::fs::create_dir(p) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(format!("creating {}: {e}", p.display())),
    }
}

/// The delegated unit this process runs in a subgroup of.
#[derive(Debug, Clone)]
pub struct Unit {
    /// The unit's cgroup, absolute.
    dir: PathBuf,
    /// The same below the cgroup2 root.
    below: PathBuf,
    /// This process's subgroup, by name.
    own: String,
    /// The CPUs the unit may use.
    cpus: Vec<u32>,
}

impl Unit {
    /// `root` is the cgroup2 mount, `own` this process's cgroup below it, and `delegated`
    /// says whether a cgroup is this process's to write. Turns the cpuset controller on for
    /// the unit's children. The error says why the host cannot confine anything.
    pub fn open(
        root: &Path,
        own: &Path,
        delegated: &dyn Fn(&Path) -> bool,
    ) -> Result<Self, String> {
        let below = own
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .ok_or_else(|| format!("the agent's cgroup /{} has no parent", own.display()))?;
        let dir = root.join(below);
        if !delegated(&dir) {
            return Err(format!(
                "{} is not delegated to the agent (systemd Delegate=yes with DelegateSubgroup=)",
                dir.display()
            ));
        }
        let controllers = read(&dir.join("cgroup.controllers"))?;
        if !controllers.split_whitespace().any(|c| c == "cpuset") {
            return Err(format!(
                "{} has no cpuset controller ({})",
                dir.display(),
                controllers.trim()
            ));
        }
        write(&dir.join("cgroup.subtree_control"), "+cpuset")?;
        let available = read(&dir.join("cpuset.cpus.effective"))?;
        let cpus = parse_cpu_list(&available)
            .ok_or_else(|| format!("cpuset.cpus.effective is not a CPU list: {available:?}"))?;
        Ok(Unit {
            dir,
            below: below.to_path_buf(),
            own: own
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            cpus,
        })
    }

    /// The unit of the running process.
    pub fn for_this_process() -> Result<Self, String> {
        let own = std::fs::read_to_string("/proc/self/cgroup")
            .map_err(|e| format!("reading /proc/self/cgroup: {e}"))?;
        let own = own_cgroup(&own).ok_or("no cgroup2 entry in /proc/self/cgroup")?;
        Self::open(
            Path::new(crate::cgroup::CGROUP2_ROOT),
            &own,
            &systemd_delegated,
        )
    }

    /// The CPUs the unit may use, ascending.
    pub fn cpus(&self) -> &[u32] {
        &self.cpus
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The subgroup this process runs in, by name.
    pub fn own(&self) -> &str {
        &self.own
    }

    /// Turn a further controller on for the unit's children (`memory`, for a cgroup whose
    /// own children are to be measured).
    pub fn enable(&self, controller: &str) -> Result<(), String> {
        write(
            &self.dir.join("cgroup.subtree_control"),
            &format!("+{controller}"),
        )
    }
}

/// A cgroup under a unit whose processes run on exactly `cpus`.
#[derive(Debug)]
pub struct CpuCgroup {
    dir: PathBuf,
    /// `dir` below the cgroup2 root: an `FCVM_CGROUP_PARENT`.
    below: String,
    cpus: Vec<u32>,
}

impl CpuCgroup {
    /// Make (or take over) the unit's child `name` and confine it to `cpus`. The claim is the
    /// kernel's answer: a cgroup the kernel left on other CPUs is an error.
    pub fn create(unit: &Unit, name: &str, cpus: &[u32]) -> Result<Self, String> {
        if let Some(missing) = cpus.iter().find(|c| !unit.cpus.contains(c)) {
            return Err(format!(
                "CPU {missing} of {name} is not one the unit may use ({})",
                cpu_list(&unit.cpus)
            ));
        }
        let dir = unit.dir.join(name);
        mkdir(&dir)?;
        write(&dir.join("cpuset.cpus"), &cpu_list(cpus))?;
        let effective = read(&dir.join("cpuset.cpus.effective"))?;
        if parse_cpu_list(&effective).as_deref() != Some(cpus) {
            return Err(format!(
                "{} runs on CPUs {:?}, not the {} asked for",
                dir.display(),
                effective.trim(),
                cpu_list(cpus)
            ));
        }
        Ok(CpuCgroup {
            dir,
            below: unit.below.join(name).display().to_string(),
            cpus: cpus.to_vec(),
        })
    }

    /// A cgroup under this one on `cpus`, a subset of its own. This cgroup then holds no
    /// process itself: its processes join its children.
    pub fn child(&self, name: &str, cpus: &[u32]) -> Result<Self, String> {
        if let Some(missing) = cpus.iter().find(|c| !self.cpus.contains(c)) {
            return Err(format!(
                "CPU {missing} of {name} is not one of {}'s ({})",
                self.dir.display(),
                cpu_list(&self.cpus)
            ));
        }
        self.enable("cpuset")?;
        let dir = self.dir.join(name);
        mkdir(&dir)?;
        write(&dir.join("cpuset.cpus"), &cpu_list(cpus))?;
        let effective = read(&dir.join("cpuset.cpus.effective"))?;
        if parse_cpu_list(&effective).as_deref() != Some(cpus) {
            return Err(format!(
                "{} runs on CPUs {:?}, not the {} asked for",
                dir.display(),
                effective.trim(),
                cpu_list(cpus)
            ));
        }
        Ok(CpuCgroup {
            dir,
            below: format!("{}/{name}", self.below),
            cpus: cpus.to_vec(),
        })
    }

    pub fn cpus(&self) -> &[u32] {
        &self.cpus
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// `dir` below the cgroup2 root.
    pub fn below(&self) -> &str {
        &self.below
    }

    /// Where a process joins: the cgroup itself, or its child `child`, made if absent. A
    /// cgroup whose children carry a controller holds no process of its own, so its
    /// processes join a child.
    pub fn join(&self, child: Option<&str>) -> Result<Join, String> {
        let dir = match child {
            Some(c) => {
                let d = self.dir.join(c);
                mkdir(&d)?;
                d
            }
            None => self.dir.clone(),
        };
        let procs = CString::new(dir.join("cgroup.procs").as_os_str().as_encoded_bytes())
            .map_err(|e| e.to_string())?;
        Ok(Join { dir, procs })
    }

    /// Turn `controller` on for this cgroup's children.
    pub fn enable(&self, controller: &str) -> Result<(), String> {
        write(
            &self.dir.join("cgroup.subtree_control"),
            &format!("+{controller}"),
        )
    }

    /// The CPU time its processes have used, in microseconds (`cpu.stat`'s `usage_usec`,
    /// which counts every cgroup under it).
    pub fn usage_usec(&self) -> Option<u64> {
        stat_field(&self.dir.join("cpu.stat"), "usage_usec")
    }

    /// The time in which some task of the cgroup waited for a CPU, in microseconds
    /// (`cpu.pressure`, the `some` line's `total`).
    pub fn pressure_usec(&self) -> Option<u64> {
        pressure_some_total(&std::fs::read_to_string(self.dir.join("cpu.pressure")).ok()?)
    }

    /// Whether any process is left in it.
    pub fn populated(&self) -> bool {
        populated(&self.dir)
    }

    /// SIGKILL every process left in it (`cgroup.kill`).
    pub fn kill(&self) -> std::io::Result<()> {
        std::fs::write(self.dir.join("cgroup.kill"), "1")
    }

    /// Wait until no process is left, for at most `deadline`.
    pub async fn emptied(&self, deadline: Duration) -> bool {
        let until = tokio::time::Instant::now() + deadline;
        while self.populated() {
            if tokio::time::Instant::now() >= until {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        true
    }
}

/// `key value` from a flat-keyed cgroup file (`cpu.stat`, `memory.stat`).
pub fn stat_field(path: &Path, key: &str) -> Option<u64> {
    let stat = std::fs::read_to_string(path).ok()?;
    stat.lines().find_map(|l| {
        let (k, v) = l.split_once(' ')?;
        (k == key).then(|| v.trim().parse().ok())?
    })
}

/// The `some` line's `total` of a pressure file.
pub fn pressure_some_total(text: &str) -> Option<u64> {
    text.lines()
        .find(|l| l.starts_with("some "))?
        .split_whitespace()
        .find_map(|f| f.strip_prefix("total="))?
        .parse()
        .ok()
}

/// Whether any process is left in the cgroup at `dir` or below it.
pub fn populated(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join("cgroup.events"))
        .map(|e| e.lines().any(|l| l.trim() == "populated 1"))
        .unwrap_or(false)
}

/// End a cgroup that may still hold processes: `cgroup.kill`, wait until it is empty (at
/// most `deadline`), then remove it, children first. True when the directory is gone. For a
/// process that outlives the one that made the cgroup, so it blocks.
pub fn destroy(dir: &Path, deadline: Duration) -> bool {
    if !dir.exists() {
        return true;
    }
    let _ = std::fs::write(dir.join("cgroup.kill"), "1");
    let until = std::time::Instant::now() + deadline;
    while populated(dir) && std::time::Instant::now() < until {
        std::thread::sleep(Duration::from_millis(20));
    }
    remove_tree(dir);
    !dir.exists()
}

/// rmdir a cgroup and the cgroups under it. A cgroup directory holds only cgroups and
/// interface files, and rmdir is what removes one.
fn remove_tree(dir: &Path) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                remove_tree(&e.path());
            }
        }
    }
    let _ = std::fs::remove_dir(dir);
}

/// Where a process joins a cgroup.
#[derive(Debug, Clone)]
pub struct Join {
    dir: PathBuf,
    procs: CString,
}

impl Join {
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// `<dir>/cgroup.procs`, for a program that writes its own pid there.
    pub fn procs(&self) -> PathBuf {
        self.dir.join("cgroup.procs")
    }

    /// Make `cmd` a process of the cgroup: it joins after the fork and before it executes,
    /// so nothing of it runs outside the cgroup's CPUs. A process that cannot join does not
    /// start.
    pub fn confine(&self, cmd: &mut std::process::Command) {
        use std::os::unix::process::CommandExt;
        let procs = self.procs.clone();
        // SAFETY: runs in the forked child before exec and makes three async-signal-safe
        // calls (open, write, close) on memory allocated before the fork.
        unsafe {
            cmd.pre_exec(move || {
                let fd = libc::open(procs.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
                if fd < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // "0" is the writing process
                let n = libc::write(fd, b"0".as_ptr().cast(), 1);
                let result = if n == 1 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                };
                libc::close(fd);
                result
            });
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) const OWN: &str = "system.slice/agent.service/agent";

    /// A cgroup2 tree as files: the unit's interface files as the kernel would show them,
    /// and for each `(name, effective)` a child cgroup whose effective CPUs are what the
    /// kernel would answer after the write.
    pub(crate) fn tree(
        controllers: &str,
        unit_cpus: &str,
        children: &[(&str, &str)],
    ) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        let unit = root.path().join("system.slice/agent.service");
        std::fs::create_dir_all(unit.join("agent")).unwrap();
        std::fs::write(unit.join("cgroup.controllers"), controllers).unwrap();
        std::fs::write(unit.join("cpuset.cpus.effective"), unit_cpus).unwrap();
        for (name, effective) in children {
            std::fs::create_dir(unit.join(name)).unwrap();
            std::fs::write(unit.join(name).join("cpuset.cpus.effective"), effective).unwrap();
        }
        root
    }

    /// Every caller's cgroups: the ring's one on the unit's last CPUs, and pages mode's
    /// three on a 96-CPU host.
    const CALLERS: [(&str, &str, &str); 4] = [
        ("potato", "12-15", "12,13,14,15"),
        ("renders-fcvm", "0-39", ""),
        ("renders-engines", "40-79", ""),
        ("renders-harness", "80-95", ""),
    ];

    #[test]
    fn each_callers_cgroup_is_confined_to_the_cpus_it_asks_for() {
        for (name, cpus, _) in CALLERS {
            let root = tree("cpuset cpu memory\n", "0-95\n", &[(name, cpus)]);
            let unit = Unit::open(root.path(), Path::new(OWN), &|_| true).unwrap();
            assert_eq!(unit.cpus().len(), 96);
            assert_eq!(unit.own(), "agent");
            let want = parse_cpu_list(cpus).unwrap();
            let group = CpuCgroup::create(&unit, name, &want).unwrap();
            assert_eq!(group.cpus(), &want[..], "{name}");
            assert_eq!(
                group.below(),
                format!("system.slice/agent.service/{name}"),
                "{name}"
            );
            let dir = root.path().join("system.slice/agent.service");
            assert_eq!(
                std::fs::read_to_string(dir.join("cgroup.subtree_control")).unwrap(),
                "+cpuset"
            );
            assert_eq!(
                std::fs::read_to_string(dir.join(name).join("cpuset.cpus")).unwrap(),
                cpu_list(&want),
                "{name}"
            );
        }
    }

    /// Confinement is claimed only when the kernel says so, for every caller's cgroup.
    #[test]
    fn each_callers_cgroup_says_why_it_cannot_be_confined() {
        let own = Path::new(OWN);
        for (name, cpus, _) in CALLERS {
            let want = parse_cpu_list(cpus).unwrap();
            let root = tree("cpuset\n", "0-95\n", &[(name, cpus)]);
            let refused = Unit::open(root.path(), own, &|_| false).unwrap_err();
            assert!(refused.contains("not delegated"), "{name}: {refused}");
            assert!(
                !root
                    .path()
                    .join("system.slice/agent.service/cgroup.subtree_control")
                    .exists(),
                "nothing is written to a cgroup that is not the agent's"
            );
            let root = tree("cpu memory\n", "0-95\n", &[(name, cpus)]);
            let refused = Unit::open(root.path(), own, &|_| true).unwrap_err();
            assert!(
                refused.contains("no cpuset controller"),
                "{name}: {refused}"
            );
            // the unit may not use the CPUs asked for
            let root = tree("cpuset\n", "0-7\n", &[(name, cpus)]);
            let unit = Unit::open(root.path(), own, &|_| true).unwrap();
            let refused = CpuCgroup::create(&unit, name, &want).unwrap_err();
            assert!(
                refused.contains("not one the unit may use"),
                "{name}: {refused}"
            );
            // the kernel left the cgroup on other CPUs than the write asked for
            let root = tree("cpuset\n", "0-95\n", &[(name, "0-95\n")]);
            let unit = Unit::open(root.path(), own, &|_| true).unwrap();
            let refused = CpuCgroup::create(&unit, name, &want).unwrap_err();
            assert!(refused.contains("asked for"), "{name}: {refused}");
            // no interface file answers: not a cgroup2 tree
            let root = tree("cpuset\n", "0-95\n", &[]);
            let unit = Unit::open(root.path(), own, &|_| true).unwrap();
            let refused = CpuCgroup::create(&unit, name, &want).unwrap_err();
            assert!(
                refused.contains("cpuset.cpus.effective"),
                "{name}: {refused}"
            );
        }
        let root = tree("cpuset\n", "0-95\n", &[]);
        assert!(Unit::open(root.path(), Path::new("agent"), &|_| true)
            .unwrap_err()
            .contains("no parent"));
    }

    /// A process joins the cgroup, or a child of it, before it executes, and one that
    /// cannot does not start.
    #[test]
    fn a_process_joins_before_it_executes_or_does_not_start() {
        for (name, cpus, _) in CALLERS {
            let root = tree("cpuset\n", "0-95\n", &[(name, cpus)]);
            let unit = Unit::open(root.path(), Path::new(OWN), &|_| true).unwrap();
            let group = CpuCgroup::create(&unit, name, &parse_cpu_list(cpus).unwrap()).unwrap();
            for child in [None, Some("tools")] {
                let join = group.join(child).unwrap();
                std::fs::write(join.procs(), "").unwrap();
                let mut cmd = std::process::Command::new("/bin/true");
                join.confine(&mut cmd);
                assert!(cmd.output().unwrap().status.success());
                assert_eq!(std::fs::read_to_string(join.procs()).unwrap(), "0");
                std::fs::remove_file(join.procs()).unwrap();
                let mut cmd = std::process::Command::new("/bin/true");
                join.confine(&mut cmd);
                assert!(
                    cmd.output().is_err(),
                    "{name}: no cgroup to join, so the process is not started outside it"
                );
            }
        }
    }

    /// The harness's cgroup is split by CPUs (each replay, the drivers); a child is held to
    /// the same rule as its parent.
    #[test]
    fn a_child_cgroup_is_confined_to_a_subset_of_its_parents_cpus() {
        let root = tree("cpuset\n", "0-95\n", &[("renders-harness", "80-95")]);
        let unit = Unit::open(root.path(), Path::new(OWN), &|_| true).unwrap();
        let harness =
            CpuCgroup::create(&unit, "renders-harness", &(80..96).collect::<Vec<_>>()).unwrap();
        let outside = harness.child("replay", &[79, 80]).unwrap_err();
        assert!(outside.contains("is not one of"), "{outside}");
        std::fs::create_dir(harness.dir().join("replay")).unwrap();
        std::fs::write(
            harness.dir().join("replay/cpuset.cpus.effective"),
            "80-83\n",
        )
        .unwrap();
        let replay = harness.child("replay", &[80, 81, 82, 83]).unwrap();
        assert_eq!(replay.cpus(), &[80, 81, 82, 83]);
        assert_eq!(
            replay.below(),
            "system.slice/agent.service/renders-harness/replay"
        );
        assert_eq!(
            std::fs::read_to_string(harness.dir().join("cgroup.subtree_control")).unwrap(),
            "+cpuset"
        );
        // the kernel left it on the parent's CPUs
        std::fs::create_dir(harness.dir().join("drivers")).unwrap();
        std::fs::write(
            harness.dir().join("drivers/cpuset.cpus.effective"),
            "80-95\n",
        )
        .unwrap();
        let refused = harness.child("drivers", &[88, 89]).unwrap_err();
        assert!(refused.contains("asked for"), "{refused}");
    }

    #[test]
    fn counters_are_read_from_the_cgroups_files() {
        let root = tree("cpuset\n", "0-95\n", &[("renders-fcvm", "0-39")]);
        let unit = Unit::open(root.path(), Path::new(OWN), &|_| true).unwrap();
        let group = CpuCgroup::create(&unit, "renders-fcvm", &(0..40).collect::<Vec<_>>()).unwrap();
        assert_eq!(group.usage_usec(), None);
        assert_eq!(group.pressure_usec(), None);
        assert!(!group.populated());
        std::fs::write(
            group.dir().join("cpu.stat"),
            "usage_usec 1234567\nuser_usec 1\nsystem_usec 2\n",
        )
        .unwrap();
        std::fs::write(
            group.dir().join("cpu.pressure"),
            "some avg10=1.50 avg60=0.40 avg300=0.10 total=987654\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=12\n",
        )
        .unwrap();
        std::fs::write(group.dir().join("cgroup.events"), "populated 1\nfrozen 0\n").unwrap();
        assert_eq!(group.usage_usec(), Some(1_234_567));
        assert_eq!(group.pressure_usec(), Some(987_654));
        assert!(group.populated());
        assert_eq!(pressure_some_total("full avg10=0 total=3\n"), None);
    }

    #[test]
    fn cpu_lists_parse() {
        assert_eq!(
            parse_cpu_list("0-3,8,10-11\n"),
            Some(vec![0, 1, 2, 3, 8, 10, 11])
        );
        assert_eq!(parse_cpu_list(""), Some(vec![]));
        assert_eq!(parse_cpu_list("3-1"), None);
        assert_eq!(parse_cpu_list("a"), None);
        assert_eq!(
            own_cgroup("0::/system.slice/agent.service/agent\n"),
            Some(PathBuf::from("system.slice/agent.service/agent"))
        );
        assert_eq!(own_cgroup("1:name=systemd:/x\n"), None);
    }
}

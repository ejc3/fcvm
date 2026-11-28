//! The potato ring's CPUs (docs/fleet/ARCHITECTURE.md, "Potato mode", "The CPU allotment").
//!
//! Two things make "N machines on K CPUs" true.
//!
//! The ledger ([`Cpus`]) is the agent's one record of which machines are not asleep. A
//! machine takes a CPU before its boot or wake starts and gives it back once its save has
//! finished and its fcvm has exited, and a take is refused while K are held, so the count
//! never passes K. The potato driver is one task, and it is the ledger's only writer.
//!
//! The cpuset ([`RingCgroup`]) makes the K physical. It is a cgroup under the agent's
//! delegated unit whose `cpuset.cpus` is exactly K host CPUs. Every process the agent starts
//! for the ring joins it before it executes, and fcvm puts each VM's own scope under it
//! (`FCVM_CGROUP_PARENT`), so the VMMs, the restores and the `fcvm snapshot create` commands
//! run on those K CPUs and nowhere else. A host where the cgroup cannot be made (no
//! delegation, no cpuset controller, fewer than K CPUs) keeps the ledger alone, and the
//! agent says so (`confined: false`).

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

pub use crate::fleet::cpuset::{own_cgroup, parse_cpu_list, systemd_delegated};
use crate::fleet::cpuset::{CpuCgroup, Join, Unit};

/// The fewest CPUs a ring can be played on: one machine holds the ball, one is saving and
/// one must be waking.
pub const MIN_CPUS: u32 = 3;

/// The ring's CPU allotment from `--potato-cpus`, refused below [`MIN_CPUS`].
pub fn allotment(cpus: u32) -> anyhow::Result<u32> {
    anyhow::ensure!(
        cpus >= MIN_CPUS,
        "--potato-cpus {cpus}: a ring needs at least {MIN_CPUS} CPUs, since one machine holds \
         the ball, one is saving and one must be waking"
    );
    Ok(cpus)
}

/// What holds one of the ring's CPUs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Holder {
    /// The VM the player snapshot is made from, while it is being made.
    Gold,
    Player(u32),
}

/// Which machines hold a CPU. Never more than `cap`.
#[derive(Debug, Clone)]
pub struct Cpus {
    cap: u32,
    held: BTreeSet<Holder>,
    /// The most held at once since [`Cpus::take_peak`] last read it.
    peak: u32,
}

impl Cpus {
    pub fn new(cap: u32) -> Self {
        Cpus {
            cap,
            held: BTreeSet::new(),
            peak: 0,
        }
    }

    pub fn cap(&self) -> u32 {
        self.cap
    }

    pub fn in_use(&self) -> u32 {
        self.held.len() as u32
    }

    pub fn holds(&self, h: Holder) -> bool {
        self.held.contains(&h)
    }

    pub fn has_free(&self) -> bool {
        self.in_use() < self.cap
    }

    /// Give `h` a CPU. False, with nothing changed, when every CPU is held by another.
    #[must_use]
    pub fn take(&mut self, h: Holder) -> bool {
        if self.held.contains(&h) {
            return true;
        }
        if !self.has_free() {
            return false;
        }
        self.held.insert(h);
        self.peak = self.peak.max(self.in_use());
        true
    }

    pub fn release(&mut self, h: Holder) {
        self.held.remove(&h);
    }

    /// The most held at once since the last call, and start counting again from now.
    pub fn take_peak(&mut self) -> u32 {
        let peak = self.peak.max(self.in_use());
        self.peak = self.in_use();
        peak
    }

    pub fn holders(&self) -> impl Iterator<Item = Holder> + '_ {
        self.held.iter().copied()
    }
}

/// The cgroup the ring's processes run in, confined to K host CPUs
/// ([`crate::fleet::cpuset`], which pages mode's cgroups share).
#[derive(Debug)]
pub struct RingCgroup {
    /// `<delegated unit>/potato`: its `cpuset.cpus` is the K CPUs. Its path below the
    /// cgroup2 root is the `FCVM_CGROUP_PARENT` of the ring's fcvm commands.
    group: CpuCgroup,
    /// `<potato>/tools`: a process the agent starts for the ring writes itself here before
    /// it executes. `fcvm snapshot create` stays there; an fcvm that runs a VM then moves to
    /// its own scope under the ring's cgroup.
    join: Join,
}

/// The ring's cgroup, by name, under the delegated unit.
const RING: &str = "potato";
const TOOLS: &str = "tools";

impl RingCgroup {
    /// Make the ring's cgroup for `k` CPUs. `root` is the cgroup2 mount, `own` the agent's
    /// cgroup below it, and `delegated` says whether a cgroup is the agent's to write.
    ///
    /// The agent must run in a subgroup of a delegated unit (systemd `Delegate=yes` with
    /// `DelegateSubgroup=`): the unit's cgroup then holds no process, so the cpuset controller
    /// can be enabled for its children, and the ring's cgroup is the agent's sibling. The K
    /// CPUs are the highest-numbered ones the unit may use. The error says why the host cannot
    /// confine the ring.
    pub fn create(
        root: &Path,
        own: &Path,
        delegated: &dyn Fn(&Path) -> bool,
        k: u32,
    ) -> Result<Self, String> {
        let unit = Unit::open(root, own, delegated)?;
        let available = unit.cpus();
        if (available.len() as u32) < k {
            return Err(format!(
                "the agent may use {} CPUs, fewer than the {k} of the ring",
                available.len()
            ));
        }
        let cpus = available[available.len() - k as usize..].to_vec();
        let group = CpuCgroup::create(&unit, RING, &cpus)?;
        let join = group.join(Some(TOOLS))?;
        Ok(RingCgroup { group, join })
    }

    /// The ring's cgroup on this host, or why the ring is not confined.
    pub fn for_this_agent(k: u32) -> Result<Self, String> {
        let own = std::fs::read_to_string("/proc/self/cgroup")
            .map_err(|e| format!("reading /proc/self/cgroup: {e}"))?;
        let own = own_cgroup(&own).ok_or("no cgroup2 entry in /proc/self/cgroup")?;
        Self::create(
            Path::new(crate::cgroup::CGROUP2_ROOT),
            &own,
            &systemd_delegated,
            k,
        )
    }

    /// The K CPUs.
    pub fn cpus(&self) -> &[u32] {
        self.group.cpus()
    }

    /// Make `cmd` a process of the ring: it joins the ring's cgroup after the fork and before
    /// it executes, so nothing of it runs outside the K CPUs, and an fcvm that runs a VM
    /// makes that VM's scope under the ring's cgroup. A process that cannot join does not
    /// start.
    pub fn confine(&self, cmd: &mut std::process::Command) {
        cmd.env(crate::cgroup::PARENT_ENV, self.group.below());
        self.join.confine(cmd);
    }

    /// The CPU time the ring's processes have used, in microseconds (`cpu.stat`'s
    /// `usage_usec`, which counts every cgroup under the ring's).
    pub fn usage_usec(&self) -> Option<u64> {
        self.group.usage_usec()
    }

    /// SIGKILL every process left in the ring's cgroup (`cgroup.kill`).
    pub fn kill(&self) -> std::io::Result<()> {
        self.group.kill()
    }

    /// Wait until no process of the ring is left, for at most `deadline`. The ring's cgroup
    /// is inside the agent's unit, so whatever is left when the agent exits is killed by
    /// systemd with SIGKILL, and an fcvm killed that way leaves its VM's network behind.
    pub async fn emptied(&self, deadline: Duration) -> bool {
        self.group.emptied(deadline).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn the_ledger_never_holds_more_than_its_cap() {
        let mut c = Cpus::new(3);
        assert!(c.take(Holder::Gold));
        assert!(c.take(Holder::Player(0)));
        assert!(c.take(Holder::Player(1)));
        assert!(!c.take(Holder::Player(2)), "the fourth is refused");
        assert_eq!(c.in_use(), 3);
        assert!(c.take(Holder::Player(1)), "a holder keeps the CPU it has");
        assert_eq!(c.in_use(), 3);
        c.release(Holder::Gold);
        c.release(Holder::Gold);
        assert_eq!(c.in_use(), 2, "a release counts once");
        assert!(c.take(Holder::Player(2)));
        assert!(!c.has_free());
    }

    #[test]
    fn fewer_than_three_cpus_are_refused() {
        for k in 0..MIN_CPUS {
            let refused = allotment(k).unwrap_err().to_string();
            assert!(refused.contains("at least 3 CPUs"), "{refused}");
        }
        assert_eq!(allotment(3).unwrap(), 3);
        assert_eq!(allotment(8).unwrap(), 8);
    }

    #[test]
    fn the_peak_is_the_most_held_between_two_reads() {
        let mut c = Cpus::new(8);
        for n in 0..5 {
            assert!(c.take(Holder::Player(n)));
        }
        for n in 0..3 {
            c.release(Holder::Player(n));
        }
        assert_eq!(c.in_use(), 2);
        assert_eq!(c.take_peak(), 5, "the spike between the reads");
        assert_eq!(c.take_peak(), 2, "then what is held now");
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

    /// A cgroup2 tree as files: the unit's interface files as the kernel would show them,
    /// and, when `ring_effective` is given, the ring's effective CPUs as the kernel would
    /// answer after the write.
    fn tree(controllers: &str, unit_cpus: &str, ring_effective: Option<&str>) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        let unit = root.path().join("system.slice/agent.service");
        std::fs::create_dir_all(unit.join("agent")).unwrap();
        std::fs::write(unit.join("cgroup.controllers"), controllers).unwrap();
        std::fs::write(unit.join("cpuset.cpus.effective"), unit_cpus).unwrap();
        if let Some(effective) = ring_effective {
            std::fs::create_dir(unit.join(RING)).unwrap();
            std::fs::write(unit.join(RING).join("cpuset.cpus.effective"), effective).unwrap();
        }
        root
    }

    const OWN: &str = "system.slice/agent.service/agent";

    #[test]
    fn the_ring_is_confined_to_the_last_k_cpus_of_a_delegated_unit() {
        let root = tree("cpuset cpu memory\n", "0-15\n", Some("12-15\n"));
        let ring = RingCgroup::create(root.path(), Path::new(OWN), &|_| true, 4).unwrap();
        assert_eq!(ring.cpus(), &[12, 13, 14, 15]);
        let unit = root.path().join("system.slice/agent.service");
        assert_eq!(
            std::fs::read_to_string(unit.join("cgroup.subtree_control")).unwrap(),
            "+cpuset"
        );
        assert_eq!(
            std::fs::read_to_string(unit.join("potato/cpuset.cpus")).unwrap(),
            "12,13,14,15"
        );
        assert!(unit.join("potato/tools").is_dir());
        assert_eq!(ring.group.below(), "system.slice/agent.service/potato");
        std::fs::write(
            unit.join("potato/cpu.stat"),
            "usage_usec 1234567\nuser_usec 1\nsystem_usec 2\n",
        )
        .unwrap();
        assert_eq!(ring.usage_usec(), Some(1_234_567));
    }

    /// Confinement is claimed only when the kernel says the ring runs on exactly the K CPUs.
    #[test]
    fn a_host_that_cannot_confine_the_ring_says_why() {
        let own = Path::new(OWN);
        let no = |root: &tempfile::TempDir, delegated: bool, k: u32| {
            RingCgroup::create(root.path(), own, &|_| delegated, k).unwrap_err()
        };
        let root = tree("cpuset\n", "0-15\n", Some("12-15\n"));
        assert!(no(&root, false, 4).contains("not delegated"));
        assert!(
            !root
                .path()
                .join("system.slice/agent.service/cgroup.subtree_control")
                .exists(),
            "nothing is written to a cgroup that is not the agent's"
        );
        let root = tree("cpu memory\n", "0-15\n", Some("12-15\n"));
        assert!(no(&root, true, 4).contains("no cpuset controller"));
        let root = tree("cpuset\n", "0-2\n", Some("0-2\n"));
        assert!(no(&root, true, 4).contains("fewer than the 4"));
        // the kernel left the ring on other CPUs than the write asked for
        let root = tree("cpuset\n", "0-15\n", Some("0-15\n"));
        assert!(no(&root, true, 4).contains("not the 12,13,14,15 asked for"));
        // no interface file answers: not a cgroup2 tree
        let root = tree("cpuset\n", "0-15\n", None);
        assert!(no(&root, true, 4).contains("cpuset.cpus.effective"));
        assert!(
            RingCgroup::create(root.path(), Path::new("agent"), &|_| true, 4)
                .unwrap_err()
                .contains("no parent")
        );
    }

    /// A process made one of the ring's writes itself into the ring's cgroup before it
    /// executes, and one that cannot does not start.
    #[test]
    fn a_ring_process_joins_the_cgroup_before_it_executes() {
        let root = tree("cpuset\n", "0-15\n", Some("12-15\n"));
        let ring = RingCgroup::create(root.path(), Path::new(OWN), &|_| true, 4).unwrap();
        let procs = root
            .path()
            .join("system.slice/agent.service/potato/tools/cgroup.procs");
        std::fs::write(&procs, "").unwrap();
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.args(["-c", "echo $FCVM_CGROUP_PARENT"]);
        ring.confine(&mut cmd);
        let out = cmd.output().unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "system.slice/agent.service/potato"
        );
        assert_eq!(std::fs::read_to_string(&procs).unwrap(), "0");
        std::fs::remove_file(&procs).unwrap();
        let mut cmd = std::process::Command::new("/bin/true");
        ring.confine(&mut cmd);
        assert!(
            cmd.output().is_err(),
            "no cgroup to join: the process is not started outside it"
        );
    }
}

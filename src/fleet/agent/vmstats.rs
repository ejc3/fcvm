//! A phone VM's CPU, read on the host from its Firecracker process: `utime + stime` from
//! `/proc/<pid>/stat` over an interval, divided by the VM's vCPU threads. Nothing here enters
//! the guest, and a read that fails reports nothing, never zero.
//!
//! The VMM is the Firecracker child of the fcvm process the agent supervises. fcvm runs a
//! content-addressed binary (`firecracker-<profile>-<sha>.bin`), so the kernel's 15-byte
//! command name reads `firecracker-gpu` or `firecracker-def`, and the process is found as
//! the test helpers and the benchmarks find it: among the children of every thread of the
//! fcvm process (a multi-thread runtime forks from whichever thread spawned it), by a
//! `firecracker` command-name prefix. It is pinned by pid and start time (`stat` field 22),
//! so a reused pid is never sampled. Memory is not reported: the process's resident set
//! excludes hugetlb pages, and a pinned guest's figure would be its configured size.

use fleet_core::protocol::VmStats;
use std::time::Instant;

/// A process: its pid and its start time in clock ticks since boot, which together name
/// one process for as long as the host runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identity {
    pub pid: u32,
    pub start: u64,
}

/// One read of the VMM process.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Raw {
    pub who: Identity,
    /// `utime + stime`, in clock ticks.
    pub ticks: u64,
    pub at: Instant,
}

/// The reading between two reads: CPU as a percent of `vcpus`. None for the first read, a
/// different process, a zero or backwards interval, or a tick counter that went backwards.
pub fn sample(prev: Option<&Raw>, cur: &Raw, vcpus: u32, clk_tck: u64) -> Option<VmStats> {
    let prev = prev?;
    if vcpus == 0 || clk_tck == 0 || prev.who != cur.who {
        return None;
    }
    let secs = cur.at.checked_duration_since(prev.at)?.as_secs_f64();
    if secs <= 0.0 || cur.ticks < prev.ticks {
        return None;
    }
    let busy = (cur.ticks - prev.ticks) as f64 / clk_tck as f64;
    let cpu_pct = (busy / secs / vcpus as f64 * 100.0).clamp(0.0, 100.0);
    Some(VmStats {
        cpu_pct: (cpu_pct * 10.0).round() / 10.0,
        vcpus,
        at: None,
    })
}

/// The command name, `utime + stime` and start time from the text of `/proc/<pid>/stat`. The
/// name is in parentheses and may hold spaces, so the fields are counted from the last `)`.
pub fn parse_stat(text: &str) -> Option<(String, u64, u64)> {
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    let comm = text.get(open + 1..close)?.to_string();
    let rest: Vec<&str> = text.get(close + 1..)?.split_whitespace().collect();
    // rest[0] is the state (field 3): utime (14) and stime (15) are rest[11] and rest[12],
    // starttime (22) is rest[19].
    let ticks = rest.get(11)?.parse::<u64>().ok()? + rest.get(12)?.parse::<u64>().ok()?;
    Some((comm, ticks, rest.get(19)?.parse().ok()?))
}

/// How many of a VMM's thread names (`/proc/<pid>/task/*/comm`) are vCPU threads, which is
/// the VM's configured vCPU count. Firecracker names them `fc_vcpu <index>`
/// (`src/vmm/src/vstate/vcpu.rs`, `.name(format!("fc_vcpu {}", ...))`).
pub fn count_vcpus<'a>(comms: impl IntoIterator<Item = &'a str>) -> u32 {
    comms
        .into_iter()
        .filter(|c| c.trim().starts_with("fc_vcpu"))
        .count() as u32
}

/// The vCPU count of the VMM `pid`, from its threads.
pub fn read_vcpus(pid: u32) -> Option<u32> {
    let mut comms = Vec::new();
    for t in std::fs::read_dir(format!("/proc/{pid}/task"))
        .ok()?
        .flatten()
    {
        comms.push(std::fs::read_to_string(t.path().join("comm")).unwrap_or_default());
    }
    Some(count_vcpus(comms.iter().map(String::as_str))).filter(|n| *n > 0)
}

/// Whether a command name is a Firecracker's: the binary is `firecracker-<profile>-<sha>.bin`
/// and the kernel keeps 15 bytes of it.
pub fn is_firecracker_comm(comm: &str) -> bool {
    comm.starts_with("firecracker")
}

/// Reads one process, None when it is gone or is not a Firecracker. With `expect`, also None
/// when the process at that pid started at another time.
pub fn read(pid: u32, expect: Option<Identity>) -> Option<Raw> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (comm, ticks, start) = parse_stat(&stat)?;
    let who = Identity { pid, start };
    if !is_firecracker_comm(&comm) || expect.is_some_and(|e| e != who) {
        return None;
    }
    Some(Raw {
        who,
        ticks,
        at: Instant::now(),
    })
}

/// The Firecracker child of the fcvm process `fcvm_pid`, from the `children` list of each of
/// its threads.
pub fn find_vmm(fcvm_pid: u32) -> Option<Identity> {
    for task in std::fs::read_dir(format!("/proc/{fcvm_pid}/task"))
        .ok()?
        .flatten()
    {
        let Ok(kids) = std::fs::read_to_string(task.path().join("children")) else {
            continue;
        };
        for pid in kids
            .split_whitespace()
            .filter_map(|p| p.parse::<u32>().ok())
        {
            if let Some(raw) = read(pid, None) {
                return Some(raw.who);
            }
        }
    }
    None
}

pub fn clk_tck() -> u64 {
    // SAFETY: sysconf(3) has no preconditions.
    u64::try_from(unsafe { libc::sysconf(libc::_SC_CLK_TCK) }).unwrap_or(0)
}

/// The sampler of one slot: the VMM's identity and its last read.
#[derive(Default)]
pub struct Sampler {
    who: Option<Identity>,
    prev: Option<Raw>,
}

impl Sampler {
    /// One reading of the VM run by `fcvm_pid`; None while there is nothing to report.
    pub fn poll(&mut self, fcvm_pid: u32) -> Option<VmStats> {
        let cur = self.who.and_then(|w| read(w.pid, Some(w))).or_else(|| {
            self.prev = None;
            self.who = find_vmm(fcvm_pid);
            self.who.and_then(|w| read(w.pid, Some(w)))
        });
        let Some(cur) = cur else {
            self.prev = None;
            self.who = None;
            return None;
        };
        let vcpus = read_vcpus(cur.who.pid).unwrap_or(0);
        let out = sample(self.prev.as_ref(), &cur, vcpus, clk_tck());
        self.prev = Some(cur);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const WHO: Identity = Identity { pid: 7, start: 100 };

    fn raw(ticks: u64, at: Instant) -> Raw {
        Raw {
            who: WHO,
            ticks,
            at,
        }
    }

    #[test]
    fn cpu_is_the_tick_delta_over_the_interval_and_the_vcpus() {
        let t0 = Instant::now();
        // 6 vCPUs, 1 s apart, 300 ticks at 100/s = 3 CPU seconds = 50 %.
        let s = sample(
            Some(&raw(1000, t0)),
            &raw(1300, t0 + Duration::from_secs(1)),
            6,
            100,
        )
        .unwrap();
        assert_eq!((s.cpu_pct, s.vcpus, s.at), (50.0, 6, None));
    }

    #[test]
    fn nothing_for_a_first_read_a_zero_interval_a_counter_that_went_backwards_or_another_process() {
        let t0 = Instant::now();
        let a = raw(1000, t0);
        let later = t0 + Duration::from_secs(1);
        assert!(sample(None, &a, 6, 100).is_none());
        assert!(sample(Some(&a), &raw(1100, t0), 6, 100).is_none());
        assert!(sample(Some(&a), &raw(1100, t0 - Duration::from_secs(1)), 6, 100).is_none());
        assert!(sample(Some(&a), &raw(10, later), 6, 100).is_none());
        assert!(sample(Some(&a), &raw(1100, later), 0, 100).is_none());
        let reused = Raw {
            who: Identity { pid: 7, start: 999 },
            ..raw(1100, later)
        };
        assert!(sample(Some(&a), &reused, 6, 100).is_none());
    }

    #[test]
    fn a_busier_than_possible_read_is_held_at_100() {
        let t0 = Instant::now();
        let s = sample(
            Some(&raw(0, t0)),
            &raw(5000, t0 + Duration::from_secs(1)),
            2,
            100,
        )
        .unwrap();
        assert_eq!(s.cpu_pct, 100.0);
    }

    #[test]
    fn stat_is_read_from_the_last_paren_so_a_name_with_spaces_does_not_shift_fields() {
        // utime 40, stime 2, starttime 777
        let text = "123 (fire (cracker) x) S 1 2 3 4 5 6 7 8 9 10 40 2 0 0 20 0 1 0 777 5";
        assert_eq!(
            parse_stat(text),
            Some(("fire (cracker) x".to_string(), 42, 777))
        );
        assert_eq!(parse_stat("123 (x) S 1 2"), None);
    }

    #[test]
    fn the_command_name_fcvm_runs_is_a_firecracker_whatever_the_cut() {
        for c in ["firecracker", "firecracker-gpu", "firecracker-def"] {
            assert!(is_firecracker_comm(c), "{c}");
        }
        for c in ["fcvm", "pasta", "holder", "nsenter"] {
            assert!(!is_firecracker_comm(c), "{c}");
        }
    }

    #[test]
    fn vcpus_are_the_vcpu_threads() {
        let comms = [
            "firecracker-def\n",
            "fc_vcpu 0\n",
            "fc_vcpu 1\n",
            "fc_vcpu 5\n",
            "fc_api\n",
            "vmm\n",
        ];
        assert_eq!(count_vcpus(comms), 3);
        assert_eq!(count_vcpus(["fc_api"]), 0);
    }

    #[test]
    fn a_pid_that_is_not_a_firecracker_reads_nothing() {
        assert!(read(std::process::id(), None).is_none());
        assert!(read(u32::MAX, None).is_none());
    }
}

//! A phone VM's CPU and memory, read on the host from its Firecracker process: `utime +
//! stime` from `/proc/<pid>/stat` and the resident set from `statm`. Nothing here enters the
//! guest, and a read that fails reports nothing, never zero.

use fleet_core::protocol::VmStats;
use std::time::Instant;

/// One read of the VMM process.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Raw {
    /// `utime + stime`, in clock ticks.
    pub ticks: u64,
    pub rss_bytes: u64,
    pub at: Instant,
}

/// The reading between two reads: CPU as a percent of `vcpus`, memory at the later read.
/// None for the first read, a zero or backwards interval, or a tick counter that went
/// backwards (a different process behind the pid, such as after a restore).
pub fn sample(prev: Option<&Raw>, cur: &Raw, vcpus: u32, clk_tck: u64) -> Option<VmStats> {
    let prev = prev?;
    if vcpus == 0 || clk_tck == 0 {
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
        mem_mb: (cur.rss_bytes as f64 / 1_048_576.0 * 10.0).round() / 10.0,
        vcpus,
        at: None,
    })
}

/// `utime + stime` from the text of `/proc/<pid>/stat`. The command name is in parentheses
/// and may hold spaces, so the fields are counted from the last `)`.
pub fn parse_stat(text: &str) -> Option<(String, u64)> {
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    let comm = text.get(open + 1..close)?.to_string();
    let rest: Vec<&str> = text.get(close + 1..)?.split_whitespace().collect();
    // rest[0] is the state (field 3), so utime (14) and stime (15) are rest[11] and rest[12].
    let ticks = rest.get(11)?.parse::<u64>().ok()? + rest.get(12)?.parse::<u64>().ok()?;
    Some((comm, ticks))
}

/// How many of a VMM's thread names (`/proc/<pid>/task/*/comm`) are vCPU threads, which is
/// the VM's configured vCPU count (Firecracker names them `fc_vcpu <n>`).
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

/// The resident pages of `/proc/<pid>/statm` (its second field).
pub fn parse_statm_rss_pages(text: &str) -> Option<u64> {
    text.split_whitespace().nth(1)?.parse().ok()
}

/// Reads one pid. None when the process is gone or is not a `firecracker`.
pub fn read(pid: u32) -> Option<Raw> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (comm, ticks) = parse_stat(&stat)?;
    if comm != "firecracker" {
        return None;
    }
    let pages =
        parse_statm_rss_pages(&std::fs::read_to_string(format!("/proc/{pid}/statm")).ok()?)?;
    // SAFETY: sysconf(3) has no preconditions.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    let page = u64::try_from(page).ok().filter(|p| *p > 0)?;
    Some(Raw {
        ticks,
        rss_bytes: pages.checked_mul(page)?,
        at: Instant::now(),
    })
}

/// The Firecracker child of the fcvm process `fcvm_pid`, from its task's `children` list.
pub fn find_vmm(fcvm_pid: u32) -> Option<u32> {
    let kids =
        std::fs::read_to_string(format!("/proc/{fcvm_pid}/task/{fcvm_pid}/children")).ok()?;
    kids.split_whitespace()
        .filter_map(|p| p.parse::<u32>().ok())
        .find(|p| read(*p).is_some())
}

pub fn clk_tck() -> u64 {
    // SAFETY: sysconf(3) has no preconditions.
    u64::try_from(unsafe { libc::sysconf(libc::_SC_CLK_TCK) }).unwrap_or(0)
}

/// The sampler of one slot: the VMM's pid and its last read.
#[derive(Default)]
pub struct Sampler {
    pid: Option<u32>,
    prev: Option<Raw>,
}

impl Sampler {
    /// One reading of the VM run by `fcvm_pid`; None while there is nothing to report.
    pub fn poll(&mut self, fcvm_pid: u32) -> Option<VmStats> {
        let cur = self.pid.and_then(read).or_else(|| {
            self.prev = None;
            self.pid = find_vmm(fcvm_pid);
            self.pid.and_then(read)
        });
        let Some(cur) = cur else {
            self.prev = None;
            self.pid = None;
            return None;
        };
        let vcpus = self.pid.and_then(read_vcpus).unwrap_or(0);
        let out = sample(self.prev.as_ref(), &cur, vcpus, clk_tck());
        self.prev = Some(cur);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn raw(ticks: u64, mb: u64, at: Instant) -> Raw {
        Raw {
            ticks,
            rss_bytes: mb * 1_048_576,
            at,
        }
    }

    #[test]
    fn cpu_is_the_tick_delta_over_the_interval_and_the_vcpus() {
        let t0 = Instant::now();
        let a = raw(1000, 1500, t0);
        // 6 vCPUs, 1 s apart, 300 ticks at 100/s = 3 CPU seconds = 50 %.
        let b = raw(1300, 1800, t0 + Duration::from_secs(1));
        let s = sample(Some(&a), &b, 6, 100).unwrap();
        assert_eq!(
            (s.cpu_pct, s.mem_mb, s.vcpus, s.at),
            (50.0, 1800.0, 6, None)
        );
    }

    #[test]
    fn nothing_for_a_first_read_a_zero_interval_or_a_counter_that_went_backwards() {
        let t0 = Instant::now();
        let a = raw(1000, 1, t0);
        assert!(sample(None, &a, 6, 100).is_none());
        assert!(sample(Some(&a), &raw(1100, 1, t0), 6, 100).is_none());
        assert!(sample(Some(&a), &raw(1100, 1, t0 - Duration::from_secs(1)), 6, 100).is_none());
        assert!(sample(Some(&a), &raw(10, 1, t0 + Duration::from_secs(1)), 6, 100).is_none());
        assert!(sample(Some(&a), &raw(1100, 1, t0 + Duration::from_secs(1)), 0, 100).is_none());
    }

    #[test]
    fn a_busier_than_possible_read_is_held_at_100() {
        let t0 = Instant::now();
        let s = sample(
            Some(&raw(0, 1, t0)),
            &raw(5000, 1, t0 + Duration::from_secs(1)),
            2,
            100,
        )
        .unwrap();
        assert_eq!(s.cpu_pct, 100.0);
    }

    #[test]
    fn stat_is_read_from_the_last_paren_so_a_name_with_spaces_does_not_shift_fields() {
        let text = "123 (fire (cracker) x) S 1 2 3 4 5 6 7 8 9 10 40 2 0 0 20 0 1";
        assert_eq!(parse_stat(text), Some(("fire (cracker) x".to_string(), 42)));
        assert_eq!(parse_stat("123 (x) S 1 2"), None);
        assert_eq!(parse_statm_rss_pages("100 25 3 0 0 5 0"), Some(25));
        assert_eq!(parse_statm_rss_pages("100"), None);
    }

    #[test]
    fn vcpus_are_the_vcpu_threads() {
        let comms = [
            "firecracker\n",
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
        assert!(read(std::process::id()).is_none());
        assert!(read(u32::MAX).is_none());
    }
}

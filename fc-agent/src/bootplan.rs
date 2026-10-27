//! Boot-plan transport.
//!
//! fc-agent fetches its container plan + host metadata either from **MMDS**
//! (Firecracker's metadata service) or over **vsock** (for VMMs without MMDS, e.g.
//! Cloud Hypervisor — #632 P0.5). The transport is chosen from the kernel command
//! line: `fcvm_bootplan=vsock` selects vsock, otherwise MMDS (the Firecracker default).
//! The clock step at boot reads the vsock document on both transports
//! ([`sync_clock_from_host`]): the host serves it for every backend.
//!
//! Wire format (vsock): the host serves the same `latest` object MMDS would expose —
//! `{ "container-plan": { ...Plan... }, "host-time-ns": "<epoch ns>" }` — on
//! [`BOOTPLAN_VSOCK_PORT`]. The guest connects to the host (CID 2), reads the JSON to
//! EOF, and extracts the plan and metadata. Snapshot restore always exposes the
//! restore epoch on this vsock port, including for Firecracker: a snapshot is
//! captured with eth0 down, so restore control must not depend on MMDS ingress.

use anyhow::{Context, Result};
use tokio::io::AsyncReadExt;

use crate::types::{LatestMetadata, Plan};
use crate::vsock::{VsockStream, HOST_CID};

/// Vsock port the host serves the boot plan on (guest connects to host CID 2).
/// Distinct from 4996 (tty) / 4997 (output) / 4998 (exec) / 4999 (status) / 5000+ (volumes).
pub const BOOTPLAN_VSOCK_PORT: u32 = 4995;

/// Where fc-agent gets its boot plan and host metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// Firecracker MMDS V2 at 169.254.169.254.
    Mmds,
    /// Host-served boot plan over vsock (VMMs without a metadata service).
    Vsock,
}

/// Decide the boot-plan transport from `/proc/cmdline`.
/// `fcvm_bootplan=vsock` selects vsock; anything else defaults to MMDS.
pub fn detect_transport() -> Transport {
    match std::fs::read_to_string("/proc/cmdline") {
        Ok(cmdline)
            if cmdline
                .split_whitespace()
                .any(|tok| tok == "fcvm_bootplan=vsock") =>
        {
            eprintln!("[fc-agent] boot-plan transport: vsock (port {BOOTPLAN_VSOCK_PORT})");
            Transport::Vsock
        }
        _ => {
            eprintln!("[fc-agent] boot-plan transport: MMDS");
            Transport::Mmds
        }
    }
}

/// Read the full boot-plan document (the `latest` object) from the host over vsock.
async fn read_vsock_doc() -> Result<serde_json::Value> {
    let mut stream = VsockStream::connect(HOST_CID, BOOTPLAN_VSOCK_PORT)
        .context("connecting to host boot-plan vsock port")?;
    let mut buf = Vec::new();
    // Bounded read: if the host accepts but never closes (a regressed listener/proxy),
    // don't hang the boot fetch forever — time out so the caller's retry loop recovers.
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        stream.read_to_end(&mut buf),
    )
    .await
    .context("timed out reading boot plan over vsock")?
    .context("reading boot plan over vsock")?;
    serde_json::from_slice(&buf)
        .with_context(|| format!("parsing boot-plan JSON ({} bytes)", buf.len()))
}

/// Fetch the container plan via the selected transport.
pub async fn fetch_plan(transport: Transport) -> Result<Plan> {
    match transport {
        Transport::Mmds => crate::mmds::fetch_plan().await,
        Transport::Vsock => {
            let doc = read_vsock_doc().await?;
            let plan = doc
                .get("container-plan")
                .context("boot-plan document missing 'container-plan'")?
                .clone();
            serde_json::from_value(plan).context("parsing container-plan from vsock boot plan")
        }
    }
}

/// Fetch host metadata (host-time, restore-epoch, clone-ipv6) via the selected transport.
/// Polled by the restore-epoch watcher ([`crate::mmds::watch_restore_epoch`]) on both
/// transports, and used for one-shot clock sync.
pub async fn fetch_metadata(transport: Transport) -> Result<LatestMetadata> {
    match transport {
        Transport::Mmds => crate::mmds::fetch_latest_metadata_default().await,
        Transport::Vsock => {
            let doc = read_vsock_doc().await?;
            serde_json::from_value(doc).context("parsing metadata from vsock boot plan")
        }
    }
}

/// How many times [`sync_clock_from_host`] reads the host's clock. It steps to the
/// reading with the shortest round trip.
const HOST_CLOCK_READINGS: usize = 3;

/// One reading of the host's clock from the boot-plan listener.
struct HostClockReading {
    /// The document's "host-time-ns".
    host_time_ns: String,
    /// When the guest finished reading the document.
    read: std::time::Instant,
    /// From the guest's connect to the end of its read. The host stamped its clock
    /// inside this interval, so a step to the stamp plus the time since `read` is
    /// behind the host by at most this much.
    round_trip: std::time::Duration,
}

async fn read_host_clock() -> Result<HostClockReading> {
    let started = std::time::Instant::now();
    let doc = read_vsock_doc().await?;
    let read = std::time::Instant::now();
    let meta: LatestMetadata =
        serde_json::from_value(doc).context("parsing host-time from vsock boot plan")?;
    Ok(HostClockReading {
        host_time_ns: meta.host_time_ns,
        read,
        round_trip: read - started,
    })
}

/// The reading with the shortest round trip, whose stamp is the closest to its read.
fn closest_reading(readings: Vec<HostClockReading>) -> Option<HostClockReading> {
    readings
        .into_iter()
        .min_by_key(|reading| reading.round_trip)
}

/// Step the VM clock to the host's, read over vsock on both transports. Used at boot
/// and by a restored clone right before its step (`restore::handle_clone_restore`).
///
/// The host binds the boot-plan listener for every backend and stamps its clock at
/// each accept, so a reading is at most one vsock round trip old when the guest
/// steps to it. Of [`HOST_CLOCK_READINGS`] readings the one with the shortest round
/// trip is used: a guest that stalls during one (a restore paging its memory in from
/// a cold file) would otherwise step behind by the stall. A failed reading ends the
/// sampling; this fails only when no reading succeeded.
///
/// The MMDS copy of the plan is stamped while the host builds it, before
/// the VM boots; a guest that stepped to it came up 0.86 to 1.14 s behind the host
/// (issue 1354). At boot this runs before fc-agent starts its own chronyd, so the daemon
/// measures the stepped clock and has no correction to make for the boot.
///
/// The step goes both ways (`set_system_clock`, not `advance_system_clock`). Before it,
/// an arm64 guest's clock comes from the RTC: whole seconds, to which the kernel's
/// `rtc_hctosys` adds half a second, so it can be up to 0.5 s ahead of the host, and a
/// forward-only step left one boot 367 ms ahead.
pub async fn sync_clock_from_host() -> Result<()> {
    let mut readings = Vec::with_capacity(HOST_CLOCK_READINGS);
    let mut failure = None;
    for _ in 0..HOST_CLOCK_READINGS {
        match read_host_clock().await {
            Ok(reading) => readings.push(reading),
            Err(error) => {
                failure = Some(error);
                break;
            }
        }
    }
    let round_trips: Vec<_> = readings.iter().map(|r| r.round_trip).collect();
    let Some(reading) = closest_reading(readings) else {
        return Err(failure.unwrap_or_else(|| anyhow::anyhow!("no host clock reading taken")));
    };
    eprintln!(
        "[fc-agent] host clock round trips {round_trips:?}; stepping to the {:?} one",
        reading.round_trip
    );
    set_system_clock(&reading.host_time_ns, reading.read.elapsed());
    Ok(())
}

/// Set the system clock to the host's clock. Shared by both transports.
///
/// Steps the clock with `clock_settime(2)` directly: fc-agent holds
/// CAP_SYS_TIME, and this runs on the restore critical path where a process
/// spawn is pure latency. Failure is soft by construction (this returns
/// nothing): chronyd converges the clock eventually, and refusing restore
/// over a clock step would trade a one-second skew for a dead clone.
///
/// `since_read` is how long ago the host's value was read: a caller that steps
/// the clock some time after it fetched the value passes that time, so the
/// clock lands on the host's present rather than on the moment of the read.
///
/// `host_time` is the document's "host-time-ns", nanoseconds since the epoch.
/// The document also carries "host-time", whole seconds, which only an agent
/// frozen in a snapshot taken before "host-time-ns" existed reads.
pub(crate) fn set_system_clock(host_time: &str, since_read: std::time::Duration) {
    step_system_clock(host_time, since_read, ClockStep::Always);
}

/// [`set_system_clock`], except that a clock already at or past the target is left
/// alone.
///
/// The target is a lower bound on the present: the host read its clock before the
/// value reached the guest, and only the time since the guest read it is added. So a
/// forward step always moves the clock closer to the host's, and a backward step can
/// only move a clock that is already right behind it. A snapshot's source uses this
/// (`snapshot_network::resume_source_network`): its clock is behind by the pause when
/// the VMM froze it, and right when it did not (the arm64 `default` profile keeps it
/// running through a pause, and a pause that failed may not have happened).
pub(crate) fn advance_system_clock(host_time: &str, since_read: std::time::Duration) {
    step_system_clock(host_time, since_read, ClockStep::ForwardOnly);
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ClockStep {
    Always,
    ForwardOnly,
}

impl ClockStep {
    /// Whether to step by `step` seconds (target minus the current clock).
    fn takes(self, step: f64) -> bool {
        self == ClockStep::Always || step > 0.0
    }
}

fn step_system_clock(host_time: &str, since_read: std::time::Duration, mode: ClockStep) {
    eprintln!("[fc-agent] received host time: {host_time} ns (read {since_read:?} ago)");
    match parse_epoch_nanos(host_time) {
        Ok(instant) => {
            let spec = clock_target(instant, since_read);
            let before = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            let step = std::time::Duration::new(spec.tv_sec as u64, spec.tv_nsec as u32)
                .as_secs_f64()
                - before.as_secs_f64();
            if !mode.takes(step) {
                eprintln!(
                    "[fc-agent] system clock already at or past the host's (target {step:+.6} s); \
                     not stepped"
                );
                return;
            }
            // SAFETY: `spec` is fully initialized and outlives the call.
            let rc = unsafe { libc::clock_settime(libc::CLOCK_REALTIME, &spec) };
            if rc != 0 {
                eprintln!(
                    "[fc-agent] WARNING: failed to set clock: {}",
                    std::io::Error::last_os_error()
                );
                eprintln!("[fc-agent] continuing anyway (will rely on chronyd)");
            } else {
                eprintln!("[fc-agent] system clock synchronized from host (stepped {step:+.6} s)");
            }
        }
        Err(error) => {
            eprintln!("[fc-agent] WARNING: failed to set clock: {error:#}");
            eprintln!("[fc-agent] continuing anyway (will rely on chronyd)");
        }
    }
}

/// The time to set: the host's instant plus the time since it was read.
fn clock_target(instant: std::time::Duration, since_read: std::time::Duration) -> libc::timespec {
    let total = instant + since_read;
    libc::timespec {
        tv_sec: total.as_secs() as _,
        tv_nsec: total.subsec_nanos() as _,
    }
}

/// Parse the host's clock value: decimal nanoseconds since the epoch.
fn parse_epoch_nanos(host_time: &str) -> Result<std::time::Duration> {
    let nanos: u64 = host_time
        .trim()
        .parse()
        .with_context(|| format!("host time {host_time:?} is not decimal epoch nanoseconds"))?;
    Ok(std::time::Duration::from_nanos(nanos))
}

#[cfg(test)]
mod clock_tests {
    use super::*;

    #[test]
    fn the_clock_target_adds_the_time_since_the_host_value_was_read() {
        let host = std::time::Duration::from_secs(1_700_000_000);
        let spec = clock_target(host, std::time::Duration::from_millis(31_250));
        assert_eq!(spec.tv_sec as i64, 1_700_000_031);
        assert_eq!(spec.tv_nsec as i64, 250_000_000);
        let spec = clock_target(host, std::time::Duration::ZERO);
        assert_eq!(
            (spec.tv_sec as i64, spec.tv_nsec as i64),
            (1_700_000_000, 0)
        );
    }

    /// The host stamps its clock to the nanosecond in "host-time-ns", and the
    /// guest steps to that instant. Whole seconds left a restored guest behind
    /// the host by the fraction of the second at which the host read its clock:
    /// up to a second, half a second on average (issue 1194).
    ///
    /// RED BEFORE THE FIX: the guest stepped to the whole second, 750 ms
    /// behind here.
    #[test]
    fn the_clock_lands_on_the_hosts_instant_to_the_nanosecond() {
        let meta: crate::types::LatestMetadata = serde_json::from_str(
            r#"{"host-time": "1700000000", "host-time-ns": "1700000000750000123"}"#,
        )
        .unwrap();
        let spec = clock_target(
            parse_epoch_nanos(&meta.host_time_ns).unwrap(),
            std::time::Duration::from_millis(20),
        );
        assert_eq!(
            (spec.tv_sec as i64, spec.tv_nsec as i64),
            (1_700_000_000, 770_000_123)
        );
    }

    /// The boot step moves the clock back as well as forward: before it, an arm64
    /// guest's clock is the RTC's second plus the half second `rtc_hctosys` adds, up to
    /// 0.5 s ahead of the host. With a forward-only step one cold boot stayed 367 ms
    /// ahead (`test_cold_boot_clock_matches_the_host`).
    #[test]
    fn the_boot_step_moves_the_clock_back_too() {
        let source = include_str!("bootplan.rs");
        let start = source
            .find("pub async fn sync_clock_from_host(")
            .expect("sync_clock_from_host is gone");
        let end = source[start..].find("\n}\n").unwrap() + start;
        let body = &source[start..end];
        assert!(
            body.contains("set_system_clock(&reading.host_time_ns")
                && !body.contains("advance_system_clock("),
            "the boot clock step must go both ways: {body}"
        );
    }

    /// The step uses the reading with the shortest round trip: the host stamped its
    /// clock inside the round trip, so that reading's stamp is the closest to its read.
    #[test]
    fn the_step_uses_the_reading_with_the_shortest_round_trip() {
        let now = std::time::Instant::now();
        let reading = |ns: &str, ms: u64| HostClockReading {
            host_time_ns: ns.to_string(),
            read: now,
            round_trip: std::time::Duration::from_millis(ms),
        };
        let best = closest_reading(vec![reading("1", 130), reading("2", 1), reading("3", 4)])
            .expect("three readings");
        assert_eq!(best.host_time_ns, "2");
        assert!(closest_reading(Vec::new()).is_none());
    }

    /// The source's step never moves its clock back: the host's instant plus the
    /// time since the guest read it is a lower bound on the present, so a clock past
    /// it is closer to the host's than the target is.
    #[test]
    fn a_forward_only_step_leaves_a_clock_that_is_ahead_alone() {
        assert!(ClockStep::ForwardOnly.takes(0.9));
        assert!(!ClockStep::ForwardOnly.takes(-0.004));
        assert!(!ClockStep::ForwardOnly.takes(0.0));
        assert!(ClockStep::Always.takes(-0.004));
    }

    #[test]
    fn epoch_nanos_parse_rejects_garbage_and_pre_epoch_values() {
        assert_eq!(
            parse_epoch_nanos("1786000000250000000\n").unwrap(),
            std::time::Duration::new(1_786_000_000, 250_000_000)
        );
        assert!(parse_epoch_nanos("-1").is_err(), "before the epoch");
        assert!(parse_epoch_nanos("").is_err(), "empty");
        assert!(parse_epoch_nanos("1786000000.5").is_err(), "fractional");
    }
}

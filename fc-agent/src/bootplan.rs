//! Boot-plan transport.
//!
//! fc-agent fetches its container plan + host metadata either from **MMDS**
//! (Firecracker's metadata service) or over **vsock** (for VMMs without MMDS, e.g.
//! Cloud Hypervisor — #632 P0.5). The transport is chosen from the kernel command
//! line: `fcvm_bootplan=vsock` selects vsock, otherwise MMDS (the Firecracker default).
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

/// Sync the VM clock from host time via the selected transport.
pub async fn sync_clock_from_host(transport: Transport) -> Result<()> {
    match transport {
        Transport::Mmds => crate::mmds::sync_clock_from_host().await,
        Transport::Vsock => {
            let doc = read_vsock_doc().await?;
            let meta: LatestMetadata =
                serde_json::from_value(doc).context("parsing host-time from vsock boot plan")?;
            set_system_clock(&meta.host_time_ns, std::time::Duration::ZERO);
            Ok(())
        }
    }
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
    eprintln!("[fc-agent] received host time: {host_time} ns (read {since_read:?} ago)");
    match parse_epoch_nanos(host_time) {
        Ok(instant) => {
            let spec = clock_target(instant, since_read);
            // SAFETY: `spec` is fully initialized and outlives the call.
            let rc = unsafe { libc::clock_settime(libc::CLOCK_REALTIME, &spec) };
            if rc != 0 {
                eprintln!(
                    "[fc-agent] WARNING: failed to set clock: {}",
                    std::io::Error::last_os_error()
                );
                eprintln!("[fc-agent] continuing anyway (will rely on chronyd)");
            } else {
                eprintln!("[fc-agent] system clock synchronized from host");
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

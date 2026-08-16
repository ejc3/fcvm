mod agent;
mod bootplan;
mod console;
mod container;
mod exec;
mod fuse;
mod lock_test;
mod mmds;
mod mounts;
mod network;
mod output;
mod proxy;
mod restore;
mod snapshot_network;
mod system;
mod tty;
mod types;
mod vitals;
mod vsock;

use std::fmt;

use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::EnvFilter;

/// Format tracing events as `[fc-agent] message` — no timestamp, level, or target.
///
/// The host reads these lines from Firecracker's serial console and adds its own
/// timestamp and level. Including `[fc-agent]` ensures the host logs them at INFO
/// (the host checks `contains("fc-agent")` to distinguish important lines).
struct FcAgentFormat;

impl<S, N> FormatEvent<S, N> for FcAgentFormat
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> fmt::Result {
        write!(writer, "[fc-agent] ")?;
        ctx.format_fields(writer.by_ref(), event)?;
        writeln!(writer)
    }
}

#[tokio::main]
async fn main() {
    // `fc-agent --notify-reboot`: a one-shot invoked by the systemd system-shutdown
    // hook when the shutdown verb is "reboot". It sends a single vsock message to the
    // host so the host relaunches the VM in place (disk-only-clone semantics) instead
    // of treating the firecracker exit as termination. Must be fast and side-effect
    // free (it runs late in shutdown after services are stopped), so it short-circuits
    // before any of the normal agent setup.
    if std::env::args().any(|a| a == "--notify-reboot") {
        let ok = vsock::notify_reboot();
        std::process::exit(if ok { 0 } else { 1 });
    }

    // Snapshot network boundary one-shots are invoked by the host immediately
    // around the VMM pause/save/resume transaction. They must not start the
    // long-running agent a second time, and any failure must be visible in the
    // exec exit status so snapshot creation fails closed.
    let one_shot = std::env::args().nth(1);
    let one_shot_result = match one_shot.as_deref() {
        Some("--prepare-snapshot-network") => {
            Some(snapshot_network::prepare_snapshot_network().await)
        }
        Some("--resume-snapshot-network") => Some(snapshot_network::resume_source_network().await),
        _ => None,
    };
    if let Some(result) = one_shot_result {
        if let Err(error) = result {
            eprintln!("[fc-agent] snapshot network boundary failed: {error:#}");
            std::process::exit(1);
        }
        return;
    }

    // Use a non-blocking writer for stderr so that log writes never block the
    // tokio runtime. Without this, heavy FUSE traffic generates thousands of
    // INFO messages/sec which synchronously write to the serial console (virtio),
    // starving the async output handler that drains the container's stdout pipe.
    // The container's entrypoint then deadlocks on pipe write.
    let (non_blocking, _guard) = tracing_appender::non_blocking(std::io::stderr());

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("fc_agent=info,warn")),
        )
        .event_format(FcAgentFormat)
        .with_writer(non_blocking)
        .init();

    eprintln!("[fc-agent] starting");

    // Arm guest failpoints from the kernel cmdline: fcvm forwards the host env
    // FCVM_GUEST_FAILPOINT as `fcvm_failpoint=<spec>` (the fuse_trace_rate
    // pattern) because fc-agent cannot read host env. Sleep actions only in the
    // guest — block_until_file is host-only, rejected by fcvm before boot.
    if let Ok(cmdline) = std::fs::read_to_string("/proc/cmdline") {
        if let Some(spec) = cmdline
            .split_whitespace()
            .find_map(|part| part.strip_prefix("fcvm_failpoint="))
        {
            failpoint::arm_from_str(spec);
        }
    }

    if let Err(e) = agent::run().await {
        eprintln!("[fc-agent] ==========================================");
        eprintln!("[fc-agent] FATAL ERROR: Container failed to start");
        eprintln!("[fc-agent] Error: {:?}", e);
        // Guest resources at the instant of failure. Without this the host sees
        // only podman's one-liner, and issue #841 showed where that leads: two
        // occurrences of `conmon bytes ""` with no way to tell an out-of-memory
        // guest from a full /run tmpfs from PTY exhaustion.
        eprintln!("[fc-agent] VITALS guest begin (container failed to start)");
        eprint!(
            "{}",
            crate::vitals::snapshot_bounded(std::time::Duration::from_secs(3))
        );
        eprintln!("[fc-agent] VITALS guest end");
        eprintln!("[fc-agent] ==========================================");
        vsock::notify_container_exit(1);
        system::shutdown_vm(1).await;
    }
}

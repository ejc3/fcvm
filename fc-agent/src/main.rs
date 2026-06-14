mod agent;
mod bootplan;
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
mod system;
mod tty;
mod types;
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

    if let Err(e) = agent::run().await {
        eprintln!("[fc-agent] ==========================================");
        eprintln!("[fc-agent] FATAL ERROR: Container failed to start");
        eprintln!("[fc-agent] Error: {:?}", e);
        eprintln!("[fc-agent] ==========================================");
        vsock::notify_container_exit(1);
        system::shutdown_vm(1).await;
    }
}

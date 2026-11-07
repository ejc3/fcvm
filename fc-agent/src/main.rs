// fc-agent's own `eprintln!`: each line is one write, so a line reaches the console whole
// (see `console::write_line`). Defined before every module so it replaces std's throughout
// the crate. fc-agent prints nothing to stdout.
macro_rules! eprintln {
    () => {
        $crate::console::eprint_line(format_args!(""))
    };
    ($($arg:tt)*) => {
        $crate::console::eprint_line(format_args!($($arg)*))
    };
}

mod agent;
mod bootplan;
mod console;
mod container;
mod exec;
mod fuse;
mod gpu;
mod gpu_park;
mod lock_test;
mod mmds;
mod mounts;
mod netlink;
mod network;
mod nftables;
mod output;
mod payload_freeze;
mod proxy;
mod restore;
mod retired_address;
mod snapshot_network;
mod system;
mod thp;
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
            // The container is frozen first, so it opens no socket and faults no page
            // while the boundary closes, and a restored clone starts with it frozen
            // (payload_freeze). A failed prepare thaws it again: the host resumes the
            // source on its own when the prepare fails.
            payload_freeze::freeze();
            let prepared = snapshot_network::prepare_snapshot_network().await;
            if prepared.is_err() {
                payload_freeze::thaw();
            }
            Some(prepared)
        }
        Some("--resume-snapshot-network") => {
            let args: Vec<String> = std::env::args().collect();
            let resumed =
                snapshot_network::resume_source_network(snapshot_network::resume_host_time(&args))
                    .await;
            payload_freeze::thaw();
            Some(resumed)
        }
        _ => None,
    };
    if let Some(result) = one_shot_result {
        if let Err(error) = result {
            eprintln!("[fc-agent] snapshot network boundary failed: {error:#}");
            std::process::exit(1);
        }
        return;
    }

    // Warm GPU snapshot one-shots, invoked by the host around a `snapshot create
    // --gpu-warm` (gpu_park.rs). Failure is the exit status, so the host stops.
    // They run podman (the container's GPU hook) as the container's user, whose
    // context the agent set up and recorded.
    let gpu_result = match one_shot.as_deref() {
        Some("--gpu-park") => Some(match container::restore_podman_cmd_prefix() {
            Ok(()) => gpu_park::park().await.map(|_| ()),
            Err(e) => Err(e),
        }),
        Some("--gpu-unpark") => Some(match container::restore_podman_cmd_prefix() {
            Ok(()) => gpu_park::unpark_source().await,
            Err(e) => Err(e),
        }),
        _ => None,
    };
    if let Some(result) = gpu_result {
        if let Err(error) = result {
            eprintln!(
                "[fc-agent] {} failed: {error:#}",
                one_shot.unwrap_or_default()
            );
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

    // Thaws a snapshot boundary's container freeze whose deadline passed with no thaw: a
    // resume that never reached the guest, an fcvm that died mid-snapshot (payload_freeze).
    payload_freeze::spawn_watchdog();

    // Arm guest failpoints from the kernel cmdline: fcvm forwards the host env
    // FCVM_GUEST_FAILPOINT as `fcvm_failpoint=<spec>` (the fuse_trace_rate
    // pattern) because fc-agent cannot read host env. Guest specs take sleep and
    // burn; block_until_file is host-only, rejected by fcvm before boot.
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
        eprintln!("{}", fatal_error_line(&e));
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

/// The console line that says why the agent failed, with every cause on it.
///
/// fcvm logs a console line from the agent at INFO only when it carries the
/// `[fc-agent]` prefix (console_line_is_important in src/utils.rs). anyhow's `{:?}` puts
/// each cause on a line of its own under "Caused by:", without the prefix, so
/// the host showed the outermost context and nothing of what went wrong.
fn fatal_error_line(error: &anyhow::Error) -> String {
    let chain = format!("{error:#}");
    let chain: Vec<&str> = chain
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    format!("[fc-agent] Error: {}", chain.join(" "))
}

#[cfg(test)]
mod tests {
    /// The container is frozen before the boundary's prepare and thawed when the prepare
    /// fails and when the source resumes; a restored clone thaws it before publication
    /// (payload_freeze). A freeze with no thaw on one of those paths would leave the
    /// container stopped, so each is pinned.
    #[test]
    fn the_container_is_frozen_across_the_boundary_and_thawed_on_every_way_out() {
        let main = include_str!("main.rs");
        let main = &main[..main.find("#[cfg(test)]\nmod tests {").unwrap()];
        let squeezed: String = main.split_whitespace().collect();
        let prepare = squeezed
            .find("Some(\"--prepare-snapshot-network\")=>{")
            .expect("no prepare one-shot");
        let freeze = squeezed[prepare..]
            .contains("payload_freeze::freeze();letprepared=snapshot_network::prepare_snapshot_network().await;ifprepared.is_err(){payload_freeze::thaw();}");
        assert!(
            freeze,
            "the prepare does not freeze first and thaw on failure"
        );
        assert!(
            squeezed.contains(".await;payload_freeze::thaw();Some(resumed)"),
            "the source resume does not thaw"
        );

        let restore: String = crate::restore::handle_clone_restore_source()
            .split_whitespace()
            .collect();
        let thaw = restore
            .find("crate::payload_freeze::thaw();")
            .expect("a clone never thaws");
        let publish = restore
            .find(".publish(restore_hook,")
            .expect("no publication");
        assert!(
            thaw < publish,
            "a clone thaws after publication, so its hook runs frozen"
        );
        // The long-running agent runs the deadline's watchdog; the one-shots do not.
        let watchdog = squeezed
            .find("payload_freeze::spawn_watchdog();")
            .expect("the agent starts no freeze watchdog");
        let agent = squeezed.find("agent::run()").expect("no agent run");
        assert!(watchdog < agent && prepare < watchdog);
    }

    use super::fatal_error_line;
    use anyhow::Context;

    /// The line names every cause, innermost last, and is one line even when
    /// a cause is a command's multi-line stderr.
    ///
    /// RED BEFORE THE FIX: printed with `{:?}`, the prefixed line of the first
    /// error below was "[fc-agent] Error: mounting FUSE volumes", and fcvm's
    /// log at INFO had no line with the mount point or the errno.
    #[test]
    fn the_fatal_error_line_carries_the_whole_cause_chain() {
        let error = Err::<(), _>(std::io::Error::from_raw_os_error(libc::EROFS))
            .context("creating mount point: /mnt/outer/inner")
            .context("mounting FUSE volumes")
            .unwrap_err();
        assert_eq!(
            fatal_error_line(&error),
            "[fc-agent] Error: mounting FUSE volumes: creating mount point: /mnt/outer/inner: \
             Read-only file system (os error 30)"
        );

        let error = anyhow::anyhow!(
            "Failed to mount /dev/vdb at /data: mount: wrong fs type,\n       bad superblock.\n"
        )
        .context("mounting extra disks");
        assert_eq!(
            fatal_error_line(&error),
            "[fc-agent] Error: mounting extra disks: Failed to mount /dev/vdb at /data: mount: \
             wrong fs type, bad superblock."
        );
    }

    /// The GPU one-shots run podman as the container's user: they take the agent's
    /// command prefix before parking or unparking. Without it a VM run with a nonzero
    /// --user looked for its container in root's store, found no GPU hook, and the park
    /// failed on the emulator still holding the GPU.
    #[test]
    fn the_gpu_one_shots_take_the_agents_podman_prefix_first() {
        let source = include_str!("main.rs");
        let code = source.split("#[cfg(test)]").next().unwrap();
        for one_shot in ["gpu_park::park()", "gpu_park::unpark_source()"] {
            let call = code.find(one_shot).expect(one_shot);
            let arm = code[..call].rfind("Some(\"--gpu-").unwrap();
            assert!(
                code[arm..call].contains("container::restore_podman_cmd_prefix()"),
                "{one_shot} runs without the agent's podman command prefix"
            );
        }
    }
}

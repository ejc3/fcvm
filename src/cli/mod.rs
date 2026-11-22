pub mod args;

// Re-export all CLI types for convenience
pub use args::*;

/// The log filter when RUST_LOG is unset. Warnings only, except for the fleet agent's own
/// modules at info: a fleet host's agent runs under systemd
/// (fleet/ami/host/fcvm-fleet-agent.service, which sets no RUST_LOG), and its info lines (a
/// phone action's result, `open_browser`'s time to the front, a restore's timings) are what its
/// journal keeps of what a slot did. The coordinator's task sets RUST_LOG=info for the same
/// reason. The VMs' output the agent relays (`fcvm::fleet::vm`) is at debug, so it stays out.
pub fn default_log_filter(cmd: &Commands) -> &'static str {
    match cmd {
        Commands::Fleet(FleetArgs {
            cmd: FleetCommand::Agent(_),
        }) => "warn,fcvm::fleet=info",
        _ => "warn",
    }
}

/// Worker threads for the tokio runtime of a command that runs one VM, or a short query.
///
/// Tokio's default is one worker per CPU, 192 on a c8g.metal-48xl. A `snapshot run` clone
/// needs a few: its tasks are its health monitor, its VM's console and its signal handlers, and
/// its blocking work goes to the blocking pool, which this does not size. Every fork it makes
/// (a `pre_exec` spawn) then copies the page tables and write-protects the pages of every
/// worker's stack, and each later write by any of those threads is a copy-on-write fault with
/// a broadcast TLB invalidation: `ptep_clear_flush` was 15.3% of all host samples while 128
/// clones started at once (bench/start-rate). The memory server, the fleet commands and setup
/// keep the default: they serve many peers or run parallel work.
pub const VM_COMMAND_WORKERS: usize = 2;

/// The worker count for `cmd`'s runtime, or `None` for tokio's default. `FCVM_RUNTIME_WORKERS`
/// overrides it for any command (a positive integer; anything else is ignored), for measuring.
pub fn runtime_workers(cmd: &Commands) -> Option<usize> {
    runtime_workers_with(cmd, std::env::var("FCVM_RUNTIME_WORKERS").ok().as_deref())
}

fn runtime_workers_with(cmd: &Commands, env: Option<&str>) -> Option<usize> {
    if let Some(n) = env
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
    {
        return Some(n);
    }
    match cmd {
        Commands::Snapshot(SnapshotArgs {
            cmd: SnapshotCommands::Run(_),
        })
        | Commands::Exec(_)
        | Commands::Ls(_)
        | Commands::Snapshots(_)
        | Commands::Balloon(_) => Some(VM_COMMAND_WORKERS),
        _ => None,
    }
}

#[cfg(test)]
mod runtime_workers_tests {
    use super::*;
    use clap::Parser;

    fn workers(args: &[&str], env: Option<&str>) -> Option<usize> {
        let cli = Cli::try_parse_from(args).expect("parses");
        runtime_workers_with(&cli.cmd, env)
    }

    /// A clone's process, and the short queries run beside it, get a runtime of
    /// VM_COMMAND_WORKERS workers, not one per CPU.
    #[test]
    fn a_clone_gets_a_small_runtime() {
        let run = ["fcvm", "snapshot", "run", "--pid", "1", "--name", "c"];
        assert_eq!(workers(&run, None), Some(VM_COMMAND_WORKERS));
        assert_eq!(
            workers(&["fcvm", "ls", "--json"], None),
            Some(VM_COMMAND_WORKERS)
        );
        assert_eq!(
            workers(&["fcvm", "exec", "--pid", "1", "--", "true"], None),
            Some(VM_COMMAND_WORKERS)
        );
    }

    /// The memory server serves every clone's faults, so it keeps one worker per CPU.
    #[test]
    fn the_memory_server_keeps_the_default() {
        assert_eq!(workers(&["fcvm", "snapshot", "serve", "tag"], None), None);
    }

    #[test]
    fn the_environment_overrides_any_command() {
        assert_eq!(
            workers(&["fcvm", "snapshot", "serve", "tag"], Some("8")),
            Some(8)
        );
        let run = ["fcvm", "snapshot", "run", "--pid", "1", "--name", "c"];
        assert_eq!(workers(&run, Some("0")), Some(VM_COMMAND_WORKERS));
        assert_eq!(workers(&run, Some("many")), Some(VM_COMMAND_WORKERS));
    }

    /// main builds its runtime from runtime_workers, after parsing the command line.
    #[test]
    fn main_sizes_its_runtime_from_the_command() {
        let main = include_str!("../main.rs");
        let parse = main
            .find("let cli = cli::Cli::parse();")
            .expect("main parses first");
        let sized = main
            .find("fcvm::cli::runtime_workers(&cli.cmd)")
            .expect("main asks runtime_workers");
        let built = main.find(".build()?").expect("main builds the runtime");
        assert!(parse < sized && sized < built);
    }
}

#[cfg(test)]
mod log_filter_tests {
    use super::*;
    use clap::Parser;
    use std::sync::{Arc, Mutex};

    /// The lines one event leaves under `filter`, written as the agent writes them.
    fn emitted(filter: &str, emit: impl FnOnce()) -> String {
        #[derive(Clone, Default)]
        struct Buf(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Buf {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let buf = Buf::default();
        let out = buf.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
            .with_writer(move || out.clone())
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, emit);
        let bytes = buf.0.lock().unwrap().clone();
        String::from_utf8(bytes).unwrap()
    }

    fn agent_cli() -> Cli {
        Cli::try_parse_from([
            "fcvm",
            "fleet",
            "agent",
            "--image",
            "localhost/renderer",
            "--base-port",
            "20000",
            "--stdio",
        ])
        .expect("the agent's CLI parses")
    }

    /// A fleet host's journal keeps the agent's per-action line, open_browser's time to the
    /// front included, with no RUST_LOG set (the agent's systemd unit sets none). With the
    /// plain `warn` default it kept nothing (staging, 2026-11-08: 0 journal lines from the
    /// agent's process after ten take-over actions).
    #[test]
    fn the_fleet_agent_logs_its_phone_actions_with_no_rust_log() {
        let filter = default_log_filter(&agent_cli().cmd);
        let out = emitted(filter, || {
            // the line slot.rs writes, under slot.rs's own target
            tracing::info!(target: "fcvm::fleet::agent::slot", slot = "phone-1", action = "open_browser", seq = 3u64,
                out = "ok open_browser shown_ms=656", "phone action done");
        });
        assert!(
            out.contains("phone action done") && out.contains("shown_ms=656"),
            "filter {filter:?} dropped it: {out:?}"
        );
        // the VMs' output the agent relays stays out
        let relayed = emitted(
            filter,
            || tracing::debug!(target: "fcvm::fleet::vm", slot = "phone-1", "a VM line"),
        );
        assert!(
            relayed.is_empty(),
            "filter {filter:?} let the VMs' output in: {relayed:?}"
        );
        // and other modules stay at warnings
        let other = emitted(
            filter,
            || tracing::info!(target: "fcvm::commands::snapshot", "an info line"),
        );
        assert!(
            other.is_empty(),
            "filter {filter:?} raised another module: {other:?}"
        );
    }

    #[test]
    fn every_other_command_stays_at_warnings() {
        let cli = Cli::try_parse_from(["fcvm", "ls"]).expect("ls parses");
        assert_eq!(default_log_filter(&cli.cmd), "warn");
    }
}

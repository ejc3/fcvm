use anyhow::Result;
use clap::Parser;
use fcvm::cli::Commands;
use fcvm::{cli, commands, paths};
use tracing::error;
use tracing_log::LogTracer;
use tracing_subscriber::EnvFilter;

/// The file descriptor limit fcvm runs with (soft and hard alike), from the hard limit it was
/// started with and the kernel's ceiling (`fs.nr_open`): the hard limit, at least 65536 (which
/// only a privileged process can raise a lower hard limit to), and never above the ceiling,
/// since the kernel refuses any limit above it, even a hard limit it already has.
///
/// A volume server holds one descriptor for every inode the guest holds on a `--map`, so the
/// soft limit is how many files a guest can hold there: at 65536 a guest that looked up 100k
/// files got EMFILE from the server. The hard limit is the most the host allows, and fcvm used to
/// lower it to 65536 as well.
fn nofile_limits(hard: libc::rlim_t, nr_open: Option<libc::rlim_t>) -> libc::rlim_t {
    let wanted = hard.max(65536);
    nr_open.map_or(wanted, |ceiling| wanted.min(ceiling))
}

/// The kernel's per-process descriptor ceiling.
fn nr_open() -> Option<libc::rlim_t> {
    std::fs::read_to_string("/proc/sys/fs/nr_open")
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Raise the file descriptor limit (see [`nofile_limits`]).
fn raise_resource_limits() {
    use libc::{getrlimit, rlimit, setrlimit, RLIMIT_NOFILE};

    let mut current = rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // Safe: getrlimit only writes the struct it is given.
    if unsafe { getrlimit(RLIMIT_NOFILE, &mut current) } != 0 {
        current.rlim_max = 65536;
    }
    let ceiling = nr_open();
    // The target, then (unprivileged with a hard limit below 65536) the hard limit itself, both
    // within the kernel's ceiling.
    for target in [
        nofile_limits(current.rlim_max, ceiling),
        ceiling.map_or(current.rlim_max, |ceiling| current.rlim_max.min(ceiling)),
    ] {
        let limit = rlimit {
            rlim_cur: target,
            rlim_max: target,
        };
        // Safe: setrlimit only reads the struct it is given.
        if unsafe { setrlimit(RLIMIT_NOFILE, &limit) } == 0 {
            return;
        }
    }
    eprintln!(
        "[fcvm] warning: failed to raise RLIMIT_NOFILE: {}",
        std::io::Error::last_os_error()
    );
}

fn main() -> Result<()> {
    // Everything that mutates PROCESS-WIDE state runs here, on the one thread
    // that exists, before the tokio runtime is built.
    //
    // `#[tokio::main]` is not a place to do this: it expands to "build a
    // multi-thread runtime, then block_on the body", so by the time the body
    // runs the worker threads are already up. `std::env::set_var` is unsafe
    // precisely because it is a data race against any concurrent reader of the
    // environment, and `Command::spawn` reads it. Doing it here means there is
    // no second thread to race with.
    raise_resource_limits();

    // Make /usr/sbin and /sbin reachable before anything shells out. fcvm calls
    // sfdisk, resize2fs, e2fsck, losetup and friends by bare name; sudo resolves
    // those through secure_path, so only the unprivileged paths break, and they
    // break with a bare ENOENT that names no tool.
    fcvm::utils::ensure_sbin_on_path();

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run())
}

async fn run() -> Result<()> {
    // Arm deterministic lifecycle failpoints from FCVM_FAILPOINT (test-only
    // instrumentation; no-op when unset). Once, before any command runs.
    failpoint::arm_from_env();

    // Parse CLI arguments
    let cli = cli::Cli::parse();

    // A value of FCVM_FIRECRACKER_LOG_LEVEL that names no level stops the command here, before
    // it prepares a disk or a namespace for a VM that could not start.
    fcvm::firecracker::check_log_level_env()?;

    // Initialize paths - some commands don't need config
    match &cli.cmd {
        Commands::Setup(_) => {
            // Setup handles its own path initialization after loading config
            // Don't pre-initialize here since setup may create storage first
        }
        Commands::Completions(_)
        | Commands::Fleet(_)
        | Commands::Gpu(_)
        | Commands::Vm(cli::VmArgs {
            cmd: cli::VmCommands::NsHelper(_),
        }) => {
            // Shell completions, the fleet control plane, the GPU pool commands and the
            // raw VM's namespace helper don't need config
            paths::init_with_defaults();
        }
        _ => {
            // All other commands require config
            paths::init_from_config();
        }
    }

    // Initialize logging
    // Use RUST_LOG if set, otherwise warnings only (the fleet agent keeps its own info lines,
    // cli::default_log_filter)
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(fcvm::cli::default_log_filter(&cli.cmd)));

    // `log_internal_errors(false)` on both: when stderr's reader is gone, tracing-subscriber
    // would report the failed write with eprintln!, which panics on the same broken stderr
    // and kills fcvm mid-command (a VM's teardown included). Logs are lost; nothing else is.
    // If --sub-process flag is set, disable timestamps AND level (subprocess mode)
    // Parent process already shows timestamp and level, so subprocess just shows the message
    // But KEEP target tags to show the nesting hierarchy!
    // Otherwise, show full formatting (outermost process)
    if cli.sub_process {
        // Subprocesses NEVER have colors (their output is captured and re-logged)
        tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .with_writer(std::io::stderr) // Logs to stderr, keep stdout clean for command output
            .with_target(true) // KEEP targets to show nesting hierarchy
            .without_time()
            .with_level(false) // Disable level prefix too (INFO, DEBUG, etc.)
            .with_ansi(false) // NEVER use ANSI in subprocesses
            .log_internal_errors(false)
            .try_init()
            .ok();
    } else {
        // Parent process: only use colors when outputting to a TTY (not when piped to file)
        use std::io::IsTerminal;
        let use_color = std::io::stderr().is_terminal();
        tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .with_writer(std::io::stderr) // Logs to stderr, keep stdout clean for command output
            .with_target(true) // Show targets for all processes
            .with_ansi(use_color) // Only use ANSI when outputting to TTY
            .log_internal_errors(false)
            .try_init()
            .ok();
    }

    // Bridge log crate (used by fuse-backend-rs) to tracing
    // This must be done AFTER setting up the tracing subscriber
    let _ = LogTracer::init();

    // A command that starts a VMM or a memory server moves this process into the VM's own
    // cgroup before it starts anything, so what its children allocate is charged there and
    // not to the caller's session (#1028, `fcvm::cgroup`).
    let scope = match fcvm::cgroup::scope_request(&cli.cmd) {
        Some(no_swap) => fcvm::cgroup::enter_this_process(no_swap),
        None => Ok(None),
    };
    let scope = match scope {
        Ok(scope) => scope,
        Err(e) => {
            error!("Error: {:#}", e);
            std::process::exit(1);
        }
    };

    // Dispatch to appropriate command handler
    let result = match cli.cmd {
        Commands::Ls(args) => commands::cmd_ls(args).await,
        Commands::Podman(args) => commands::cmd_podman(*args).await,
        Commands::Snapshot(args) => commands::cmd_snapshot(args).await,
        Commands::Snapshots(args) => commands::cmd_snapshots(args).await,
        Commands::Exec(args) => commands::cmd_exec(args).await,
        Commands::Balloon(args) => commands::cmd_balloon(args).await,
        Commands::Gpu(args) => commands::cmd_gpu(args).await,
        Commands::Serve(args) => commands::cmd_serve(args).await,
        Commands::Setup(args) => commands::cmd_setup(args).await,
        Commands::Completions(args) => {
            commands::cmd_completions(args);
            Ok(())
        }
        Commands::Fleet(args) => fcvm::fleet::cmd_fleet(args).await,
        Commands::Vm(args) => commands::cmd_vm(args).await,
    };

    if let Some(scope) = scope {
        scope.leave();
    }

    // Handle errors
    if let Err(e) = &result {
        error!("Error: {:#}", e);
        std::process::exit(commands::podman::process_exit_status(e));
    }

    result
}

#[cfg(test)]
mod tests {
    /// fcvm runs at the host's hard descriptor limit, never below it: a guest holding more
    /// than 65536 files on a `--map` needs that many descriptors in the volume server.
    #[test]
    fn the_descriptor_limit_is_the_hard_limit() {
        assert_eq!(super::nofile_limits(1_048_576, Some(1_048_576)), 1_048_576);
        assert_eq!(super::nofile_limits(4096, Some(1_048_576)), 65536);
        assert_eq!(super::nofile_limits(1_048_576, None), 1_048_576);
    }

    /// A hard limit above the kernel's ceiling (`fs.nr_open` lowered after the launcher set it)
    /// is never asked for: the kernel refuses it even unchanged, which left fcvm at its soft
    /// limit (1024) where the old fixed 65536 was granted (review of #1792).
    #[test]
    fn the_descriptor_limit_stays_within_the_kernel_ceiling() {
        assert_eq!(super::nofile_limits(1_048_576, Some(65536)), 65536);
        assert_eq!(super::nofile_limits(4096, Some(32768)), 32768);
    }
}

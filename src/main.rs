use anyhow::Result;
use clap::Parser;
use fcvm::cli::Commands;
use fcvm::{cli, commands, paths};
use tracing::error;
use tracing_log::LogTracer;
use tracing_subscriber::EnvFilter;

/// Raise file descriptor limit for high-parallelism workloads.
/// The fuse-pipe server can have many open files when serving parallel tests.
fn raise_resource_limits() {
    use libc::{rlimit, setrlimit, RLIMIT_NOFILE};

    let new_limit = rlimit {
        rlim_cur: 65536,
        rlim_max: 65536,
    };

    let result = unsafe { setrlimit(RLIMIT_NOFILE, &new_limit) };
    if result != 0 {
        eprintln!(
            "[fcvm] warning: failed to raise RLIMIT_NOFILE: {}",
            std::io::Error::last_os_error()
        );
    }
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
        Commands::Completions(_) | Commands::Fleet(_) | Commands::Gpu(_) => {
            // Shell completions, the fleet control plane and the GPU pool commands don't need config
            paths::init_with_defaults();
        }
        _ => {
            // All other commands require config
            paths::init_from_config();
        }
    }

    // Initialize logging
    // Use RUST_LOG if set, otherwise default to WARN (quiet by default, use RUST_LOG=info for verbose)
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));

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
    };

    // Handle errors
    if let Err(e) = &result {
        error!("Error: {:#}", e);
        std::process::exit(commands::podman::process_exit_status(e));
    }

    result
}

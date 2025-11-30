//! Stress test for fuse-pipe multi-reader FUSE performance.
//!
//! Custom test harness (harness = false) for full control over output.
//!
//! Usage:
//!   cargo test --test stress --release
//!   cargo test --test stress --release -- --workers 64 --readers 64

mod harness;
mod metrics;
mod worker;

use clap::{Parser, Subcommand};
use fuse_pipe::{mount_with_options, AsyncServer, PassthroughFs, ServerConfig};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "stress")]
#[command(about = "FUSE multi-reader stress test")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Number of worker processes
    #[arg(short, long, default_value = "4")]
    workers: usize,

    /// Operations per worker
    #[arg(short, long, default_value = "1000")]
    ops: usize,

    /// Directory to serve
    #[arg(short, long, default_value = "/tmp/fuse-stress-data")]
    data: PathBuf,

    /// Mount point
    #[arg(short, long, default_value = "/tmp/fuse-stress-mount")]
    mount: PathBuf,

    /// Number of FUSE reader threads
    #[arg(short, long, default_value = "4")]
    readers: usize,

    /// Trace every Nth request (0 = disabled)
    #[arg(short, long, default_value = "0")]
    trace_rate: u64,
}

#[derive(Subcommand)]
enum Commands {
    /// Internal: run server (used by stress test)
    Server {
        #[arg(short, long)]
        socket: String,
        #[arg(short, long)]
        root: PathBuf,
    },

    /// Internal: run client (used by stress test)
    Client {
        #[arg(short, long)]
        socket: String,
        #[arg(short, long)]
        mount: PathBuf,
        #[arg(short, long, default_value = "1")]
        readers: usize,
        #[arg(short, long, default_value = "0")]
        trace_rate: u64,
    },

    /// Internal: stress worker (used by stress test)
    StressWorker {
        #[arg(long)]
        id: usize,
        #[arg(long)]
        ops: usize,
        #[arg(long)]
        mount: PathBuf,
        #[arg(long)]
        results: PathBuf,
    },
}

fn main() -> anyhow::Result<()> {
    raise_fd_limit();
    metrics::init();

    let cli = Cli::parse_from(filtered_args());

    match cli.command {
        Some(Commands::Server { socket, root }) => {
            std::fs::create_dir_all(&root)?;
            eprintln!("[server] serving {} on {}", root.display(), socket);

            let fs = PassthroughFs::new(&root);
            let config = ServerConfig::default();
            let server = AsyncServer::with_config(fs, config);

            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(server.serve_unix(&socket))?;
        }

        Some(Commands::Client {
            socket,
            mount,
            readers,
            trace_rate,
        }) => {
            std::fs::create_dir_all(&mount)?;
            eprintln!(
                "[client] mounting at {} via {} (readers: {}, trace_rate: {})",
                mount.display(),
                socket,
                readers,
                trace_rate
            );

            mount_with_options(&socket, &mount, readers, trace_rate)?;
            eprintln!("[client] unmounted");
        }

        Some(Commands::StressWorker {
            id,
            ops,
            mount,
            results,
        }) => {
            worker::run_stress_worker(id, ops, &mount, &results)?;
        }

        None => {
            // Run the stress test
            if let Err(e) = harness::run_stress_test(
                cli.workers,
                cli.ops,
                &cli.data,
                &cli.mount,
                cli.readers,
                cli.trace_rate,
            ) {
                eprintln!("[stress] failed: {:#}", e);
                return Err(e);
            }
        }
    }

    Ok(())
}

/// Drop cargo test harness flags so Clap doesn't choke on them.
fn filtered_args() -> Vec<String> {
    let mut args = std::env::args();
    let mut filtered = Vec::new();
    if let Some(bin) = args.next() {
        filtered.push(bin);
    }
    for arg in args {
        if matches!(
            arg.as_str(),
            "--nocapture" | "--ignored" | "--quiet" | "--test-threads" | "--exact"
        ) || arg.starts_with("--color")
            || arg.starts_with("--format")
        {
            continue;
        }
        filtered.push(arg);
    }
    filtered
}

fn raise_fd_limit() {
    use std::mem::MaybeUninit;

    unsafe {
        let mut rlim = MaybeUninit::<libc::rlimit>::uninit();
        if libc::getrlimit(libc::RLIMIT_NOFILE, rlim.as_mut_ptr()) == 0 {
            let mut rlim = rlim.assume_init();
            let target = if rlim.rlim_max == libc::RLIM_INFINITY {
                1_048_576
            } else {
                rlim.rlim_max
            };
            rlim.rlim_cur = target;
            libc::setrlimit(libc::RLIMIT_NOFILE, &rlim);
        }
    }
}

//! Stress test for fuse-pipe multi-reader FUSE performance.
//!
//! Custom test harness (harness = false) for full control over output.
//!
//! Usage:
//!   cargo test --test stress --release
//!   cargo test --test stress --release -- --workers 64 --readers 64
//!
//! Enable debug logging with RUST_LOG:
//!   RUST_LOG=debug cargo test --test stress --release
//!   RUST_LOG=passthrough=trace cargo test --test stress --release

mod harness;
mod metrics;
mod worker;

use clap::{Parser, Subcommand};
use fuse_pipe::{
    mount_with_telemetry, AsyncServer, PassthroughFs, ServerConfig, SpanCollector,
};
use std::path::PathBuf;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

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
    #[arg(short, long, default_value = "256")]
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
        /// Output telemetry JSON to this file path when unmounted
        #[arg(long)]
        telemetry_output: Option<PathBuf>,
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
    // Initialize tracing subscriber with env filter (RUST_LOG)
    tracing_subscriber::registry()
        .with(fmt::layer().with_writer(std::io::stderr))
        .with(EnvFilter::from_default_env())
        .init();

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
            telemetry_output,
        }) => {
            std::fs::create_dir_all(&mount)?;
            eprintln!(
                "[client] mounting at {} via {} (readers: {}, trace_rate: {})",
                mount.display(),
                socket,
                readers,
                trace_rate
            );

            // Create collector if telemetry output is requested
            let collector = if telemetry_output.is_some() && trace_rate > 0 {
                Some(SpanCollector::new())
            } else {
                None
            };

            mount_with_telemetry(&socket, &mount, readers, trace_rate, collector.clone())?;
            eprintln!("[client] unmounted");

            // Output telemetry if requested
            if let (Some(output_path), Some(collector)) = (telemetry_output, collector) {
                if let Some(json) = collector.summary_json() {
                    std::fs::write(&output_path, &json)?;
                    eprintln!("[client] telemetry written to {}", output_path.display());
                } else {
                    eprintln!("[client] no telemetry data collected");
                }
            }
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

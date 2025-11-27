//! Stress test harness for fuse-pipe.
//!
//! This program provides both standalone server/client modes and an integrated
//! stress test that exercises the fuse-pipe implementation.
//!
//! Usage:
//!   # Terminal 1: Start the server
//!   ./fuse-test server --socket /tmp/fuse.sock --root /tmp/fuse-data
//!
//!   # Terminal 2: Start the client (mount FUSE)
//!   ./fuse-test client --socket /tmp/fuse.sock --mount /tmp/fuse-mount
//!
//!   # Run stress test
//!   ./fuse-test stress --workers 4 --ops 1000

mod metrics;
mod stress;
mod worker;

use clap::{Parser, Subcommand};
use fuse_pipe::{AsyncServer, PassthroughFs, ServerConfig, mount_with_options};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "fuse-test")]
#[command(about = "Stress test harness for fuse-pipe")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the server (serves a local directory over Unix socket)
    Server {
        /// Unix socket path
        #[arg(short, long, default_value = "/tmp/fuse-test.sock")]
        socket: String,

        /// Root directory to serve
        #[arg(short, long)]
        root: PathBuf,
    },

    /// Run the client (FUSE filesystem that connects to server)
    Client {
        /// Unix socket path
        #[arg(short, long, default_value = "/tmp/fuse-test.sock")]
        socket: String,

        /// Mount point
        #[arg(short, long)]
        mount: PathBuf,

        /// Number of FUSE reader threads (protocol-level multiplexing)
        #[arg(short, long, default_value = "1")]
        readers: usize,

        /// Trace every Nth request (0 = disabled)
        #[arg(short, long, default_value = "0")]
        trace_rate: u64,
    },

    /// Run both server and client (for quick testing)
    Test {
        /// Directory to serve
        #[arg(short, long, default_value = "/tmp/fuse-test-data")]
        data: PathBuf,

        /// Mount point
        #[arg(short, long, default_value = "/tmp/fuse-test-mount")]
        mount: PathBuf,
    },

    /// Run stress test with multiple workers and readers
    Stress {
        /// Number of worker processes
        #[arg(short, long, default_value = "4")]
        workers: usize,

        /// Operations per worker
        #[arg(short, long, default_value = "1000")]
        ops: usize,

        /// Directory to serve
        #[arg(short, long, default_value = "/tmp/fuse-stress-data")]
        data: PathBuf,

        /// Mount point (or same as data for bare mode)
        #[arg(short, long, default_value = "/tmp/fuse-stress-mount")]
        mount: PathBuf,

        /// Number of FUSE reader threads
        #[arg(short, long, default_value = "4")]
        readers: usize,

        /// Trace every Nth request (0 = disabled)
        #[arg(short, long, default_value = "0")]
        trace_rate: u64,
    },

    /// Internal: stress worker process (spawned by stress command)
    StressWorker {
        /// Worker ID
        #[arg(long)]
        id: usize,

        /// Operations to perform
        #[arg(long)]
        ops: usize,

        /// Mount point
        #[arg(long)]
        mount: PathBuf,

        /// Results file path
        #[arg(long)]
        results: PathBuf,
    },
}

fn main() -> anyhow::Result<()> {
    // Raise file descriptor limit for stress tests with many readers/workers
    raise_fd_limit();

    // Initialize metrics collection
    metrics::init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Server { socket, root } => {
            // Create root directory if it doesn't exist
            std::fs::create_dir_all(&root)?;

            eprintln!("[server] serving {} on {}", root.display(), socket);

            // Use fuse-pipe's async pipelined server
            let fs = PassthroughFs::new(&root);
            let config = ServerConfig::default();
            let server = AsyncServer::with_config(fs, config);

            // Run the server
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(server.serve_unix(&socket))?;
        }

        Commands::Client { socket, mount, readers, trace_rate } => {
            // Create mount point if it doesn't exist
            std::fs::create_dir_all(&mount)?;

            eprintln!(
                "[client] mounting at {} via {} (readers: {}, trace_rate: {})",
                mount.display(),
                socket,
                readers,
                trace_rate
            );

            // Use fuse-pipe's client
            mount_with_options(&socket, &mount, readers, trace_rate)?;
            eprintln!("[client] unmounted");
        }

        Commands::Test { data, mount } => {
            run_quick_test(&data, &mount)?;
        }

        Commands::Stress { workers, ops, data, mount, readers, trace_rate } => {
            stress::run_stress_test(workers, ops, &data, &mount, readers, trace_rate)?;
        }

        Commands::StressWorker { id, ops, mount, results } => {
            worker::run_stress_worker(id, ops, &mount, &results)?;
        }
    }

    Ok(())
}

/// Raise file descriptor limit to support many FUSE readers and workers.
fn raise_fd_limit() {
    use std::mem::MaybeUninit;

    unsafe {
        let mut rlim = MaybeUninit::<libc::rlimit>::uninit();
        if libc::getrlimit(libc::RLIMIT_NOFILE, rlim.as_mut_ptr()) == 0 {
            let mut rlim = rlim.assume_init();
            // Try to set soft limit to hard limit (or 1M if hard is unlimited)
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

/// Run a quick integration test with server + client.
fn run_quick_test(data: &PathBuf, mount: &PathBuf) -> anyhow::Result<()> {
    use std::process::{Command, Stdio};
    use std::thread;
    use std::time::Duration;

    let socket = "/tmp/fuse-test.sock";

    // Create directories
    std::fs::create_dir_all(data)?;
    std::fs::create_dir_all(mount)?;

    // Remove old socket
    let _ = std::fs::remove_file(socket);

    // Get the current executable path
    let exe = std::env::current_exe()?;

    // Start server in background
    eprintln!("[test] starting server...");
    let mut server = Command::new(&exe)
        .args(["server", "--socket", socket, "--root", data.to_str().unwrap()])
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?;

    // Wait for socket to be ready
    thread::sleep(Duration::from_millis(500));

    // Start client in background
    eprintln!("[test] starting client...");
    let mut client = Command::new(&exe)
        .args(["client", "--socket", socket, "--mount", mount.to_str().unwrap()])
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?;

    // Wait for mount to be ready
    thread::sleep(Duration::from_millis(500));

    // Run basic tests
    eprintln!("\n[test] Running basic filesystem tests...\n");

    let test_file = mount.join("test.txt");
    let test_dir = mount.join("testdir");

    // Test 1: Create and write file
    eprintln!("[test] 1. Creating file...");
    std::fs::write(&test_file, "Hello, fuse-pipe!\n")?;
    eprintln!("[test]    ✓ File created");

    // Test 2: Read file
    eprintln!("[test] 2. Reading file...");
    let content = std::fs::read_to_string(&test_file)?;
    assert_eq!(content, "Hello, fuse-pipe!\n");
    eprintln!("[test]    ✓ Content matches");

    // Test 3: Create directory
    eprintln!("[test] 3. Creating directory...");
    std::fs::create_dir(&test_dir)?;
    eprintln!("[test]    ✓ Directory created");

    // Test 4: List directory
    eprintln!("[test] 4. Listing directory...");
    let entries: Vec<_> = std::fs::read_dir(mount)?.collect();
    eprintln!("[test]    Found {} entries", entries.len());
    for entry in &entries {
        if let Ok(e) = entry {
            eprintln!("[test]    - {}", e.file_name().to_string_lossy());
        }
    }

    // Test 5: Create file in subdirectory
    eprintln!("[test] 5. Creating file in subdirectory...");
    let subfile = test_dir.join("sub.txt");
    std::fs::write(&subfile, "Nested file\n")?;
    eprintln!("[test]    ✓ Nested file created");

    // Cleanup tests
    eprintln!("[test] 6. Removing files...");
    std::fs::remove_file(&subfile)?;
    std::fs::remove_file(&test_file)?;
    eprintln!("[test]    ✓ Files removed");

    eprintln!("[test] 7. Removing directory...");
    std::fs::remove_dir(&test_dir)?;
    eprintln!("[test]    ✓ Directory removed");

    eprintln!("\n[test] All tests passed! ✓\n");

    // Clean up
    eprintln!("[test] Cleaning up...");

    // Kill client first (unmounts)
    client.kill()?;
    let _ = client.wait();

    // Then kill server
    server.kill()?;
    let _ = server.wait();

    // Clean up socket
    let _ = std::fs::remove_file(socket);

    eprintln!("[test] Done!");
    Ok(())
}

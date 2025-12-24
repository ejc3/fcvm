//! Common utilities for pjdfstest integration.
//!
//! Provides FUSE mount setup and category execution for POSIX compliance tests.

use fuse_pipe::{mount_spawn, AsyncServer, MountConfig, PassthroughFs, ServerConfig};
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Once;
use std::time::Duration;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

const PJDFSTEST_BIN: &str = "/tmp/pjdfstest-check/pjdfstest";
const PJDFSTEST_TESTS: &str = "/tmp/pjdfstest-check/tests";
const SOCKET_BASE: &str = "/tmp/fuse-pjdfs.sock";
const DATA_BASE: &str = "/tmp/fuse-pjdfs-data";
const MOUNT_BASE: &str = "/tmp/fuse-pjdfs-mount";
const NUM_READERS: usize = 256;
const TIMEOUT_SECS: u64 = 600;

/// Target name for logs (consistent with library naming)
const TARGET: &str = "fuse_pipe::pjdfstest";

/// Initialize tracing once for the test process.
static TRACING_INIT: Once = Once::new();

fn init_tracing() {
    TRACING_INIT.call_once(|| {
        tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::from_default_env())
            .with_writer(std::io::stderr)
            .init();
    });
}

/// Increase file descriptor limit to avoid "Too many open files" errors.
/// Required when running with 256 FUSE readers + parallel test jobs.
fn raise_fd_limit() {
    #[cfg(unix)]
    {
        use std::mem::MaybeUninit;
        let mut rlim = MaybeUninit::<libc::rlimit>::uninit();
        unsafe {
            if libc::getrlimit(libc::RLIMIT_NOFILE, rlim.as_mut_ptr()) == 0 {
                let mut rlim = rlim.assume_init();
                let target = 65536u64.min(rlim.rlim_max);
                if rlim.rlim_cur < target {
                    rlim.rlim_cur = target;
                    if libc::setrlimit(libc::RLIMIT_NOFILE, &rlim) == 0 {
                        eprintln!("[init] Raised fd limit to {}", target);
                    }
                }
            }
        }
    }
}

#[derive(Debug)]
struct CategoryResult {
    category: String,
    passed: bool,
    tests: usize,
    failures: usize,
    duration_secs: f64,
    output: String,
}

fn run_category(category: &str, mount_dir: &Path, jobs: usize) -> CategoryResult {
    let start = std::time::Instant::now();
    let tests_dir = Path::new(PJDFSTEST_TESTS);
    let category_tests = tests_dir.join(category);

    // Safety check: Verify we're on FUSE filesystem
    let marker = mount_dir.join(".fuse-pipe-test-marker");
    if !marker.exists() {
        return CategoryResult {
            category: category.to_string(),
            passed: false,
            tests: 0,
            failures: 0,
            duration_secs: start.elapsed().as_secs_f64(),
            output: format!(
                "FATAL: Test directory is NOT on FUSE filesystem! Marker {} not found.",
                marker.display()
            ),
        };
    }

    let work_dir = mount_dir.join(category);
    let _ = fs::remove_dir_all(&work_dir);
    if let Err(e) = fs::create_dir_all(&work_dir) {
        return CategoryResult {
            category: category.to_string(),
            passed: false,
            tests: 0,
            failures: 0,
            duration_secs: start.elapsed().as_secs_f64(),
            output: format!("Failed to create work dir {}: {}", work_dir.display(), e),
        };
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&work_dir, fs::Permissions::from_mode(0o777));
    }

    let output = Command::new("timeout")
        .args([
            &TIMEOUT_SECS.to_string(),
            "prove",
            "-v",
            "-j",
            &jobs.to_string(),
            "-r",
            category_tests.to_str().unwrap(),
        ])
        .current_dir(&work_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();

    let duration = start.elapsed().as_secs_f64();

    match output {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            let combined = format!("{}\n{}", stdout, stderr);

            let (tests, failures) = parse_prove_output(&combined);
            let passed = out.status.success() && failures == 0;

            CategoryResult {
                category: category.to_string(),
                passed,
                tests,
                failures,
                duration_secs: duration,
                output: combined,
            }
        }
        Err(e) => CategoryResult {
            category: category.to_string(),
            passed: false,
            tests: 0,
            failures: 0,
            duration_secs: duration,
            output: format!("Failed to run prove: {}", e),
        },
    }
}

fn parse_prove_output(output: &str) -> (usize, usize) {
    let mut tests = 0usize;
    let mut failures = 0usize;

    for line in output.lines() {
        if line.starts_with("Files=") {
            if let Some(tests_part) = line.split("Tests=").nth(1) {
                if let Some(num_str) = tests_part.split(',').next() {
                    tests = num_str.trim().parse().unwrap_or(0);
                }
            }
        }

        if line.contains("Failed") && line.contains("subtests") {
            let parts: Vec<&str> = line.split_whitespace().collect();
            for (i, part) in parts.iter().enumerate() {
                if *part == "Failed" && i + 1 < parts.len() {
                    if let Some(failed_str) = parts[i + 1].split('/').next() {
                        failures += failed_str.parse::<usize>().unwrap_or(0);
                    }
                }
            }
        }
    }

    (tests, failures)
}

/// Check if pjdfstest is installed.
pub fn is_pjdfstest_installed() -> bool {
    Path::new(PJDFSTEST_BIN).exists()
}

/// Run a single pjdfstest category against FUSE filesystem.
/// Each call sets up its own server/mount for test isolation.
/// Returns (passed, tests, failures).
pub fn run_single_category(category: &str, jobs: usize) -> (bool, usize, usize) {
    init_tracing();
    raise_fd_limit();

    assert!(
        is_pjdfstest_installed(),
        "pjdfstest binary not found - install it or exclude pjdfstest tests from run"
    );

    // Unique paths for this test process
    let pid = std::process::id();
    let run_suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let run_id = format!("{}-{}-{}", pid, category, run_suffix);

    let socket = std::path::PathBuf::from(format!("{}-{}", SOCKET_BASE, run_id));
    let data_dir = std::path::PathBuf::from(format!("{}-{}", DATA_BASE, run_id));
    let mount_dir = std::path::PathBuf::from(format!("{}-{}", MOUNT_BASE, run_id));

    let _ = fs::remove_file(&socket);
    let _ = fs::remove_dir_all(&data_dir);
    let _ = fs::remove_dir_all(&mount_dir);
    fs::create_dir_all(&data_dir).expect("create data dir");
    fs::create_dir_all(&mount_dir).expect("create mount dir");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o777);
        let _ = std::fs::set_permissions(&data_dir, perms.clone());
        let _ = std::fs::set_permissions(&mount_dir, perms);
    }

    // Start server
    info!(target: TARGET, socket = %socket.display(), category = category, "Starting server for category");
    let server_data_dir = data_dir.clone();
    let server_socket = socket.clone();
    let _server_handle = std::thread::spawn(move || {
        let fs = PassthroughFs::new(&server_data_dir);
        let config = ServerConfig::default();
        let server = AsyncServer::with_config(fs, config);

        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                if let Err(e) = server.serve_unix(server_socket.to_str().unwrap()).await {
                    error!(target: TARGET, error = %e, "Server error");
                }
            });
    });

    // Wait for socket
    for _ in 0..50 {
        if socket.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if !socket.exists() {
        error!(target: TARGET, socket = %socket.display(), "Server socket not created");
        return (false, 0, 0);
    }

    // Mount FUSE
    let config = MountConfig::new().readers(NUM_READERS);
    let _mount_handle = match mount_spawn(socket.to_str().unwrap(), mount_dir.clone(), config) {
        Ok(handle) => handle,
        Err(e) => {
            error!(target: TARGET, error = %e, "Mount failed");
            return (false, 0, 0);
        }
    };

    // Wait for mount
    let mount_path_str = mount_dir.to_str().unwrap();
    let mut mounted = false;
    for _ in 0..100 {
        if let Ok(mounts) = fs::read_to_string("/proc/mounts") {
            if mounts
                .lines()
                .any(|line| line.contains(mount_path_str) && line.contains("fuse"))
            {
                mounted = true;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if !mounted {
        error!(target: TARGET, "FUSE mount did not appear");
        return (false, 0, 0);
    }

    // Create marker
    let marker = mount_dir.join(".fuse-pipe-test-marker");
    if let Err(e) = fs::write(&marker, "fuse-pipe") {
        error!(target: TARGET, error = %e, "Failed to create marker");
        return (false, 0, 0);
    }

    std::thread::sleep(Duration::from_millis(100));

    // Run the category
    info!(target: TARGET, category = category, "Running category tests");
    let result = run_category(category, &mount_dir, jobs);

    let status = if result.passed { "✓" } else { "✗" };
    println!(
        "[FUSE] {} {} ({} tests, {} failures, {:.1}s)",
        status, result.category, result.tests, result.failures, result.duration_secs
    );

    if !result.passed {
        // Print failure details
        for line in result.output.lines() {
            if line.contains("not ok")
                || line.contains("Failed")
                || line.contains("expected")
                || line.contains("got ")
                || line.contains("FATAL")
            {
                println!("{}", line);
            }
        }
    }

    // RAII cleanup via _mount_handle drop
    (
        result.passed && result.failures == 0,
        result.tests,
        result.failures,
    )
}

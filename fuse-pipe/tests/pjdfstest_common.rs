use fuse_pipe::{mount_with_options, AsyncServer, PassthroughFs, ServerConfig};
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Once;
use std::time::Duration;
use std::{sync::mpsc, thread};
use tracing::{debug, error, info};
use tracing_subscriber::EnvFilter;

const PJDFSTEST_BIN: &str = "/tmp/pjdfstest-check/pjdfstest";
const PJDFSTEST_TESTS: &str = "/tmp/pjdfstest-check/tests";
const SOCKET_BASE: &str = "/tmp/fuse-pjdfs.sock";
const DATA_BASE: &str = "/tmp/fuse-pjdfs-data";
const MOUNT_BASE: &str = "/tmp/fuse-pjdfs-mount";
const NUM_READERS: usize = 256;
// Generous timeouts to avoid premature failures on slower/loaded hosts.
const TIMEOUT_SECS: u64 = 600;
const CATEGORY_TIMEOUT_SECS: u64 = 900;

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

fn discover_categories() -> Vec<String> {
    let tests_dir = Path::new(PJDFSTEST_TESTS);
    let mut categories = Vec::new();

    if let Ok(entries) = fs::read_dir(tests_dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                if let Some(name) = entry.file_name().to_str() {
                    categories.push(name.to_string());
                }
            }
        }
    }

    categories.sort();
    categories
}

fn run_category(category: &str, mount_dir: &Path, jobs: usize, is_fuse: bool) -> CategoryResult {
    let start = std::time::Instant::now();
    let tests_dir = Path::new(PJDFSTEST_TESTS);
    let category_tests = tests_dir.join(category);

    // Safety check: If running FUSE tests, verify we're actually on FUSE filesystem
    if is_fuse {
        let marker = mount_dir.join(".fuse-pipe-test-marker");
        if !marker.exists() {
            return CategoryResult {
                category: category.to_string(),
                passed: false,
                tests: 0,
                failures: 0,
                duration_secs: start.elapsed().as_secs_f64(),
                output: format!(
                    "FATAL: Test directory is NOT on FUSE filesystem! Marker {} not found. \
                     This likely means tests would run on host filesystem instead of FUSE.",
                    marker.display()
                ),
            };
        }
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

fn cleanup_mount(mount_dir: &Path) {
    if let Some(mount) = mount_dir.to_str() {
        let _ = Command::new("fusermount3").args(["-u", mount]).output();
        let _ = Command::new("umount").args(["-f", mount]).output();
    }
    std::thread::sleep(Duration::from_millis(100));
}

fn dump_mount_state() {
    let _ = Command::new("mount")
        .arg("-t")
        .arg("fuse")
        .output()
        .map(|out| {
            eprintln!(
                "[debug] current fuse mounts:\n{}",
                String::from_utf8_lossy(&out.stdout)
            )
        });
}

fn verify_mount(mount_dir: &Path) -> bool {
    let probe = mount_dir.join(".pjdfs-probe");
    match fs::write(&probe, "probe") {
        Ok(_) => {
            let _ = fs::remove_file(&probe);
            true
        }
        Err(e) => {
            eprintln!("Mount check failed at {}: {}", mount_dir.display(), e);
            false
        }
    }
}

/// Check if pjdfstest is installed. Returns true if installed, false if not.
/// When not installed, prints instructions and the test should skip (not fail).
pub fn is_pjdfstest_installed() -> bool {
    Path::new(PJDFSTEST_BIN).exists()
}

fn run_suite(use_host_fs: bool, full: bool, jobs: usize) -> bool {
    // Initialize tracing for debug logging
    init_tracing();

    // Raise fd limit early - required for 256 FUSE readers + parallel prove jobs
    raise_fd_limit();

    // Print big banner to make it SUPER CLEAR which test is running
    if use_host_fs {
        println!("\n");
        println!("╔═══════════════════════════════════════════════════════════════════════════╗");
        println!("║                                                                           ║");
        println!("║   ⚠️  SANITY CHECK: Running against HOST FILESYSTEM (not FUSE!)           ║");
        println!("║                                                                           ║");
        println!("║   This test does NOT test fuse-pipe. It only verifies that pjdfstest      ║");
        println!("║   works correctly on this system. Failures here are informational only.   ║");
        println!("║                                                                           ║");
        println!("╚═══════════════════════════════════════════════════════════════════════════╝");
        println!();
    } else {
        println!("\n");
        println!("╔═══════════════════════════════════════════════════════════════════════════╗");
        println!("║                                                                           ║");
        println!("║   🎯 THE REAL TEST: Running against FUSE FILESYSTEM                       ║");
        println!("║                                                                           ║");
        println!("║   This is the actual fuse-pipe test! All tests must pass.                 ║");
        println!("║                                                                           ║");
        println!("╚═══════════════════════════════════════════════════════════════════════════╝");
        println!();
    }

    if !is_pjdfstest_installed() {
        // This shouldn't be reached - caller should check is_pjdfstest_installed() first
        eprintln!(
            "pjdfstest not found at {}. Install with:\n\
             git clone https://github.com/pjd/pjdfstest /tmp/pjdfstest-check\n\
             cd /tmp/pjdfstest-check && autoreconf -ifs && ./configure && make",
            PJDFSTEST_BIN
        );
        return false;
    }

    let pid = std::process::id();
    let run_suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let run_id = format!("{}-{}", pid, run_suffix);

    let socket = std::path::PathBuf::from(format!("{}-{}", SOCKET_BASE, run_id));
    let data_dir = std::path::PathBuf::from(format!("{}-{}", DATA_BASE, run_id));
    let mount_dir = if use_host_fs {
        data_dir.clone()
    } else {
        std::path::PathBuf::from(format!("{}-{}", MOUNT_BASE, run_id))
    };

    cleanup_mount(&mount_dir);
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

    if use_host_fs {
        info!(target: TARGET, path = %mount_dir.display(), "Running directly on host filesystem");
    } else {
        info!(target: TARGET, socket = %socket.display(), data = %data_dir.display(), "Starting server");
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

        for _ in 0..50 {
            if socket.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if !socket.exists() {
            error!(target: TARGET, socket = %socket.display(), "Server socket not created");
            return false;
        }

        info!(target: TARGET, mount = %mount_dir.display(), readers = NUM_READERS, "Mounting FUSE filesystem");
        let mount_dir_clone = mount_dir.clone();
        let socket_clone = socket.clone();
        let _client_handle = std::thread::spawn(move || {
            if let Err(e) = mount_with_options(
                socket_clone.to_str().unwrap(),
                mount_dir_clone.to_str().unwrap(),
                NUM_READERS,
                0,
            ) {
                error!(target: TARGET, error = %e, "Mount failed");
                std::process::exit(1);
            }
        });

        // Wait for FUSE to actually be mounted by checking /proc/mounts
        // This is more reliable than just checking if the directory exists
        let mount_path_str = mount_dir.to_str().unwrap();
        let mut mounted = false;
        for _ in 0..100 {
            // Check /proc/mounts for the FUSE mount
            if let Ok(mounts) = fs::read_to_string("/proc/mounts") {
                if mounts.lines().any(|line| line.contains(mount_path_str) && line.contains("fuse")) {
                    mounted = true;
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if !mounted {
            error!(target: TARGET, mount = %mount_dir.display(), "FUSE mount did not appear in /proc/mounts");
            return false;
        }
        // Additional verification that the mount is usable
        if !verify_mount(&mount_dir) {
            error!(target: TARGET, mount = %mount_dir.display(), "Mount verification failed");
            return false;
        }
        info!(target: TARGET, mount = %mount_dir.display(), "FUSE mounted successfully");

        // Create marker file to verify tests run on FUSE, not accidentally on host
        let marker = mount_dir.join(".fuse-pipe-test-marker");
        debug!(target: TARGET, marker = %marker.display(), "Creating FUSE marker file");
        match fs::write(&marker, "fuse-pipe") {
            Ok(_) => debug!(target: TARGET, marker = %marker.display(), "FUSE marker created successfully"),
            Err(e) => {
                error!(target: TARGET, error = %e, marker = %marker.display(), "Failed to create FUSE marker file");
                return false;
            }
        }
        // Verify marker exists
        if !marker.exists() {
            error!(target: TARGET, marker = %marker.display(), "FUSE marker does not exist after creation!");
            return false;
        }

        std::thread::sleep(Duration::from_millis(300));

        // Keep mount alive for test execution; cleaned up after results are gathered.
    }

    let mut categories = discover_categories();
    if !full {
        categories.retain(|c| c == "posix_fallocate");
    }
    let test_type = if use_host_fs { "HOST" } else { "FUSE" };
    info!(target: TARGET, count = categories.len(), ?categories, "Discovered test categories");
    println!(
        "[{}] Found {} categories: {:?}\n",
        test_type,
        categories.len(),
        categories
    );

    let start_time = std::time::Instant::now();
    let total = categories.len();
    let mut results = Vec::with_capacity(total);

    let is_fuse = !use_host_fs;
    for (idx, category) in categories.iter().enumerate() {
        debug!(target: TARGET, category = %category, "Starting test category");
        let (tx, rx) = mpsc::channel();
        let cat = category.clone();
        let mount_for_thread = mount_dir.clone();
        thread::spawn(move || {
            let result = run_category(&cat, &mount_for_thread, jobs, is_fuse);
            let _ = tx.send(result);
        });

        let result = match rx.recv_timeout(Duration::from_secs(CATEGORY_TIMEOUT_SECS)) {
            Ok(r) => r,
            Err(_) => {
                eprintln!(
                    "[timeout] category {} exceeded {}s; dumping mount state and failing",
                    category, CATEGORY_TIMEOUT_SECS
                );
                dump_mount_state();
                if !use_host_fs {
                    cleanup_mount(&mount_dir);
                }
                return false;
            }
        };

        let status = if result.passed { "✓" } else { "✗" };
        let prefix = if use_host_fs { "[HOST]" } else { "[FUSE]" };
        println!(
            "{} [{}/{}] {} {} ({} tests, {} failures, {:.1}s)",
            prefix,
            idx + 1,
            total,
            status,
            result.category,
            result.tests,
            result.failures,
            result.duration_secs
        );

        results.push(result);
    }

    let total_duration = start_time.elapsed().as_secs_f64();

    // Make it crystal clear which test this summary is for
    let (header, note) = if use_host_fs {
        (
            "HOST FILESYSTEM (Sanity Check - Does NOT Affect Pass/Fail)",
            "(This is NOT the fuse-pipe test)",
        )
    } else {
        (
            "🎯 FUSE FILESYSTEM (THE REAL TEST - Must Pass!)",
            "(This IS the fuse-pipe test)",
        )
    };

    println!("\n╔═══════════════════════════════════════════════════════════════════════════╗");
    println!("║  {}  ║", header);
    println!("╠═══════════════════════════════════════════════════════════════════════════╣");
    println!(
        "║  Total tests:      {:>10}                                             ║",
        results.iter().map(|r| r.tests).sum::<usize>()
    );
    println!(
        "║  Total failures:   {:>10}                                             ║",
        results.iter().map(|r| r.failures).sum::<usize>()
    );
    println!(
        "║  Categories:       {:>10}                                             ║",
        categories.len()
    );
    println!(
        "║  Duration:         {:>10.1}s                                            ║",
        total_duration
    );
    println!(
        "║  {:^71}  ║",
        note
    );
    println!("╚═══════════════════════════════════════════════════════════════════════════╝");

    let mut total_tests = 0usize;
    let mut total_failures = 0usize;
    let mut failed_categories = Vec::new();

    for result in results.iter() {
        total_tests += result.tests;
        total_failures += result.failures;
        if !result.passed {
            failed_categories.push(result.category.clone());
        }
    }

    if !failed_categories.is_empty() {
        println!("\nFailed categories: {:?}", failed_categories);

        for result in results.iter() {
            if !result.passed {
                println!("\n━━━ {} output (failures only) ━━━", result.category);
                // Print only failure-related lines to avoid flooding output
                // while still showing all failures regardless of output size
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
        }

        eprintln!(
            "\nFAIL: {} test failures across {} categories",
            total_failures,
            failed_categories.len()
        );
        if !use_host_fs {
            cleanup_mount(&mount_dir);
        }
        return false;
    }

    if use_host_fs {
        println!("\n✅ HOST SANITY CHECK: {} tests passed (informational only)", total_tests);
    } else {
        println!("\n🎉 FUSE TEST PASSED: ALL {} TESTS PASSED - fuse-pipe is POSIX compliant!", total_tests);
    }
    if !use_host_fs {
        cleanup_mount(&mount_dir);
    }
    true
}

pub fn run_all(full: bool, jobs: usize) -> bool {
    // Run host filesystem tests first as a sanity check, but don't fail if host has issues
    // (AWS EC2 instances have known quirks with utimensat precision)
    let host_ok = run_suite(true, full, jobs);
    if !host_ok {
        eprintln!("\n⚠️  Host filesystem has known issues (common on AWS EC2)");
        eprintln!("    This does NOT indicate a fuse-pipe bug - proceeding with FUSE tests\n");
    }

    // FUSE tests are what we actually care about
    let fuse_ok = run_suite(false, full, jobs);
    if !fuse_ok {
        // Attempt cleanup on failure
        let _ = fs::remove_dir_all(format!("{}-{}", MOUNT_BASE, std::process::id()));
    }

    // Only require FUSE tests to pass (host tests are just informational)
    fuse_ok
}

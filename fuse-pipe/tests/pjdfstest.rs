//! pjdfstest POSIX compliance test harness.
//!
//! Runs the C pjdfstest suite against fuse-pipe with parallel category isolation.
//! Each test category runs in its own directory to avoid race conditions.
//!
//! Run with: cargo test --test pjdfstest --release -- --nocapture

use fuse_pipe::{mount_with_options, AsyncServer, PassthroughFs, ServerConfig};
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const PJDFSTEST_BIN: &str = "/tmp/pjdfstest-check/pjdfstest";
const PJDFSTEST_TESTS: &str = "/tmp/pjdfstest-check/tests";
const SOCKET_BASE: &str = "/tmp/fuse-pjdfs.sock";
const DATA_BASE: &str = "/tmp/fuse-pjdfs-data";
const MOUNT_BASE: &str = "/tmp/fuse-pjdfs-mount";

const NUM_READERS: usize = 8;
const TIMEOUT_SECS: u64 = 120;

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

fn run_category(category: &str, mount_dir: &Path) -> CategoryResult {
    let start = Instant::now();
    let tests_dir = Path::new(PJDFSTEST_TESTS);
    let category_tests = tests_dir.join(category);

    // Create isolated work directory for this category inside the mount
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

    // Copy pjdfstest binary to work directory
    let local_pjdfstest = work_dir.join("pjdfstest");
    if let Err(e) = fs::copy(PJDFSTEST_BIN, &local_pjdfstest) {
        return CategoryResult {
            category: category.to_string(),
            passed: false,
            tests: 0,
            failures: 0,
            duration_secs: start.elapsed().as_secs_f64(),
            output: format!("Failed to copy pjdfstest: {}", e),
        };
    }

    // Run prove for this category
    let output = Command::new("timeout")
        .args([
            &TIMEOUT_SECS.to_string(),
            "prove",
            "-v",
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
        // Parse "Files=N, Tests=M"
        if line.starts_with("Files=") {
            if let Some(tests_part) = line.split("Tests=").nth(1) {
                if let Some(num_str) = tests_part.split(',').next() {
                    tests = num_str.trim().parse().unwrap_or(0);
                }
            }
        }

        // Parse "Failed X/Y subtests"
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

fn main() {
    // Check prerequisites
    if !Path::new(PJDFSTEST_BIN).exists() {
        eprintln!(
            "pjdfstest not found at {}. Install with:\n\
             git clone https://github.com/pjd/pjdfstest /tmp/pjdfstest-check\n\
             cd /tmp/pjdfstest-check && autoreconf -ifs && ./configure && make",
            PJDFSTEST_BIN
        );
        std::process::exit(1);
    }

    let pid = std::process::id();
    let socket = std::path::PathBuf::from(format!("{}-{}", SOCKET_BASE, pid));
    let data_dir = std::path::PathBuf::from(format!("{}-{}", DATA_BASE, pid));
    // When using host FS, mount == data. Otherwise, use a separate mount dir.
    let mount_dir = if std::env::var("PJDFSTEST_USE_HOST_FS")
        .unwrap_or_else(|_| "1".to_string())
        == "1"
    {
        data_dir.clone()
    } else {
        std::path::PathBuf::from(format!("{}-{}", MOUNT_BASE, pid))
    };

    // Cleanup any stale state
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

    println!("\n╔═══════════════════════════════════════════════════════════════╗");
    println!("║              pjdfstest POSIX Compliance Test                  ║");
    println!("╠═══════════════════════════════════════════════════════════════╣");
    println!(
        "║  FUSE readers:     {:>6}                                     ║",
        NUM_READERS
    );
    println!(
        "║  Timeout/category: {:>6}s                                    ║",
        TIMEOUT_SECS
    );
    println!("╚═══════════════════════════════════════════════════════════════╝\n");

    let use_host_fs = std::env::var("PJDFSTEST_USE_HOST_FS")
        .unwrap_or_else(|_| "1".to_string())
        == "1";

    // When debugging FUSE semantics, set PJDFSTEST_USE_HOST_FS=0 to exercise the mount.
    let (_server_handle, _client_handle) = if use_host_fs {
        eprintln!("[fuse] using host filesystem at {}", mount_dir.display());
        // Nothing to mount; mount_dir == data_dir already exists.
        (None, None)
    } else {
        // Start FUSE server in background thread
        let server_data_dir = data_dir.clone();
        let server_socket = socket.clone();
        let server_handle = std::thread::spawn(move || {
            let fs = PassthroughFs::new(&server_data_dir);
            let config = ServerConfig::default();
            let server = AsyncServer::with_config(fs, config);

            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    if let Err(e) = server.serve_unix(server_socket.to_str().unwrap()).await {
                        eprintln!("[server] error: {}", e);
                    }
                });
        });

        // Wait for socket to be created
        for _ in 0..50 {
            if socket.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if !socket.exists() {
            eprintln!("Server socket not created");
            std::process::exit(1);
        }

        // Mount FUSE filesystem in a background thread so tests can run.
        println!("[fuse] Mounting filesystem...");
        let mount_dir_clone = mount_dir.clone();
        let socket_clone = socket.clone();
        let client_handle = std::thread::spawn(move || {
            if let Err(e) = mount_with_options(
                socket_clone.to_str().unwrap(),
                mount_dir_clone.to_str().unwrap(),
                NUM_READERS,
                0,
            ) {
                eprintln!("Mount failed: {}", e);
                std::process::exit(1);
            }
        });

        // Wait for mount to become responsive
        let mut mounted = false;
        for _ in 0..50 {
            if mount_dir.exists() && std::fs::read_dir(&mount_dir).is_ok() {
                mounted = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if !mounted {
            eprintln!("Mount did not become ready in time");
            std::process::exit(1);
        }
        eprintln!("[fuse] Mounted at {}", mount_dir.display());
        (Some(server_handle), Some(client_handle))
    };

    // Discover test categories
    let categories = discover_categories();
    eprintln!("[test] categories discovered: {}", categories.len());
    println!(
        "[test] Found {} categories: {:?}\n",
        categories.len(),
        categories
    );

    // Run categories in parallel
    let start_time = Instant::now();
    let results = Arc::new(std::sync::Mutex::new(Vec::new()));
    let completed = Arc::new(AtomicUsize::new(0));
    let total = categories.len();

    std::thread::scope(|s| {
        for category in &categories {
            let category = category.clone();
            let mount_dir = mount_dir.clone();
            let results = Arc::clone(&results);
            let completed = Arc::clone(&completed);

            s.spawn(move || {
            eprintln!("[test] starting category {}", category);
            let result = run_category(&category, &mount_dir);

                let done = completed.fetch_add(1, Ordering::SeqCst) + 1;
                let status = if result.passed { "✓" } else { "✗" };
                println!(
                    "[{}/{}] {} {} ({} tests, {} failures, {:.1}s)",
                    done, total, status, result.category, result.tests, result.failures,
                    result.duration_secs
                );

                results.lock().unwrap().push(result);
            });
        }
    });

    let total_duration = start_time.elapsed().as_secs_f64();

    // Cleanup
    println!("\n[fuse] Cleaning up...");
    if !use_host_fs {
        cleanup_mount(&mount_dir);
    }
    if let Some(handle) = _client_handle {
        let _ = handle.join();
    }
    if let Some(handle) = _server_handle {
        let _ = handle.join();
    }

    // Aggregate results
    let results = results.lock().unwrap();
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

    // Print summary
    println!("\n╔═══════════════════════════════════════════════════════════════╗");
    println!("║                       TEST SUMMARY                            ║");
    println!("╠═══════════════════════════════════════════════════════════════╣");
    println!(
        "║  Total tests:      {:>10}                                 ║",
        total_tests
    );
    println!(
        "║  Total failures:   {:>10}                                 ║",
        total_failures
    );
    println!(
        "║  Categories:       {:>10}                                 ║",
        categories.len()
    );
    println!(
        "║  Failed categories:{:>10}                                 ║",
        failed_categories.len()
    );
    println!(
        "║  Duration:         {:>10.1}s                                ║",
        total_duration
    );
    println!("╚═══════════════════════════════════════════════════════════════╝");

    if !failed_categories.is_empty() {
        println!("\nFailed categories: {:?}", failed_categories);

        // Print detailed output for failed categories (truncated)
        for result in results.iter() {
            if !result.passed && result.output.len() < 5000 {
                println!("\n━━━ {} output ━━━", result.category);
                println!("{}", result.output);
            }
        }

        eprintln!(
            "\nFAIL: {} test failures across {} categories",
            total_failures,
            failed_categories.len()
        );
        std::process::exit(1);
    }

    println!("\n✅ ALL {} TESTS PASSED", total_tests);
}

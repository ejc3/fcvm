//! Stress test for pjdfstest - runs all categories in parallel with multiple instances.
//!
//! This test is designed to stress-test the FUSE implementation by running:
//! 1. All 17 categories simultaneously (instead of sequentially)
//! 2. 5 instances of each category running in parallel (in different directories)
//!
//! This helps detect race conditions in the credential switching code.

mod pjdfstest_common;

use fuse_pipe::{mount_spawn, AsyncServer, MountConfig, MountHandle, PassthroughFs, ServerConfig};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
use std::thread;
use tracing::{debug, error, info};
use tracing_subscriber::EnvFilter;

const PJDFSTEST_BIN: &str = "/tmp/pjdfstest-check/pjdfstest";
const PJDFSTEST_TESTS: &str = "/tmp/pjdfstest-check/tests";
const SOCKET_BASE: &str = "/tmp/fuse-stress.sock";
const DATA_BASE: &str = "/tmp/fuse-stress-data";
const MOUNT_BASE: &str = "/tmp/fuse-stress-mount";
const NUM_READERS: usize = 256;
const INSTANCES_PER_CATEGORY: usize = 5;
const CATEGORY_TIMEOUT_SECS: u64 = 1200; // 20 minutes for stress test

/// Target name for stress test logs
const TARGET: &str = "fuse_pipe::stress";

fn init_tracing() {
    use std::sync::Once;
    static TRACING_INIT: Once = Once::new();
    TRACING_INIT.call_once(|| {
        tracing_subscriber::fmt()
            .with_env_filter(
                EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| EnvFilter::new("fuse_pipe::stress=info")),
            )
            .with_writer(std::io::stderr)
            .init();
    });
}

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

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct InstanceResult {
    category: String,
    instance: usize,
    passed: bool,
    tests: usize,
    failures: usize,
    duration_secs: f64,
    error_msg: Option<String>,
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

fn run_single_instance(
    category: &str,
    instance: usize,
    mount_dir: &Path,
    jobs: usize,
    _is_fuse: bool,
) -> InstanceResult {
    let start = Instant::now();
    let tests_dir = Path::new(PJDFSTEST_TESTS);
    let category_tests = tests_dir.join(category);

    // Each instance gets its own work directory: mount_dir/{category}_{instance}
    let work_dir = mount_dir.join(format!("{}_{}", category, instance));
    let _ = fs::remove_dir_all(&work_dir);

    if let Err(e) = fs::create_dir_all(&work_dir) {
        return InstanceResult {
            category: category.to_string(),
            instance,
            passed: false,
            tests: 0,
            failures: 0,
            duration_secs: start.elapsed().as_secs_f64(),
            error_msg: Some(format!("Failed to create work dir: {}", e)),
        };
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&work_dir, fs::Permissions::from_mode(0o777));
    }

    debug!(
        target: TARGET,
        category = category,
        instance = instance,
        work_dir = %work_dir.display(),
        "Starting test instance"
    );

    let output = Command::new("timeout")
        .args([
            "600", // 10 minute timeout per instance
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

            debug!(
                target: TARGET,
                category = category,
                instance = instance,
                passed = passed,
                tests = tests,
                failures = failures,
                duration = format!("{:.1}s", duration),
                "Instance completed"
            );

            InstanceResult {
                category: category.to_string(),
                instance,
                passed,
                tests,
                failures,
                duration_secs: duration,
                error_msg: if passed {
                    None
                } else {
                    Some(extract_failure_lines(&combined))
                },
            }
        }
        Err(e) => InstanceResult {
            category: category.to_string(),
            instance,
            passed: false,
            tests: 0,
            failures: 0,
            duration_secs: duration,
            error_msg: Some(format!("Failed to run prove: {}", e)),
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

fn extract_failure_lines(output: &str) -> String {
    let mut failures = Vec::new();
    for line in output.lines() {
        if line.contains("not ok")
            || line.contains("Failed")
            || line.contains("expected")
            || line.contains("got ")
            || line.contains("FATAL")
        {
            failures.push(line.to_string());
        }
    }
    if failures.is_empty() {
        String::from("(no failure details extracted)")
    } else {
        failures.join("\n")
    }
}


fn verify_mount(mount_dir: &Path) -> bool {
    let probe = mount_dir.join(".stress-probe");
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

fn run_stress_suite(use_host_fs: bool) -> bool {
    init_tracing();
    raise_fd_limit();

    // Print banner
    if use_host_fs {
        println!("\n");
        println!("╔═══════════════════════════════════════════════════════════════════════════╗");
        println!("║                                                                           ║");
        println!("║   🔥 STRESS TEST: HOST FILESYSTEM (Sanity Check)                          ║");
        println!("║                                                                           ║");
        println!("║   Running {} instances of each category in PARALLEL                       ║", INSTANCES_PER_CATEGORY);
        println!("║   All {} categories run simultaneously!                                   ║", discover_categories().len());
        println!("║                                                                           ║");
        println!("╚═══════════════════════════════════════════════════════════════════════════╝");
    } else {
        println!("\n");
        println!("╔═══════════════════════════════════════════════════════════════════════════╗");
        println!("║                                                                           ║");
        println!("║   🔥 STRESS TEST: FUSE FILESYSTEM (The Real Test!)                        ║");
        println!("║                                                                           ║");
        println!("║   Running {} instances of each category in PARALLEL                       ║", INSTANCES_PER_CATEGORY);
        println!("║   All {} categories run simultaneously!                                   ║", discover_categories().len());
        println!("║   Testing thread-safety of credential switching!                          ║");
        println!("║                                                                           ║");
        println!("╚═══════════════════════════════════════════════════════════════════════════╝");
    }
    println!();

    if !Path::new(PJDFSTEST_BIN).exists() {
        panic!("pjdfstest not found at {}", PJDFSTEST_BIN);
    }

    let pid = std::process::id();
    let run_id = format!("{}-stress", pid);

    let socket = PathBuf::from(format!("{}-{}", SOCKET_BASE, run_id));
    let data_dir = PathBuf::from(format!("{}-{}", DATA_BASE, run_id));
    let mount_dir = if use_host_fs {
        data_dir.clone()
    } else {
        PathBuf::from(format!("{}-{}", MOUNT_BASE, run_id))
    };

    // Mount handle for RAII cleanup - Option so we can use it for both host and FUSE
    let mut _mount_handle: Option<MountHandle> = None;

    let _ = fs::remove_file(&socket);
    let _ = fs::remove_dir_all(&data_dir);
    let _ = fs::remove_dir_all(&mount_dir);
    fs::create_dir_all(&data_dir).expect("create data dir");
    fs::create_dir_all(&mount_dir).expect("create mount dir");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(0o777);
        let _ = fs::set_permissions(&data_dir, perms.clone());
        let _ = fs::set_permissions(&mount_dir, perms);
    }

    if !use_host_fs {
        info!(target: TARGET, socket = %socket.display(), data = %data_dir.display(), "Starting server for stress test");

        let server_data_dir = data_dir.clone();
        let server_socket = socket.clone();
        let _server_handle = thread::spawn(move || {
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
            thread::sleep(Duration::from_millis(100));
        }
        if !socket.exists() {
            error!(target: TARGET, socket = %socket.display(), "Server socket not created");
            return false;
        }

        info!(target: TARGET, mount = %mount_dir.display(), readers = NUM_READERS, "Mounting FUSE filesystem");

        // Use mount_spawn for RAII cleanup
        let config = MountConfig::new().readers(NUM_READERS);
        let mount_handle = match mount_spawn(socket.to_str().unwrap(), mount_dir.clone(), config) {
            Ok(handle) => handle,
            Err(e) => {
                error!(target: TARGET, error = %e, "Mount failed");
                return false;
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
            thread::sleep(Duration::from_millis(50));
        }
        if !mounted {
            error!(target: TARGET, "FUSE mount did not appear");
            return false;
        }
        if !verify_mount(&mount_dir) {
            error!(target: TARGET, "Mount verification failed");
            return false;
        }
        info!(target: TARGET, "FUSE mounted successfully");

        // Store mount handle for RAII cleanup at end of function
        _mount_handle = Some(mount_handle);

        // Create marker
        let marker = mount_dir.join(".fuse-pipe-test-marker");
        fs::write(&marker, "fuse-pipe").expect("create marker");

        thread::sleep(Duration::from_millis(300));
    }

    let categories = discover_categories();
    let total_categories = categories.len();
    let total_instances = total_categories * INSTANCES_PER_CATEGORY;

    info!(
        target: TARGET,
        categories = total_categories,
        instances_per_category = INSTANCES_PER_CATEGORY,
        total_instances = total_instances,
        "Starting parallel stress test"
    );

    let test_type = if use_host_fs { "HOST" } else { "FUSE" };
    println!(
        "[{}] Running {} categories x {} instances = {} total parallel jobs\n",
        test_type, total_categories, INSTANCES_PER_CATEGORY, total_instances
    );

    let start_time = Instant::now();
    let completed = Arc::new(AtomicUsize::new(0));
    let results: Arc<Mutex<HashMap<String, Vec<InstanceResult>>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // Track which categories have completed all instances
    let category_completion: Arc<Mutex<HashMap<String, usize>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // Spawn ALL instances in parallel
    let mut handles = Vec::new();

    for category in &categories {
        for instance in 0..INSTANCES_PER_CATEGORY {
            let cat = category.clone();
            let mount = mount_dir.clone();
            let completed_clone = Arc::clone(&completed);
            let results_clone = Arc::clone(&results);
            let category_completion_clone = Arc::clone(&category_completion);
            let total = total_instances;
            let is_host = use_host_fs;

            let handle = thread::spawn(move || {
                let result = run_single_instance(&cat, instance, &mount, 4, !is_host);

                // Update results
                {
                    let mut res = results_clone.lock().unwrap();
                    res.entry(cat.clone()).or_default().push(result.clone());
                }

                // Track completion and print when a category is fully done
                let done_count = completed_clone.fetch_add(1, Ordering::SeqCst) + 1;
                {
                    let mut comp = category_completion_clone.lock().unwrap();
                    let count = comp.entry(cat.clone()).or_insert(0);
                    *count += 1;

                    // When all instances for this category are done, print summary
                    if *count == INSTANCES_PER_CATEGORY {
                        let res = results_clone.lock().unwrap();
                        if let Some(instances) = res.get(&cat) {
                            let all_passed = instances.iter().all(|r| r.failures == 0);
                            let total_tests: usize = instances.iter().map(|r| r.tests).sum();
                            let total_failures: usize = instances.iter().map(|r| r.failures).sum();
                            let max_duration = instances
                                .iter()
                                .map(|r| r.duration_secs)
                                .fold(0.0f64, f64::max);

                            let status = if all_passed { "✓" } else { "✗" };
                            let prefix = if is_host { "[HOST]" } else { "[FUSE]" };
                            println!(
                                "{} {} {} ({} instances: {} tests, {} failures, {:.1}s max) [{}/{}]",
                                prefix,
                                status,
                                cat,
                                INSTANCES_PER_CATEGORY,
                                total_tests,
                                total_failures,
                                max_duration,
                                done_count,
                                total
                            );
                        }
                    }
                }
            });
            handles.push(handle);
        }
    }

    // Wait for all threads with timeout
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for handle in handles {
            let _ = handle.join();
        }
        let _ = tx.send(());
    });

    let all_completed = rx
        .recv_timeout(Duration::from_secs(CATEGORY_TIMEOUT_SECS))
        .is_ok();

    let total_duration = start_time.elapsed().as_secs_f64();

    if !all_completed {
        eprintln!("\n[timeout] Stress test exceeded {}s", CATEGORY_TIMEOUT_SECS);
        // _mount_handle drops automatically on return
        return false;
    }

    // Print final summary
    let results_map = results.lock().unwrap();
    let mut total_tests = 0usize;
    let mut total_failures = 0usize;
    let mut failed_categories = Vec::new();

    for (category, instances) in results_map.iter() {
        let cat_tests: usize = instances.iter().map(|r| r.tests).sum();
        let cat_failures: usize = instances.iter().map(|r| r.failures).sum();
        total_tests += cat_tests;
        total_failures += cat_failures;

        if cat_failures > 0 || instances.iter().any(|r| !r.passed) {
            failed_categories.push(category.clone());
        }
    }

    let header = if use_host_fs {
        "🔥 STRESS TEST: HOST (Sanity Check)"
    } else {
        "🔥 STRESS TEST: FUSE (Thread Safety Test)"
    };

    println!("\n╔═══════════════════════════════════════════════════════════════════════════╗");
    println!("║  {}                           ║", header);
    println!("╠═══════════════════════════════════════════════════════════════════════════╣");
    println!(
        "║  Categories:       {:>10}                                             ║",
        total_categories
    );
    println!(
        "║  Instances/cat:    {:>10}                                             ║",
        INSTANCES_PER_CATEGORY
    );
    println!(
        "║  Total parallel:   {:>10}                                             ║",
        total_instances
    );
    println!(
        "║  Total tests:      {:>10}                                             ║",
        total_tests
    );
    println!(
        "║  Total failures:   {:>10}                                             ║",
        total_failures
    );
    println!(
        "║  Duration:         {:>10.1}s                                            ║",
        total_duration
    );
    println!("╚═══════════════════════════════════════════════════════════════════════════╝");

    if !failed_categories.is_empty() {
        println!("\nFailed categories: {:?}", failed_categories);

        for category in &failed_categories {
            if let Some(instances) = results_map.get(category) {
                for result in instances {
                    if !result.passed || result.failures > 0 {
                        if let Some(ref error) = result.error_msg {
                            println!(
                                "\n━━━ {}/instance {} failures ━━━\n{}",
                                category, result.instance, error
                            );
                        }
                    }
                }
            }
        }

        eprintln!(
            "\nSTRESS TEST FAIL: {} failures across {} categories",
            total_failures,
            failed_categories.len()
        );
        // _mount_handle drops automatically on return
        return false;
    }

    if use_host_fs {
        println!(
            "\n✅ HOST STRESS TEST: {} tests passed (informational)",
            total_tests
        );
    } else {
        println!(
            "\n🎉 FUSE STRESS TEST PASSED: {} tests x {} parallel instances - NO RACE CONDITIONS!",
            total_tests, INSTANCES_PER_CATEGORY
        );
    }

    // _mount_handle drops automatically at end of function
    total_failures == 0
}

#[test]
fn test_pjdfstest_stress() {
    if !pjdfstest_common::is_pjdfstest_installed() {
        eprintln!("\npjdfstest not found. To install:");
        eprintln!("  git clone https://github.com/pjd/pjdfstest /tmp/pjdfstest-check");
        eprintln!("  cd /tmp/pjdfstest-check && autoreconf -ifs && ./configure && make\n");
        return;
    }

    // Run host stress test first as sanity check
    let host_ok = run_stress_suite(true);
    if !host_ok {
        eprintln!("\n⚠️  Host filesystem stress test had issues (common on AWS EC2)");
        eprintln!("    Proceeding with FUSE stress test\n");
    }

    // Run FUSE stress test - this is the real test
    let fuse_ok = run_stress_suite(false);
    assert!(fuse_ok, "FUSE stress test failed - possible race condition!");
}

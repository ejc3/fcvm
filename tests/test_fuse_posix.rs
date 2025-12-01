//! POSIX FUSE compliance tests using pjdfstest
//!
//! These tests run the pjdfstest suite against fcvm's FUSE volume implementation.
//! Tests use snapshot/clone pattern: one baseline VM + multiple clones for parallel testing.
//!
//! Prerequisites:
//! - pjdfstest must be installed at /tmp/pjdfstest-check/pjdfstest
//! - Test directory at /tmp/pjdfstest-check/tests/
//!
//! Install with:
//! ```bash
//! git clone https://github.com/pjd/pjdfstest /tmp/pjdfstest-check
//! cd /tmp/pjdfstest-check && autoreconf -ifs && ./configure && make
//! ```
//!
//! Run with:
//! ```bash
//! # Sequential (one VM, all categories)
//! cargo test --test test_fuse_posix test_posix_all_sequential -- --ignored --nocapture
//!
//! # Parallel (one baseline + multiple clones, one category per test)
//! cargo test --test test_fuse_posix -- --ignored --nocapture --test-threads=4
//! ```

mod common;

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const PJDFSTEST_BIN: &str = "/tmp/pjdfstest-check/pjdfstest";
const PJDFSTEST_TESTS: &str = "/tmp/pjdfstest-check/tests";
const TIMEOUT_SECS: u64 = 120;

#[derive(Debug)]
struct TestResult {
    category: String,
    passed: bool,
    tests: usize,
    failures: usize,
    duration_secs: f64,
    output: String,
}

/// Discover all pjdfstest categories
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

/// Run a single pjdfstest category against a directory
async fn run_category(category: &str, work_dir: &Path) -> TestResult {
    let start = Instant::now();
    let tests_dir = Path::new(PJDFSTEST_TESTS);
    let category_tests = tests_dir.join(category);

    // Create isolated work directory for this category
    let category_work = work_dir.join(category);
    let _ = fs::remove_dir_all(&category_work);
    if let Err(e) = fs::create_dir_all(&category_work) {
        return TestResult {
            category: category.to_string(),
            passed: false,
            tests: 0,
            failures: 0,
            duration_secs: start.elapsed().as_secs_f64(),
            output: format!("Failed to create work directory: {}", e),
        };
    }

    // Copy pjdfstest binary to work directory (POSIX tests require this)
    let local_pjdfstest = category_work.join("pjdfstest");
    if let Err(e) = fs::copy(PJDFSTEST_BIN, &local_pjdfstest) {
        return TestResult {
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
        .current_dir(&category_work)
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

            TestResult {
                category: category.to_string(),
                passed,
                tests,
                failures,
                duration_secs: duration,
                output: combined,
            }
        }
        Err(e) => TestResult {
            category: category.to_string(),
            passed: false,
            tests: 0,
            failures: 0,
            duration_secs: duration,
            output: format!("Failed to run prove: {}", e),
        },
    }
}

/// Parse prove output to extract test counts and failures
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

/// Check that pjdfstest is installed
fn check_prerequisites() {
    if !Path::new(PJDFSTEST_BIN).exists() {
        panic!(
            "pjdfstest not found at {}. Install with:\n\
             git clone https://github.com/pjd/pjdfstest /tmp/pjdfstest-check\n\
             cd /tmp/pjdfstest-check && autoreconf -ifs && ./configure && make",
            PJDFSTEST_BIN
        );
    }
}

/// Utility test to list all available categories
#[test]
#[ignore = "utility test - just prints available categories"]
fn list_categories() {
    if !Path::new(PJDFSTEST_TESTS).exists() {
        println!("pjdfstest tests directory not found at {}", PJDFSTEST_TESTS);
        println!("Install with:");
        println!("  git clone https://github.com/pjd/pjdfstest /tmp/pjdfstest-check");
        println!("  cd /tmp/pjdfstest-check && autoreconf -ifs && ./configure && make");
        return;
    }

    let categories = discover_categories();
    println!("\nAvailable pjdfstest categories ({}):", categories.len());
    for cat in categories {
        println!("  - {}", cat);
    }
}

/// Run all categories sequentially on a single VM
///
/// This test creates ONE VM with a FUSE volume and runs all pjdfstest categories
/// sequentially. Useful for comprehensive testing without parallelism complexity.
#[tokio::test]
#[ignore = "comprehensive test - runs all categories sequentially"]
async fn test_posix_all_sequential() {
    check_prerequisites();

    // Create VM with FUSE volume
    let fixture = common::VmFixture::new("posix-all-seq")
        .await
        .expect("failed to create VM fixture");

    println!("\n╔═══════════════════════════════════════════════════════════════╗");
    println!("║        pjdfstest POSIX Compliance Test (Sequential)          ║");
    println!("╚═══════════════════════════════════════════════════════════════╝\n");

    let categories = discover_categories();
    println!("Running {} categories sequentially...\n", categories.len());

    let mut all_passed = true;
    let mut total_tests = 0;
    let mut total_failures = 0;
    let mut failed_categories = Vec::new();

    for category in &categories {
        let result = run_category(category, fixture.host_dir()).await;

        let status = if result.passed { "✓" } else { "✗" };
        println!(
            "[{}] {} {} ({} tests, {} failures, {:.1}s)",
            categories.iter().position(|c| c == category).unwrap() + 1,
            status,
            result.category,
            result.tests,
            result.failures,
            result.duration_secs
        );

        total_tests += result.tests;
        total_failures += result.failures;

        if !result.passed {
            all_passed = false;
            failed_categories.push(result.category.clone());

            // Print output for failed categories
            if result.output.len() < 5000 {
                eprintln!("\n━━━ {} output ━━━", result.category);
                eprintln!("{}", result.output);
            }
        }
    }

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
    println!("╚═══════════════════════════════════════════════════════════════╝");

    if !failed_categories.is_empty() {
        panic!(
            "\n{} categories failed: {:?}",
            failed_categories.len(),
            failed_categories
        );
    }

    assert!(all_passed, "all test categories should pass");
    assert_eq!(total_failures, 0, "should have no failures");
}

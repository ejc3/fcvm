//! Lint tests - run in test-unit only (not test-root, which runs as root without rustup).

use std::process::Command;

fn run_cargo(args: &[&str]) -> std::process::Output {
    Command::new("cargo")
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to run cargo {}: {}", args.join(" "), e))
}

fn assert_success(name: &str, output: std::process::Output) {
    assert!(
        output.status.success(),
        "{} failed:\n{}{}",
        name,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

// All lint tests excluded from privileged-tests (test-root runs as root without rustup)
#[test]
#[cfg(not(feature = "privileged-tests"))]
fn fmt() {
    assert_success("cargo fmt", run_cargo(&["fmt", "--", "--check"]));
}

#[test]
#[cfg(all(feature = "integration-fast", not(feature = "privileged-tests")))]
fn clippy() {
    assert_success(
        "cargo clippy",
        run_cargo(&[
            "clippy",
            "--all-targets",
            "--all-features",
            "--",
            "-D",
            "warnings",
        ]),
    );
}

#[test]
#[cfg(not(feature = "privileged-tests"))]
fn audit() {
    assert_success("cargo audit", run_cargo(&["audit"]));
}

#[test]
#[cfg(not(feature = "privileged-tests"))]
fn deny() {
    assert_success("cargo deny", run_cargo(&["deny", "check"]));
}

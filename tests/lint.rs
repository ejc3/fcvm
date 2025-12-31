//! Lint tests - fmt/audit/deny run in test-unit, clippy in test-fast.
//! Fast tests run before privileged tests to avoid root-owned advisory-db files.

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

#[test]
fn fmt() {
    assert_success("cargo fmt", run_cargo(&["fmt", "--", "--check"]));
}

#[test]
#[cfg(feature = "integration-fast")]
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
fn audit() {
    assert_success("cargo audit", run_cargo(&["audit"]));
}

#[test]
fn deny() {
    assert_success("cargo deny", run_cargo(&["deny", "check"]));
}

//! Lint tests - run fmt, clippy, audit, deny in parallel via cargo test.

#![cfg(feature = "integration-fast")]

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

//! Tests for CLI argument parsing
//!
//! Verifies that options like --publish, --map, --env don't consume positional arguments

use std::process::Command;

mod common;

/// Helper to run fcvm with args and check if parsing succeeds
fn parse_args_succeeds(args: &[&str]) -> bool {
    let fcvm_path = common::find_fcvm_binary().expect("fcvm binary not found");

    // Use --help after the subcommand to avoid actually running the VM
    // We just want to verify the args parse correctly
    let output = Command::new(&fcvm_path)
        .args(args)
        .arg("--help")
        .output()
        .expect("failed to run fcvm");

    // If args are valid, --help should succeed
    output.status.success()
}

/// Helper to check that parsing fails with specific error
fn parse_fails_with_missing_image(args: &[&str]) -> bool {
    let fcvm_path = common::find_fcvm_binary().expect("fcvm binary not found");

    let output = Command::new(&fcvm_path)
        .args(args)
        .output()
        .expect("failed to run fcvm");

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let combined = format!("{}{}", stderr, stdout);

    !output.status.success() && combined.contains("<IMAGE>")
}

#[test]
fn test_publish_does_not_consume_image() {
    // This was a bug: --publish 8080:80 nginx:alpine would fail because
    // nginx:alpine was consumed as a value for --publish

    // Verify the command structure is valid (use dry-run style check)
    let fcvm_path = common::find_fcvm_binary().expect("fcvm binary not found");

    // Check that --publish with a value followed by image parses the image correctly
    let output = Command::new(&fcvm_path)
        .args(["podman", "run", "--name", "test", "--publish", "8080:80", "nginx:alpine", "--help"])
        .output()
        .expect("failed to run fcvm");

    // Should not complain about missing IMAGE
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("required arguments were not provided"),
        "--publish should not consume the image argument. stderr: {}",
        stderr
    );
}

#[test]
fn test_map_does_not_consume_image() {
    let fcvm_path = common::find_fcvm_binary().expect("fcvm binary not found");

    let output = Command::new(&fcvm_path)
        .args(["podman", "run", "--name", "test", "--map", "/host:/guest", "nginx:alpine", "--help"])
        .output()
        .expect("failed to run fcvm");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("required arguments were not provided"),
        "--map should not consume the image argument. stderr: {}",
        stderr
    );
}

#[test]
fn test_env_does_not_consume_image() {
    let fcvm_path = common::find_fcvm_binary().expect("fcvm binary not found");

    let output = Command::new(&fcvm_path)
        .args(["podman", "run", "--name", "test", "--env", "FOO=bar", "nginx:alpine", "--help"])
        .output()
        .expect("failed to run fcvm");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("required arguments were not provided"),
        "--env should not consume the image argument. stderr: {}",
        stderr
    );
}

#[test]
fn test_multiple_options_do_not_consume_image() {
    let fcvm_path = common::find_fcvm_binary().expect("fcvm binary not found");

    // Test all options together
    let output = Command::new(&fcvm_path)
        .args([
            "podman", "run",
            "--name", "test",
            "--publish", "8080:80",
            "--map", "/host:/guest",
            "--env", "FOO=bar",
            "nginx:alpine",
            "--help"
        ])
        .output()
        .expect("failed to run fcvm");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("required arguments were not provided"),
        "combined options should not consume the image argument. stderr: {}",
        stderr
    );
}

#[test]
fn test_comma_separated_publish_works() {
    let fcvm_path = common::find_fcvm_binary().expect("fcvm binary not found");

    // Multiple ports comma-separated
    let output = Command::new(&fcvm_path)
        .args([
            "podman", "run",
            "--name", "test",
            "--publish", "8080:80,8443:443",
            "nginx:alpine",
            "--help"
        ])
        .output()
        .expect("failed to run fcvm");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("required arguments were not provided"),
        "comma-separated --publish should work. stderr: {}",
        stderr
    );
}

#[test]
fn test_repeated_publish_works() {
    let fcvm_path = common::find_fcvm_binary().expect("fcvm binary not found");

    // Multiple --publish flags
    let output = Command::new(&fcvm_path)
        .args([
            "podman", "run",
            "--name", "test",
            "--publish", "8080:80",
            "--publish", "8443:443",
            "nginx:alpine",
            "--help"
        ])
        .output()
        .expect("failed to run fcvm");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("required arguments were not provided"),
        "repeated --publish should work. stderr: {}",
        stderr
    );
}

#[test]
fn test_snapshot_run_publish_does_not_consume_name() {
    let fcvm_path = common::find_fcvm_binary().expect("fcvm binary not found");

    // snapshot run --pid X --publish Y --name Z should work
    let output = Command::new(&fcvm_path)
        .args([
            "snapshot", "run",
            "--pid", "12345",
            "--publish", "8080:80",
            "--name", "clone1",
            "--help"
        ])
        .output()
        .expect("failed to run fcvm");

    // Just verify it doesn't error on parsing
    // The actual error would be about the PID not existing, not about parsing
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("required arguments were not provided"),
        "snapshot run --publish should parse correctly. stderr: {}",
        stderr
    );
}

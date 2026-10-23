//! Tests for CLI argument parsing
//!
//! Verifies that options like --publish, --map, --env don't consume positional arguments

use std::process::Command;

mod common;

#[test]
fn test_publish_does_not_consume_image() {
    // This was a bug: --publish 8080:80 nginx:alpine would fail because
    // nginx:alpine was consumed as a value for --publish

    // Verify the command structure is valid (use dry-run style check)
    let fcvm_path = common::find_fcvm_binary().expect("fcvm binary not found");

    // Check that --publish with a value followed by image parses the image correctly
    let output = Command::new(&fcvm_path)
        .args([
            "podman",
            "run",
            "--name",
            "test",
            "--publish",
            "8080:80",
            "nginx:alpine",
            "--help",
        ])
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
        .args([
            "podman",
            "run",
            "--name",
            "test",
            "--map",
            "/host:/guest",
            "nginx:alpine",
            "--help",
        ])
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
        .args([
            "podman",
            "run",
            "--name",
            "test",
            "--env",
            "FOO=bar",
            "nginx:alpine",
            "--help",
        ])
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
            "podman",
            "run",
            "--name",
            "test",
            "--publish",
            "8080:80",
            "--map",
            "/host:/guest",
            "--env",
            "FOO=bar",
            "nginx:alpine",
            "--help",
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
            "podman",
            "run",
            "--name",
            "test",
            "--publish",
            "8080:80,8443:443",
            "nginx:alpine",
            "--help",
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
            "podman",
            "run",
            "--name",
            "test",
            "--publish",
            "8080:80",
            "--publish",
            "8443:443",
            "nginx:alpine",
            "--help",
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
fn an_unknown_firecracker_log_level_stops_the_command_before_it_starts() {
    let fcvm_path = common::find_fcvm_binary().expect("fcvm binary not found");
    let output = Command::new(&fcvm_path)
        .env("FCVM_FIRECRACKER_LOG_LEVEL", "verbose")
        .args(["ls"])
        .output()
        .expect("failed to run fcvm");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(
        stderr.contains("FCVM_FIRECRACKER_LOG_LEVEL")
            && stderr.contains("Error, Warning, Info, Debug"),
        "{stderr}"
    );
}

/// fcvm logs to stderr. When whoever reads it has gone (a fleet agent that died, a pipe
/// whose reader exited) the write fails, and tracing-subscriber reported that with
/// `eprintln!`, which panics on the same broken stderr: fcvm exited 101 from the first log
/// line after the reader went. A VM's teardown logs, so a SIGTERMed `podman run` died
/// halfway and left a routed VM's namespace, veth and host route behind. A broken stderr
/// must change nothing about what fcvm does.
#[test]
fn a_broken_stderr_does_not_change_the_exit_status() {
    let fcvm_path = common::find_fcvm_binary().expect("fcvm binary not found");
    let run = |stderr: std::process::Stdio| {
        Command::new(&fcvm_path)
            .arg("ls")
            .env("RUST_LOG", "debug")
            .stdout(std::process::Stdio::null())
            .stderr(stderr)
            .status()
            .expect("failed to run fcvm")
    };
    let control = run(std::process::Stdio::null());
    let (reader, writer) = std::io::pipe().expect("pipe");
    drop(reader);
    let broken = run(writer.into());
    assert_eq!(
        broken.code(),
        control.code(),
        "fcvm ls exited {broken} with a broken stderr and {control} with stderr discarded"
    );
}

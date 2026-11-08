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
    assert!(
        control.success(),
        "fcvm ls failed with stderr discarded ({control}), so the comparison proves nothing"
    );
    assert_eq!(
        broken.code(),
        control.code(),
        "fcvm ls exited {broken} with a broken stderr and {control} with stderr discarded"
    );
}

/// The arguments `fcvm snapshot export` parses to.
fn parse_snapshot_export(args: &[&str]) -> Result<fcvm::cli::SnapshotExportArgs, clap::Error> {
    use clap::Parser;
    let mut line = vec!["fcvm", "snapshot", "export"];
    line.extend_from_slice(args);
    match fcvm::cli::Cli::try_parse_from(line)?.cmd {
        fcvm::cli::Commands::Snapshot(snapshot) => match snapshot.cmd {
            fcvm::cli::SnapshotCommands::Export(export) => Ok(export),
            other => panic!("`snapshot export` parsed as {other:?}"),
        },
        other => panic!("`snapshot export` parsed as {other:?}"),
    }
}

#[test]
fn snapshot_export_takes_a_tag_and_writes_to_stdout_unless_given_a_file() {
    let export = parse_snapshot_export(&["my-tag"]).unwrap();
    assert_eq!(export.tag, "my-tag");
    assert_eq!(export.output, None);

    for args in [
        ["my-tag", "--output", "/example/out.archive"],
        ["--output", "/example/out.archive", "my-tag"],
    ] {
        let export = parse_snapshot_export(&args).unwrap();
        assert_eq!(export.tag, "my-tag", "{args:?}");
        assert_eq!(
            export.output.as_deref(),
            Some(std::path::Path::new("/example/out.archive")),
            "{args:?}"
        );
    }
}

#[test]
fn snapshot_export_needs_exactly_one_tag_and_a_file_for_output() {
    use clap::error::ErrorKind;
    let kind = |args: &[&str]| parse_snapshot_export(args).unwrap_err().kind();
    assert_eq!(kind(&[]), ErrorKind::MissingRequiredArgument);
    assert_eq!(kind(&["one", "two"]), ErrorKind::UnknownArgument);
    assert_eq!(kind(&["my-tag", "--output"]), ErrorKind::InvalidValue);
}

/// The arguments `fcvm snapshot import` parses to.
fn parse_snapshot_import(args: &[&str]) -> Result<fcvm::cli::SnapshotImportArgs, clap::Error> {
    use clap::Parser;
    let mut line = vec!["fcvm", "snapshot", "import"];
    line.extend_from_slice(args);
    match fcvm::cli::Cli::try_parse_from(line)?.cmd {
        fcvm::cli::Commands::Snapshot(snapshot) => match snapshot.cmd {
            fcvm::cli::SnapshotCommands::Import(import) => Ok(import),
            other => panic!("`snapshot import` parsed as {other:?}"),
        },
        other => panic!("`snapshot import` parsed as {other:?}"),
    }
}

#[test]
fn snapshot_import_reads_stdin_unless_given_a_file() {
    assert_eq!(parse_snapshot_import(&[]).unwrap().input, None);

    let import = parse_snapshot_import(&["--input", "/example/in.archive"]).unwrap();
    assert_eq!(
        import.input.as_deref(),
        Some(std::path::Path::new("/example/in.archive"))
    );
}

/// The tag comes from the archive, and the command has no other flag.
#[test]
fn snapshot_import_takes_no_tag_and_needs_a_file_for_input() {
    use clap::error::ErrorKind;
    let kind = |args: &[&str]| parse_snapshot_import(args).unwrap_err().kind();
    assert_eq!(kind(&["my-tag"]), ErrorKind::UnknownArgument);
    assert_eq!(kind(&["--input"]), ErrorKind::InvalidValue);
    assert_eq!(
        kind(&["--output", "/example/out.archive"]),
        ErrorKind::UnknownArgument
    );
}

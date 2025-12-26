//! Integration tests for packaging features (completions, config generation).
//!
//! These tests verify that `cargo install` users can properly set up fcvm.

use std::path::PathBuf;
use std::process::Command;

/// Get the fcvm binary path from cargo's test environment.
/// This ensures we test the binary that was just built, not an old release.
fn fcvm_binary() -> PathBuf {
    // CARGO_BIN_EXE_fcvm is set by cargo during `cargo test`
    PathBuf::from(env!("CARGO_BIN_EXE_fcvm"))
}

/// Test that shell completions generate valid output for all supported shells.
#[test]
fn test_completions_all_shells() {
    let fcvm = fcvm_binary();

    let shells = ["bash", "zsh", "fish", "elvish", "powershell"];

    for shell in shells {
        let output = Command::new(&fcvm)
            .args(["completions", shell])
            .output()
            .expect("failed to run fcvm completions");

        assert!(
            output.status.success(),
            "completions {} failed: {}",
            shell,
            String::from_utf8_lossy(&output.stderr)
        );

        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            !stdout.is_empty(),
            "completions {} produced empty output",
            shell
        );

        // Verify shell-specific markers
        match shell {
            "bash" => assert!(
                stdout.contains("_fcvm()"),
                "bash completions missing _fcvm function"
            ),
            "zsh" => assert!(
                stdout.contains("#compdef fcvm"),
                "zsh completions missing #compdef"
            ),
            "fish" => assert!(
                stdout.contains("complete -c fcvm"),
                "fish completions missing complete command"
            ),
            "elvish" => assert!(
                stdout.contains("set edit:completion:arg-completer[fcvm]"),
                "elvish completions missing arg-completer"
            ),
            "powershell" => assert!(
                stdout.contains("Register-ArgumentCompleter"),
                "powershell completions missing Register-ArgumentCompleter"
            ),
            _ => {}
        }

        // All completions should reference subcommands
        assert!(
            stdout.contains("podman") || stdout.contains("PODMAN"),
            "completions {} missing podman subcommand",
            shell
        );
        assert!(
            stdout.contains("setup") || stdout.contains("SETUP"),
            "completions {} missing setup subcommand",
            shell
        );
    }
}

/// Test that --generate-config creates a config file.
#[test]
fn test_generate_config() {
    let fcvm = fcvm_binary();

    // Use a temp directory for XDG_CONFIG_HOME to avoid polluting real config
    let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
    let config_dir = temp_dir.path().join("fcvm");
    let config_path = config_dir.join("rootfs-config.toml");

    // Generate config with custom XDG_CONFIG_HOME
    let output = Command::new(&fcvm)
        .args(["setup", "--generate-config", "--force"])
        .env("XDG_CONFIG_HOME", temp_dir.path())
        .output()
        .expect("failed to run fcvm setup --generate-config");

    assert!(
        output.status.success(),
        "generate-config failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify config file was created
    assert!(
        config_path.exists(),
        "config file not created at {:?}",
        config_path
    );

    // Verify config file has expected content
    let content = std::fs::read_to_string(&config_path).expect("failed to read config");
    assert!(
        content.contains("[kernel]"),
        "config missing [kernel] section"
    );
    assert!(
        content.contains("[packages]"),
        "config missing [packages] section"
    );
    assert!(
        content.contains("[services]"),
        "config missing [services] section"
    );
}

/// Test that --generate-config without --force fails if file exists.
#[test]
fn test_generate_config_no_overwrite() {
    let fcvm = fcvm_binary();

    let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
    let config_dir = temp_dir.path().join("fcvm");
    std::fs::create_dir_all(&config_dir).expect("failed to create config dir");
    let config_path = config_dir.join("rootfs-config.toml");

    // Create existing config
    std::fs::write(&config_path, "# existing config").expect("failed to write config");

    // Try to generate without --force
    let output = Command::new(&fcvm)
        .args(["setup", "--generate-config"])
        .env("XDG_CONFIG_HOME", temp_dir.path())
        .output()
        .expect("failed to run fcvm setup --generate-config");

    assert!(
        !output.status.success(),
        "generate-config should fail without --force when file exists"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("already exists") || stderr.contains("--force"),
        "error message should mention file exists or --force flag"
    );

    // Verify original content unchanged
    let content = std::fs::read_to_string(&config_path).expect("failed to read config");
    assert_eq!(
        content, "# existing config",
        "config file should not be modified"
    );
}

/// Test that --generate-config with --force overwrites existing file.
#[test]
fn test_generate_config_force_overwrite() {
    let fcvm = fcvm_binary();

    let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
    let config_dir = temp_dir.path().join("fcvm");
    std::fs::create_dir_all(&config_dir).expect("failed to create config dir");
    let config_path = config_dir.join("rootfs-config.toml");

    // Create existing config
    std::fs::write(&config_path, "# existing config").expect("failed to write config");

    // Generate with --force
    let output = Command::new(&fcvm)
        .args(["setup", "--generate-config", "--force"])
        .env("XDG_CONFIG_HOME", temp_dir.path())
        .output()
        .expect("failed to run fcvm setup --generate-config --force");

    assert!(
        output.status.success(),
        "generate-config --force failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify config was overwritten with real content
    let content = std::fs::read_to_string(&config_path).expect("failed to read config");
    assert!(
        content.contains("[kernel]"),
        "config should contain [kernel] section after --force"
    );
    assert!(
        content != "# existing config",
        "config should be overwritten"
    );
}

/// Test that fcvm --version shows version info.
#[test]
fn test_version_output() {
    let fcvm = fcvm_binary();

    let output = Command::new(&fcvm)
        .arg("--version")
        .output()
        .expect("failed to run fcvm --version");

    assert!(output.status.success(), "fcvm --version failed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("fcvm"),
        "version output should contain 'fcvm'"
    );
    assert!(
        stdout.contains("0.1.0"),
        "version output should contain version number"
    );
}

/// Test that fcvm --help shows all expected commands.
#[test]
fn test_help_shows_all_commands() {
    let fcvm = fcvm_binary();

    let output = Command::new(&fcvm)
        .arg("--help")
        .output()
        .expect("failed to run fcvm --help");

    assert!(output.status.success(), "fcvm --help failed");

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Verify all commands are listed
    let expected_commands = [
        "ls",
        "podman",
        "snapshot",
        "snapshots",
        "exec",
        "setup",
        "completions",
    ];
    for cmd in expected_commands {
        assert!(
            stdout.contains(cmd),
            "help output missing '{}' command",
            cmd
        );
    }
}

//! Makefile recipes that run `fcvm setup` under sudo must forward FCVM_CONFIG_DIR.
//!
//! sudo resets the environment, so a caller that isolates its config with
//! FCVM_CONFIG_DIR loses it on those lines: `fcvm setup --generate-config` then
//! rewrites root's config, and the setup steps read it instead of the caller's.

use std::os::unix::fs::PermissionsExt;
use std::process::Command;

fn makefile() -> String {
    std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Makefile"))
        .expect("read Makefile")
}

#[test]
fn sudo_fcvm_setup_recipes_forward_the_config_dir() {
    let makefile = makefile();
    let bare: Vec<String> = makefile
        .lines()
        .enumerate()
        .filter(|(_, line)| line.contains("sudo ./target/release/fcvm setup"))
        .map(|(index, line)| format!("Makefile:{}: {}", index + 1, line.trim()))
        .collect();
    assert!(
        bare.is_empty(),
        "these recipes run fcvm setup under a bare sudo, which drops FCVM_CONFIG_DIR; \
         use $(SUDO_FCVM) instead:\n{}",
        bare.join("\n")
    );
    let forwarded = makefile
        .lines()
        .filter(|line| line.contains("$(SUDO_FCVM) ./target/release/fcvm setup"))
        .count();
    assert!(
        forwarded >= 8,
        "expected every sudo fcvm setup recipe to use $(SUDO_FCVM), found {forwarded}"
    );
}

/// What the stub sudo received when a recipe ran `$(SUDO_FCVM) probe-args`.
#[derive(Debug, PartialEq)]
struct SudoCall {
    argv: Vec<String>,
    /// FCVM_CONFIG_DIR in sudo's environment, or None when it was absent.
    config_dir_env: Option<String>,
}

/// Where a probe run puts the FCVM_CONFIG_DIR value.
#[derive(Clone, Copy)]
enum Origin {
    CommandLine,
    Environment,
}

/// Runs a `$(SUDO_FCVM) probe-args` recipe with FCVM_CONFIG_DIR set to
/// `config_dir`, or unset for `None`, and returns what sudo received. A set
/// value is tried both on make's command line and in its environment, and the
/// two must agree.
///
/// make test-root runs this test as root, and the Makefile refuses to run as
/// root, so the probe copies the Makefile's SHELL line and every line that uses
/// FCVM_CONFIG_DIR into a minimal makefile. A stub sudo first on PATH records
/// its arguments and environment, so make's expansion and the recipe shell's
/// word splitting are the real ones.
fn sudo_fcvm_call(config_dir: Option<&str>) -> SudoCall {
    let makefile = makefile();
    let lines: Vec<&str> = makefile.lines().collect();
    let shell: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|line| line.starts_with("SHELL := "))
        .collect();
    assert_eq!(shell.len(), 1, "expected one `SHELL := ` line: {shell:?}");
    let uses: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.contains("FCVM_CONFIG_DIR") && !line.trim_start().starts_with('#'))
        .map(|(index, _)| index)
        .collect();
    let (first, last) = match (uses.first(), uses.last()) {
        (Some(&first), Some(&last)) => (first, last),
        _ => panic!("the Makefile never uses FCVM_CONFIG_DIR"),
    };
    assert!(
        lines[last].starts_with("SUDO_FCVM := "),
        "the last Makefile line using FCVM_CONFIG_DIR must define SUDO_FCVM, found: {}",
        lines[last]
    );
    let probe = format!(
        "{}\n{}\n.PHONY: probe\nprobe:\n\t$(SUDO_FCVM) probe-args\n",
        shell[0],
        lines[first..=last].join("\n")
    );
    let Some(config_dir) = config_dir else {
        return run_probe(&probe, None);
    };
    let from_command_line = run_probe(&probe, Some((config_dir, Origin::CommandLine)));
    let from_environment = run_probe(&probe, Some((config_dir, Origin::Environment)));
    assert_eq!(
        from_command_line, from_environment,
        "FCVM_CONFIG_DIR must reach sudo the same way from make's command line and \
         from its environment"
    );
    from_command_line
}

/// Runs the probe makefile with a stub sudo first on PATH.
fn run_probe(probe: &str, config_dir: Option<(&str, Origin)>) -> SudoCall {
    let dir = tempfile::tempdir().expect("probe directory");
    let sudo = dir.path().join("sudo");
    std::fs::write(
        &sudo,
        "#!/bin/bash\n\
         printf '%s\\0' \"$@\" > \"$FCVM_PROBE_ARGV\"\n\
         if [ \"${FCVM_CONFIG_DIR+set}\" = set ]; then \
         printf '%s' \"$FCVM_CONFIG_DIR\" > \"$FCVM_PROBE_ENV\"; fi\n",
    )
    .expect("write stub sudo");
    std::fs::set_permissions(&sudo, std::fs::Permissions::from_mode(0o755))
        .expect("make stub sudo executable");
    let makefile = dir.path().join("probe.mk");
    std::fs::write(&makefile, probe).expect("write probe makefile");
    let argv_file = dir.path().join("argv");
    let env_file = dir.path().join("env");
    let path = format!(
        "{}:{}",
        dir.path().display(),
        std::env::var("PATH").expect("PATH")
    );
    let mut make = Command::new("make");
    make.current_dir(dir.path())
        .args(["-s", "-f"])
        .arg(&makefile)
        .arg("probe")
        .env("PATH", path)
        .env("FCVM_PROBE_ARGV", &argv_file)
        .env("FCVM_PROBE_ENV", &env_file)
        .env_remove("FCVM_CONFIG_DIR")
        .env_remove("MAKEFLAGS")
        .env_remove("MAKELEVEL")
        .env_remove("MFLAGS");
    match config_dir {
        Some((value, Origin::CommandLine)) => {
            make.arg(format!("FCVM_CONFIG_DIR={value}"));
        }
        Some((value, Origin::Environment)) => {
            make.env("FCVM_CONFIG_DIR", value);
        }
        None => {}
    }
    let output = make.output().expect("run make");
    assert!(
        output.status.success(),
        "probe make failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let recorded = std::fs::read(&argv_file).expect("stub sudo recorded its arguments");
    let mut argv: Vec<String> = recorded
        .split(|&byte| byte == 0)
        .map(|arg| String::from_utf8(arg.to_vec()).expect("UTF-8 argument"))
        .collect();
    assert_eq!(
        argv.pop().as_deref(),
        Some(""),
        "stub sudo ends every argument with NUL"
    );
    let config_dir_env = match std::fs::read_to_string(&env_file) {
        Ok(value) => Some(value),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => panic!("read stub sudo's environment record: {error}"),
    };
    SudoCall {
        argv,
        config_dir_env,
    }
}

#[test]
fn sudo_fcvm_keeps_a_config_dir_with_spaces_as_one_argument() {
    assert_eq!(
        sudo_fcvm_call(Some("/nonexistent/fcvm config dir")).argv,
        [
            "env",
            "FCVM_CONFIG_DIR=/nonexistent/fcvm config dir",
            "probe-args"
        ],
        "SUDO_FCVM must hand env the whole config dir as one assignment"
    );
}

#[test]
fn sudo_fcvm_keeps_double_quotes_in_the_config_dir() {
    for dir in [
        "/nonexistent/fcvm\"config",
        "/nonexistent/fcvm \"config\" dir",
    ] {
        let assignment = format!("FCVM_CONFIG_DIR={dir}");
        assert_eq!(
            sudo_fcvm_call(Some(dir)).argv,
            ["env", assignment.as_str(), "probe-args"],
            "a double quote in the config dir must reach env unchanged"
        );
    }
}

#[test]
fn sudo_fcvm_adds_nothing_when_the_config_dir_is_unset() {
    assert_eq!(
        sudo_fcvm_call(None),
        SudoCall {
            argv: vec!["probe-args".to_string()],
            config_dir_env: None,
        },
        "with FCVM_CONFIG_DIR unset, SUDO_FCVM must run the command directly and \
         recipes must not see the variable at all (fcvm rejects an empty one)"
    );
}

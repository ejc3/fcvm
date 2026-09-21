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

/// The arguments sudo receives when a recipe runs `$(SUDO_FCVM) probe-args` with
/// FCVM_CONFIG_DIR set to `config_dir`, or unset for `None`.
///
/// make test-root runs this test as root, and the Makefile refuses to run as
/// root, so the probe copies the Makefile's own SHELL and SUDO_FCVM definitions
/// into a minimal makefile. A stub sudo first on PATH records its arguments, so
/// make's expansion and the recipe shell's word splitting are the real ones.
fn sudo_fcvm_argv(config_dir: Option<&str>) -> Vec<String> {
    let makefile = makefile();
    let definition = |name: &str| {
        let prefix = format!("{name} := ");
        let lines: Vec<&str> = makefile
            .lines()
            .filter(|line| line.starts_with(&prefix))
            .collect();
        assert_eq!(
            lines.len(),
            1,
            "expected one `{prefix}` line in the Makefile: {lines:?}"
        );
        lines[0].to_string()
    };
    let dir = tempfile::tempdir().expect("probe directory");
    let sudo = dir.path().join("sudo");
    std::fs::write(
        &sudo,
        "#!/bin/bash\nprintf '%s\\0' \"$@\" > \"$FCVM_PROBE_ARGV\"\n",
    )
    .expect("write stub sudo");
    std::fs::set_permissions(&sudo, std::fs::Permissions::from_mode(0o755))
        .expect("make stub sudo executable");
    let probe = dir.path().join("probe.mk");
    std::fs::write(
        &probe,
        format!(
            "{}\n{}\n.PHONY: probe\nprobe:\n\t$(SUDO_FCVM) probe-args\n",
            definition("SHELL"),
            definition("SUDO_FCVM")
        ),
    )
    .expect("write probe makefile");
    let argv = dir.path().join("argv");
    let path = format!(
        "{}:{}",
        dir.path().display(),
        std::env::var("PATH").expect("PATH")
    );
    let mut make = Command::new("make");
    make.current_dir(dir.path())
        .args(["-s", "-f"])
        .arg(&probe)
        .arg("probe")
        .env("PATH", path)
        .env("FCVM_PROBE_ARGV", &argv)
        .env_remove("FCVM_CONFIG_DIR")
        .env_remove("MAKEFLAGS")
        .env_remove("MAKELEVEL")
        .env_remove("MFLAGS");
    if let Some(config_dir) = config_dir {
        make.arg(format!("FCVM_CONFIG_DIR={config_dir}"));
    }
    let output = make.output().expect("run make");
    assert!(
        output.status.success(),
        "probe make failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let recorded = std::fs::read(&argv).expect("stub sudo recorded its arguments");
    let mut args: Vec<String> = recorded
        .split(|&byte| byte == 0)
        .map(|arg| String::from_utf8(arg.to_vec()).expect("UTF-8 argument"))
        .collect();
    assert_eq!(
        args.pop().as_deref(),
        Some(""),
        "stub sudo ends every argument with NUL"
    );
    args
}

#[test]
fn sudo_fcvm_keeps_a_config_dir_with_spaces_as_one_argument() {
    assert_eq!(
        sudo_fcvm_argv(Some("/nonexistent/fcvm config dir")),
        [
            "env",
            "FCVM_CONFIG_DIR=/nonexistent/fcvm config dir",
            "probe-args"
        ],
        "SUDO_FCVM must hand env the whole config dir as one assignment"
    );
}

#[test]
fn sudo_fcvm_adds_nothing_when_the_config_dir_is_unset() {
    assert_eq!(
        sudo_fcvm_argv(None),
        ["probe-args"],
        "SUDO_FCVM must run the command directly when FCVM_CONFIG_DIR is unset"
    );
}

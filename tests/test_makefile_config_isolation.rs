//! Makefile recipes that run `fcvm setup` under sudo must forward FCVM_CONFIG_DIR.
//!
//! sudo resets the environment, so a caller that isolates its config with
//! FCVM_CONFIG_DIR loses it on those lines: `fcvm setup --generate-config` then
//! rewrites root's config, and the setup steps read it instead of the caller's.

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
    assert!(
        makefile.contains(
            "SUDO_FCVM := sudo $(if $(FCVM_CONFIG_DIR),env FCVM_CONFIG_DIR=$(FCVM_CONFIG_DIR),)"
        ),
        "SUDO_FCVM must pass FCVM_CONFIG_DIR through sudo when it is set"
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

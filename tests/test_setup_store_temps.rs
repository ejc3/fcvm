//! Store builders stage under a temp name that is unique to the builder (#1182).
//!
//! The setup stores under assets_dir are shared across VMs: nested runs `--map` the
//! host `/mnt/fcvm-btrfs` into every L1. The store lock (`lock_store_dir`) is a
//! `flock`, and a fuse-pipe mount negotiates no `FUSE_FLOCK_LOCKS`, so each guest
//! kernel grants it locally. Two L1 VMs building the same artifact on a cold store
//! both hold "the" lock, and with a fixed temp name (`layer2-<sha>.raw.tmp`,
//! `fc-agent-<sha>.initrd.tmp`, `packages-<sha>.tmp`, `<kernel>.downloading`, ...)
//! they write, remove and rename the same file.
//!
//! Every builder therefore stages through one primitive, `crate::setup::StoreTemp`
//! (or its path half, `store_temp_path`), which names a uuid-keyed sibling of the
//! final path and removes it if the build fails. These tests read the source:
//! the race needs two kernels sharing one store, which no unit test can build, and
//! what has to hold is that no builder names its temp any other way.

use std::path::PathBuf;

fn source(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

/// A source file without its `#[cfg(test)]` modules, each of which runs from the
/// attribute to the closing brace in column 0 (rustfmt's layout for a top-level item).
fn production_part(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find("\n#[cfg(test)]\nmod ") {
        out.push_str(&rest[..at]);
        rest = &rest[at + 1..];
        match rest.find("\n}\n") {
            Some(end) => rest = &rest[end + 2..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// The text of the top-level function `name`, from its signature to the closing
/// brace in column 0 (rustfmt's layout for a top-level item).
fn function_body<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let start = [
        format!("\nfn {name}("),
        format!("\nasync fn {name}("),
        format!("\npub fn {name}("),
        format!("\npub async fn {name}("),
        format!("\npub(crate) fn {name}("),
        format!("\npub(crate) async fn {name}("),
        format!("\npub(super) fn {name}("),
        format!("\npub(super) async fn {name}("),
    ]
    .iter()
    .filter_map(|sig| text.find(sig.as_str()))
    .min()?;
    let end = text[start..].find("\n}\n")? + start + 3;
    Some(&text[start..end])
}

/// Temp-name constructions that are the same for every builder of an entry.
const FIXED_TEMP_NAMES: &[&str] = &[
    "with_extension(\"tmp\")",
    "with_extension(\"raw.tmp\")",
    "with_extension(\"initrd.tmp\")",
    "with_extension(\"downloading\")",
    "with_extension(\"img.download\")",
    "with_extension(\"full\")",
    "with_extension(\"converting\")",
    ".downloading\"",
    "-extract\"",
    "packages-{}.tmp",
    "process::id()",
    "$KERNEL_PATH.tmp",
];

/// Every function that stages an entry of a shared store, by file.
const STORE_BUILDERS: &[(&str, &str)] = &[
    ("src/setup/rootfs.rs", "ensure_rootfs"),
    ("src/setup/rootfs.rs", "create_layer2_rootless"),
    ("src/setup/rootfs.rs", "ensure_fc_agent_initrd"),
    ("src/setup/rootfs.rs", "download_packages"),
    ("src/setup/rootfs.rs", "download_cloud_image"),
    ("src/setup/kernel.rs", "ensure_url_kernel"),
    ("src/setup/kernel.rs", "download_kernel_binary"),
    ("src/setup/kernel.rs", "build_kernel_locally"),
    ("src/setup/kernel.rs", "ensure_cloud_hypervisor"),
    ("src/commands/podman/image.rs", "build_storage_image"),
    ("src/commands/podman/mod.rs", "export_localhost_image_disk"),
];

#[test]
fn every_store_builder_stages_through_the_shared_temp_primitive() {
    let mut failures = Vec::new();
    for (file, name) in STORE_BUILDERS {
        let code = production_part(&source(file));
        let Some(body) = function_body(&code, name) else {
            failures.push(format!("{file}: function `{name}` not found"));
            continue;
        };
        if !body.contains("StoreTemp::new(") && !body.contains("store_temp_path(") {
            failures.push(format!(
                "{file}: `{name}` does not name its temp with StoreTemp::new or store_temp_path"
            ));
        }
        for fixed in FIXED_TEMP_NAMES {
            if body.contains(fixed) {
                failures.push(format!(
                    "{file}: `{name}` builds a fixed temp name: {fixed}"
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "store builders that two VMs sharing the store over --map would collide in:\n  {}",
        failures.join("\n  ")
    );
}

#[test]
fn no_setup_source_builds_a_fixed_temp_name() {
    let mut failures = Vec::new();
    for file in [
        "src/setup/mod.rs",
        "src/setup/rootfs.rs",
        "src/setup/kernel.rs",
        "src/setup/pasta.rs",
        "src/setup/nvidia.rs",
        "src/setup/storage.rs",
    ] {
        let text = source(file);
        for (lineno, line) in production_part(&text).lines().enumerate() {
            for fixed in FIXED_TEMP_NAMES {
                if line.contains(fixed) {
                    failures.push(format!("{file}:{}: {}", lineno + 1, line.trim()));
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "fixed temp names in the setup store code (use crate::setup::StoreTemp):\n  {}",
        failures.join("\n  ")
    );
}

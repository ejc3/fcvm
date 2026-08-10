//! Release invariants for the source-built default guest kernel.

use serde_norway::Value as YamlValue;
use sha2::{Digest, Sha256};
use std::path::PathBuf;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn rootfs_config() -> toml::Value {
    let path = repo_root().join("rootfs-config.toml");
    let text = std::fs::read_to_string(&path).unwrap();
    toml::from_str(&text).unwrap()
}

fn default_profile<'a>(config: &'a toml::Value, arch: &str) -> &'a toml::Value {
    &config["kernel_profiles"]["default"][arch]
}

/// The one kernel version every deployable profile is pinned to.
///
/// `default`, `nested`, and `btrfs` all ship to users, so they move together;
/// `mountinfo-*` are test-only reproductions of a specific kernel bug and are
/// deliberately excluded. Deriving the expectation instead of writing a literal
/// means a future bump cannot silently leave one profile behind on an unpinned
/// or end-of-life release, which is the drift the deployable-pin rule in
/// rootfs-config.toml exists to prevent.
fn deployable_kernel_version(config: &toml::Value) -> String {
    let mut versions: Vec<String> = Vec::new();
    for profile in ["nested", "btrfs"] {
        for arch in ["arm64", "amd64"] {
            let version = config["kernel_profiles"][profile][arch]["kernel_version"]
                .as_str()
                .unwrap_or_else(|| panic!("{profile}.{arch} has no kernel_version"));
            versions.push(version.to_string());
        }
    }
    versions.sort();
    versions.dedup();
    assert_eq!(
        versions.len(),
        1,
        "deployable profiles disagree on their kernel pin: {versions:?}"
    );
    versions.remove(0)
}

#[test]
fn default_release_manifest_matches_immutable_build_recipe_on_both_arches() {
    let root = repo_root();
    let config = rootfs_config();
    assert!(
        config.get("kernel").is_none(),
        "the retired Kata [kernel] path must not coexist with the explicit default profile"
    );

    let deployable_version = deployable_kernel_version(&config);
    for arch in ["arm64", "amd64"] {
        let profile = default_profile(&config, arch);
        assert_eq!(
            profile["kernel_version"].as_str(),
            Some(deployable_version.as_str()),
            "{arch} default kernel drifted off the deployable pin"
        );
        assert_eq!(profile["kernel_repo"].as_str(), Some("ejc3/fcvm"));

        let inputs = profile["build_inputs"].as_array().unwrap();
        assert_eq!(inputs.len(), 2, "{arch} default profile build input drift");
        let mut bytes = Vec::new();
        for input in inputs {
            let relative = input.as_str().unwrap();
            assert!(
                !relative
                    .chars()
                    .any(|character| matches!(character, '*' | '?' | '[')),
                "default release inputs must be exact files: {relative}"
            );
            bytes.extend(std::fs::read(root.join(relative)).unwrap());
        }
        let actual = format!("{:x}", Sha256::digest(&bytes));
        assert_eq!(
            profile["kernel_sha"].as_str(),
            Some(&actual[..12]),
            "{arch} kernel_sha does not name the configured build inputs"
        );

        let recipe_path = root.join(inputs[0].as_str().unwrap());
        let recipe: toml::Value =
            toml::from_str(&std::fs::read_to_string(recipe_path).unwrap()).unwrap();
        assert_eq!(recipe["build_spec"].as_integer(), Some(1));
        for key in ["base_config_url", "kernel_config", "patches_dir"] {
            assert_eq!(
                profile[key].as_str(),
                recipe[key].as_str(),
                "{arch} profile {key} diverges from its hashed build recipe"
            );
        }
        let base_url = profile["base_config_url"].as_str().unwrap();
        assert!(
            !base_url.contains("/main/"),
            "{arch} default kernel base config is mutable: {base_url}"
        );
        assert!(
            base_url.contains("03b096f3bde2c7f4a54bbdcc0ccdb9c6b2986781"),
            "{arch} default base config must be pinned to the reviewed Firecracker commit"
        );
    }
}

#[test]
fn every_guest_kernel_fragment_supports_snapshot_socket_cleanup() {
    let kernel_dir = repo_root().join("kernel");
    let mut checked = 0usize;
    for entry in std::fs::read_dir(&kernel_dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("conf") {
            continue;
        }
        checked += 1;
        let contents = std::fs::read_to_string(&path).unwrap();
        for option in [
            // Enumerate and retire snapshot-time sockets by kernel cookie.
            "CONFIG_INET_DIAG=y",
            "CONFIG_INET_DIAG_DESTROY=y",
            // AF_PACKET supplies the receive-path grace period that closes the
            // capture boundary.
            "CONFIG_PACKET=y",
            // The directional NEW-flow REJECT gate that holds the boundary shut
            // while cookies are captured and retired. The guest runs both
            // iptables and ip6tables, so both families must be present or the
            // gate cannot install and snapshot creation fails closed. The
            // guest's iptables is the nft backend, which creates its tables
            // dynamically and reaches the xt REJECT target and conntrack match
            // through NFT_COMPAT; the legacy filter tables are gated behind
            // NETFILTER_XTABLES_LEGACY on 6.18 and must not be requested, or
            // Kconfig drops them and the build's own guard refuses to publish.
            "CONFIG_NETFILTER_XTABLES=y",
            "CONFIG_NF_TABLES=y",
            "CONFIG_NFT_COMPAT=y",
            "CONFIG_NF_CONNTRACK=y",
            "CONFIG_NETFILTER_XT_MATCH_CONNTRACK=y",
            "CONFIG_IP_NF_IPTABLES=y",
            "CONFIG_IP_NF_TARGET_REJECT=y",
            "CONFIG_IP6_NF_IPTABLES=y",
            "CONFIG_IP6_NF_TARGET_REJECT=y",
        ] {
            assert!(
                contents.lines().any(|line| line.trim() == option),
                "{} is missing {option}",
                path.display()
            );
        }
    }
    assert!(
        checked >= 7,
        "expected every shipped guest config to be checked"
    );
}

#[test]
fn kernel_workflow_builds_and_releases_default_for_both_runner_arches() {
    let path = repo_root().join(".github/workflows/kernels.yml");
    let workflow: YamlValue =
        serde_norway::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let job = &workflow["jobs"]["build-default-kernel"];
    let matrix = job["strategy"]["matrix"]["include"].as_sequence().unwrap();

    let pairs: Vec<(&str, &str)> = matrix
        .iter()
        .map(|item| {
            (
                item["config_arch"].as_str().unwrap(),
                item["runner_arch"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(pairs, [("arm64", "ARM64"), ("amd64", "X64")]);

    let scripts = job["steps"]
        .as_sequence()
        .unwrap()
        .iter()
        .filter_map(|step| step.get("run").and_then(YamlValue::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    for required in [
        "make release-default-kernel",
        "kernel_sha",
        "gh release view",
        "gh release create",
        "vmlinux-default-",
    ] {
        assert!(
            scripts.contains(required),
            "default release job is missing `{required}`"
        );
    }

    // The workflow delegates the build to make, so the invariant that the
    // release binary comes from `--kernel-profile default` now lives in the
    // recipe. Check the whole chain, not just the hand-off: a Makefile edit
    // that drops the profile flag would otherwise leave the workflow releasing
    // whatever kernel a bare `setup` happens to produce.
    let makefile = std::fs::read_to_string(repo_root().join("Makefile")).unwrap();
    let recipe_start = makefile
        .find("\nrelease-default-kernel:")
        .expect("Makefile no longer defines release-default-kernel");
    let recipe = &makefile[recipe_start + 1..];
    let recipe = &recipe[..recipe.find("\n\n").unwrap_or(recipe.len())];
    assert!(
        recipe.contains("setup --kernel-profile default --build-kernels"),
        "release-default-kernel no longer builds the default profile; recipe is:\n{recipe}"
    );
}

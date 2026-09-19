//! Release invariants for the guest kernels that `kernels.yml` builds and publishes.

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

fn kernels_workflow() -> YamlValue {
    let path = repo_root().join(".github/workflows/kernels.yml");
    serde_norway::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// A step's `run` script, or nothing for a step that uses an action.
fn run_script(step: &YamlValue) -> &str {
    step.get("run").and_then(YamlValue::as_str).unwrap_or("")
}

/// The packages a shell script installs with apt. Comments are dropped and
/// backslash continuations joined first, so a package named only in a comment
/// does not count and one on a continuation line does.
fn apt_packages(script: &str) -> std::collections::BTreeSet<String> {
    let joined = script.replace("\\\n", " ");
    let mut packages = std::collections::BTreeSet::new();
    for line in joined.lines() {
        let line = line.split('#').next().unwrap_or("");
        let words: Vec<&str> = line.split_whitespace().collect();
        let Some(install) = words.iter().position(|word| *word == "install") else {
            continue;
        };
        if !words[..install].iter().any(|word| word.contains("apt-get")) {
            continue;
        }
        packages.extend(
            words[install + 1..]
                .iter()
                .filter(|word| !word.starts_with('-'))
                .map(|word| word.to_string()),
        );
    }
    packages
}

#[test]
fn apt_packages_reads_continuations_and_ignores_comments() {
    let script = "./scripts/ci-apt-get.sh update\n\
                  ./scripts/ci-apt-get.sh install -y flex bison \\\n    libseccomp-dev  # linked by firecracker\n\
                  ./scripts/ci-apt-get.sh install -y gh  # libelf-dev comes from the image\n\
                  echo install nothing\n";
    let packages = apt_packages(script);
    let expected = ["bison", "flex", "gh", "libseccomp-dev"];
    assert_eq!(
        packages.iter().map(String::as_str).collect::<Vec<_>>(),
        expected
    );
}

/// Every job here runs `fcvm setup`, and setup builds Firecracker, which links
/// libseccomp. The build step runs only when the pinned kernel has no release
/// yet, so a missing package stays hidden until the next kernel bump: the
/// amd64 default job first built on the move to 6.18.50 and failed with
/// `unable to find library -lseccomp` after the kernel itself was ready. The
/// arm64 runner image carried the package and the x64 bootstrap did not.
#[test]
fn every_kernel_workflow_job_installs_what_setup_links() {
    let workflow = kernels_workflow();
    let jobs = workflow["jobs"].as_mapping().expect("jobs is a mapping");
    let mut names: Vec<&str> = jobs.keys().filter_map(YamlValue::as_str).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "build-btrfs-kernel",
            "build-default-kernel",
            "build-nested-kernel"
        ],
        "a job was added to or removed from kernels.yml; decide whether it runs setup and \
         update this test"
    );
    for name in names {
        let steps = workflow["jobs"][name]["steps"].as_sequence().unwrap();
        let setup = steps
            .iter()
            .position(|step| {
                run_script(step).contains("fcvm setup")
                    || run_script(step).contains("make release-default-kernel")
            })
            .unwrap_or_else(|| panic!("job `{name}` no longer runs setup"));
        // Only a step that always runs, before setup does, provides the package.
        let installed: std::collections::BTreeSet<String> = steps[..setup]
            .iter()
            .filter(|step| step.get("if").is_none())
            .flat_map(|step| apt_packages(run_script(step)))
            .collect();
        assert!(
            installed.contains("libseccomp-dev"),
            "job `{name}` runs fcvm setup, which builds Firecracker, and no unconditional step \
             before it installs libseccomp-dev; those steps install: {installed:?}"
        );
    }
}

/// Both runner images have to carry it too, or the two architectures disagree
/// about what a job may assume. That disagreement is what hid the missing
/// install above.
#[test]
fn both_runner_images_install_libseccomp() {
    for script in ["scripts/setup-runner.sh", "scripts/build-ami.sh"] {
        let text = std::fs::read_to_string(repo_root().join(script)).unwrap();
        assert!(
            apt_packages(&text).contains("libseccomp-dev"),
            "{script} does not install libseccomp-dev"
        );
    }
}

/// The architecture a runner label builds for, under the name
/// rootfs-config.toml gives it.
fn config_arch_of_runner(label: &str) -> Option<&'static str> {
    match label {
        "ARM64" => Some("arm64"),
        "X64" => Some("amd64"),
        _ => None,
    }
}

/// Every (kernel profile, architecture) leg a kernels workflow builds and
/// publishes, read from the workflow alone.
///
/// A job's profile is the one name its scripts pass to `--kernel-profile`.
/// `make release-default-kernel` is the default profile, which
/// `kernel_workflow_builds_and_releases_default_for_both_runner_arches` pins to
/// the recipe. A job's architectures are its matrix legs, or, for a job with no
/// matrix, the one runner architecture `runs-on` names.
fn published_kernel_legs(workflow: &YamlValue) -> std::collections::BTreeSet<(String, String)> {
    let mut legs = std::collections::BTreeSet::new();
    let jobs = workflow["jobs"].as_mapping().expect("jobs is a mapping");
    for (name, job) in jobs {
        let name = name.as_str().expect("job ids are strings");
        let steps = job["steps"]
            .as_sequence()
            .unwrap_or_else(|| panic!("job `{name}` has no steps"));
        let mut profiles = std::collections::BTreeSet::new();
        for script in steps.iter().map(run_script) {
            if script.contains("make release-default-kernel") {
                profiles.insert("default".to_string());
            }
            let mut words = script.split_whitespace();
            while let Some(word) = words.next() {
                if word == "--kernel-profile" {
                    let profile = words.next().unwrap_or_else(|| {
                        panic!("job `{name}` ends a script on --kernel-profile")
                    });
                    profiles.insert(profile.to_string());
                }
            }
        }
        assert_eq!(
            profiles.len(),
            1,
            "job `{name}` must build exactly one kernel profile, found {profiles:?}"
        );
        let profile = profiles.into_iter().next().unwrap();

        let runs_on: Vec<&str> = job["runs-on"]
            .as_sequence()
            .unwrap_or_else(|| panic!("job `{name}` does not list its runner labels"))
            .iter()
            .filter_map(YamlValue::as_str)
            .collect();
        let arches: Vec<&str> = match job["strategy"]["matrix"]["include"].as_sequence() {
            Some(include) => {
                assert!(
                    runs_on.contains(&"${{ matrix.runner_arch }}"),
                    "job `{name}` has a matrix and does not take its runner from it: {runs_on:?}"
                );
                include
                    .iter()
                    .map(|leg| {
                        let config_arch = leg["config_arch"]
                            .as_str()
                            .unwrap_or_else(|| panic!("a `{name}` leg has no config_arch"));
                        let runner_arch = leg["runner_arch"]
                            .as_str()
                            .unwrap_or_else(|| panic!("a `{name}` leg has no runner_arch"));
                        assert_eq!(
                            config_arch_of_runner(runner_arch),
                            Some(config_arch),
                            "job `{name}` builds {config_arch} on a {runner_arch} runner"
                        );
                        config_arch
                    })
                    .collect()
            }
            None => {
                let arches: Vec<&str> = runs_on
                    .iter()
                    .filter_map(|label| config_arch_of_runner(label))
                    .collect();
                assert_eq!(
                    arches.len(),
                    1,
                    "job `{name}` has no matrix, so runs-on must name one runner architecture: \
                     {runs_on:?}"
                );
                arches
            }
        };
        for arch in arches {
            legs.insert((profile.clone(), arch.to_string()));
        }
    }
    legs
}

#[test]
fn published_kernel_legs_reads_matrix_jobs_and_single_runner_jobs() {
    let workflow: YamlValue = serde_norway::from_str(
        r#"
jobs:
  both:
    strategy:
      matrix:
        include:
          - config_arch: arm64
            runner_arch: ARM64
          - config_arch: amd64
            runner_arch: X64
    runs-on: [self-hosted, Linux, "${{ matrix.runner_arch }}"]
    steps:
      - run: make release-default-kernel
  one:
    runs-on: [self-hosted, Linux, ARM64]
    steps:
      - uses: actions/checkout@v7
      - run: |
          sudo ./target/release/fcvm setup --kernel-profile btrfs --build-kernels
"#,
    )
    .unwrap();
    let legs: Vec<(String, String)> = published_kernel_legs(&workflow).into_iter().collect();
    let legs: Vec<(&str, &str)> = legs
        .iter()
        .map(|(profile, arch)| (profile.as_str(), arch.as_str()))
        .collect();
    assert_eq!(
        legs,
        [
            ("btrfs", "arm64"),
            ("default", "amd64"),
            ("default", "arm64")
        ]
    );
}

/// `fcvm setup --kernel-profile <name>` downloads the release named for the
/// host it runs on, and without `--build-kernels` it fails when that release
/// does not exist. An architecture table that names a `kernel_repo` is what
/// makes setup look for one. The btrfs job built arm64 only while
/// `kernel_profiles.btrfs.amd64` named a repo, so once the pin moved to 6.18.50
/// an x86_64 host got a 404 for `kernel-btrfs-6.18.50-x86_64-<sha>`. The nested
/// job had the same gap: `kernel_profiles.nested.amd64` names a repo and no
/// x86_64 nested kernel was ever published.
///
/// A profile no job builds (the mountinfo experiment) is a local build. Nothing
/// publishes it for any architecture, so it needs no leg and no exemption here.
#[test]
fn every_published_kernel_profile_is_built_for_each_architecture_it_names() {
    let config = rootfs_config();
    let legs = published_kernel_legs(&kernels_workflow());
    assert!(!legs.is_empty(), "kernels.yml builds no kernel at all");
    let published: std::collections::BTreeSet<&str> =
        legs.iter().map(|(profile, _)| profile.as_str()).collect();

    let mut missing = Vec::new();
    for profile in &published {
        let tables = config["kernel_profiles"]
            .get(*profile)
            .and_then(toml::Value::as_table)
            .unwrap_or_else(|| {
                panic!("kernels.yml builds `{profile}`, which rootfs-config.toml does not define")
            });
        let mut named = 0usize;
        for (arch, table) in tables {
            let repo = table
                .get("kernel_repo")
                .and_then(toml::Value::as_str)
                .unwrap_or("");
            if repo.is_empty() {
                continue;
            }
            named += 1;
            if !legs.contains(&(profile.to_string(), arch.clone())) {
                missing.push(format!("{profile}.{arch}"));
            }
        }
        assert!(
            named > 0,
            "kernels.yml publishes `{profile}`, and no architecture table of it names a kernel_repo"
        );
    }
    assert!(
        missing.is_empty(),
        "rootfs-config.toml names a kernel_repo for {missing:?}, so `fcvm setup` downloads a \
         release for each, and no kernels.yml job leg builds them. Legs built: {legs:?}"
    );
}

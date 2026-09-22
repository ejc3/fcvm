//! Release invariants for the guest kernels that `kernels.yml` builds and publishes.

use fcvm::setup::kernel::{
    compute_profile_kernel_sha_at_root, custom_kernel_filename, custom_kernel_release_tag,
    vm_kernel_patches_dir,
};
use fcvm::setup::KernelProfile;
use serde_norway::Value as YamlValue;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;

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

        // The recipe and the config fragment, then each patch the recipe's patches_dir
        // applies, every one named as an exact file.
        let inputs = profile["build_inputs"].as_array().unwrap();
        let patches = match profile["patches_dir"].as_str() {
            None | Some("") => Vec::new(),
            Some(dir) => applied_patches(&root, dir),
        };
        assert_eq!(
            inputs.len(),
            2 + patches.len(),
            "{arch} default profile build input drift"
        );
        for (input, patch) in inputs[2..].iter().zip(&patches) {
            assert_eq!(
                root.join(input.as_str().unwrap()),
                *patch,
                "{arch} default profile must hash each applied patch by its exact path"
            );
        }
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
        "scripts/kernel-release-identity.py default",
        "gh release view",
        "gh release create",
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
    let recipe = makefile_recipe("release-default-kernel");
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
            "build-nested-kernel",
            "verify-releases"
        ],
        "a job was added to or removed from kernels.yml; decide whether it runs setup and \
         update this test"
    );
    // verify-releases asks whether releases exist and builds nothing. The other
    // three must each still build: a job that lost its build command would drop
    // out of every test that walks the build jobs.
    let mut building: Vec<&str> = kernel_build_jobs(&workflow)
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    building.sort_unstable();
    assert_eq!(building, names[..3], "the jobs that build a kernel changed");
    for name in building {
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

/// A script's command lines as words: backslash continuations joined, comments
/// dropped, blank lines skipped.
fn command_lines(script: &str) -> Vec<Vec<String>> {
    script
        .replace("\\\n", " ")
        .lines()
        .map(|line| {
            line.split('#')
                .next()
                .unwrap_or("")
                .split_whitespace()
                .map(str::to_string)
                .collect::<Vec<String>>()
        })
        .filter(|words| !words.is_empty())
        .collect()
}

/// The kernel profiles a script builds.
///
/// A build is a command that runs the checkout's own binary with a build flag,
/// `[sudo] ./target/release/fcvm setup ... --kernel-profile <name>`, or `make
/// release-default-kernel`, which
/// `kernel_workflow_builds_and_releases_default_for_both_runner_arches` pins to
/// the default profile. Only the start of a command line counts. The release
/// notes quote `fcvm setup --kernel-profile <name>` as usage, and prose must not
/// keep a job classified after its build command loses the flag.
fn profiles_built_by(script: &str) -> BTreeSet<String> {
    let mut profiles = BTreeSet::new();
    for words in command_lines(script) {
        let words: Vec<&str> = words.iter().map(String::as_str).collect();
        let command = match words.as_slice() {
            ["sudo", rest @ ..] => rest,
            all => all,
        };
        match command {
            ["make", "release-default-kernel", ..] => {
                profiles.insert("default".to_string());
            }
            ["./target/release/fcvm", "setup", flags @ ..]
                if flags
                    .iter()
                    .any(|flag| matches!(*flag, "--build-kernels" | "--force-build-kernels")) =>
            {
                let named = flags
                    .iter()
                    .position(|flag| *flag == "--kernel-profile")
                    .and_then(|at| flags.get(at + 1));
                if let Some(profile) = named {
                    profiles.insert(profile.to_string());
                }
            }
            _ => {}
        }
    }
    profiles
}

#[test]
fn profiles_built_by_reads_commands_and_ignores_prose() {
    let built = |script: &str| -> Vec<String> { profiles_built_by(script).into_iter().collect() };

    // Both arms of a forced-or-not build name one profile, and a continuation
    // line belongs to its command.
    assert_eq!(
        built(
            "if [ \"$FORCE_BUILD\" = \"true\" ]; then\n  \
               sudo ./target/release/fcvm setup --kernel-profile nested \\\n    --force-build-kernels\n\
             else\n  \
               sudo ./target/release/fcvm setup --kernel-profile nested --build-kernels\n\
             fi\n"
        ),
        ["nested"]
    );
    assert_eq!(built("make release-default-kernel\n"), ["default"]);

    // A release step quotes setup commands as usage. With the build command's
    // flag gone, nothing here names a profile.
    assert_eq!(
        built(
            "sudo ./target/release/fcvm setup --build-kernels\n\
             gh release create \"$TAG\" --notes \"Usage:\n\
             fcvm setup --kernel-profile nested\n\
             fcvm setup --kernel-profile nested --build-kernels\n\
             \"\n"
        ),
        Vec::<String>::new()
    );
    // Neither does a comment, nor a setup that builds nothing.
    assert_eq!(
        built(
            "# sudo ./target/release/fcvm setup --kernel-profile btrfs --build-kernels\n\
             ./target/release/fcvm setup --generate-config --force\n\
             ./target/release/fcvm setup --kernel-profile btrfs\n"
        ),
        Vec::<String>::new()
    );
}

/// The jobs of a workflow, by id.
fn workflow_jobs(workflow: &YamlValue) -> Vec<(&str, &YamlValue)> {
    workflow["jobs"]
        .as_mapping()
        .expect("jobs is a mapping")
        .iter()
        .map(|(name, job)| (name.as_str().expect("job ids are strings"), job))
        .collect()
}

/// The jobs that build and publish a kernel, as opposed to the job that checks
/// the published releases. `every_kernel_workflow_job_installs_what_setup_links`
/// pins which jobs these are.
fn kernel_build_jobs(workflow: &YamlValue) -> Vec<(&str, &YamlValue)> {
    workflow_jobs(workflow)
        .into_iter()
        .filter(|(name, job)| {
            job_steps(name, job)
                .iter()
                .any(|step| !profiles_built_by(run_script(step)).is_empty())
        })
        .collect()
}

fn job_steps<'a>(name: &str, job: &'a YamlValue) -> &'a [YamlValue] {
    job["steps"]
        .as_sequence()
        .unwrap_or_else(|| panic!("job `{name}` has no steps"))
}

/// The one step of a job that builds its kernel, and the profile it builds.
fn kernel_build_step<'a>(name: &str, job: &'a YamlValue) -> (&'a YamlValue, String) {
    let mut builds: Vec<(&YamlValue, BTreeSet<String>)> = job_steps(name, job)
        .iter()
        .map(|step| (step, profiles_built_by(run_script(step))))
        .filter(|(_, profiles)| !profiles.is_empty())
        .collect();
    assert_eq!(
        builds.len(),
        1,
        "job `{name}` must build a kernel profile in exactly one step, found {}",
        builds.len()
    );
    let (step, profiles) = builds.remove(0);
    assert_eq!(
        profiles.len(),
        1,
        "job `{name}` must build exactly one kernel profile, found {profiles:?}"
    );
    (step, profiles.into_iter().next().unwrap())
}

/// The step of a job that computes the release identity the later steps read
/// as `steps.kernel.outputs.*`.
fn identity_step<'a>(name: &str, job: &'a YamlValue) -> &'a YamlValue {
    let steps: Vec<&YamlValue> = job_steps(name, job)
        .iter()
        .filter(|step| step.get("id").and_then(YamlValue::as_str) == Some("kernel"))
        .collect();
    assert_eq!(
        steps.len(),
        1,
        "job `{name}` must have exactly one step with id `kernel`"
    );
    steps[0]
}

/// The step of a job that publishes the release.
fn release_step<'a>(name: &str, job: &'a YamlValue) -> &'a YamlValue {
    let steps: Vec<&YamlValue> = job_steps(name, job)
        .iter()
        .filter(|step| {
            command_lines(run_script(step))
                .iter()
                .any(|words| words.len() >= 3 && words[..3] == ["gh", "release", "create"])
        })
        .collect();
    assert_eq!(
        steps.len(),
        1,
        "job `{name}` must create its release in exactly one step"
    );
    steps[0]
}

/// The architectures a job builds for, under the names rootfs-config.toml
/// uses: its matrix legs, or, for a job with no matrix, the one runner
/// architecture `runs-on` names.
fn job_config_arches(name: &str, job: &YamlValue) -> Vec<String> {
    let runs_on: Vec<&str> = job["runs-on"]
        .as_sequence()
        .unwrap_or_else(|| panic!("job `{name}` does not list its runner labels"))
        .iter()
        .filter_map(YamlValue::as_str)
        .collect();
    match job["strategy"]["matrix"]["include"].as_sequence() {
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
                    config_arch.to_string()
                })
                .collect()
        }
        None => {
            let arches: Vec<String> = runs_on
                .iter()
                .filter_map(|label| config_arch_of_runner(label))
                .map(str::to_string)
                .collect();
            assert_eq!(
                arches.len(),
                1,
                "job `{name}` has no matrix, so runs-on must name one runner architecture: \
                 {runs_on:?}"
            );
            arches
        }
    }
}

/// Every (kernel profile, architecture) leg a kernels workflow builds and
/// publishes, read from the workflow alone: the profile each job's build step
/// names, for each architecture the job runs on.
fn published_kernel_legs(workflow: &YamlValue) -> BTreeSet<(String, String)> {
    let mut legs = BTreeSet::new();
    for (name, job) in kernel_build_jobs(workflow) {
        let (_, profile) = kernel_build_step(name, job);
        for arch in job_config_arches(name, job) {
            legs.insert((profile.clone(), arch));
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

const IDENTITY_SCRIPT: &str = "scripts/kernel-release-identity.py";

/// The name `uname -m` and Rust's `std::env::consts::ARCH` give the
/// architecture rootfs-config.toml calls `config_arch`.
fn runtime_arch_of(config_arch: &str) -> &'static str {
    match config_arch {
        "arm64" => "aarch64",
        "amd64" => "x86_64",
        other => panic!("no runtime architecture for config arch `{other}`"),
    }
}

fn host_config_arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => panic!("rootfs-config.toml has no kernel tables for {other}"),
    }
}

/// What a script did when run outside the runner.
struct StepRun {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl StepRun {
    fn from_output(output: std::process::Output) -> Self {
        Self {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }

    /// The argument lists a `logging_stub` recorded under `marker`.
    fn calls(&self, marker: &str) -> Vec<&str> {
        self.stdout
            .lines()
            .filter_map(|line| line.strip_prefix(marker))
            .map(str::trim_start)
            .collect()
    }
}

/// Shell that replaces `command` with a function logging its arguments on one
/// `<marker> ...` line. A function rather than a file on PATH, so the test
/// never execs something it has just written.
fn logging_stub(command: &str, marker: &str) -> String {
    format!("{command}() {{ printf '{marker} %s\\n' \"$*\"; }}\n")
}

/// Run a step's `run` script the way the runner does when the step names no
/// shell, `bash -e`, from `cwd`, with only PATH, HOME and `env` set.
///
/// `expressions` gives the value of every `${{ ... }}` the script interpolates.
/// One it does not list is a panic, so a new interpolation cannot run as
/// literal text. `prelude` is shell that runs first; the tests define functions
/// there for the commands a test must not really run (`sudo`, `gh`).
fn run_step_script(
    script: &str,
    expressions: &[(&str, &str)],
    prelude: &str,
    env: &[(&str, &str)],
    cwd: &Path,
) -> StepRun {
    let mut text = script.to_string();
    for (expression, value) in expressions {
        text = text.replace(&format!("${{{{ {expression} }}}}"), value);
    }
    assert!(
        !text.contains("${{"),
        "the step interpolates an expression this test gives no value for:\n{text}"
    );
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("step.sh");
    std::fs::write(&path, format!("{prelude}\n{text}")).unwrap();
    let mut command = Command::new("bash");
    command.arg("-e").arg(&path).current_dir(cwd).env_clear();
    for key in ["PATH", "HOME"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command.envs(env.iter().copied());
    StepRun::from_output(command.output().expect("run bash"))
}

/// The recipe of a Makefile target, up to the blank line that ends it.
fn makefile_recipe(target: &str) -> String {
    let makefile = std::fs::read_to_string(repo_root().join("Makefile")).unwrap();
    let start = makefile
        .find(&format!("\n{target}:"))
        .unwrap_or_else(|| panic!("Makefile no longer defines {target}"));
    let recipe = &makefile[start + 1..];
    recipe[..recipe.find("\n\n").unwrap_or(recipe.len())].to_string()
}

/// A kernel file name no build produces, so a build step run here stops at its
/// own "built kernel not found" check and copies nothing.
const UNBUILT_KERNEL: &str = "vmlinux-kernel-workflow-test-never-built.bin";

/// The condition on a job's build step and on its release step: build when the
/// release is absent or a manual run asks for a replacement, and publish
/// exactly then.
const BUILD_OR_FORCE: &str = "steps.check.outputs.exists == 'false' || inputs.force_build == true";

/// A job reaches its build step only after it has decided to build: the release
/// is absent, or `force_build` asked for a replacement. Either way the artifact
/// has to come from source in that run. `fcvm setup --build-kernels` returns a
/// cached file when there is one and otherwise downloads the published release,
/// building only if that download fails. So a kernel file left on a persistent
/// runner by an earlier CI job under the same content-addressed name became the
/// release, and a forced rebuild uploaded the artifact it was asked to replace.
/// `--force-build-kernels` skips both (`rebuild_kernel_from_source`).
/// `--build-kernels` stays beside it because setup fetches the default kernel
/// first and has to build that when its release is absent.
///
/// The nested and btrfs build steps run here for real, with `sudo` replaced by
/// a function that logs the setup command. They do not read `force_build`: the
/// step's `if:` decides whether it runs, and whenever it runs it builds from
/// source. The default job hands the build to `make release-default-kernel`,
/// which forces only under FORCE=1: the recipe then passes
/// `--force-build-kernels` without `--build-kernels`, and setup cannot fetch a
/// default kernel that has no release yet without permission to build it.
#[test]
fn a_kernel_build_step_always_builds_from_source() {
    let workflow = kernels_workflow();
    let scratch = tempfile::tempdir().unwrap();
    let mut ran = 0usize;
    for (name, job) in kernel_build_jobs(&workflow) {
        let (step, profile) = kernel_build_step(name, job);
        // Pinned whole: `&&` in place of `||` keeps both clauses and never
        // builds a release that is merely absent.
        assert_eq!(
            step["if"].as_str(),
            Some(BUILD_OR_FORCE),
            "job `{name}` must build when its release is absent or force_build asks for a \
             replacement"
        );
        assert_eq!(
            release_step(name, job)["if"].as_str(),
            Some(BUILD_OR_FORCE),
            "job `{name}` must publish exactly when it builds, or a forced rebuild builds a \
             kernel and never releases it"
        );
        let script = run_script(step);

        if profile == "default" {
            assert_eq!(
                step["env"]["FORCE"].as_str(),
                Some("${{ inputs.force_build == true && '1' || '0' }}"),
                "job `{name}` no longer passes force_build to make as FORCE"
            );
            let recipe = makefile_recipe("release-default-kernel");
            assert!(
                recipe.contains("if [ \"$(FORCE)\" = \"1\" ]; then")
                    && recipe.contains("setup --kernel-profile default --force-build-kernels"),
                "release-default-kernel no longer turns FORCE=1 into --force-build-kernels; \
                 recipe is:\n{recipe}"
            );
            continue;
        }

        let run = run_step_script(
            script,
            &[("steps.kernel.outputs.filename", UNBUILT_KERNEL)],
            &logging_stub("sudo", "SUDO"),
            &[],
            scratch.path(),
        );
        let setups = run.calls("SUDO");
        assert_eq!(
            setups.len(),
            1,
            "job `{name}` must run setup once, ran {setups:?}\n{}",
            run.stderr
        );
        let words: Vec<&str> = setups[0].split_whitespace().collect();
        assert!(
            words
                .windows(2)
                .any(|pair| pair == ["--kernel-profile", profile.as_str()]),
            "job `{name}` ran `{}`, which does not build `{profile}`",
            setups[0]
        );
        assert!(
            words.contains(&"--force-build-kernels"),
            "job `{name}` ran `{}`. Without --force-build-kernels setup returns a kernel file \
             already on the runner, or downloads the release being replaced, and that becomes \
             the release",
            setups[0]
        );
        assert!(
            words.contains(&"--build-kernels"),
            "job `{name}` ran `{}`. Without --build-kernels setup cannot build the default \
             kernel it fetches first when that has no release yet",
            setups[0]
        );
        assert!(
            !script.contains("inputs."),
            "job `{name}` interpolates a workflow input into its build script"
        );
        ran += 1;
    }
    assert_eq!(ran, 2, "expected to run the nested and btrfs build steps");
}

/// The commit a release step is told it built, in the tests that run one.
const BUILT_COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

/// A forced rebuild deletes the published release before it creates the new
/// one. Anything in the release step that can refuse the leg has to run before
/// that delete, or the refusal costs the release users are downloading. The
/// nested step picks its notes by architecture and refused an architecture it
/// has no notes for only after the delete.
///
/// Every release step runs here for an architecture no job has notes for, with
/// `gh` replaced by a function that logs its arguments. A step that publishes
/// creates its tag at the commit that was built: without `--target` the tag
/// lands on the default branch's head, whose tree can hash to a different
/// identity than the one the tag names.
#[test]
fn a_release_step_that_refuses_a_leg_has_not_deleted_the_release() {
    let workflow = kernels_workflow();
    let scratch = tempfile::tempdir().unwrap();
    let tag = "kernel-test-1.2.3-riscv64-0123456789ab";
    let mut refused = 0usize;
    for (name, job) in kernel_build_jobs(&workflow) {
        let run = run_step_script(
            run_script(release_step(name, job)),
            &[
                ("steps.kernel.outputs.tag", tag),
                ("steps.kernel.outputs.filename", UNBUILT_KERNEL),
                ("steps.kernel.outputs.version", "1.2.3"),
                ("steps.kernel.outputs.sha", "0123456789ab"),
                ("steps.kernel.outputs.arch", "riscv64"),
                ("steps.check.outputs.exists", "true"),
                ("inputs.force_build", "true"),
            ],
            &logging_stub("gh", "GH"),
            &[
                ("CONFIG_ARCH", "riscv64"),
                ("GH_TOKEN", "unused"),
                ("GITHUB_SHA", BUILT_COMMIT),
            ],
            scratch.path(),
        );
        let calls: Vec<&str> = run
            .calls("GH")
            .into_iter()
            .filter(|call| call.starts_with("release "))
            .collect();
        if run.code == Some(0) {
            // This job's notes do not depend on the architecture.
            assert_eq!(calls.len(), 2, "job `{name}` ran {calls:?}");
            assert!(
                calls[0].starts_with("release delete "),
                "job `{name}` ran {calls:?}"
            );
            assert!(
                calls[1].starts_with(&format!("release create {tag} --target {BUILT_COMMIT} ")),
                "job `{name}` does not create its tag at the commit it built: `{}`",
                calls[1]
            );
        } else {
            assert_eq!(
                run.code,
                Some(1),
                "job `{name}` failed some other way than refusing the leg:\n{}\n{}",
                run.stdout,
                run.stderr
            );
            assert!(
                run.stdout.contains("unsupported config arch riscv64"),
                "job `{name}` exited 1 without saying it refused the architecture:\n{}",
                run.stdout
            );
            assert!(
                calls.is_empty(),
                "job `{name}` refused the leg after running {calls:?}. On a forced rebuild that \
                 deletes the published release and publishes nothing in its place"
            );
            refused += 1;
        }
    }
    assert!(
        refused >= 1,
        "no release step refused an architecture it has no notes for. The nested step picks \
         its notes by CONFIG_ARCH, so this test no longer reaches that branch"
    );
}

/// GitHub's default job timeout is six hours. ci.yml records a wedged runner
/// holding a job all day that way while the lease logic kept renewing it, and
/// these jobs boot a setup VM under sudo on the same self-hosted runners.
#[test]
fn every_kernel_workflow_job_bounds_its_runtime() {
    for (name, job) in workflow_jobs(&kernels_workflow()) {
        let minutes = job["timeout-minutes"].as_u64().unwrap_or_else(|| {
            panic!(
                "job `{name}` sets no timeout-minutes, so a wedged self-hosted runner holds it \
                 for GitHub's six-hour default"
            )
        });
        assert!(
            (1..=120).contains(&minutes),
            "job `{name}` allows {minutes} minutes. ci.yml gives 120 to the lane that builds \
             these same kernels and runs the privileged suite twice"
        );
    }
}

/// The tag and file name a job publishes under are the ones `fcvm setup`
/// downloads, so both sides must derive them the same way. Each job used to
/// carry its own copy of the derivation, and the copies had drifted: only the
/// default job checked `kernel_sha`, and only the other two noticed a helper
/// that printed nothing. One checked-in script now does it for all three.
#[test]
fn every_kernel_job_takes_its_release_identity_from_the_shared_script() {
    for (name, job) in kernel_build_jobs(&kernels_workflow()) {
        let (_, profile) = kernel_build_step(name, job);
        let step = identity_step(name, job);
        assert_eq!(
            step["env"]["CONFIG_ARCH"].as_str(),
            Some("${{ matrix.config_arch }}"),
            "job `{name}` must compute its identity for the leg's own architecture"
        );
        let calls: Vec<Vec<String>> = command_lines(run_script(step))
            .into_iter()
            .filter(|words| words.iter().any(|word| word.ends_with(IDENTITY_SCRIPT)))
            .collect();
        assert_eq!(
            calls.len(),
            1,
            "job `{name}` must call {IDENTITY_SCRIPT} once in its `kernel` step"
        );
        let at = calls[0]
            .iter()
            .position(|word| word.ends_with(IDENTITY_SCRIPT))
            .unwrap();
        let arguments: Vec<&str> = calls[0][at + 1..]
            .iter()
            .map(|word| word.trim_end_matches(')'))
            .collect();
        assert_eq!(
            arguments,
            [profile.as_str(), "\"$CONFIG_ARCH\""],
            "job `{name}` builds `{profile}` and must publish under that profile's identity"
        );

        for step in job_steps(name, job) {
            for words in command_lines(run_script(step)) {
                let line = words.join(" ");
                for derivation in [
                    "hashlib",
                    "tomllib",
                    "sha256",
                    "build_inputs",
                    "tag=kernel-",
                    "filename=vmlinux-",
                ] {
                    assert!(
                        !line.contains(derivation),
                        "job `{name}` derives part of its release identity itself (`{line}`). \
                         {IDENTITY_SCRIPT} is the one place that mirrors src/setup/kernel.rs"
                    );
                }
            }
        }
    }
}

/// Re-run while a `uname` shim the test has just written is still busy.
///
/// Under a threaded test harness another test can fork while the shim's
/// descriptor is open for writing, and until that child execs, running the
/// shim fails with ETXTBSY. src/setup/kernel.rs retries its fixtures for the
/// same reason.
fn retrying_text_file_busy(mut run: impl FnMut() -> StepRun) -> StepRun {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let result = run();
        if !result.stderr.contains("Text file busy") || std::time::Instant::now() >= deadline {
            return result;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// A directory holding a `uname` that reports `machine`, and a PATH that
/// finds it first. The identity check reads the runner's architecture from
/// `uname -m`, and a test has to be able to run the other architecture's leg.
fn uname_shim(machine: &str) -> (tempfile::TempDir, String) {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let shim = dir.path().join("uname");
    std::fs::write(&shim, format!("#!/bin/sh\necho {machine}\n")).unwrap();
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        dir.path().display(),
        std::env::var("PATH").expect("PATH is set")
    );
    (dir, path)
}

/// Run a job's identity step for one leg on a runner that reports `machine`.
/// Returns the run and what it wrote to `$GITHUB_OUTPUT`.
fn run_identity_step(
    step: &YamlValue,
    config_arch: &str,
    machine: &str,
    prelude: &str,
) -> (StepRun, String) {
    let (_shim, path) = uname_shim(machine);
    let scratch = tempfile::tempdir().unwrap();
    let github_output = scratch.path().join("github_output");
    let run = retrying_text_file_busy(|| {
        std::fs::write(&github_output, "").unwrap();
        run_step_script(
            run_script(step),
            &[],
            prelude,
            &[
                ("CONFIG_ARCH", config_arch),
                ("GITHUB_OUTPUT", github_output.to_str().unwrap()),
                ("PATH", &path),
            ],
            &repo_root(),
        )
    });
    let written = std::fs::read_to_string(&github_output).unwrap();
    (run, written)
}

/// `key=value` lines, the format `$GITHUB_OUTPUT` takes.
fn identity_lines(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .map(|line| {
            let (key, value) = line
                .split_once('=')
                .unwrap_or_else(|| panic!("not a key=value line: {line:?}"));
            (key.to_string(), value.to_string())
        })
        .collect()
}

fn kernel_profile_table(config: &toml::Value, profile: &str, arch: &str) -> KernelProfile {
    config["kernel_profiles"][profile][arch]
        .clone()
        .try_into()
        .unwrap_or_else(|error| panic!("kernel_profiles.{profile}.{arch}: {error}"))
}

/// Each leg's identity step, run as the workflow runs it, must name what the
/// client downloads. The SHA comes from `compute_profile_kernel_sha_at_root`,
/// the function `fcvm setup` uses, which also rejects a table whose `kernel_sha`
/// is stale. On the host's own architecture the tag and file name come from the
/// client's functions as well; the other architecture differs from them only
/// in the architecture name.
#[test]
fn every_kernel_leg_publishes_under_the_name_setup_downloads() {
    let root = repo_root();
    let config = rootfs_config();
    let workflow = kernels_workflow();
    let mut checked = BTreeSet::new();
    for (name, job) in kernel_build_jobs(&workflow) {
        let (_, profile_name) = kernel_build_step(name, job);
        let step = identity_step(name, job);
        for config_arch in job_config_arches(name, job) {
            let machine = runtime_arch_of(&config_arch);
            let profile = kernel_profile_table(&config, &profile_name, &config_arch);
            let version = profile.kernel_version.clone();
            let sha = compute_profile_kernel_sha_at_root(&profile, Some(&root))
                .unwrap_or_else(|error| panic!("{profile_name}.{config_arch}: {error:#}"));
            if let Some(manifest) = profile.kernel_sha.as_deref() {
                assert_eq!(manifest, sha, "{profile_name}.{config_arch} kernel_sha");
            }

            let (run, written) = run_identity_step(step, &config_arch, machine, "");
            assert_eq!(
                run.code,
                Some(0),
                "job `{name}` leg {config_arch} failed:\n{}\n{}",
                run.stdout,
                run.stderr
            );
            let expected: BTreeMap<String, String> = [
                ("repo", profile.kernel_repo.clone()),
                ("version", version.clone()),
                ("arch", machine.to_string()),
                ("sha", sha.clone()),
                (
                    "tag",
                    format!("kernel-{profile_name}-{version}-{machine}-{sha}"),
                ),
                (
                    "filename",
                    format!("vmlinux-{profile_name}-{version}-{machine}-{sha}.bin"),
                ),
            ]
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect();
            assert_eq!(
                identity_lines(&written),
                expected,
                "job `{name}` leg {config_arch}"
            );
            if machine == std::env::consts::ARCH {
                assert_eq!(
                    expected["tag"],
                    custom_kernel_release_tag(&profile_name, &version, &sha)
                );
                assert_eq!(
                    expected["filename"],
                    custom_kernel_filename(&profile_name, &version, &sha)
                );
            }

            // The same leg on a runner of the other architecture publishes nothing.
            let other = if machine == "aarch64" {
                "x86_64"
            } else {
                "aarch64"
            };
            let (run, written) = run_identity_step(step, &config_arch, other, "");
            assert_ne!(
                run.code,
                Some(0),
                "job `{name}` leg {config_arch} accepted a {other} runner"
            );
            assert_eq!(written, "", "job `{name}` leg {config_arch} on {other}");

            checked.insert((profile_name.clone(), config_arch));
        }
    }
    assert_eq!(checked, published_kernel_legs(&workflow));
    assert!(checked.len() >= 6, "expected six legs, checked {checked:?}");
}

/// A helper that fails must fail the step. The default job read the helper
/// through a process substitution, which discards its exit status, and then
/// compared an empty SHA with an empty `kernel_sha`: equal, so the step went on
/// and published outputs with no version and no SHA in them.
#[test]
fn a_failing_identity_helper_fails_the_step_and_publishes_nothing() {
    let arch = host_config_arch();
    for (name, job) in kernel_build_jobs(&kernels_workflow()) {
        let (run, written) = run_identity_step(
            identity_step(name, job),
            arch,
            runtime_arch_of(arch),
            "python3() { echo version=6.18.50; return 1; }\n",
        );
        assert_ne!(
            run.code,
            Some(0),
            "job `{name}` carried on after its identity helper failed and wrote:\n{written}"
        );
        assert_eq!(
            written, "",
            "job `{name}` published outputs from a failed identity helper"
        );
    }
}

/// A throwaway checkout: the identity script and the release check, a
/// rootfs-config.toml holding `tables`, and `files`.
fn identity_fixture(tables: &str, files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("scripts")).unwrap();
    for script in [IDENTITY_SCRIPT, VERIFY_SCRIPT] {
        std::fs::copy(repo_root().join(script), dir.path().join(script)).unwrap();
    }
    std::fs::write(dir.path().join("rootfs-config.toml"), tables).unwrap();
    for (relative, contents) in files {
        let path = dir.path().join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }
    dir
}

/// Run a checkout's identity script on a runner that reports `machine`, from
/// a directory that is not the checkout.
fn run_identity_script(root: &Path, profile: &str, config_arch: &str, machine: &str) -> StepRun {
    let (_shim, path) = uname_shim(machine);
    retrying_text_file_busy(|| {
        StepRun::from_output(
            Command::new("python3")
                .arg(root.join(IDENTITY_SCRIPT))
                .args([profile, config_arch])
                .env("PATH", &path)
                .current_dir("/")
                .output()
                .expect("run python3"),
        )
    })
}

fn sha256_short(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))[..12].to_string()
}

/// The script is Python and the client is Rust, and their globs differ in two
/// places a build input can reach. The Rust `glob` crate matches a dot-prefixed
/// file with `*`; Python's skips it unless asked. With no `build_inputs` the
/// client uses the constant `000000000000`; hashing nothing gives
/// `e3b0c44298fc`. Both sides run here over the same fixture checkout.
#[test]
fn the_identity_script_hashes_build_inputs_the_way_setup_does() {
    let arch = host_config_arch();
    let machine = runtime_arch_of(arch);
    let tables = format!(
        r#"
[kernel_profiles.dotted.{arch}]
kernel_version = "1.2.3"
kernel_repo = "example/kernels"
build_inputs = ["inputs/*.conf"]

[kernel_profiles.ordered.{arch}]
kernel_version = "1.2.3"
kernel_repo = "example/kernels"
build_inputs = ["inputs/z.conf", "inputs/patches/*", "inputs/a.conf"]

[kernel_profiles.bare.{arch}]
kernel_version = "1.2.3"
kernel_repo = "example/kernels"
"#
    );
    let fixture = identity_fixture(
        &tables,
        &[
            ("inputs/.hidden.conf", "hidden\n"),
            ("inputs/a.conf", "a\n"),
            ("inputs/z.conf", "z\n"),
            ("inputs/patches/0002-second.patch", "second\n"),
            ("inputs/patches/0001-first.patch", "first\n"),
            ("inputs/patches/.0000-dotted.patch", "dotted\n"),
            ("inputs/patches/0003-off.patch.disabled", "off\n"),
        ],
    );
    let config: toml::Value = toml::from_str(&tables).unwrap();

    // What the client does with each fixture, spelled out once.
    let client = |name: &str| {
        compute_profile_kernel_sha_at_root(
            &kernel_profile_table(&config, name, arch),
            Some(fixture.path()),
        )
        .unwrap()
    };
    assert_eq!(client("dotted"), sha256_short(b"hidden\na\nz\n"));
    assert_eq!(
        client("ordered"),
        sha256_short(b"z\ndotted\nfirst\nsecond\na\n")
    );
    assert_eq!(client("bare"), "000000000000");

    for name in ["dotted", "ordered", "bare"] {
        let sha = client(name);
        let run = run_identity_script(fixture.path(), name, arch, machine);
        assert_eq!(run.code, Some(0), "profile `{name}`:\n{}", run.stderr);
        let identity = identity_lines(&run.stdout);
        assert_eq!(identity["sha"], sha, "profile `{name}`");
        assert_eq!(identity["repo"], "example/kernels");
        assert_eq!(identity["version"], "1.2.3");
        assert_eq!(identity["arch"], machine);
        assert_eq!(
            identity["tag"],
            custom_kernel_release_tag(name, "1.2.3", &sha)
        );
        assert_eq!(
            identity["filename"],
            custom_kernel_filename(name, "1.2.3", &sha)
        );
    }
}

/// The script refused: a non-zero exit, a message, and nothing on stdout for a
/// step to publish.
fn assert_refused(what: &str, run: StepRun) {
    assert_ne!(run.code, Some(0), "{what}: accepted\n{}", run.stdout);
    assert_eq!(run.stdout, "", "{what}: printed an identity while failing");
    assert!(
        run.stderr.contains("ERROR"),
        "{what}: no error message:\n{}",
        run.stderr
    );
}

/// Whatever the client refuses, the script must refuse too, with nothing on
/// stdout for a step to publish. It also refuses what only a release job can
/// get wrong: a table that is not a source release, and a runner of the wrong
/// architecture.
#[test]
fn the_identity_script_refuses_what_setup_refuses() {
    let arch = host_config_arch();
    let machine = runtime_arch_of(arch);
    let other_arch = if arch == "arm64" { "amd64" } else { "arm64" };
    let pinned = sha256_short(b"a\n");
    let source = "kernel_version = \"1.2.3\"\nkernel_repo = \"example/kernels\"";
    let tables = format!(
        r#"
[kernel_profiles.pinned.{arch}]
{source}
build_inputs = ["inputs/a.conf"]
kernel_sha = "{pinned}"

[kernel_profiles.stale.{arch}]
{source}
build_inputs = ["inputs/a.conf"]
kernel_sha = "000000000000"

[kernel_profiles.malformed.{arch}]
{source}
build_inputs = ["inputs/a.conf"]
kernel_sha = "NOT-A-SHA"

[kernel_profiles.unmatched.{arch}]
{source}
build_inputs = ["inputs/*.missing"]

[kernel_profiles.alldisabled.{arch}]
{source}
build_inputs = ["inputs/*.disabled"]

[kernel_profiles.recursive.{arch}]
{source}
build_inputs = ["inputs/**/*.conf"]

[kernel_profiles.url.{arch}]
{source}
kernel_url = "https://example.invalid/kernel.tar.zst"
build_inputs = ["inputs/a.conf"]

[kernel_profiles.inherits.{arch}]
description = "runtime settings only"
"#
    );
    // `inputs/sub/b.conf` is what `inputs/**/*.conf` matches when `**` is read as
    // `*`, the way Python's glob reads it without `recursive`. With the file in
    // place, only the script's own refusal of `**` can fail that table.
    let fixture = identity_fixture(
        &tables,
        &[
            ("inputs/a.conf", "a\n"),
            ("inputs/off.disabled", "off\n"),
            ("inputs/sub/b.conf", "b\n"),
        ],
    );
    let config: toml::Value = toml::from_str(&tables).unwrap();

    // The control: the same fixture publishes a table that is in order.
    let run = run_identity_script(fixture.path(), "pinned", arch, machine);
    assert_eq!(run.code, Some(0), "the control failed:\n{}", run.stderr);
    assert_eq!(identity_lines(&run.stdout)["sha"], pinned);

    let refused = assert_refused;
    for name in ["stale", "malformed", "unmatched", "alldisabled"] {
        assert!(
            compute_profile_kernel_sha_at_root(
                &kernel_profile_table(&config, name, arch),
                Some(fixture.path())
            )
            .is_err(),
            "the client accepts `{name}`, so this fixture proves nothing"
        );
        refused(
            name,
            run_identity_script(fixture.path(), name, arch, machine),
        );
    }
    for name in ["recursive", "url", "inherits", "absent"] {
        refused(
            name,
            run_identity_script(fixture.path(), name, arch, machine),
        );
    }
    refused(
        "a table for the other architecture only",
        run_identity_script(
            fixture.path(),
            "pinned",
            other_arch,
            runtime_arch_of(other_arch),
        ),
    );
    refused(
        "an unknown architecture",
        run_identity_script(fixture.path(), "pinned", "riscv64", "riscv64"),
    );
    refused(
        "a runner of the other architecture",
        run_identity_script(fixture.path(), "pinned", arch, runtime_arch_of(other_arch)),
    );
}

/// An installed binary has no `kernel/` tree to hash, so it finds the default
/// release only through the table's `kernel_sha`. The default job's own step
/// failed on a table without one, and the shared script has to as well. Only
/// the default profile: the nested and btrfs tables carry no `kernel_sha`.
#[test]
fn a_default_table_without_kernel_sha_is_refused() {
    let arch = host_config_arch();
    let machine = runtime_arch_of(arch);
    let source = "kernel_version = \"1.2.3\"\nkernel_repo = \"example/kernels\"\n\
                  build_inputs = [\"inputs/a.conf\"]";
    let files = [("inputs/a.conf", "a\n")];

    let bare = identity_fixture(
        &format!(
            "[kernel_profiles.default.{arch}]\n{source}\n\n\
             [kernel_profiles.other.{arch}]\n{source}\n"
        ),
        &files,
    );
    let run = run_identity_script(bare.path(), "default", arch, machine);
    let said = run.stderr.clone();
    assert_refused("a default table with no kernel_sha", run);
    assert!(
        said.contains("kernel_sha"),
        "refused for some other reason:\n{said}"
    );
    let run = run_identity_script(bare.path(), "other", arch, machine);
    assert_eq!(run.code, Some(0), "{}", run.stderr);

    // The control: the same default table with its kernel_sha publishes.
    let pinned = identity_fixture(
        &format!(
            "[kernel_profiles.default.{arch}]\n{source}\nkernel_sha = \"{}\"\n",
            sha256_short(b"a\n")
        ),
        &files,
    );
    let run = run_identity_script(pinned.path(), "default", arch, machine);
    assert_eq!(run.code, Some(0), "{}", run.stderr);
}

/// The patch files a VM kernel build applies from `dir`. The generated build
/// script loops over `"$PATCHES_DIR"/*.patch`, and a shell glob skips
/// dot-prefixed names.
fn applied_patches(root: &Path, dir: &str) -> Vec<PathBuf> {
    let mut patches: Vec<PathBuf> = std::fs::read_dir(root.join(dir))
        .unwrap_or_else(|error| panic!("patches_dir `{dir}`: {error}"))
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            let name = path.file_name().unwrap().to_string_lossy();
            name.ends_with(".patch") && !name.starts_with('.') && path.is_file()
        })
        .collect();
    patches.sort();
    patches
}

/// Every source-built table in rootfs-config.toml, as `(profile, arch, table)`.
fn source_built_tables(config: &toml::Value) -> Vec<(String, String, KernelProfile)> {
    let mut tables = Vec::new();
    for (profile_name, arches) in config["kernel_profiles"].as_table().unwrap() {
        for arch in arches.as_table().unwrap().keys() {
            let profile = kernel_profile_table(config, profile_name, arch);
            if profile.is_custom() && !profile.is_url_based() {
                tables.push((profile_name.clone(), arch.clone(), profile));
            }
        }
    }
    tables
}

/// The files a table's `build_inputs` hash: each glob expanded and `*.disabled`
/// left out, as the client does.
fn hashed_build_inputs(root: &Path, profile: &KernelProfile) -> BTreeSet<PathBuf> {
    let mut hashed = BTreeSet::new();
    for pattern in &profile.build_inputs {
        let pattern = root.join(pattern).to_string_lossy().into_owned();
        for path in glob::glob(&pattern).unwrap().filter_map(Result::ok) {
            if !path.to_string_lossy().ends_with(".disabled") {
                hashed.insert(path);
            }
        }
    }
    hashed
}

/// A kernel's tag is the hash of its table's `build_inputs`, and the release
/// check skips the build when that tag already has a release. So every patch a
/// build applies has to be one of those inputs. Both btrfs tables omitted
/// `patches_dir`, which makes the build apply `kernel/patches`
/// (`vm_kernel_patches_dir`), while `build_inputs` listed only the config
/// fragment: editing a FUSE patch left both btrfs tags unchanged, the check
/// found the old release, and hosts kept a kernel without the change.
#[test]
fn every_patch_a_build_applies_is_part_of_what_its_tag_hashes() {
    let root = repo_root();
    let mut unhashed = Vec::new();
    let mut tables_with_patches = 0usize;
    for (profile_name, arch, profile) in source_built_tables(&rootfs_config()) {
        let Some(dir) = vm_kernel_patches_dir(&profile) else {
            continue;
        };
        let applied = applied_patches(&root, dir);
        assert!(
            !applied.is_empty(),
            "{profile_name}.{arch} applies `{dir}`, which holds no patch"
        );
        tables_with_patches += 1;

        let hashed = hashed_build_inputs(&root, &profile);
        for patch in applied {
            if !hashed.contains(&patch) {
                unhashed.push(format!(
                    "{profile_name}.{arch}: {}",
                    patch.strip_prefix(&root).unwrap().display()
                ));
            }
        }
    }
    assert!(
        tables_with_patches >= 4,
        "expected the nested and btrfs tables at least to apply patches, found \
         {tables_with_patches} tables that do"
    );
    assert!(
        unhashed.is_empty(),
        "a build applies these patches and its table's build_inputs does not list them, so \
         editing one leaves the kernel's tag unchanged and the release check skips the \
         rebuild:\n  {}",
        unhashed.join("\n  ")
    );
}

/// The config fragment is a build input the same way: the build applies it, so
/// the tag has to change when it does.
#[test]
fn every_kernel_config_is_part_of_what_its_tag_hashes() {
    let root = repo_root();
    let mut checked = 0usize;
    for (profile_name, arch, profile) in source_built_tables(&rootfs_config()) {
        let Some(fragment) = profile
            .kernel_config
            .as_deref()
            .filter(|fragment| !fragment.is_empty())
        else {
            continue;
        };
        checked += 1;
        assert!(
            hashed_build_inputs(&root, &profile).contains(&root.join(fragment)),
            "{profile_name}.{arch} builds with `{fragment}` and its build_inputs does not list \
             it, so editing the fragment leaves the kernel's tag unchanged"
        );
    }
    assert!(
        checked >= 6,
        "expected the default, nested and btrfs tables at least to name a kernel_config, \
         found {checked}"
    );
}

/// Two tools read a table's `patches_dir`, and they read an omitted one
/// differently. The build applies `kernel/patches` (`vm_kernel_patches_dir`);
/// scripts/kernel-patch.sh falls back to `kernel/patches-arm64` on arm64. A
/// table that names its directory, or names none with an empty string, gives
/// both the same answer, so `make kernel-patch-validate` checks the patches the
/// build applies.
#[test]
fn every_source_built_table_names_its_patches_dir() {
    let tables = source_built_tables(&rootfs_config());
    assert!(
        tables.len() >= 6,
        "expected the default, nested and btrfs tables at least, found {}",
        tables.len()
    );
    let omitted: Vec<String> = tables
        .iter()
        .filter(|(_, _, profile)| profile.patches_dir.is_none())
        .map(|(profile_name, arch, _)| format!("{profile_name}.{arch}"))
        .collect();
    assert!(
        omitted.is_empty(),
        "these tables omit patches_dir, which the build and scripts/kernel-patch.sh resolve \
         differently: {omitted:?}"
    );
    // The script's fallback is why an omission matters. If it goes, so can this rule.
    let script = std::fs::read_to_string(repo_root().join("scripts/kernel-patch.sh")).unwrap();
    assert!(
        script.contains("kernel/patches-arm64"),
        "scripts/kernel-patch.sh no longer falls back to kernel/patches-arm64, so the premise \
         of this test is gone"
    );
}

/// What a release tells its users has to be true for the architecture it is
/// for. On x86_64, nested KVM does not survive a snapshot restore of the outer
/// VM (#664, commit fc81ab1c): an outer VM restored from the snapshot cache has
/// no usable VMX and its inner VM's start times out, while the same tests pass
/// with snapshots disabled. The inner-VM tests are gated to aarch64 for that
/// reason. The x86_64 notes published the fcvm-inside-fcvm flow with no caveat.
#[test]
fn the_x86_64_nested_notes_say_the_outer_vm_must_cold_boot() {
    let workflow = kernels_workflow();
    let scratch = tempfile::tempdir().unwrap();
    let (name, job) = kernel_build_jobs(&workflow)
        .into_iter()
        .find(|(name, job)| kernel_build_step(name, job).1 == "nested")
        .expect("kernels.yml builds the nested profile");
    let run = run_step_script(
        run_script(release_step(name, job)),
        &[
            (
                "steps.kernel.outputs.tag",
                "kernel-nested-1.2.3-x86_64-0123456789ab",
            ),
            ("steps.kernel.outputs.filename", UNBUILT_KERNEL),
            ("steps.kernel.outputs.version", "1.2.3"),
            ("steps.kernel.outputs.sha", "0123456789ab"),
            ("steps.kernel.outputs.arch", "x86_64"),
            ("inputs.force_build", "false"),
        ],
        &logging_stub("gh", "GH"),
        &[
            ("CONFIG_ARCH", "amd64"),
            ("GH_TOKEN", "unused"),
            ("GITHUB_SHA", BUILT_COMMIT),
        ],
        scratch.path(),
    );
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    let notes = run.stdout;
    assert!(
        notes.contains(&format!(
            "GH release create kernel-nested-1.2.3-x86_64-0123456789ab --target {BUILT_COMMIT} "
        )),
        "the release step did not create the release at the commit it built:\n{notes}"
    );
    for required in ["--no-snapshot", "#664"] {
        assert!(
            notes.contains(required),
            "the x86_64 nested release notes do not mention `{required}`:\n{notes}"
        );
    }
    // The usage the notes print has to be the flow that works.
    assert!(
        notes.lines().any(|line| line.contains("fcvm podman run")
            && line.contains("--kernel-profile nested")
            && line.contains("--no-snapshot")),
        "the x86_64 notes show an outer VM started without --no-snapshot:\n{notes}"
    );

    // The flag the notes name has to exist, and the guide has to agree.
    let args = std::fs::read_to_string(repo_root().join("src/cli/args.rs")).unwrap();
    assert!(
        args.contains("pub no_snapshot: bool"),
        "src/cli/args.rs no longer defines --no-snapshot, which the release notes name"
    );
    let guide = std::fs::read_to_string(repo_root().join("NESTED.md")).unwrap();
    for required in ["x86_64", "--no-snapshot", "#664"] {
        assert!(
            guide.contains(required),
            "NESTED.md does not mention `{required}`"
        );
    }
}

/// A matrix leg that is listed and then gated off publishes nothing, while
/// every test that reads the matrix still counts it as built.
#[test]
fn no_kernel_job_gates_itself_or_a_step_on_its_matrix_leg() {
    let mut conditions = 0usize;
    for (name, job) in workflow_jobs(&kernels_workflow()) {
        let mut gates = vec![(
            "the job".to_string(),
            job["if"].as_str().unwrap_or("").to_string(),
        )];
        for (index, step) in job_steps(name, job).iter().enumerate() {
            let label = step["name"]
                .as_str()
                .or_else(|| step["uses"].as_str())
                .map(str::to_string)
                .unwrap_or_else(|| format!("step {}", index + 1));
            gates.push((label, step["if"].as_str().unwrap_or("").to_string()));
        }
        for (what, condition) in gates {
            if condition.is_empty() {
                continue;
            }
            conditions += 1;
            assert!(
                !condition.contains("matrix."),
                "job `{name}`, {what}: `if: {condition}` reads the matrix, so a leg can be \
                 listed and build nothing"
            );
        }
    }
    assert!(
        conditions > 0,
        "found no `if:` to inspect, and the build steps carry one"
    );
}

/// Each leg builds the table `kernel_profiles.<profile>.<arch>`. A leg with no
/// such table, or with one that is not a source release, has nothing to build
/// or publish.
#[test]
fn every_kernel_leg_has_its_profile_table() {
    let config = rootfs_config();
    let legs = published_kernel_legs(&kernels_workflow());
    assert!(!legs.is_empty(), "kernels.yml builds no kernel at all");
    let mut missing = Vec::new();
    for (profile, arch) in &legs {
        let table = config["kernel_profiles"]
            .get(profile.as_str())
            .and_then(|arches| arches.get(arch.as_str()));
        let source_release = table.is_some_and(|table| {
            let named = |key: &str| {
                table
                    .get(key)
                    .and_then(toml::Value::as_str)
                    .is_some_and(|value| !value.is_empty())
            };
            named("kernel_version") && named("kernel_repo") && table.get("kernel_url").is_none()
        });
        if !source_release {
            missing.push(format!("kernel_profiles.{profile}.{arch}"));
        }
    }
    assert!(
        missing.is_empty(),
        "kernels.yml has a leg for each of {missing:?}, and rootfs-config.toml has no \
         source-release table there for it to build"
    );
}

const VERIFY_SCRIPT: &str = "scripts/verify-kernel-releases.py";

/// The workflow that runs the release check weekly.
const WEEKLY_CHECK: &str = "verify-kernel-releases.yml";

/// What the release check prints only when every asset is there.
const PUBLISHED_LINE: &str = "kernel release assets are published";

/// #949 went unnoticed because nothing asks whether a published release
/// exists: every automated consumer passes `--build-kernels`, so a 404 becomes
/// a silent local build. The release check has to cover exactly the legs the
/// workflow publishes.
#[test]
fn the_release_check_covers_every_leg_the_workflow_publishes() {
    let output = Command::new("python3")
        .arg(repo_root().join(VERIFY_SCRIPT))
        .arg("--list-legs")
        .output()
        .expect("run python3");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let listed: BTreeSet<(String, String)> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| {
            let (profile, arch) = line
                .split_once(' ')
                .unwrap_or_else(|| panic!("not a `<profile> <arch>` line: {line:?}"));
            (profile.to_string(), arch.to_string())
        })
        .collect();
    assert_eq!(
        listed,
        published_kernel_legs(&kernels_workflow()),
        "{VERIFY_SCRIPT} checks a different set of legs than kernels.yml publishes"
    );
}

/// Every workflow file, by file name.
fn all_workflows() -> Vec<(String, YamlValue)> {
    let dir = repo_root().join(".github/workflows");
    let mut workflows = Vec::new();
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if !matches!(
            path.extension().and_then(|extension| extension.to_str()),
            Some("yml" | "yaml")
        ) {
            continue;
        }
        let file = path.file_name().unwrap().to_string_lossy().into_owned();
        let workflow = serde_norway::from_str(&std::fs::read_to_string(&path).unwrap())
            .unwrap_or_else(|error| panic!("{file} is not valid YAML: {error}"));
        workflows.push((file, workflow));
    }
    workflows.sort_by(|left, right| left.0.cmp(&right.0));
    workflows
}

/// A workflow's `on:` block. YAML 1.1 reads a bare `on` as boolean true and
/// YAML 1.2 keeps the string.
fn workflow_triggers<'a>(file: &str, workflow: &'a YamlValue) -> &'a YamlValue {
    workflow
        .get("on")
        .or_else(|| workflow.get(YamlValue::Bool(true)))
        .unwrap_or_else(|| panic!("{file} has no `on:` block"))
}

/// The events a workflow starts on. `on:` may be one event, a list of events,
/// or a mapping keyed by event.
fn trigger_events(file: &str, workflow: &YamlValue) -> BTreeSet<String> {
    match workflow_triggers(file, workflow) {
        YamlValue::String(event) => BTreeSet::from([event.clone()]),
        YamlValue::Sequence(events) => events
            .iter()
            .map(|event| event.as_str().expect("an event name").to_string())
            .collect(),
        YamlValue::Mapping(events) => events
            .keys()
            .map(|event| event.as_str().expect("an event name").to_string())
            .collect(),
        other => panic!("{file}: `on:` is neither an event, a list nor a mapping: {other:?}"),
    }
}

/// A `workflow_run` consumer starts on every completed run of the workflow it
/// names, whatever event began that run, unless it filters the event itself.
/// ci.yml and build-runner-ami.yml name "Build Kernels" and filter nothing. The
/// weekly release check was a schedule trigger on that workflow, so every
/// Monday a run that only asks six URLs would have started the full
/// self-hosted CI matrix and queued a runner AMI build, and both consumers
/// cancel what is already running on main.
///
/// So a workflow that a consumer names runs on no schedule. Work that needs one
/// gets a workflow name of its own.
#[test]
fn no_workflow_run_consumer_names_a_workflow_that_runs_on_a_schedule() {
    let workflows = all_workflows();
    let mut edges = 0usize;
    let mut scheduled = Vec::new();
    for (consumer_file, consumer) in &workflows {
        let sources = workflow_triggers(consumer_file, consumer)
            .get("workflow_run")
            .and_then(|trigger| trigger.get("workflows"))
            .and_then(YamlValue::as_sequence);
        let Some(sources) = sources else { continue };
        for source in sources {
            let source = source.as_str().expect("a workflow name");
            let named: Vec<&(String, YamlValue)> = workflows
                .iter()
                .filter(|(_, workflow)| workflow["name"].as_str() == Some(source))
                .collect();
            assert_eq!(
                named.len(),
                1,
                "{consumer_file} starts on runs of `{source}`, and {} workflow files carry \
                 that name",
                named.len()
            );
            edges += 1;
            let (source_file, source_workflow) = named[0];
            if trigger_events(source_file, source_workflow).contains("schedule") {
                scheduled.push(format!(
                    "{consumer_file} starts on every completed run of `{source}`, and \
                     {source_file} runs on a schedule"
                ));
            }
        }
    }
    assert!(
        edges >= 3,
        "expected at least three workflows that start on another workflow's runs, two on \
         Build Kernels and one on CI, found {edges} such edges"
    );
    assert!(
        scheduled.is_empty(),
        "a scheduled run completes like any other, so its consumers start every time the \
         schedule fires. Give the scheduled work a workflow name of its own:\n  {}",
        scheduled.join("\n  ")
    );
}

/// A job that runs the release check and cannot be skipped or silenced: a
/// hosted runner, a timeout, read-only permissions, the script as a command,
/// and nothing that lets a failure pass.
fn assert_runs_the_release_check(file: &str, name: &str, job: &YamlValue) {
    assert_eq!(
        job["runs-on"].as_str(),
        Some("ubuntu-latest"),
        "{file}: `{name}` needs no self-hosted runner"
    );
    let minutes = job["timeout-minutes"]
        .as_u64()
        .unwrap_or_else(|| panic!("{file}: `{name}` sets no timeout-minutes"));
    assert!(
        (1..=30).contains(&minutes),
        "{file}: `{name}` allows {minutes} minutes for six requests"
    );
    let permissions = job["permissions"]
        .as_mapping()
        .unwrap_or_else(|| panic!("{file}: `{name}` does not limit its token"));
    assert!(
        permissions.len() == 1 && job["permissions"]["contents"].as_str() == Some("read"),
        "{file}: `{name}` needs `contents: read` and nothing else, has {permissions:?}"
    );
    assert!(
        job.get("continue-on-error").is_none(),
        "{file}: `{name}` sets continue-on-error, so a missing release would not fail the run"
    );
    let steps = job_steps(name, job);
    for step in steps {
        assert!(
            step.get("if").is_none() && step.get("continue-on-error").is_none(),
            "{file}: a step of `{name}` carries `if:` or continue-on-error, which can skip or \
             silence the check: {step:?}"
        );
    }
    assert!(
        steps
            .iter()
            .flat_map(|step| command_lines(run_script(step)))
            .any(|words| words == ["python3", VERIFY_SCRIPT]),
        "{file}: `{name}` does not run {VERIFY_SCRIPT}"
    );
}

/// The check runs after the build jobs of every Build Kernels run, whatever
/// they concluded. It does not run on a cancelled run: the concurrency group
/// cancels a run that a newer push supersedes, its legs may not have published
/// yet, and a red check there would only report the cancellation.
#[test]
fn the_release_check_runs_after_the_builds_of_every_build_kernels_run() {
    let workflow = kernels_workflow();
    let job = &workflow["jobs"]["verify-releases"];
    assert!(job.is_mapping(), "kernels.yml has no verify-releases job");

    let needs: BTreeSet<&str> = job["needs"]
        .as_sequence()
        .expect("verify-releases lists its needs")
        .iter()
        .filter_map(YamlValue::as_str)
        .collect();
    let builders: BTreeSet<&str> = kernel_build_jobs(&workflow)
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert_eq!(
        needs, builders,
        "verify-releases must wait for every build job"
    );
    assert_eq!(
        job["if"].as_str(),
        Some("${{ !cancelled() }}"),
        "verify-releases must run when a build job failed or was skipped, and not on a \
         cancelled run"
    );
    assert_runs_the_release_check("kernels.yml", "verify-releases", job);

    let events = trigger_events("kernels.yml", &workflow);
    assert!(
        events.contains("workflow_dispatch"),
        "kernels.yml cannot be run by hand"
    );
    assert!(
        !events.contains("schedule"),
        "kernels.yml runs on a schedule. ci.yml and build-runner-ami.yml start on every \
         completed Build Kernels run on main; the weekly check belongs in {WEEKLY_CHECK}"
    );
}

/// The weekly run of the same check, in a workflow no `workflow_run` consumer
/// names (`no_workflow_run_consumer_names_a_workflow_that_runs_on_a_schedule`).
#[test]
fn the_weekly_release_check_is_a_workflow_of_its_own() {
    let path = repo_root().join(".github/workflows").join(WEEKLY_CHECK);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    let workflow: YamlValue = serde_norway::from_str(&text).unwrap();
    assert_ne!(
        workflow["name"].as_str(),
        kernels_workflow()["name"].as_str(),
        "{WEEKLY_CHECK} must not share the name the workflow_run consumers listen for"
    );
    assert_eq!(
        trigger_events(WEEKLY_CHECK, &workflow),
        BTreeSet::from(["schedule".to_string(), "workflow_dispatch".to_string()]),
        "{WEEKLY_CHECK} runs weekly and by hand, and on nothing else"
    );
    let crons = workflow_triggers(WEEKLY_CHECK, &workflow)["schedule"]
        .as_sequence()
        .expect("a schedule");
    assert!(
        crons.len() == 1 && crons[0]["cron"].as_str().is_some(),
        "{WEEKLY_CHECK} needs exactly one cron entry, has {crons:?}"
    );

    let jobs = workflow_jobs(&workflow);
    assert_eq!(jobs.len(), 1, "{WEEKLY_CHECK} has one job");
    let (name, job) = jobs[0];
    assert!(
        job.get("if").is_none() && job.get("needs").is_none(),
        "{WEEKLY_CHECK}: `{name}` must run every time the workflow does"
    );
    assert_runs_the_release_check(WEEKLY_CHECK, name, job);
}

/// One whole HTTP response with no body.
fn http_answer(status: &str, headers: &str) -> String {
    format!("HTTP/1.1 {status}\r\n{headers}Content-Length: 0\r\nConnection: close\r\n\r\n")
}

/// What GitHub answers for a release asset that exists.
fn found_answer() -> String {
    http_answer(
        "302 Found",
        "Location: http://127.0.0.1:1/asset-storage\r\n",
    )
}

/// A local stand-in for the release host. It sends `answers[path]` as the whole
/// response, and "404 Not Found" for any other path.
fn release_host(answers: BTreeMap<String, String>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            if reader.read_line(&mut request).is_err() {
                continue;
            }
            loop {
                let mut header = String::new();
                match reader.read_line(&mut header) {
                    Ok(0) | Err(_) => break,
                    Ok(_) if header == "\r\n" => break,
                    Ok(_) => {}
                }
            }
            let path = request.split_whitespace().nth(1).unwrap_or("");
            let response = answers
                .get(path)
                .cloned()
                .unwrap_or_else(|| http_answer("404 Not Found", ""));
            let _ = stream.write_all(response.as_bytes());
        }
    });
    format!("http://127.0.0.1:{port}")
}

/// Run a checkout's release check against `base_url`, with no proxy in the way.
fn run_release_check_at(root: &Path, base_url: &str) -> StepRun {
    let mut command = Command::new("python3");
    command
        .arg(root.join(VERIFY_SCRIPT))
        .args(["--base-url", base_url])
        .current_dir("/");
    for proxy in [
        "http_proxy",
        "https_proxy",
        "all_proxy",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
    ] {
        command.env_remove(proxy);
    }
    StepRun::from_output(command.output().expect("run python3"))
}

fn run_release_check(base_url: &str) -> StepRun {
    run_release_check_at(&repo_root(), base_url)
}

/// What the check printed after `prefix` on the lines that start with it.
fn lines_with(run: &StepRun, prefix: &str) -> Vec<String> {
    run.stdout
        .lines()
        .filter_map(|line| line.strip_prefix(prefix))
        .map(|rest| rest.trim().to_string())
        .collect()
}

/// Each published leg's asset as the check prints it, and the path the client
/// downloads it from, keyed by `(profile, config arch)`.
type PublishedAssets = BTreeMap<(String, String), (String, String)>;

/// The published assets of the real config. Names and paths come from the
/// client's own functions.
fn published_assets() -> PublishedAssets {
    let root = repo_root();
    let config = rootfs_config();
    let mut assets = BTreeMap::new();
    for (profile_name, config_arch) in published_kernel_legs(&kernels_workflow()) {
        let machine = runtime_arch_of(&config_arch);
        let profile = kernel_profile_table(&config, &profile_name, &config_arch);
        let version = &profile.kernel_version;
        let sha = compute_profile_kernel_sha_at_root(&profile, Some(&root)).unwrap();
        let asset = format!(
            "kernel-{profile_name}-{version}-{machine}-{sha}/\
             vmlinux-{profile_name}-{version}-{machine}-{sha}.bin"
        );
        let path = format!("/{}/releases/download/{asset}", profile.kernel_repo);
        assets.insert((profile_name, config_arch), (asset, path));
    }
    assets
}

/// A host on which every asset in `assets` exists.
fn all_found(assets: &PublishedAssets) -> BTreeMap<String, String> {
    assets
        .values()
        .map(|(_, path)| (path.clone(), found_answer()))
        .collect()
}

/// The check asks for what the client downloads, names exactly what is
/// missing, and never reports an asset it could not ask about as missing or as
/// present.
#[test]
fn the_release_check_names_what_is_missing_and_fails_closed() {
    let assets = published_assets();

    // Every asset published.
    let run = run_release_check(&release_host(all_found(&assets)));
    assert_eq!(run.code, Some(0), "{}\n{}", run.stdout, run.stderr);
    assert_eq!(lines_with(&run, "present").len(), assets.len());
    assert!(run.stdout.contains(PUBLISHED_LINE), "{}", run.stdout);

    // One missing: exit 1, and that asset alone is named.
    let (gone_asset, gone_path) = &assets[&("btrfs".to_string(), "amd64".to_string())];
    let mut answers = all_found(&assets);
    answers.remove(gone_path);
    let run = run_release_check(&release_host(answers.clone()));
    assert_eq!(run.code, Some(1), "{}\n{}", run.stdout, run.stderr);
    assert_eq!(
        lines_with(&run, "MISSING"),
        std::slice::from_ref(gone_asset)
    );
    assert!(!run.stdout.contains(PUBLISHED_LINE), "{}", run.stdout);

    // One the host cannot answer for: exit 2, and it is not called missing.
    answers.insert(
        gone_path.clone(),
        http_answer("500 Internal Server Error", ""),
    );
    let run = run_release_check(&release_host(answers));
    assert_eq!(run.code, Some(2), "{}\n{}", run.stdout, run.stderr);
    assert!(lines_with(&run, "MISSING").is_empty(), "{}", run.stdout);
    assert_eq!(lines_with(&run, "UNKNOWN").len(), 1, "{}", run.stdout);
    assert!(!run.stdout.contains(PUBLISHED_LINE), "{}", run.stdout);

    // Nothing listening: exit 2, and nothing is called missing or present.
    let closed = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port())
    };
    let run = run_release_check(&closed);
    assert_eq!(run.code, Some(2), "{}\n{}", run.stdout, run.stderr);
    assert!(lines_with(&run, "MISSING").is_empty(), "{}", run.stdout);
    assert!(lines_with(&run, "present").is_empty(), "{}", run.stdout);
    assert_eq!(lines_with(&run, "UNKNOWN").len(), assets.len());
}

/// GitHub answers a release download with a 302 to the asset's storage. A
/// renamed or transferred repository answers 301 for every path under its old
/// name, whether the asset exists or not, so no other redirect may read as
/// present: the check would stay green for good without checking anything.
#[test]
fn the_release_check_does_not_read_another_redirect_as_present() {
    let assets = published_assets();
    let (_, moved_path) = &assets[&("btrfs".to_string(), "amd64".to_string())];
    for status in [
        "301 Moved Permanently",
        "303 See Other",
        "307 Temporary Redirect",
        "308 Permanent Redirect",
    ] {
        let mut answers = all_found(&assets);
        answers.insert(
            moved_path.clone(),
            http_answer(status, "Location: http://127.0.0.1:1/elsewhere\r\n"),
        );
        let run = run_release_check(&release_host(answers));
        assert_eq!(
            run.code,
            Some(2),
            "{status}:\n{}\n{}",
            run.stdout,
            run.stderr
        );
        assert_eq!(
            lines_with(&run, "present").len(),
            assets.len() - 1,
            "{status}:\n{}",
            run.stdout
        );
        assert_eq!(
            lines_with(&run, "UNKNOWN").len(),
            1,
            "{status}:\n{}",
            run.stdout
        );
        assert!(
            lines_with(&run, "MISSING").is_empty(),
            "{status}:\n{}",
            run.stdout
        );
        assert!(
            !run.stdout.contains(PUBLISHED_LINE),
            "{status}:\n{}",
            run.stdout
        );
    }
}

/// A leg whose identity cannot be derived is a leg the check did not check. A
/// stale `kernel_sha` is the ordinary way to get there. It must read UNKNOWN,
/// fail the run, and never be followed by the all-published line: with the
/// bookkeeping for that branch removed, the script printed UNKNOWN, then said
/// all six assets were published, and exited 0.
#[test]
fn the_release_check_fails_a_leg_whose_identity_it_cannot_derive() {
    let sha = sha256_short(b"a\n");
    let mut tables = String::new();
    let mut answers = BTreeMap::new();
    for profile in ["default", "nested", "btrfs"] {
        for (arch, machine) in [("arm64", "aarch64"), ("amd64", "x86_64")] {
            tables.push_str(&format!(
                "[kernel_profiles.{profile}.{arch}]\n\
                 kernel_version = \"1.2.3\"\n\
                 kernel_repo = \"example/kernels\"\n\
                 build_inputs = [\"inputs/a.conf\"]\n"
            ));
            if profile == "default" {
                // The arm64 default table carries a kernel_sha its inputs do not hash to.
                let manifest = if arch == "arm64" {
                    "000000000000"
                } else {
                    sha.as_str()
                };
                tables.push_str(&format!("kernel_sha = \"{manifest}\"\n"));
            }
            tables.push('\n');
            answers.insert(
                format!(
                    "/example/kernels/releases/download/kernel-{profile}-1.2.3-{machine}-{sha}/\
                     vmlinux-{profile}-1.2.3-{machine}-{sha}.bin"
                ),
                found_answer(),
            );
        }
    }
    let fixture = identity_fixture(&tables, &[("inputs/a.conf", "a\n")]);

    let run = run_release_check_at(fixture.path(), &release_host(answers));
    assert_eq!(run.code, Some(2), "{}\n{}", run.stdout, run.stderr);
    let unknown = lines_with(&run, "UNKNOWN");
    assert_eq!(unknown.len(), 1, "{}", run.stdout);
    assert!(
        unknown[0].starts_with("default arm64") && unknown[0].contains("kernel_sha"),
        "{}",
        run.stdout
    );
    assert_eq!(lines_with(&run, "present").len(), 5, "{}", run.stdout);
    assert!(lines_with(&run, "MISSING").is_empty(), "{}", run.stdout);
    assert!(!run.stdout.contains(PUBLISHED_LINE), "{}", run.stdout);
}

/// Exit 1 means an asset is missing. Every way of failing to check exits 2
/// with a message instead, never 1 under a traceback.
#[test]
fn the_release_check_exits_2_whenever_it_cannot_check() {
    let cannot_check = |what: &str, run: &StepRun| {
        assert_eq!(run.code, Some(2), "{what}:\n{}\n{}", run.stdout, run.stderr);
        assert!(
            run.stderr.contains("ERROR") && !run.stderr.contains("Traceback"),
            "{what}:\n{}",
            run.stderr
        );
        assert!(
            lines_with(run, "MISSING").is_empty(),
            "{what}:\n{}",
            run.stdout
        );
        assert!(
            !run.stdout.contains(PUBLISHED_LINE),
            "{what}:\n{}",
            run.stdout
        );
    };

    // An answer that is not HTTP.
    let assets = published_assets();
    let (_, garbled_path) = &assets[&("btrfs".to_string(), "amd64".to_string())];
    let mut answers = all_found(&assets);
    answers.insert(garbled_path.clone(), "garbage\r\n\r\n".to_string());
    let run = run_release_check(&release_host(answers));
    cannot_check("an answer that is not HTTP", &run);
    assert_eq!(lines_with(&run, "UNKNOWN").len(), 1, "{}", run.stdout);

    // A base URL with no scheme. A bare host name is the spelling that matters:
    // urllib reads `host:port` as a scheme and fails in a way the script already
    // caught, and refuses a bare name with a ValueError it did not.
    let run = run_release_check("release-host.invalid");
    cannot_check("a base URL with no scheme", &run);
    assert!(lines_with(&run, "present").is_empty(), "{}", run.stdout);

    // A config whose kernel_profiles is not a table.
    let fixture = identity_fixture("kernel_profiles = \"oops\"\n", &[]);
    let run = run_release_check_at(fixture.path(), &release_host(BTreeMap::new()));
    cannot_check("kernel_profiles that is not a table", &run);
    assert_eq!(lines_with(&run, "UNKNOWN").len(), 6, "{}", run.stdout);

    // An identity script that does not load.
    let fixture = identity_fixture("", &[]);
    std::fs::write(fixture.path().join(IDENTITY_SCRIPT), "def (\n").unwrap();
    let run = run_release_check_at(fixture.path(), &release_host(BTreeMap::new()));
    cannot_check("an identity script that does not load", &run);
    assert!(lines_with(&run, "present").is_empty(), "{}", run.stdout);
}

/// Upstream commit 0d0eff39ceb3 ("virtio_ring: fix infinite loop in
/// virtnet_poll_cleantx when device is broken") is not in the pinned 6.18 stable
/// release. Without it about one guest shutdown in 200 never finishes:
/// device_shutdown() breaks the virtio-net TX queue while completions are still
/// unreclaimed, virtnet_poll_cleantx() then spins in softirq holding the TX queue
/// lock, and the guest never reaches its power-off call. Every kernel fcvm ships as
/// a guest has to carry the backport until the pinned release does.
#[test]
fn every_shipped_guest_kernel_applies_the_virtio_ring_shutdown_fix() {
    const FIX: &str = "the backend will never update it";
    let root = repo_root();
    let mut missing = Vec::new();
    let mut checked = 0usize;
    for (profile_name, arch, profile) in source_built_tables(&rootfs_config()) {
        if !["default", "nested", "btrfs"].contains(&profile_name.as_str()) {
            continue;
        }
        checked += 1;
        let applies_fix = vm_kernel_patches_dir(&profile).is_some_and(|dir| {
            applied_patches(&root, dir)
                .iter()
                .any(|patch| std::fs::read_to_string(patch).is_ok_and(|text| text.contains(FIX)))
        });
        if !applies_fix {
            missing.push(format!("{profile_name}.{arch}"));
        }
    }
    assert_eq!(
        checked, 6,
        "expected default, nested and btrfs tables for both arches"
    );
    assert!(
        missing.is_empty(),
        "these guest kernels do not apply the virtio_ring broken-queue fix: {missing:?}"
    );
}

//! Compile-time guards for architecture-specific kernel patches.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

/// The excerpts in `fixtures/vncr/nested.c` stand in for the pinned kernel's
/// source. When the pin moved without them, the test below kept applying the
/// patch to a file the build never sees, and stayed green while the patch no
/// longer applied to the pinned kernel.
#[test]
fn the_vncr_fixture_is_cut_from_the_pinned_arm64_kernel() {
    let config: toml::Value = toml::from_str(
        &fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("rootfs-config.toml"))
            .expect("read rootfs-config.toml"),
    )
    .expect("parse rootfs-config.toml");
    let pinned = config["kernel_profiles"]["nested"]["arm64"]["kernel_version"]
        .as_str()
        .expect("nested.arm64 pins a kernel_version");
    let fixture = include_str!("fixtures/vncr/nested.c");
    assert!(
        fixture.contains(&format!("Linux v{pinned} arch/arm64/kvm/nested.c")),
        "tests/fixtures/vncr/nested.c is not cut from the pinned Linux {pinned}. Cut the \
         excerpts again from that release's arch/arm64/kvm/nested.c and record its SHA-256 in \
         the header. Editing the header alone hides a stale fixture: the VNCR code changed in \
         6.18.51 and 6.18.52"
    );
}

/// Exercise the pinned kernel's VNCR functions, unpatched: 6.18.52 took
/// upstream 5949004d7032 ("KVM: arm64: nv: Fully update VNCR fixmap state in
/// kvm_translate_vncr()"), which replaced the patch fcvm carried for it. Only
/// their dependencies are stubbed, so a pin without the fix (6.18.51 and
/// earlier) leaves the stale CPU/flag state that trips the BUG_ON in
/// kvm_vcpu_put_hw_mmu(), without running a kernel-crashing NV2 guest.
#[test]
fn arm64_vncr_retranslation_releases_cpu_mapping_before_vcpu_put() {
    let temp = tempfile::tempdir().expect("create isolated VNCR fixture");
    let source_dir = temp.path().join("arch/arm64/kvm");
    fs::create_dir_all(&source_dir).unwrap();
    fs::write(
        source_dir.join("nested.c"),
        include_str!("fixtures/vncr/nested.c"),
    )
    .unwrap();
    fs::write(
        temp.path().join("harness.c"),
        include_str!("fixtures/vncr/harness.c"),
    )
    .unwrap();

    let binary = temp.path().join("vncr-regression");
    let compiled = Command::new("cc")
        .args(["-std=gnu11", "-Werror=implicit-function-declaration", "-o"])
        .arg(&binary)
        .arg(temp.path().join("harness.c"))
        .output()
        .expect("compile the actual VNCR functions with test dependencies");
    assert!(
        compiled.status.success(),
        "VNCR fixture failed to compile:\n{}",
        String::from_utf8_lossy(&compiled.stderr)
    );

    let mut failures = Vec::new();
    for (scenario, name) in [
        ("1", "mapped successful retranslation"),
        ("2", "mapped translation failure"),
        ("0", "unmapped control"),
    ] {
        let result = Command::new(&binary).arg(scenario).output().unwrap();
        if !result.status.success() {
            failures.push(format!(
                "{name}: {}",
                String::from_utf8_lossy(&result.stderr)
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Bash's `local value=$(command)` reports the status of `local`, not of the
/// command substitution. A failed profile lookup must therefore be assigned
/// separately or validation can incorrectly treat the missing value as an
/// intentionally empty `patches_dir` and return success.
#[test]
fn kernel_patch_commands_do_not_mask_profile_lookup_failures() {
    let script_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scripts/kernel-patch.sh");
    let script = fs::read_to_string(&script_path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", script_path.display()));

    assert!(
        !script.contains("local version=$(get_kernel_version \"$profile\")"),
        "`local version=$(...)` masks a failed kernel-version lookup"
    );
    assert!(
        !script.contains("local patches_dir=$(get_patches_dir \"$profile\")"),
        "`local patches_dir=$(...)` masks a failed patch-directory lookup"
    );
    assert_eq!(
        script.matches("local version patches_dir").count(),
        4,
        "create, finish, edit, and validate must declare lookup results before assignment"
    );
    assert_eq!(
        script
            .matches("version=$(get_kernel_version \"$profile\")")
            .count(),
        4,
        "all four kernel-patch command flows must resolve the version separately"
    );
    assert_eq!(
        script
            .matches("patches_dir=$(get_patches_dir \"$profile\")")
            .count(),
        4,
        "all four kernel-patch command flows must resolve patches_dir separately"
    );
}

/// A helper invoked from a checked command substitution runs in a context
/// where `set -e` does not stop its body. Propagate the architecture lookup
/// explicitly so an unsupported architecture cannot fall through to a
/// different profile or patch directory.
#[test]
fn kernel_patch_helpers_propagate_architecture_lookup_failures() {
    let script_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scripts/kernel-patch.sh");
    let script = fs::read_to_string(&script_path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", script_path.display()));

    assert_eq!(
        script
            .matches("config_arch=$(get_config_arch) || return 1")
            .count(),
        2,
        "both kernel-version and patch-directory helpers must return when architecture resolution fails"
    );
    assert_eq!(
        script
            .lines()
            .filter(|line| line.trim() == "config_arch=$(get_config_arch)")
            .count(),
        0,
        "a bare architecture assignment can continue with an empty architecture when the caller checks the helper status"
    );
}

/// The four kernel files `nv2-targeted-shadow-s2-unmap.patch` changes. Each
/// `tests/fixtures/nv2-s2-tracking-<major>.<minor>` directory holds excerpts of
/// them cut from one pinned release.
const S2_TRACKING_FILES: [&str; 4] = [
    "arch/arm64/include/asm/kvm_host.h",
    "arch/arm64/include/asm/kvm_nested.h",
    "arch/arm64/kvm/mmu.c",
    "arch/arm64/kvm/nested.c",
];

/// One kernel that carries `nv2-targeted-shadow-s2-unmap.patch`: the release it
/// pins, the patch in its own patch set, and the fixture cut from that release.
struct S2Port {
    /// The table in rootfs-config.toml that pins it.
    table: &'static str,
    release: String,
    patch: PathBuf,
    fixture: PathBuf,
}

/// The arm64 nested guest kernel and its host kernel both carry the patch, each
/// from its own `patches_dir` (#659: a 7.3 host under 6.18 guests). Read both
/// from rootfs-config.toml, so the harness runs the patch the build applies,
/// against excerpts of the release the build compiles.
fn s2_ports() -> Vec<S2Port> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let config: toml::Value = toml::from_str(
        &fs::read_to_string(root.join("rootfs-config.toml")).expect("read rootfs-config.toml"),
    )
    .expect("parse rootfs-config.toml");
    let nested = &config["kernel_profiles"]["nested"]["arm64"];
    [
        ("kernel_profiles.nested.arm64", nested),
        (
            "kernel_profiles.nested.arm64.host_kernel",
            &nested["host_kernel"],
        ),
    ]
    .into_iter()
    .map(|(table, value)| {
        let release = value["kernel_version"]
            .as_str()
            .unwrap_or_else(|| panic!("[{table}] pins a kernel_version"))
            .to_string();
        let patches_dir = value["patches_dir"]
            .as_str()
            .unwrap_or_else(|| panic!("[{table}] names a patches_dir"));
        let series = release.splitn(3, '.').take(2).collect::<Vec<_>>().join(".");
        S2Port {
            table,
            patch: root
                .join(patches_dir)
                .join("nv2-targeted-shadow-s2-unmap.patch"),
            fixture: root.join(format!("tests/fixtures/nv2-s2-tracking-{series}")),
            release,
        }
    })
    .collect()
}

/// Fixtures cut from another release would let the patch apply here and fail
/// to apply in the build, or the reverse.
#[test]
fn the_nv2_s2_tracking_fixture_is_cut_from_the_pinned_arm64_kernels() {
    for port in s2_ports() {
        for file in S2_TRACKING_FILES {
            let text = fs::read_to_string(port.fixture.join(file)).unwrap_or_else(|e| {
                panic!(
                    "[{}] pins Linux {}, and {} has no {file} excerpt: {e}",
                    port.table,
                    port.release,
                    port.fixture.display()
                )
            });
            assert!(
                text.contains(&format!("Linux v{} {file}", port.release)),
                "{}/{file} is not cut from Linux {}, which [{}] pins. Cut the same line ranges \
                 again from that release and record its SHA-256 in the header.",
                port.fixture.display(),
                port.release,
                port.table
            );
        }
    }
}

/// Copy the lines from the one starting with `start` through the first line
/// equal to `end` (or just that line, for a `#define`).
fn extract_block(text: &str, start: &str, end: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let first = lines.iter().position(|line| line.starts_with(start))?;
    if start.starts_with("#define") {
        return Some(format!("{}\n", lines[first]));
    }
    let last = first + lines[first..].iter().position(|line| *line == end)?;
    Some(format!("{}\n\n", lines[first..=last].join("\n")))
}

/// Run each pinned kernel's shadow stage-2 unmap paths, with its own
/// `nv2-targeted-shadow-s2-unmap.patch` applied to the excerpts in its
/// `tests/fixtures/nv2-s2-tracking-<series>`, in a C harness that stubs the
/// page tables and logs every range KVM asks them to unmap (fcvm #1274).
///
/// Without the patch, an MMU notifier invalidation of one canonical page, or a
/// flush of a one-page memslot, unmaps every nested MMU over its whole IPA
/// range, and the `targeted` and `memslot` scenarios fail on that. With it they
/// must unmap only the shadow mappings of the invalidated range. The other
/// scenarios cover what the patch adds: record removal on a nested unmap, one
/// record per refaulted block, a record naming the right MMU after
/// kvm_vcpu_init_nested() adds MMUs, and an invalidation that rescheduled while
/// kvm_arch_flush_shadow_all() freed the shadow page tables and the canonical
/// MMU (built with AddressSanitizer, so a read of freed memory aborts).
#[test]
fn nv2_invalidation_unmaps_only_the_shadow_mappings_of_the_invalidated_range() {
    let mut failures = Vec::new();
    for port in s2_ports() {
        failures.extend(
            run_s2_tracking_harness(&port)
                .into_iter()
                .map(|failure| format!("[{}] Linux {}: {failure}", port.table, port.release)),
        );
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The kernel functions the harness compiles, in source order:
/// (file, first line, last line, present only with the patch). The 6.18
/// backport frees records in kvm_destroy_guest_s2_mappings(); the 7.3 port
/// (upstream's series from v7.4-rc1) frees them in guest_s2_tracking_destroy()
/// when kvm_free_stage2_pgd() tears down the canonical MMU.
fn s2_tracking_blocks(series: &str) -> Vec<(&'static str, &'static str, &'static str, bool)> {
    const HOST_H: &str = "arch/arm64/include/asm/kvm_host.h";
    const MMU_C: &str = "arch/arm64/kvm/mmu.c";
    const NESTED_C: &str = "arch/arm64/kvm/nested.c";
    let upstream = series != "6.18";
    let mut blocks = vec![
        (HOST_H, "struct kvm_guest_s2_mapping {", "};", true),
        (NESTED_C, "#define S2_MMU_PER_VCPU", "", false),
    ];
    if upstream {
        blocks.push((NESTED_C, "#define S2_MMU_PER_VM", "", true));
    }
    blocks.extend([
        (NESTED_C, "int kvm_init_nested(", "}", false),
        (NESTED_C, "void kvm_destroy_nested(", "}", false),
        (NESTED_C, "static int init_nested_s2_mmu(", "}", false),
        (NESTED_C, "int kvm_vcpu_init_nested(", "}", false),
        (NESTED_C, "#define S2_MMU_IDX_MASK", "", true),
        (NESTED_C, "static void tag_s2_mapping_mmu(", "}", true),
        (
            NESTED_C,
            "static struct kvm_s2_mmu *s2_mapping_to_mmu(",
            "}",
            true,
        ),
        (
            NESTED_C,
            "static unsigned long s2_mapping_to_nested_start(",
            "}",
            true,
        ),
        (NESTED_C, "void kvm_record_guest_s2_mapping(", "}", true),
        (NESTED_C, "void kvm_remove_guest_s2_mappings(", "}", true),
    ]);
    if !upstream {
        blocks.push((NESTED_C, "void kvm_destroy_guest_s2_mappings(", "}", true));
    }
    blocks.extend([
        (NESTED_C, "void kvm_init_nested_s2_mmu(", "}", false),
        (
            NESTED_C,
            "static void kvm_invalidate_vncr_ipa_all(",
            "}",
            false,
        ),
        (NESTED_C, "void kvm_nested_unmap_cipa_range(", "}", true),
        (NESTED_C, "void kvm_nested_s2_unmap(", "}", false),
        (NESTED_C, "void kvm_arch_flush_shadow_all(", "}", false),
        (
            MMU_C,
            "static phys_addr_t __stage2_range_addr_end(",
            "}",
            false,
        ),
        (
            MMU_C,
            "static phys_addr_t stage2_range_addr_end(",
            "}",
            false,
        ),
        (MMU_C, "static int stage2_apply_range(", "}", false),
        (
            MMU_C,
            "static int kvm_pgtable_stage2_unmap_tracked(",
            "}",
            true,
        ),
        (MMU_C, "static void __unmap_stage2_range(", "}", false),
        (MMU_C, "void kvm_stage2_unmap_range(", "}", false),
        (MMU_C, "int kvm_init_stage2_mmu(", "}", false),
        (MMU_C, "void kvm_uninit_stage2_mmu(", "}", false),
    ]);
    if upstream {
        blocks.push((MMU_C, "static void guest_s2_tracking_destroy(", "}", true));
    }
    blocks.extend([
        (MMU_C, "void kvm_free_stage2_pgd(", "}", false),
        (MMU_C, "bool kvm_unmap_gfn_range(", "}", false),
        (MMU_C, "void kvm_arch_flush_shadow_memslot(", "}", false),
    ]);
    blocks
}

/// Build the harness for one port and run its scenarios; returns the failures.
fn run_s2_tracking_harness(port: &S2Port) -> Vec<String> {
    let release = port.release.as_str();
    let series = release.splitn(3, '.').take(2).collect::<Vec<_>>().join(".");
    let temp = tempfile::tempdir().expect("create the shadow stage-2 fixture directory");
    for file in S2_TRACKING_FILES {
        let dest = temp.path().join(file);
        fs::create_dir_all(dest.parent().unwrap()).unwrap();
        fs::copy(port.fixture.join(file), &dest)
            .unwrap_or_else(|e| panic!("copy the {file} excerpt: {e}"));
    }

    // Without the patch, run the release's own functions and fail on what they
    // unmap, not on the missing file.
    let patched = port.patch.exists();
    if patched {
        let applied = Command::new("patch")
            .args(["--batch", "--forward", "--fuzz=0", "-p1", "-i"])
            .arg(&port.patch)
            .current_dir(temp.path())
            .output()
            .expect("run patch");
        assert!(
            applied.status.success(),
            "{} does not apply to the {release} excerpts:\n{}{}",
            port.patch.display(),
            String::from_utf8_lossy(&applied.stdout),
            String::from_utf8_lossy(&applied.stderr)
        );
    }

    let mut extracted = String::new();
    if patched {
        extracted.push_str("#define HAVE_GUEST_S2_TRACKING 1\n");
    }
    for (file, start, end, with_patch) in s2_tracking_blocks(&series) {
        let source = fs::read_to_string(temp.path().join(file)).unwrap();
        match extract_block(&source, start, end) {
            Some(block) => extracted.push_str(&block),
            None => assert!(
                with_patch && !patched,
                "`{start}` is missing from the {} {release} excerpts",
                if patched { "patched" } else { "unpatched" }
            ),
        }
    }
    fs::write(temp.path().join("extracted.c"), extracted).unwrap();
    fs::copy(
        port.fixture.join("harness.c"),
        temp.path().join("harness.c"),
    )
    .unwrap();

    let binary = temp.path().join("s2-tracking");
    let compiled = Command::new("cc")
        .args([
            "-std=gnu11",
            "-g",
            "-O1",
            "-pthread",
            "-fsanitize=address,undefined",
            "-fno-sanitize-recover=all",
            "-Werror=implicit-function-declaration",
            "-Werror=incompatible-pointer-types",
            "-o",
        ])
        .arg(&binary)
        .arg(temp.path().join("harness.c"))
        .output()
        .expect("run cc");
    assert!(
        compiled.status.success(),
        "the {release} shadow stage-2 harness failed to compile:\n{}",
        String::from_utf8_lossy(&compiled.stderr)
    );

    let mut scenarios = vec!["targeted", "memslot"];
    if patched {
        scenarios.extend(["tlbi_removal", "tlbi_refault", "grow", "teardown_race"]);
    }
    let mut failures = Vec::new();
    for scenario in scenarios {
        // Without address randomization: GCC 13's AddressSanitizer crashes in a loop of
        // "AddressSanitizer:DEADLYSIGNAL" for some layouts when vm.mmap_rnd_bits is 32
        // (Ubuntu 24.04's kernels), which failed this test at random on CI's x64 lanes.
        let result = Command::new("setarch")
            .args([std::env::consts::ARCH, "-R"])
            .arg(&binary)
            .arg(scenario)
            // The harness counts its own allocations; LeakSanitizer needs
            // ptrace, which some CI containers refuse.
            .env("ASAN_OPTIONS", "detect_leaks=0")
            .output()
            .expect("run the shadow stage-2 harness");
        if !result.status.success() {
            failures.push(format!(
                "{scenario}: {}",
                String::from_utf8_lossy(&result.stderr)
            ));
        }
    }
    failures
}

/// Committing the whole kernel tree leaves tens of thousands of loose objects, so
/// git starts `gc --auto` in the background after the commit. `validate` then
/// removes its work directory while that gc still writes into `.git`, and the
/// removal fails with "Directory not empty", failing every validate run.
#[test]
fn the_kernel_source_repository_runs_no_background_gc() {
    let script = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scripts/kernel-patch.sh"),
    )
    .expect("read scripts/kernel-patch.sh");
    let setup = extract_block(&script, "setup_kernel_source() {", "}")
        .expect("kernel-patch.sh defines setup_kernel_source");
    let init = setup
        .find("git init")
        .expect("setup_kernel_source makes a repository");
    let commit = setup[init..]
        .find("git commit")
        .map(|at| init + at)
        .expect("setup_kernel_source commits the tree");
    assert!(
        setup[init..commit].contains("git config gc.auto 0"),
        "setup_kernel_source must turn off automatic gc before its first commit"
    );
}

/// The NFS identity renewal (#1737) replaces an NFSv4.0 client's owner identifier only while it
/// holds the state manager's running bit, so no manager iteration is part way through a
/// SETCLIENTID under the old identifier or with the purge verifier when the purge is flagged;
/// that SETCLIENTID would have told the server the snapshot's client rebooted and dropped the
/// state of every other copy. It leaves v4.1 and later alone, whose session trunking sends
/// EXCHANGE_ID, which reads the identifier, outside the state manager. Pinned in the patch
/// text: the kernel build is the only other place it runs.
#[test]
fn the_nfs_identity_renewal_holds_the_state_manager_and_skips_sessions() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let name = "0006-nfs-give-restored-clients-identities-of-their-own.vm.patch";
    let arm64 = root.join("kernel/patches-arm64").join(name);
    let x86 = root
        .join("kernel/patches-x86")
        .join("0007-nfs-give-restored-clients-identities-of-their-own.vm.patch");
    assert_eq!(
        fs::canonicalize(&x86).expect("the x86 nested patch set links the NFS patch"),
        fs::canonicalize(&arm64).unwrap(),
        "the x86 nested kernel applies the same NFS patch as the arm64 one"
    );
    let patch = fs::read_to_string(&arm64).expect("read the NFS patch");
    let renew_one = extract_block(
        &patch,
        "+static void nfs4_renew_client_identity(struct nfs_client *clp, u32 salt)",
        "+}",
    )
    .expect("the patch defines nfs4_renew_client_identity");
    let mut at = 0;
    for step in [
        "wait_on_bit_lock_action(&clp->cl_state, NFS4CLNT_MANAGER_RUNNING",
        "spin_lock(&nn->nfs_client_lock);",
        "clp->cl_owner_id = NULL;",
        "spin_unlock(&nn->nfs_client_lock);",
        "set_bit(NFS4CLNT_PURGE_STATE, &clp->cl_state);",
        "nfs4_clear_state_manager_bit(clp);",
        "nfs4_schedule_state_manager(clp);",
    ] {
        let found = renew_one[at..]
            .find(step)
            .unwrap_or_else(|| panic!("nfs4_renew_client_identity lacks, or misorders, `{step}`"));
        at += found + step.len();
    }
    let renew_all = extract_block(
        &patch,
        "+void nfs4_renew_client_identities(struct net *net, u32 salt)",
        "+}",
    )
    .expect("the patch defines nfs4_renew_client_identities");
    assert!(
        renew_one.contains("clp->cl_owner_salt = salt;") && !renew_one.contains("get_random"),
        "nfs4_renew_client_identity salts the identifier with the kernel's own randomness, which \
         twin clones restored from one snapshot share"
    );
    assert!(
        renew_all.contains("clp->cl_minorversion != 0"),
        "nfs4_renew_client_identities renews v4.1+ clients, whose trunking reads the identifier \
         outside the state manager"
    );
}

/// The host kernel of a profile can carry a different release and patch set from its guest
/// kernel (#659: a 7.3 host under 6.18 guests). `kernel-patch.sh` must then work on the
/// host's own `[kernel_profiles.<p>.<arch>.host_kernel]` table, not the guest's.
#[test]
fn kernel_patch_resolves_a_host_kernel_profile_from_its_own_table() {
    let root = tempfile::tempdir().expect("create the fixture checkout");
    fs::create_dir_all(root.path().join("scripts")).unwrap();
    let script = root.path().join("scripts/kernel-patch.sh");
    fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scripts/kernel-patch.sh"),
        &script,
    )
    .expect("copy kernel-patch.sh");
    fs::write(
        root.path().join("rootfs-config.toml"),
        "[kernel_profiles.nested.arm64]\n\
         kernel_version = \"6.18.60\"\n\
         patches_dir = \"kernel/patches-arm64\"\n\n\
         [kernel_profiles.nested.arm64.host_kernel]\n\
         kernel_version = \"7.3.3\"\n\
         patches_dir = \"kernel/patches-arm64-host\"\n",
    )
    .unwrap();
    let resolve = |profile: &str| {
        let out = Command::new("bash")
            .arg(&script)
            .args(["resolve", profile])
            .env("KERNEL_PATCH_ARCH", "aarch64")
            .output()
            .expect("run kernel-patch.sh resolve");
        assert!(
            out.status.success(),
            "resolve {profile} failed: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    };
    let root = root.path().display();
    assert_eq!(
        resolve("nested.host_kernel"),
        format!("version=7.3.3\npatches_dir={root}/kernel/patches-arm64-host\n")
    );
    assert_eq!(
        resolve("nested"),
        format!("version=6.18.60\npatches_dir={root}/kernel/patches-arm64\n")
    );

    // A guest-only patch is not the host's to edit: the host build never applies it.
    let patches = std::path::Path::new(&root.to_string()).join("kernel/patches-arm64-host");
    fs::create_dir_all(&patches).unwrap();
    fs::write(patches.join("0006-guest-only.vm.patch"), "").unwrap();
    let edit = Command::new("bash")
        .arg(&script)
        .args(["edit", "nested.host_kernel", "0006"])
        .env("KERNEL_PATCH_ARCH", "aarch64")
        .output()
        .expect("run kernel-patch.sh edit");
    assert!(
        !edit.status.success() && String::from_utf8_lossy(&edit.stderr).contains("guest-only"),
        "edit on the host profile took a *.vm.patch: {}{}",
        String::from_utf8_lossy(&edit.stdout),
        String::from_utf8_lossy(&edit.stderr)
    );
}

/// The three kernel files the arm64 host's vgic-v3 patch changes, excerpted in
/// `tests/fixtures/vgic-v3-hcr-<major>.<minor>` from the release the host pins.
const VGIC_HCR_FILES: [&str; 3] = [
    "arch/arm64/kvm/hyp/vgic-v3-sr.c",
    "arch/arm64/kvm/vgic/vgic.c",
    "include/kvm/arm_vgic.h",
];

const VGIC_HCR_PATCH: &str = "vgic-v3-keep-hcr-on-quiet-exits.patch";

/// The arm64 host kernel's release, one patch of its patch set and the fixture
/// `tests/fixtures/<fixture>-<major>.<minor>` cut from that release.
fn host_kernel_port(patch: &str, fixture: &str) -> (String, PathBuf, PathBuf) {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let config: toml::Value = toml::from_str(
        &fs::read_to_string(root.join("rootfs-config.toml")).expect("read rootfs-config.toml"),
    )
    .expect("parse rootfs-config.toml");
    let host = &config["kernel_profiles"]["nested"]["arm64"]["host_kernel"];
    let release = host["kernel_version"]
        .as_str()
        .expect("[kernel_profiles.nested.arm64.host_kernel] pins a kernel_version")
        .to_string();
    let patches_dir = host["patches_dir"]
        .as_str()
        .expect("[kernel_profiles.nested.arm64.host_kernel] names a patches_dir");
    let series = release.splitn(3, '.').take(2).collect::<Vec<_>>().join(".");
    (
        release,
        root.join(patches_dir).join(patch),
        root.join(format!("tests/fixtures/{fixture}-{series}")),
    )
}

/// The arm64 host kernel's release, its patch set and the vgic-v3 fixture cut from it.
fn vgic_hcr_port() -> (String, PathBuf, PathBuf) {
    host_kernel_port(VGIC_HCR_PATCH, "vgic-v3-hcr")
}

/// A fixture cut from another release would let the patch apply here and fail to
/// apply in the host build, or the reverse.
#[test]
fn the_vgic_hcr_fixture_is_cut_from_the_pinned_arm64_host_kernel() {
    let (release, _, fixture) = vgic_hcr_port();
    for file in VGIC_HCR_FILES {
        let text = fs::read_to_string(fixture.join(file)).unwrap_or_else(|e| {
            panic!(
                "the arm64 host pins Linux {release}, and {} has no {file} excerpt: {e}",
                fixture.display()
            )
        });
        assert!(
            text.contains(&format!("Linux v{release} {file}")),
            "{}/{file} is not cut from Linux {release}, which the arm64 host kernel pins. \
             Cut the same line range again from that release and record its SHA-256 in the \
             header.",
            fixture.display()
        );
    }
}

/// Run the arm64 host kernel's GICv3 world switch with its own
/// `vgic-v3-keep-hcr-on-quiet-exits.patch` applied, in a C harness that models one CPU's
/// virtual CPU interface and computes ICH_MISR_EL2 from it (fcvm #659): vcpu load and put
/// and guest entry and exit from `arch/arm64/kvm/hyp/vgic-v3-sr.c`, and the run loop's
/// `kvm_vgic_flush_hwstate()` and `kvm_vgic_sync_hwstate()` from
/// `arch/arm64/kvm/vgic/vgic.c`.
///
/// Since 6.19 every exit walks the ap_list under its lock to fill and prune it, folds the
/// LRs back, clears ICH_HCR_EL2 and reads ICH_MISR_EL2, and every entry writes
/// ICH_HCR_EL2 again, whether or not an interrupt is in flight. On a c9g.metal-48xl host that took an HVC
/// exit from 396 ns on 6.18.60 to 453 ns on 7.3.3. Without the patch the `loop` scenario
/// fails on that work. `matrix` and `putload` pass with or without it: they fail a change
/// that leaves the host holding a register state that raises a maintenance interrupt, or
/// that enters the guest with ICH_HCR_EL2 other than what the vgic computed.
#[test]
fn quiet_vgic_exits_leave_ich_hcr_el2_programmed_without_a_host_maintenance_interrupt() {
    let (release, patch, fixture) = vgic_hcr_port();
    let temp = tempfile::tempdir().expect("create the vgic-v3 fixture directory");
    for file in VGIC_HCR_FILES {
        let dest = temp.path().join(file);
        fs::create_dir_all(dest.parent().unwrap()).unwrap();
        fs::copy(fixture.join(file), &dest)
            .unwrap_or_else(|e| panic!("copy the {file} excerpt: {e}"));
    }

    // Without the patch, run the release's own functions and fail on what they do.
    let patched = patch.exists();
    if patched {
        let applied = Command::new("patch")
            .args(["--batch", "--forward", "--fuzz=0", "-p1", "-i"])
            .arg(&patch)
            .current_dir(temp.path())
            .output()
            .expect("run patch");
        assert!(
            applied.status.success(),
            "{} does not apply to the {release} excerpts:\n{}{}",
            patch.display(),
            String::from_utf8_lossy(&applied.stdout),
            String::from_utf8_lossy(&applied.stderr)
        );
    }

    let read = |file: &str| fs::read_to_string(temp.path().join(file)).unwrap();
    let types = extract_block(
        &read("include/kvm/arm_vgic.h"),
        "struct vgic_v3_cpu_if {",
        "};",
    )
    .expect("struct vgic_v3_cpu_if is missing from the arm_vgic.h excerpt");
    fs::write(temp.path().join("extracted-types.c"), types).unwrap();
    let mut extracted = String::new();
    for (file, start, with_patch) in [
        (VGIC_HCR_FILES[0], "static u64 compute_ich_hcr(", false),
        (
            VGIC_HCR_FILES[0],
            "static bool __vgic_v3_hcr_can_signal_mi(",
            true,
        ),
        (VGIC_HCR_FILES[0], "void __vgic_v3_save_state(", false),
        (VGIC_HCR_FILES[0], "void __vgic_v3_restore_state(", false),
        (VGIC_HCR_FILES[0], "void __vgic_v3_activate_traps(", false),
        (VGIC_HCR_FILES[0], "void __vgic_v3_deactivate_traps(", false),
        (
            VGIC_HCR_FILES[1],
            "static inline void vgic_save_state(",
            false,
        ),
        (VGIC_HCR_FILES[1], "void kvm_vgic_sync_hwstate(", false),
        (
            VGIC_HCR_FILES[1],
            "static inline void vgic_restore_state(",
            false,
        ),
        (VGIC_HCR_FILES[1], "static void vgic_flush_no_lrs(", true),
        (VGIC_HCR_FILES[1], "static void vgic_flush_state(", false),
        (VGIC_HCR_FILES[1], "void kvm_vgic_flush_hwstate(", false),
    ] {
        match extract_block(&read(file), start, "}") {
            Some(block) => extracted.push_str(&block),
            None => assert!(
                with_patch && !patched,
                "`{start}` is missing from the {} {release} excerpts",
                if patched { "patched" } else { "unpatched" }
            ),
        }
    }
    fs::write(temp.path().join("extracted.c"), extracted).unwrap();
    fs::copy(fixture.join("harness.c"), temp.path().join("harness.c")).unwrap();

    let binary = temp.path().join("vgic-hcr");
    let compiled = Command::new("cc")
        .args([
            "-std=gnu11",
            "-g",
            "-O1",
            "-fsanitize=address,undefined",
            "-fno-sanitize-recover=all",
            "-Werror=implicit-function-declaration",
            "-Werror=incompatible-pointer-types",
            "-o",
        ])
        .arg(&binary)
        .arg(temp.path().join("harness.c"))
        .output()
        .expect("run cc");
    assert!(
        compiled.status.success(),
        "the {release} vgic-v3 harness failed to compile:\n{}",
        String::from_utf8_lossy(&compiled.stderr)
    );

    let mut failures = Vec::new();
    for scenario in ["loop", "matrix", "putload"] {
        // As for the shadow stage-2 harness: no address randomization, no LeakSanitizer.
        let result = Command::new("setarch")
            .args([std::env::consts::ARCH, "-R"])
            .arg(&binary)
            .arg(scenario)
            .env("ASAN_OPTIONS", "detect_leaks=0")
            .output()
            .expect("run the vgic-v3 harness");
        if !result.status.success() {
            let stderr = String::from_utf8_lossy(&result.stderr);
            let first: Vec<&str> = stderr.lines().take(5).collect();
            failures.push(format!("{scenario}: {}", first.join("\n  ")));
        }
    }
    assert!(
        failures.is_empty(),
        "Linux {release} with{} {VGIC_HCR_PATCH}:\n{}",
        if patched { "" } else { "out" },
        failures.join("\n")
    );
}

/// The kernel files the arm64 host's guest hypervisor exit patch changes, excerpted in
/// `tests/fixtures/nv-hyp-exit-<major>.<minor>` from the release the host pins.
const NV_EXIT_FILES: [&str; 4] = [
    "arch/arm64/kvm/hyp/vhe/switch.c",
    "arch/arm64/include/asm/kvm_host.h",
    "arch/arm64/include/asm/kvm_nested.h",
    "arch/arm64/include/asm/kvm_emulate.h",
];

const NV_EXIT_PATCH: &str = "nv-guest-hypervisor-exits-without-calls.patch";

/// The guest hypervisor exit fixture with the host's patch applied when it exists, in a
/// temporary directory: (release, whether the patch was applied, directory).
fn nv_exit_excerpts() -> (String, bool, tempfile::TempDir) {
    let (release, patch, fixture) = host_kernel_port(NV_EXIT_PATCH, "nv-hyp-exit");
    let temp = tempfile::tempdir().expect("create the guest hypervisor exit fixture directory");
    for file in NV_EXIT_FILES {
        let dest = temp.path().join(file);
        fs::create_dir_all(dest.parent().unwrap()).unwrap();
        fs::copy(fixture.join(file), &dest)
            .unwrap_or_else(|e| panic!("copy the {file} excerpt: {e}"));
    }
    // Without the patch, check the release's own code and fail on what it does.
    let patched = patch.exists();
    if patched {
        let applied = Command::new("patch")
            .args(["--batch", "--forward", "--fuzz=0", "-p1", "-i"])
            .arg(&patch)
            .current_dir(temp.path())
            .output()
            .expect("run patch");
        assert!(
            applied.status.success(),
            "{} does not apply to the {release} excerpts:\n{}{}",
            patch.display(),
            String::from_utf8_lossy(&applied.stdout),
            String::from_utf8_lossy(&applied.stderr)
        );
    }
    (release, patched, temp)
}

#[test]
fn the_nv_exit_fixture_is_cut_from_the_pinned_arm64_host_kernel() {
    let (release, _, fixture) = host_kernel_port(NV_EXIT_PATCH, "nv-hyp-exit");
    for file in NV_EXIT_FILES {
        let text = fs::read_to_string(fixture.join(file)).unwrap_or_else(|e| {
            panic!(
                "the arm64 host pins Linux {release}, and {} has no {file} excerpt: {e}",
                fixture.display()
            )
        });
        assert!(
            text.contains(&format!("Linux v{release} {file}")),
            "{}/{file} is not cut from Linux {release}, which the arm64 host kernel pins. \
             Cut the same line ranges again from that release and record its SHA-256 in the \
             header.",
            fixture.display()
        );
    }
}

/// Run the arm64 host kernel's HCR_EL2 hand-off around a guest hypervisor's run, with
/// its own `nv-guest-hypervisor-exits-without-calls.patch` applied, in a C harness
/// (fcvm #659): `__compute_hcr()` and `fixup_nv_guest_exit()` from
/// `arch/arm64/kvm/hyp/vhe/switch.c`.
///
/// 7.3 (3fc7867a95c0) moved HCR_EL2 out of the VNCR page and copies it to the page's
/// NVHCR_EL2 slot on every entry in hyp context and back on every exit, each copy a
/// masked read and a masked write: four more out-of-line `kvm_vcpu_apply_reg_masks()`
/// calls per exit of a guest hypervisor's vCPU, two of them, on a host without
/// FEAT_NV3, for a register that has no masks. Without the patch the `quiet` scenario
/// fails on that masking. `write` and `nv3` pass with or without it: they fail a change
/// that leaves HCR_EL2 other than the guest's write sanitised, or publishes the guest
/// anything else on entry.
#[test]
fn a_guest_hypervisors_quiet_exit_does_not_mask_its_hcr_el2_copies() {
    let (release, patched, temp) = nv_exit_excerpts();
    let (_, _, fixture) = host_kernel_port(NV_EXIT_PATCH, "nv-hyp-exit");
    let text = fs::read_to_string(temp.path().join(NV_EXIT_FILES[0])).unwrap();
    let mut extracted = String::new();
    for start in [
        "#define NV_HCR_GUEST_EXCLUDE",
        "static u64 __compute_hcr(",
        "static void fixup_nv_guest_exit(",
    ] {
        let block = extract_block(&text, start, "}")
            .unwrap_or_else(|| panic!("`{start}` is missing from the {release} excerpt"));
        extracted.push_str(&block);
    }
    fs::write(temp.path().join("extracted.c"), extracted).unwrap();
    fs::copy(fixture.join("harness.c"), temp.path().join("harness.c")).unwrap();

    let binary = temp.path().join("nv-hcr");
    let compiled = Command::new("cc")
        .args([
            "-std=gnu11",
            "-g",
            "-O1",
            "-fsanitize=address,undefined",
            "-fno-sanitize-recover=all",
            "-Werror=implicit-function-declaration",
            "-Werror=incompatible-pointer-types",
            "-o",
        ])
        .arg(&binary)
        .arg(temp.path().join("harness.c"))
        .output()
        .expect("run cc");
    assert!(
        compiled.status.success(),
        "the {release} HCR_EL2 harness failed to compile:\n{}",
        String::from_utf8_lossy(&compiled.stderr)
    );

    let mut failures = Vec::new();
    for scenario in ["quiet", "write", "nv3"] {
        let result = Command::new(&binary)
            .arg(scenario)
            .env("ASAN_OPTIONS", "detect_leaks=0")
            .output()
            .expect("run the HCR_EL2 harness");
        if !result.status.success() {
            let stderr = String::from_utf8_lossy(&result.stderr);
            let first: Vec<&str> = stderr.lines().take(5).collect();
            failures.push(format!("{scenario}: {}", first.join("\n  ")));
        }
    }
    assert!(
        failures.is_empty(),
        "Linux {release} with{} {NV_EXIT_PATCH}:\n{}",
        if patched { "" } else { "out" },
        failures.join("\n")
    );
}

/// The accessors the VHE world switch calls on every exit of a guest hypervisor's vCPU
/// must be inlined there (fcvm #659). 7.3's 62ed180231f4 grew `__activate_traps()` and
/// `__kvm_vcpu_run_vhe()` until GCC stopped inlining `___ctxt_sys_reg()`, and with the
/// release's `static inline` the world switch calls out-of-line copies of it,
/// `vcpu_has_nv()`, `is_hyp_ctxt()` and `vcpu_el2_e2h_is_set()`: 173 more host
/// instructions per exit at that commit. `__always_inline` is what keeps a later change
/// from moving them out again, so the patched headers must declare each of them that
/// way, and apply the register masks inline in the VHE world switch, computing what
/// `kvm_vcpu_apply_reg_masks()` computes.
#[test]
fn the_world_switch_inlines_the_nv_accessors_it_calls_on_every_exit() {
    let (release, patched, temp) = nv_exit_excerpts();
    let read = |file: &str| fs::read_to_string(temp.path().join(file)).unwrap();
    let mut missing = Vec::new();
    for (file, definition) in [
        (NV_EXIT_FILES[2], "static __always_inline bool vcpu_has_nv("),
        (
            NV_EXIT_FILES[1],
            "static __always_inline u64 *___ctxt_sys_reg(",
        ),
        (
            NV_EXIT_FILES[1],
            "static __always_inline bool __vcpu_has_feature(",
        ),
        (
            NV_EXIT_FILES[3],
            "static __always_inline bool vcpu_el2_e2h_is_set(",
        ),
        (
            NV_EXIT_FILES[3],
            "static __always_inline bool vcpu_el2_tge_is_set(",
        ),
        (NV_EXIT_FILES[3], "static __always_inline bool is_hyp_ctxt("),
        (
            NV_EXIT_FILES[3],
            "static __always_inline bool is_nested_ctxt(",
        ),
    ] {
        if !read(file).lines().any(|line| line.starts_with(definition)) {
            missing.push(format!("{file}: `{definition}`"));
        }
    }
    let host = read(NV_EXIT_FILES[1]);
    let vhe = host
        .split("#ifdef __KVM_VHE_HYPERVISOR__")
        .nth(1)
        .and_then(|rest| rest.split("#else").next());
    if !vhe.is_some_and(|block| {
        block.contains("static __always_inline u64 __kvm_apply_reg_masks(")
            && block.contains("#define kvm_vcpu_apply_reg_masks(")
    }) {
        missing.push(format!(
            "{}: an `#ifdef __KVM_VHE_HYPERVISOR__` block defining kvm_vcpu_apply_reg_masks() \
             through an __always_inline helper",
            NV_EXIT_FILES[1]
        ));
    }
    assert!(
        missing.is_empty(),
        "Linux {release} with{} {NV_EXIT_PATCH} leaves the world switch calling out of line:\n{}",
        if patched { "" } else { "out" },
        missing.join("\n")
    );

    // The inline masking must compute what kvm_vcpu_apply_reg_masks() does.
    let (_, _, fixture) = host_kernel_port(NV_EXIT_PATCH, "nv-hyp-exit");
    let mut extracted = String::new();
    for (start, end) in [
        ("struct resx {", "};"),
        ("struct kvm_sysreg_masks {", "};"),
        ("static inline struct resx __kvm_get_sysreg_resx(", "}"),
        ("static __always_inline u64 __kvm_apply_reg_masks(", "}"),
    ] {
        let block = extract_block(&host, start, end)
            .unwrap_or_else(|| panic!("`{start}` is missing from the patched kvm_host.h excerpt"));
        extracted.push_str(&block);
    }
    fs::write(temp.path().join("extracted-masks.c"), extracted).unwrap();
    fs::copy(fixture.join("masks.c"), temp.path().join("masks.c")).unwrap();
    let binary = temp.path().join("masks");
    let compiled = Command::new("cc")
        .args([
            "-std=gnu11",
            "-O1",
            "-fsanitize=address,undefined",
            "-fno-sanitize-recover=all",
            "-Werror=implicit-function-declaration",
            "-o",
        ])
        .arg(&binary)
        .arg(temp.path().join("masks.c"))
        .output()
        .expect("run cc");
    assert!(
        compiled.status.success(),
        "the {release} register masking harness failed to compile:\n{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let result = Command::new(&binary)
        .env("ASAN_OPTIONS", "detect_leaks=0")
        .output()
        .expect("run the register masking harness");
    assert!(
        result.status.success(),
        "the VHE world switch's inline register masking in {NV_EXIT_PATCH} differs from \
         kvm_vcpu_apply_reg_masks():\n{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

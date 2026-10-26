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
         the header. Editing the header alone hides a stale fixture: the file is the same from \
         6.18.44 to 6.18.50 and changes in 6.18.51"
    );
}

/// Exercise the actual stable-kernel functions after applying our carried fix.
/// Stub only their dependencies, so removing the production patch reintroduces
/// the stale CPU/flag/count state without running a kernel-crashing NV2 guest.
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

    let patch = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("kernel/patches-arm64/vncr-fixmap-state.patch");
    // When the fix is absent, execute the original functions and fail on the
    // broken state, not merely on a missing patch file.
    if patch.exists() {
        let applied = Command::new("patch")
            .args(["--batch", "--forward", "--fuzz=0", "-p1", "-i"])
            .arg(patch)
            .current_dir(temp.path())
            .output()
            .expect("apply the VNCR fix to stable source excerpts");
        assert!(
            applied.status.success(),
            "VNCR patch did not apply:\n{}{}",
            String::from_utf8_lossy(&applied.stdout),
            String::from_utf8_lossy(&applied.stderr)
        );
    }

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

/// Apply the ARM64 vsock patch to a minimal source file with the same line and
/// statement layout as Linux 7.1.7, then ask a C compiler to parse it.
///
/// A zero-context hunk once inserted `dsb(sy)` between the two lines of the
/// `vsock = container_of(...)` declaration. `patch --dry-run` accepted that
/// placement, but the kernel build failed because a statement cannot appear in
/// the middle of an initializer. Keeping the split declaration in this fixture
/// makes that exact regression deterministic without downloading a kernel.
#[test]
fn arm64_vsock_rx_barrier_patch_preserves_the_lock_boundary_syntax() {
    let temp = tempfile::tempdir().expect("create isolated kernel-patch fixture");
    let source_dir = temp.path().join("net/vmw_vsock");
    let linux_include = temp.path().join("include/linux");
    let asm_include = temp.path().join("include/asm");
    fs::create_dir_all(&source_dir).expect("create fixture source directory");
    fs::create_dir_all(&linux_include).expect("create fixture Linux include directory");
    fs::create_dir_all(&asm_include).expect("create fixture ARM include directory");

    fs::write(
        linux_include.join("virtio_vsock.h"),
        r#"#include <stddef.h>
struct work_struct { int unused; };
struct delayed_work { int unused; };
struct virtqueue { int unused; };
struct mutex { int unused; };
struct virtio_vsock {
    struct delayed_work rx_work;
    struct virtqueue *vqs[1];
    struct mutex rx_lock;
    int rx_run;
};
#define VSOCK_VQ_RX 0
#define to_delayed_work(work) ((struct delayed_work *)(work))
#define container_of(ptr, type, member) ((type *)0)
#define mutex_lock(lock) ((void)(lock))
"#,
    )
    .expect("write fixture Linux header");
    fs::write(
        asm_include.join("barrier.h"),
        "#define dsb(opt) do { } while (0)\n",
    )
    .expect("write fixture ARM barrier header");

    let mut lines = Vec::new();
    while lines.len() < 18 {
        lines.push(String::new());
    }
    lines.push("#include <linux/virtio_vsock.h>".to_owned()); // Linux 7.1.7 line 19.
    while lines.len() < 630 {
        lines.push(String::new());
    }
    lines.extend([
        "static void virtio_transport_rx_work(struct work_struct *work)".to_owned(),
        "{".to_owned(),
        "\tstruct virtio_vsock *vsock =".to_owned(),
        "\t\tcontainer_of(to_delayed_work(work), struct virtio_vsock, rx_work);".to_owned(),
        "\tstruct virtqueue *vq = vsock->vqs[VSOCK_VQ_RX];".to_owned(),
        "\tsize_t buf_len;".to_owned(),
        "\tvoid *buf;".to_owned(),
        String::new(),
        "\tmutex_lock(&vsock->rx_lock);".to_owned(),
        String::new(),
        "\tif (!vsock->rx_run)".to_owned(),
        "\t\tgoto out;".to_owned(),
        "\t(void)vq;".to_owned(),
        "\t(void)buf_len;".to_owned(),
        "\t(void)buf;".to_owned(),
        "out:".to_owned(),
        "\t;".to_owned(),
        "}".to_owned(),
    ]);
    let source = source_dir.join("virtio_transport.c");
    fs::write(&source, format!("{}\n", lines.join("\n"))).expect("write fixture source");

    let patch_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("kernel/patches-arm64/nv2-vsock-rx-barrier.patch");
    let applied = Command::new("patch")
        .args(["--batch", "--forward", "-p1", "-i"])
        .arg(&patch_path)
        .current_dir(temp.path())
        .output()
        .expect("run patch on the Linux 7.1.7 fixture");
    assert!(
        applied.status.success(),
        "ARM64 vsock patch did not apply to its Linux 7.1.7 fixture:\n{}{}",
        String::from_utf8_lossy(&applied.stdout),
        String::from_utf8_lossy(&applied.stderr)
    );

    let parsed = Command::new("cc")
        .args(["-DCONFIG_ARM64", "-fsyntax-only", "-I"])
        .arg(temp.path().join("include"))
        .arg(&source)
        .output()
        .expect("run the C syntax check");
    assert!(
        parsed.status.success(),
        "ARM64 vsock patch broke C syntax at the RX lock boundary:\n{}{}",
        String::from_utf8_lossy(&parsed.stdout),
        String::from_utf8_lossy(&parsed.stderr)
    );

    let applied_source = fs::read_to_string(source).expect("read patched fixture source");
    let lock = applied_source
        .find("mutex_lock(&vsock->rx_lock);")
        .expect("patched source lost rx_lock acquisition");
    let barrier = applied_source
        .find("dsb(sy);")
        .expect("patched source lost the ARM64 DSB barrier");
    let guard = applied_source
        .find("if (!vsock->rx_run)")
        .expect("patched source lost the rx_run guard");
    assert!(
        lock < barrier && barrier < guard,
        "the DSB must execute after rx_lock acquisition and before the rx_run guard"
    );
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

/// The fixture directory for `nv2-targeted-shadow-s2-unmap.patch`, and the four
/// kernel files it holds excerpts of.
const S2_TRACKING_FIXTURE: &str = "tests/fixtures/nv2-s2-tracking";
const S2_TRACKING_FILES: [&str; 4] = [
    "arch/arm64/include/asm/kvm_host.h",
    "arch/arm64/include/asm/kvm_nested.h",
    "arch/arm64/kvm/mmu.c",
    "arch/arm64/kvm/nested.c",
];

/// The patch is applied to the host kernel and to the nested guest kernel,
/// both pinned in rootfs-config.toml. Fixtures cut from another release would
/// let the patch apply here and fail to apply in the build, or the reverse.
#[test]
fn the_nv2_s2_tracking_fixture_is_cut_from_the_pinned_arm64_kernels() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let config: toml::Value = toml::from_str(
        &fs::read_to_string(root.join("rootfs-config.toml")).expect("read rootfs-config.toml"),
    )
    .expect("parse rootfs-config.toml");
    let nested = &config["kernel_profiles"]["nested"]["arm64"];
    let guest = nested["kernel_version"]
        .as_str()
        .expect("nested.arm64 pins a kernel_version");
    let host = nested["host_kernel"]["kernel_version"]
        .as_str()
        .expect("nested.arm64.host_kernel pins a kernel_version");
    for file in S2_TRACKING_FILES {
        let text = fs::read_to_string(root.join(S2_TRACKING_FIXTURE).join(file))
            .unwrap_or_else(|e| panic!("read the {file} excerpt: {e}"));
        for pinned in [guest, host] {
            assert!(
                text.contains(&format!("Linux v{pinned} {file}")),
                "{S2_TRACKING_FIXTURE}/{file} is not cut from the pinned Linux {pinned}. Cut the \
                 same line ranges again from that release and record its SHA-256 in the header."
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

/// Run Linux 6.18.50's shadow stage-2 unmap paths, with
/// `kernel/patches-arm64/nv2-targeted-shadow-s2-unmap.patch` applied to the
/// excerpts in `tests/fixtures/nv2-s2-tracking`, in a C harness that stubs the
/// page tables and logs every range KVM asks them to unmap (fcvm #1274).
///
/// Without the patch, an MMU notifier invalidation of one canonical page, or a
/// flush of a one-page memslot, unmaps every nested MMU over its whole IPA
/// range, and the `targeted` and `memslot` scenarios fail on that. With it they
/// must unmap only the shadow mappings of the invalidated range. The other
/// scenarios cover what the patch adds: record removal on a nested unmap, a
/// record surviving kvm_vcpu_init_nested() moving the MMU array, and an
/// invalidation that rescheduled while kvm_arch_flush_shadow_all() freed that
/// array (built with AddressSanitizer, so a read of the freed array aborts).
#[test]
fn nv2_invalidation_unmaps_only_the_shadow_mappings_of_the_invalidated_range() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let temp = tempfile::tempdir().expect("create the shadow stage-2 fixture directory");
    for file in S2_TRACKING_FILES {
        let dest = temp.path().join(file);
        fs::create_dir_all(dest.parent().unwrap()).unwrap();
        fs::copy(root.join(S2_TRACKING_FIXTURE).join(file), &dest)
            .unwrap_or_else(|e| panic!("copy the {file} excerpt: {e}"));
    }

    let patch = root.join("kernel/patches-arm64/nv2-targeted-shadow-s2-unmap.patch");
    // Without the patch, run 6.18.50's own functions and fail on what they
    // unmap, not on the missing file.
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
            "nv2-targeted-shadow-s2-unmap.patch does not apply to the 6.18.50 excerpts:\n{}{}",
            String::from_utf8_lossy(&applied.stdout),
            String::from_utf8_lossy(&applied.stderr)
        );
    }

    let read = |file: &str| fs::read_to_string(temp.path().join(file)).unwrap();
    let host_h = read("arch/arm64/include/asm/kvm_host.h");
    let mmu_c = read("arch/arm64/kvm/mmu.c");
    let nested_c = read("arch/arm64/kvm/nested.c");
    // (source, first line, last line, present only with the patch)
    let blocks: &[(&str, &str, &str, bool)] = &[
        (&host_h, "struct kvm_guest_s2_mapping {", "};", true),
        (&nested_c, "#define S2_MMU_PER_VCPU", "", false),
        (&nested_c, "void kvm_init_nested(", "}", false),
        (&nested_c, "static int init_nested_s2_mmu(", "}", false),
        (&nested_c, "int kvm_vcpu_init_nested(", "}", false),
        (&nested_c, "#define S2_MMU_IDX_MASK", "", true),
        (&nested_c, "static void tag_s2_mapping_mmu(", "}", true),
        (
            &nested_c,
            "static struct kvm_s2_mmu *s2_mapping_to_mmu(",
            "}",
            true,
        ),
        (
            &nested_c,
            "static unsigned long s2_mapping_to_nested_start(",
            "}",
            true,
        ),
        (&nested_c, "void kvm_record_guest_s2_mapping(", "}", true),
        (&nested_c, "void kvm_remove_guest_s2_mappings(", "}", true),
        (&nested_c, "void kvm_destroy_guest_s2_mappings(", "}", true),
        (&nested_c, "void kvm_init_nested_s2_mmu(", "}", false),
        (&nested_c, "void kvm_nested_unmap_cipa_range(", "}", true),
        (&nested_c, "void kvm_nested_s2_unmap(", "}", false),
        (&nested_c, "void kvm_arch_flush_shadow_all(", "}", false),
        (
            &mmu_c,
            "static phys_addr_t __stage2_range_addr_end(",
            "}",
            false,
        ),
        (
            &mmu_c,
            "static phys_addr_t stage2_range_addr_end(",
            "}",
            false,
        ),
        (&mmu_c, "static int stage2_apply_range(", "}", false),
        (
            &mmu_c,
            "static int kvm_pgtable_stage2_unmap_tracked(",
            "}",
            true,
        ),
        (&mmu_c, "static void __unmap_stage2_range(", "}", false),
        (&mmu_c, "void kvm_stage2_unmap_range(", "}", false),
        (&mmu_c, "int kvm_init_stage2_mmu(", "}", false),
        (&mmu_c, "void kvm_uninit_stage2_mmu(", "}", false),
        (&mmu_c, "void kvm_free_stage2_pgd(", "}", false),
        (&mmu_c, "bool kvm_unmap_gfn_range(", "}", false),
        (&mmu_c, "void kvm_arch_flush_shadow_memslot(", "}", false),
    ];
    let mut extracted = String::new();
    if patched {
        extracted.push_str("#define HAVE_GUEST_S2_TRACKING 1\n");
    }
    for (source, start, end, with_patch) in blocks {
        match extract_block(source, start, end) {
            Some(block) => extracted.push_str(&block),
            None => assert!(
                *with_patch && !patched,
                "`{start}` is missing from the {} excerpts",
                if patched { "patched" } else { "6.18.50" }
            ),
        }
    }
    fs::write(temp.path().join("extracted.c"), extracted).unwrap();
    fs::copy(
        root.join(S2_TRACKING_FIXTURE).join("harness.c"),
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
        "the shadow stage-2 harness failed to compile:\n{}",
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
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

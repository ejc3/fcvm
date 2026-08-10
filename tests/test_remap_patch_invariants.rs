//! Guards on the FUSE `remap_file_range` kernel patch.
//!
//! These exist because the 6.18.3 -> 7.0.14 kernel rebase silently dropped the
//! u32-truncation fix from the patch, and nothing caught it: the largest FICLONE
//! test in the tree clones 1 MiB, while the defect only appears above 4 GiB.
//! A reviewer reading the patch found it. No test could have.
//!
//! The defect: `fuse_write_out.size` is a `u32`, so a clone larger than 4 GiB
//! saturates it. Using that value to update the destination inode records ~4 GiB
//! for a larger file, and subsequent guest reads past the cached boundary come
//! back short even though the host clone completed correctly.
//!
//! Why a static check on the patch text rather than a >4 GiB runtime clone:
//! the runtime path needs the patched guest kernel built and booted, which makes
//! it a poor guard against the thing that actually went wrong — a rebase
//! dropping a hunk. This catches that at `cargo test` speed, with no kernel.
//! `test_ficlone_cp_reflink_in_vm` covers the runtime behaviour by booting the
//! exact patched nested-profile kernel; it never substitutes the runner's
//! potentially older host kernel for the artifact under test.

use std::path::PathBuf;

fn repo_file(path: &str) -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(path);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()))
}

fn patch_text() -> String {
    repo_file("kernel/patches/0001-fuse-add-remap_file_range-support.patch")
}

fn added_fuse_remap_callback(patch: &str) -> String {
    let added: Vec<&str> = patch
        .lines()
        .filter_map(|line| {
            if line.starts_with("+++") {
                None
            } else {
                line.strip_prefix('+')
            }
        })
        .collect();
    let start = added
        .iter()
        .position(|line| line.starts_with("static loff_t fuse_remap_file_range("))
        .expect("added fuse_remap_file_range callback is missing");
    let end = added[start..]
        .iter()
        .position(|line| *line == "}")
        .map(|offset| start + offset)
        .expect("added fuse_remap_file_range callback is unterminated");
    added[start..=end].join("\n")
}

fn toml_section<'a>(config: &'a str, name: &str) -> &'a str {
    let marker = format!("[{name}]");
    let start = config
        .lines()
        .position(|line| line == marker)
        .unwrap_or_else(|| panic!("missing TOML section {marker}"));
    let lines: Vec<&str> = config.lines().collect();
    let end = lines[start + 1..]
        .iter()
        .position(|line| line.starts_with('['))
        .map(|offset| start + 1 + offset)
        .unwrap_or(lines.len());
    let byte_start: usize = lines[..=start].iter().map(|line| line.len() + 1).sum();
    let byte_end: usize = lines[..end].iter().map(|line| line.len() + 1).sum();
    &config[byte_start.min(config.len())..byte_end.min(config.len())]
}

/// The clone length must be derived from the REQUEST, not from the 32-bit reply.
#[test]
fn remap_patch_derives_length_from_the_request_not_the_u32_reply() {
    let patch = patch_text();
    assert!(
        patch.contains("effective"),
        "the `effective` length variable is gone from the remap patch. That is \
         the u32-truncation fix: without it the destination inode is updated \
         from `fuse_write_out.size`, a u32 that saturates above 4 GiB, so a \
         >4 GiB FICLONE records ~4 GiB and later reads come back short. This \
         hunk was dropped once already by the 6.18.3 -> 7.0.14 rebase."
    );
}

/// The token alone is not enough: preserve the complete request-derived data
/// flow so `effective = outarg.size` cannot satisfy the regression guard.
#[test]
fn remap_patch_propagates_the_prepared_length_through_inode_updates() {
    let patch = patch_text();
    let added: Vec<&str> = patch
        .lines()
        .filter(|line| line.starts_with('+') && !line.starts_with("+++"))
        .map(|line| line.trim_start_matches('+').trim())
        .collect();

    assert!(
        added.contains(&"effective = len;"),
        "the clone result must come from the length prepared in the request"
    );
    assert!(
        !added
            .iter()
            .any(|line| line.contains("effective = outarg.size")),
        "the 32-bit reply must never feed the effective clone length"
    );
    assert!(
        added
            .iter()
            .any(|line| line.contains("ALIGN(pos_out + effective, PAGE_SIZE)")),
        "cache invalidation must cover the request-derived effective length"
    );
    assert!(
        added.iter().any(|line| {
            line.contains("fuse_write_update_attr(inode_out, pos_out + effective, effective)")
        }),
        "the destination inode update must use the request-derived effective length"
    );
}

/// The reason must survive next to the code, or the next rebase drops it again.
#[test]
fn remap_patch_explains_why_outarg_size_is_unusable() {
    let patch = patch_text();
    let explains = patch.contains("u32") || patch.contains("32-bit") || patch.contains("4 GiB");
    assert!(
        explains,
        "the remap patch no longer explains WHY `outarg.size` must not be used \
         to update the inode. The previous rebase dropped this fix precisely \
         because nothing in the patch said it was load-bearing."
    );
}

/// Added lines must not feed `outarg.size` into the post-success inode update.
/// Anchored on added (`+`) lines so context from surrounding kernel code, which
/// legitimately references the field, does not trip it.
#[test]
fn remap_patch_does_not_size_the_destination_from_outarg() {
    let patch = patch_text();
    let offenders: Vec<&str> = patch
        .lines()
        .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
        .filter(|l| l.contains("outarg.size"))
        // Assigning INTO the struct, or reading it to build `effective`, is fine;
        // what must not happen is sizing the destination directly from it.
        .filter(|l| {
            let t = l.trim_start_matches('+').trim();
            !t.starts_with("outarg.size =") && !t.contains("effective")
        })
        .filter(|l| {
            l.contains("i_size_write")
                || l.contains("truncate_setsize")
                || l.contains("fuse_write_update_attr")
                || l.contains("invalidate_inode_pages2_range")
        })
        .collect();

    assert!(
        offenders.is_empty(),
        "these added lines size the destination from the 32-bit `outarg.size` \
         instead of the request-derived `effective` length, which truncates any \
         clone above 4 GiB:\n{}",
        offenders.join("\n")
    );
}

/// Dedupe has different validation and metadata semantics from clone. Until the
/// FUSE protocol implements those semantics, the kernel callback must reject it
/// before generic preparation or any destination mutation.
#[test]
fn remap_patch_rejects_dedupe_before_preparation_or_mutation() {
    let patch = patch_text();
    let callback = added_fuse_remap_callback(&patch);
    let reject = callback
        .find("if (remap_flags & REMAP_FILE_DEDUP)")
        .expect("remap patch must explicitly reject REMAP_FILE_DEDUP");
    let prepare = callback
        .find("generic_remap_file_range_prep")
        .expect("remap patch must use generic remap preparation");

    assert!(
        reject < prepare,
        "REMAP_FILE_DEDUP must be rejected before generic preparation can mutate \
         destination metadata"
    );
}

/// `vfs_clone_file_range()` does not run the generic preparation helper for a
/// filesystem callback. The callback therefore owns canonical two-inode
/// locking and generic range validation/preparation.
#[test]
fn remap_patch_prepares_the_range_under_canonical_two_inode_locking() {
    let patch = patch_text();
    let callback = added_fuse_remap_callback(&patch);
    let lock = callback
        .find("lock_two_nondirectories(inode_in, inode_out)")
        .expect("remap patch must acquire the canonical two-inode lock");
    let prepare = callback
        .find("generic_remap_file_range_prep(file_in, pos_in, file_out, pos_out,")
        .expect("remap patch must prepare and validate the caller's range");
    let unlock = callback
        .find("unlock_two_nondirectories(inode_in, inode_out)")
        .expect("remap patch must release the canonical two-inode lock");

    assert!(
        lock < prepare && prepare < unlock,
        "generic remap preparation must run while both inodes are locked"
    );
    assert!(
        callback[prepare..unlock].contains("&len, remap_flags)"),
        "generic remap preparation must be allowed to adjust the concrete request length"
    );
    assert!(
        !callback.lines().any(|line| {
            line.contains("inode_lock(inode_in)") || line.contains("inode_lock(inode_out)")
        }),
        "separate source/destination inode locks leave a source-change race; use \
         lock_two_nondirectories()"
    );
}

/// Removed lines and added code outside the callback must not satisfy its
/// semantic guards.
#[test]
fn added_remap_callback_scope_excludes_diff_decoys() {
    let synthetic = r#"--- a/fs/fuse/file.c
+++ b/fs/fuse/file.c
-if (remap_flags & REMAP_FILE_DEDUP)
+static loff_t fuse_remap_file_range(void)
+{
+	return 0;
+}
+static void unrelated(void)
+{
+	lock_two_nondirectories(inode_in, inode_out);
+	generic_remap_file_range_prep(file_in, pos_in, file_out, pos_out, &len, remap_flags);
+	unlock_two_nondirectories(inode_in, inode_out);
+}
"#;
    let callback = added_fuse_remap_callback(synthetic);

    assert!(
        !callback.contains("REMAP_FILE_DEDUP"),
        "removed lines must not satisfy callback semantic checks"
    );
    assert!(
        !callback.contains("lock_two_nondirectories"),
        "unrelated added functions must not satisfy callback semantic checks"
    );
}

/// This VM test explicitly boots the patched nested profile on btrfs. Once
/// those preconditions are met, ENOSYS means the promised end-to-end backend
/// path is broken and must fail rather than silently pass as a skip.
#[test]
fn nested_profile_remap_test_does_not_skip_enosys() {
    let test = repo_file("tests/test_remap_file_range.rs");

    assert!(
        !test.contains("code == 38"),
        "the nested-profile VM remap test still treats ENOSYS (exit 38) as a \
         skip, so CI can pass without exercising FICLONE end to end"
    );
}

/// Runtime coverage must keep both sides of the 32-bit boundary proof on the
/// branch-built guest kernel: exact length plus readable data at the last byte.
#[test]
fn nested_profile_remap_test_covers_request_lengths_above_u32() {
    let test = repo_file("tests/test_remap_file_range.rs");
    let helper_start = test
        .find("async fn run_remap_test_in_vm")
        .expect("nested-profile VM test helper is missing");
    let test_start = test
        .find("async fn test_ficlone_cp_reflink_in_vm")
        .expect("FICLONE VM regression is missing");
    let next_test = test[test_start + 1..]
        .find("#[tokio::test]")
        .map(|offset| test_start + 1 + offset)
        .unwrap_or(test.len());
    let helper = &test[helper_start..test_start];
    let regression = &test[test_start..next_test];

    assert!(
        helper.contains("\"--kernel-profile\"") && helper.contains("\"nested\""),
        "the >4 GiB regression must boot the branch-built nested-profile kernel"
    );
    assert!(
        regression.contains("size=5368709120"),
        "the runtime clone must remain larger than u32::MAX"
    );
    assert!(
        regression.contains("stat -c %s dest.bin")
            && regression.contains("test \"$actual\" -eq \"$size\""),
        "the runtime regression must verify the exact destination length"
    );
    assert!(
        regression.contains("tail -c 1 dest.bin")
            && regression.contains("cmp source.tail dest.tail"),
        "the runtime regression must read data beyond the 4 GiB boundary"
    );
}

/// These profiles are user-deployable, so they must pin a kernel that is both
/// kernel.org-supported and actually works as an NV2 L1. Linux 7.0.14 reached
/// EOL on 2026-06-27. Worse, every 7.x kernel probed to date (7.0.14, 7.1.7,
/// 7.1.8) wedges nested guests: the L2 kernel boots and stays scheduled, but
/// its userspace freezes at the first burst of virtio-blk reads against the
/// FUSE-backed root disk, while the same test is green on 6.18.x with every
/// other variable held fixed (same host kernel, same Firecracker, same box).
/// Until a fixed 7.x release is verified, the pin stays on the 6.18 longterm
/// line.
#[test]
fn deployable_kernel_profiles_pin_a_supported_working_kernel() {
    let config = repo_file("rootfs-config.toml");

    assert!(
        !config.contains("kernel_version = \"7.0.14\""),
        "rootfs-config.toml still pins deployable profiles to EOL Linux 7.0.14"
    );
    for broken in ["7.0.14", "7.1.7", "7.1.8"] {
        assert!(
            !config.contains(&format!("kernel_version = \"{broken}\"")),
            "rootfs-config.toml pins {broken}, which wedges nested (NV2) guests: L2 \
             userspace freezes on its first heavy virtio-blk reads of the FUSE-backed \
             root disk (verified RED on c7gd.metal, 2026-08-10, with 6.18.x green as \
             the control)"
        );
    }

    for profile in [
        "kernel_profiles.nested.arm64",
        "kernel_profiles.nested.arm64.host_kernel",
        "kernel_profiles.nested.amd64",
        "kernel_profiles.nested.amd64.host_kernel",
        "kernel_profiles.btrfs.arm64",
        "kernel_profiles.btrfs.amd64",
    ] {
        let section = toml_section(&config, profile);
        assert!(
            section
                .lines()
                .any(|line| line.trim() == "kernel_version = \"6.18.44\""),
            "deployable profile [{profile}] must pin exact supported longterm Linux 6.18.44"
        );
    }
}

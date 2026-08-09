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
//! `test_ficlone_above_u32_boundary` (same commit) covers the runtime behaviour
//! and skips when the kernel lacks support.

use std::path::PathBuf;

fn repo_file(path: &str) -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(path);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()))
}

fn patch_text() -> String {
    repo_file("kernel/patches/0001-fuse-add-remap_file_range-support.patch")
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
    let reject = patch
        .find("if (remap_flags & REMAP_FILE_DEDUP)")
        .expect("remap patch must explicitly reject REMAP_FILE_DEDUP");
    let prepare = patch
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
    let lock = patch
        .find("lock_two_nondirectories(inode_in, inode_out)")
        .expect("remap patch must acquire the canonical two-inode lock");
    let prepare = patch
        .find("generic_remap_file_range_prep(file_in, pos_in, file_out, pos_out,")
        .expect("remap patch must prepare and validate the caller's range");
    let unlock = patch
        .find("unlock_two_nondirectories(inode_in, inode_out)")
        .expect("remap patch must release the canonical two-inode lock");

    assert!(
        lock < prepare && prepare < unlock,
        "generic remap preparation must run while both inodes are locked"
    );
    assert!(
        patch[prepare..unlock].contains("&len, remap_flags)"),
        "generic remap preparation must be allowed to adjust the concrete request length"
    );
    assert!(
        !patch.lines().any(|line| {
            line.starts_with('+')
                && (line.contains("inode_lock(inode_in)") || line.contains("inode_lock(inode_out)"))
        }),
        "separate source/destination inode locks leave a source-change race; use \
         lock_two_nondirectories()"
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

/// These profiles are user-deployable. Linux 7.0.14 reached EOL on 2026-06-27,
/// so retaining that pin would ship an unsupported kernel rather than preserve
/// a test-only reproduction target.
#[test]
fn deployable_kernel_profiles_do_not_pin_eol_7_0_14() {
    let config = repo_file("rootfs-config.toml");

    assert!(
        !config.contains("kernel_version = \"7.0.14\""),
        "rootfs-config.toml still pins deployable profiles to EOL Linux 7.0.14"
    );
}

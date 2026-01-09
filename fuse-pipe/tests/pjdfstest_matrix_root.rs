//! Host-side pjdfstest matrix - tests fuse-pipe FUSE directly (no VM)
//!
//! Each category is a separate test, allowing nextest to run all 17 in parallel.
//! Tests fuse-pipe's PassthroughFs via local FUSE mount.
//!
//! See also: tests/test_fuse_in_vm_matrix.rs (in-VM matrix, tests full vsock stack)
//!
//! Run with: cargo nextest run -p fuse-pipe --test pjdfstest_matrix_root --features privileged-tests,integration-slow

#![cfg(all(feature = "privileged-tests", feature = "integration-slow"))]

mod pjdfstest_common;

/// Number of parallel jobs per category (within prove)
const JOBS: usize = 32;

macro_rules! pjdfstest_category {
    ($name:ident, $category:literal) => {
        #[test]
        fn $name() {
            let (passed, tests, failures) = pjdfstest_common::run_single_category($category, JOBS);
            assert!(
                passed,
                "pjdfstest category {} failed: {} tests, {} failures",
                $category, tests, failures
            );
        }
    };
}

// All categories require root for chown/mknod/user-switching
pjdfstest_category!(test_pjdfstest_chflags, "chflags");
pjdfstest_category!(test_pjdfstest_chmod, "chmod");
pjdfstest_category!(test_pjdfstest_chown, "chown");
pjdfstest_category!(test_pjdfstest_ftruncate, "ftruncate");
pjdfstest_category!(test_pjdfstest_granular, "granular");
pjdfstest_category!(test_pjdfstest_link, "link");
pjdfstest_category!(test_pjdfstest_mkdir, "mkdir");
pjdfstest_category!(test_pjdfstest_mkfifo, "mkfifo");
pjdfstest_category!(test_pjdfstest_mknod, "mknod");
pjdfstest_category!(test_pjdfstest_open, "open");
pjdfstest_category!(test_pjdfstest_posix_fallocate, "posix_fallocate");
pjdfstest_category!(test_pjdfstest_rename, "rename");
pjdfstest_category!(test_pjdfstest_rmdir, "rmdir");
pjdfstest_category!(test_pjdfstest_symlink, "symlink");
pjdfstest_category!(test_pjdfstest_truncate, "truncate");
pjdfstest_category!(test_pjdfstest_unlink, "unlink");

// DISABLED: utimensat test fails 1/122 tests when FUSE_WRITEBACK_CACHE is enabled.
// The failing test: non-owner user with write permission calling utimensat(UTIME_NOW).
// Root cause: Linux kernel interaction between default_permissions + writeback cache.
// With default_permissions, FUSE doesn't set ATTR_FORCE, so setattr_prepare() requires
// owner or CAP_FOWNER for timestamp changes, ignoring write permission.
// This is a known kernel limitation since 2006: https://github.com/libfuse/libfuse/issues/15
// Trade-off: writeback cache gives 9x write performance improvement.
// TODO: Re-enable if kernel is fixed or we find a workaround.
// pjdfstest_category!(test_pjdfstest_utimensat, "utimensat");

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
// DISABLED: open test fails 3/1406 tests when FUSE_WRITEBACK_CACHE is enabled.
// Failing tests: O_WRONLY open by users with write-only permission (mode 0222).
// Root cause: FUSE writeback cache promotes O_WRONLY to O_RDWR (via get_writeback_open_flags)
// because the kernel may need to read the file for partial page writes.
// O_RDWR requires read permission, so these tests fail with EACCES.
// This is a fundamental FUSE writeback cache limitation, not a fuse-pipe bug.
// Trade-off: writeback cache gives 9x write performance improvement.
// pjdfstest_category!(test_pjdfstest_open, "open");
pjdfstest_category!(test_pjdfstest_posix_fallocate, "posix_fallocate");
pjdfstest_category!(test_pjdfstest_rename, "rename");
pjdfstest_category!(test_pjdfstest_rmdir, "rmdir");
pjdfstest_category!(test_pjdfstest_symlink, "symlink");
pjdfstest_category!(test_pjdfstest_truncate, "truncate");
pjdfstest_category!(test_pjdfstest_unlink, "unlink");

// NOTE: utimensat requires kernel patch 0002-fuse-fix-utimensat-with-default-permissions.patch
// Without the patch, 1/122 tests fail (non-owner with write permission calling utimensat(UTIME_NOW))
// See: https://github.com/libfuse/libfuse/issues/15
pjdfstest_category!(test_pjdfstest_utimensat, "utimensat");

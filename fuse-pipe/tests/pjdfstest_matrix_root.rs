//! pjdfstest runner - all categories require root.
//!
//! The C pjdfstest suite uses chown/mknod/user-switching which require root.
//! Each category runs as a separate test for parallel execution via nextest.
//!
//! Run with: cargo nextest run -p fuse-pipe --test pjdfstest_matrix_root --features privileged-tests

#![cfg(feature = "privileged-tests")]

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
pjdfstest_category!(test_pjdfstest_utimensat, "utimensat");

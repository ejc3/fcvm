//! Serialized access to the process-wide environment, for tests.
//!
//! `std::env::set_var` changes state every thread in the process shares.
//! Under nextest that is invisible, because each test gets its own process.
//! Plain `cargo test` runs the whole library suite in one process with a
//! thread per test, so a test that empties PATH makes any sibling that
//! resolves a program by name fail with a bare ENOENT while it does so. About
//! twenty tests in this crate spawn by bare name: `sh`, `bash`, `true`,
//! `sleep`, `cat`, `git`, `tar`, `xz`, `ip`.
//!
//! Measured at 3f5bfddb on a 192-core box, when the Firecracker
//! resolution-order test still set `PATH=""`: 42 of 50 plain
//! `cargo test -p fcvm --lib` runs failed, across seven different siblings,
//! most often `setup::kernel::tests::vm_kernel_build_replaces_interrupted_cached_source_archive`
//! (spawns `tar`, 41 runs) and
//! `uffd::server::tests::dropping_unpolled_admitted_task_kills_the_pinned_vmm`
//! (spawns `sleep`, 7 runs). Skipping only that one test and changing nothing
//! else: 2 of 30, both
//! `firecracker::vm::tests::start_reports_immediate_firecracker_exit`, which
//! spawns by absolute path and times out under the load of 192 test threads.
//! No ENOENT.
//!
//! Two rules follow.
//!
//! A test whose subject can take the value as a parameter must do that and
//! leave the environment alone. `setup::rootfs::setup_vm_firecracker_bin_from`
//! reads `FCVM_FIRECRACKER_BIN` and PATH and hands both to
//! `setup_vm_firecracker_bin_resolved`, so the resolution-order test states
//! "nothing on PATH" as an argument instead of emptying the real one.
//!
//! A test that genuinely needs the process environment goes through
//! [`lock_process_env`], and so does a test whose own result depends on
//! resolving a program name through PATH. [`ProcessEnv::set`] and
//! [`ProcessEnv::unset`] exist only on the handle the lock hands back, so
//! taking the lock is a consequence of mutating rather than something each
//! test has to remember. The scan in
//! `no_test_mutates_the_process_environment_outside_this_module` holds the
//! other half: no test may reach `std::env::set_var` directly.

use std::ffi::{OsStr, OsString};

/// Held by every test that mutates the process environment, and by every test
/// whose result depends on resolving a program name through PATH.
///
/// A tokio mutex because the one async holder keeps it across an await, and
/// because it does not poison when a holder's assertion panics.
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Take the environment, from a synchronous test.
pub(crate) fn lock_process_env() -> ProcessEnv {
    ProcessEnv::new(ENV_LOCK.blocking_lock())
}

/// Take the environment, from an async test. `blocking_lock` panics inside a
/// runtime, so an async holder needs this one.
pub(crate) async fn lock_process_env_async() -> ProcessEnv {
    ProcessEnv::new(ENV_LOCK.lock().await)
}

/// Take the environment only if it is free, for the test that observes the
/// exclusion itself.
pub(crate) fn try_lock_process_env() -> Option<ProcessEnv> {
    ENV_LOCK.try_lock().ok().map(ProcessEnv::new)
}

/// Exclusive use of the process environment, with the original values put
/// back on drop.
///
/// Restoring on drop rather than on a test's last line matters for the same
/// reason the lock does: an assertion that fires early must not leave PATH or
/// `FCVM_FIRECRACKER_BIN` at a test's value for the rest of the run.
pub(crate) struct ProcessEnv {
    /// First-seen value per key, so repeated writes still restore the original.
    restore: Vec<(&'static str, Option<OsString>)>,
    /// Released after `Drop::drop` has put every value back: a struct's fields
    /// are dropped after its own `Drop` runs, so the next holder never sees a
    /// half-restored environment.
    _lock: tokio::sync::MutexGuard<'static, ()>,
}

impl ProcessEnv {
    fn new(lock: tokio::sync::MutexGuard<'static, ()>) -> Self {
        Self {
            restore: Vec::new(),
            _lock: lock,
        }
    }

    /// Set a variable for as long as this handle lives.
    pub(crate) fn set(&mut self, key: &'static str, value: impl AsRef<OsStr>) {
        self.record(key);
        std::env::set_var(key, value);
    }

    /// Unset a variable for as long as this handle lives.
    pub(crate) fn unset(&mut self, key: &'static str) {
        self.record(key);
        std::env::remove_var(key);
    }

    fn record(&mut self, key: &'static str) {
        if !self.restore.iter().any(|(seen, _)| *seen == key) {
            self.restore.push((key, std::env::var_os(key)));
        }
    }
}

impl Drop for ProcessEnv {
    fn drop(&mut self) {
        for (key, previous) in std::mem::take(&mut self.restore).into_iter().rev() {
            match previous {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// A name nothing in the crate reads, so "unset before" is a fact rather
    /// than an assumption.
    const PROBE: &str = "FCVM_ENV_GUARD_PROBE";

    /// The handle has to survive the case it exists for: an assertion that
    /// fires while a variable is held at a test's value.
    ///
    /// RED BEFORE THE FIX (the handle without its Drop): `PATH was left at the
    /// test's value after a panic`.
    #[test]
    fn a_process_env_handle_restores_its_variables_through_a_panic() {
        // A variable that was set keeps its value. PATH is the one this module
        // is about, and it is set in every environment fcvm builds in.
        let before = {
            let _held = lock_process_env();
            std::env::var_os("PATH").expect("PATH is set for this process")
        };
        let panicked = std::panic::catch_unwind(|| {
            let mut env = lock_process_env();
            env.set("PATH", "/fcvm-test-only");
            assert_eq!(
                std::env::var_os("PATH").as_deref(),
                Some(OsStr::new("/fcvm-test-only"))
            );
            env.unset("PATH");
            panic!("the assertion this stands in for");
        });
        assert!(panicked.is_err(), "the probe did not panic");

        // A variable that was not set stays unset.
        let panicked = std::panic::catch_unwind(|| {
            let mut env = lock_process_env();
            env.unset(PROBE);
            env.set(PROBE, "during");
            panic!("the assertion this stands in for");
        });
        assert!(panicked.is_err(), "the probe did not panic");

        // Both reads happen under the lock. Every mutation in the suite is
        // scoped to a handle that restores on drop, so what a lock holder sees
        // is the value the process started with, whatever a sibling test is in
        // the middle of. Reading them unlocked would race the bridged PATH
        // prepend.
        let _held = lock_process_env();
        assert_eq!(
            std::env::var_os("PATH"),
            Some(before),
            "PATH was left at the test's value after a panic"
        );
        assert_eq!(
            std::env::var_os(PROBE),
            None,
            "a variable the test invented outlived it"
        );
    }

    /// Two mutators cannot overlap. Stated with `try_lock` rather than a
    /// sleeping thread, so the observation is a fact and not a deadline.
    #[test]
    fn a_second_mutator_cannot_take_the_environment_while_one_holds_it() {
        let held = lock_process_env();
        assert!(
            try_lock_process_env().is_none(),
            "a second handle was issued while the first was alive, so two \
             tests could mutate the environment at once"
        );
        drop(held);
        // Release is asserted by taking it again, which BLOCKS when a sibling
        // is mid-mutation instead of reporting a failure. A `try_lock` here
        // was red 30 times out of 30 under plain `cargo test`, which is the
        // suite working; a handle that never released hangs here rather than
        // passing.
        drop(lock_process_env());
    }

    /// The half of the rule the type cannot state: no test may call
    /// `std::env::set_var` or `remove_var` itself and bypass the lock.
    ///
    /// RED BEFORE THE FIX: 12 sites, the whole set the Codex finding is about.
    ///   network/bridged.rs:685: std::env::set_var("PATH", format!("{}:{}", ...
    ///   network/bridged.rs:689: std::env::set_var("PATH", prev);
    ///   setup/rootfs.rs:2595: std::env::set_var(key, value);
    ///   setup/rootfs.rs:2605: std::env::remove_var(key);
    ///   setup/rootfs.rs:2611: std::env::set_var(self.key, value);
    ///   setup/rootfs.rs:2616: std::env::remove_var(self.key);
    ///   setup/rootfs.rs:2623: Some(value) => std::env::set_var(self.key, value),
    ///   setup/rootfs.rs:2624: None => std::env::remove_var(self.key),
    ///   setup/rootfs.rs:2765: std::env::set_var(KEY, "before");
    ///   setup/rootfs.rs:2780: std::env::remove_var(KEY);
    ///   setup/rootfs.rs:2801: std::env::set_var("FCVM_CONFIG_DIR", "relative/not-absolute");
    ///   setup/rootfs.rs:2804: std::env::remove_var("FCVM_CONFIG_DIR");
    ///
    /// Production writers are listed here rather than excluded by location:
    /// there is exactly one, it runs before any thread exists, and a second
    /// one deserves to be read before it is added.
    #[test]
    fn no_test_mutates_the_process_environment_outside_this_module() {
        /// (file name, exact source line) of every permitted production write.
        const PRODUCTION_WRITERS: &[(&str, &str)] = &[(
            "utils.rs",
            r#"unsafe { std::env::set_var("PATH", updated) };"#,
        )];

        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        collect_rust_sources(&src, &mut files);
        assert!(
            files.len() > 30,
            "the walk found {} files under {}, so this test would pass by \
             seeing nothing",
            files.len(),
            src.display()
        );

        let mut offenders = Vec::new();
        for file in &files {
            let name = file
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            if name == "test_env.rs" {
                continue;
            }
            let text = std::fs::read_to_string(file).expect("reading a crate source file");
            for (index, line) in text.lines().enumerate() {
                let trimmed = line.trim();
                if trimmed.starts_with("//")
                    || (!trimmed.contains("set_var(") && !trimmed.contains("remove_var("))
                {
                    continue;
                }
                if PRODUCTION_WRITERS
                    .iter()
                    .any(|(f, l)| *f == name && *l == trimmed)
                {
                    continue;
                }
                offenders.push(format!(
                    "{}:{}: {trimmed}",
                    file.strip_prefix(&src).unwrap_or(file).display(),
                    index + 1
                ));
            }
        }

        assert!(
            offenders.is_empty(),
            "these lines mutate the process environment without crate::test_env's \
             lock; route them through lock_process_env(), or pass the value to \
             the code under test as a parameter:\n  {}",
            offenders.join("\n  ")
        );
    }

    fn collect_rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("reading a crate source directory") {
            let path = entry.expect("reading a directory entry").path();
            if path.is_dir() {
                collect_rust_sources(&path, out);
            } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                out.push(path);
            }
        }
    }
}

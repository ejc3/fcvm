//! The rule this crate's tests follow about the process-wide environment: they
//! do not touch it.
//!
//! `std::env::set_var` and `remove_var` change state every thread in the
//! process shares, and on Unix they are undefined behaviour while any other
//! thread reads the environment. Rust 2024 makes both `unsafe` for exactly
//! that reason. A mutex among the writers does not fix it: the readers are
//! `std::env::var`, `Command::new` resolving a bare program name, and every
//! library call underneath them, none of which take a lock.
//!
//! Under nextest the sharing is invisible, because each test gets its own
//! process. Plain `cargo test` runs the whole library suite in one process
//! with a thread per test, so about twenty tests here are resolving `sh`,
//! `bash`, `true`, `sleep`, `cat`, `git`, `tar`, `xz` or `ip` through PATH at
//! any moment.
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
//! With every mutation removed rather than locked: 30 of 30 plain
//! `cargo test --release --no-default-features -p fcvm --lib` runs passed,
//! 539 tests each, on a 64-core box carrying other work.
//!
//! Two ways to write the test instead, both in use here.
//!
//! Pass the value to the code under test. `setup::rootfs::find_config_file`
//! reads `FCVM_CONFIG_DIR` and hands it to `find_config_file_with`, and
//! `setup_vm_firecracker_bin_from` reads `FCVM_FIRECRACKER_BIN` and PATH and
//! hands both to `setup_vm_firecracker_bin_resolved`, so the rules those
//! encode are asserted with arguments. `network::bridged`'s route probe takes
//! the `ip` binary as a parameter for the same reason, so its fail-closed test
//! names a stub instead of prepending a directory to PATH.
//!
//! Set it on a child with `Command::env`. A child's environment is private to
//! it, so nothing in this process observes the write.
//! `setup_vm_firecracker_bin_from_reads_the_environment_override` re-executes
//! the test binary once per case to prove the wrapper reads the variable at
//! all, which is the one claim an argument cannot carry.
//!
//! `no_test_mutates_the_process_environment_outside_this_module` below holds
//! the half neither technique can state: no source in the crate may
//! reach `set_var` or `remove_var`, bar the single production writer it names.

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    /// The whole rule: no source in this crate mutates the process
    /// environment, except the one production writer named below.
    ///
    /// RED BEFORE THE FIX: 6 lines, the last of this module's own mutators.
    ///   test_env.rs:92: std::env::set_var(key, value);
    ///   test_env.rs:98: std::env::remove_var(key);
    ///   test_env.rs:112: Some(value) => std::env::set_var(key, value),
    ///   test_env.rs:113: None => std::env::remove_var(key),
    ///   test_env.rs:224: r#"unsafe { std::env::set_var("PATH", updated) };"#,
    ///   test_env.rs:248: || (!trimmed.contains("set_var(") ...
    ///
    /// The last two are this test's own literals, and they are why the scan
    /// used to skip this file. It no longer skips anything: the needles and
    /// the permitted line are assembled at run time, so this source does not
    /// contain them and the module that held the exception is covered like
    /// every other.
    #[test]
    fn no_test_mutates_the_process_environment_outside_this_module() {
        let needles = [format!("{}_var(", "set"), format!("{}_var(", "remove")];
        // (file name, exact source line) of every permitted production write.
        // Listed here rather than excluded by location: there is exactly one,
        // it runs before any thread exists, and a second one deserves to be
        // read before it is added.
        let production_writers = [(
            "utils.rs",
            format!(r#"unsafe {{ std::env::{}_var("PATH", updated) }};"#, "set"),
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
        let this_file = src.join("test_env.rs");
        assert!(
            files.contains(&this_file),
            "the walk missed {}, so the scan no longer covers the module the \
             mutators used to live in",
            this_file.display()
        );

        let mut offenders = Vec::new();
        for file in &files {
            let name = file
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            let text = std::fs::read_to_string(file).expect("reading a crate source file");
            for (index, line) in text.lines().enumerate() {
                let trimmed = line.trim();
                if trimmed.starts_with("//") || !needles.iter().any(|n| trimmed.contains(n)) {
                    continue;
                }
                if production_writers
                    .iter()
                    .any(|(f, l)| *f == name && l == trimmed)
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
            "these lines mutate the process environment, which is undefined \
             behaviour while a sibling test thread reads it; pass the value to \
             the code under test as a parameter, or set it on a child with \
             Command::env:\n  {}",
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

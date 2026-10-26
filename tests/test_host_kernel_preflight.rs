//! `make install-host-kernel` checks its build dependencies and free space
//! before compiling (scripts/check-host-kernel-build.sh), and builds on the
//! scratch disk when /tmp is on the root disk.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scripts/check-host-kernel-build.sh")
}

/// A directory with a `dpkg-query` that reports every package installed except
/// the ones named in `missing`.
fn stub_dpkg_query(dir: &Path, missing: &[&str]) {
    let path = dir.join("dpkg-query");
    let cases: String = missing
        .iter()
        .map(|pkg| format!("  {pkg}) exit 1 ;;\n"))
        .collect();
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\n# last argument is the package\nfor pkg; do :; done\ncase \"$pkg\" in\n{cases}  *) printf 'install ok installed' ;;\nesac\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn run(stubs: &Path, env: &[(&str, &str)]) -> Output {
    let path = format!("{}:{}", stubs.display(), std::env::var("PATH").unwrap());
    let mut cmd = Command::new(script());
    cmd.env_clear().env("PATH", path);
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().expect("run check-host-kernel-build.sh")
}

fn dev(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).unwrap().dev()
}

/// A fresh Ubuntu box lacked debhelper and libdw-dev, and the build died deep
/// inside bindeb-pkg. The check names every missing package up front and
/// exits non-zero.
#[test]
fn missing_build_packages_are_named_and_fail_the_check() {
    let stubs = tempfile::tempdir().unwrap();
    stub_dpkg_query(stubs.path(), &["debhelper", "libdw-dev"]);
    let build = tempfile::tempdir().unwrap();
    let out = run(
        stubs.path(),
        &[
            ("FCVM_HOST_KERNEL_BUILD_DIR", build.path().to_str().unwrap()),
            ("FCVM_HOST_KERNEL_MIN_FREE_GIB", "0"),
        ],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "stderr: {stderr}");
    let named = stderr
        .lines()
        .find_map(|l| l.split("missing packages: ").nth(1))
        .unwrap_or_default();
    let mut named: Vec<&str> = named.split_whitespace().collect();
    named.sort_unstable();
    assert_eq!(named, ["debhelper", "libdw-dev"], "stderr: {stderr}");
    assert!(stderr.contains("apt-get install"), "stderr: {stderr}");
    assert!(out.stdout.is_empty(), "no build directory on failure");
}

/// The check also fails closed on space, and says which directory and what to set.
#[test]
fn a_build_directory_without_room_fails_the_check() {
    let stubs = tempfile::tempdir().unwrap();
    stub_dpkg_query(stubs.path(), &[]);
    let build = tempfile::tempdir().unwrap();
    let out = run(
        stubs.path(),
        &[
            ("FCVM_HOST_KERNEL_BUILD_DIR", build.path().to_str().unwrap()),
            ("FCVM_HOST_KERNEL_MIN_FREE_GIB", "1000000000"),
        ],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "stderr: {stderr}");
    assert!(stderr.contains("GiB free"), "stderr: {stderr}");
    assert!(
        stderr.contains(build.path().to_str().unwrap()),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("FCVM_HOST_KERNEL_BUILD_DIR"),
        "stderr: {stderr}"
    );
}

#[test]
fn an_explicit_build_directory_with_room_is_printed() {
    let stubs = tempfile::tempdir().unwrap();
    stub_dpkg_query(stubs.path(), &[]);
    let build = tempfile::tempdir().unwrap();
    let out = run(
        stubs.path(),
        &[
            ("FCVM_HOST_KERNEL_BUILD_DIR", build.path().to_str().unwrap()),
            ("FCVM_HOST_KERNEL_MIN_FREE_GIB", "0"),
        ],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        build.path().to_str().unwrap()
    );
}

/// With the default directory on the same device as / and a scratch directory on
/// another, the build moves to the scratch directory. /dev/shm is the other
/// device; a working directory under the repository is the same device as the
/// directory the test treats as the default.
#[test]
fn a_default_directory_on_the_root_disk_moves_to_the_scratch_disk() {
    let stubs = tempfile::tempdir().unwrap();
    stub_dpkg_query(stubs.path(), &[]);
    let scratch = tempfile::tempdir_in("/dev/shm").unwrap();
    assert_ne!(
        dev(scratch.path()),
        dev(Path::new("/")),
        "/dev/shm must be a different device from /"
    );
    let out = run(
        stubs.path(),
        &[
            ("FCVM_HOST_KERNEL_DEFAULT_DIR", "/kernel-build-host-default"),
            ("FCVM_HOST_KERNEL_SCRATCH", scratch.path().to_str().unwrap()),
            ("FCVM_HOST_KERNEL_MIN_FREE_GIB", "0"),
        ],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        scratch.path().join("kernel-build-host").to_str().unwrap()
    );
}

/// A scratch directory on the same device as / is no better than /tmp: the
/// default directory stays.
#[test]
fn a_scratch_directory_on_the_root_disk_does_not_move_the_build() {
    let stubs = tempfile::tempdir().unwrap();
    stub_dpkg_query(stubs.path(), &[]);
    let out = run(
        stubs.path(),
        &[
            ("FCVM_HOST_KERNEL_DEFAULT_DIR", "/kernel-build-host-default"),
            ("FCVM_HOST_KERNEL_SCRATCH", "/"),
            ("FCVM_HOST_KERNEL_MIN_FREE_GIB", "0"),
        ],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "/kernel-build-host-default"
    );
}

/// Under `make -j` a prerequisite's position orders nothing, so the check must
/// be an order-only prerequisite of the build steps when the goal is
/// install-host-kernel. Read from make's own database, not the Makefile text.
#[test]
fn install_host_kernel_orders_the_check_before_the_build_steps() {
    // The Makefile refuses to run as root on a host, and the privileged lanes run this
    // test as root, so make reads its database as the user sudo came from.
    let mut cmd = if nix::unistd::geteuid().is_root() {
        let uid = std::env::var("SUDO_UID").unwrap_or_else(|_| "65534".into());
        let gid = std::env::var("SUDO_GID").unwrap_or_else(|_| "65534".into());
        let mut c = Command::new("setpriv");
        c.args([
            &format!("--reuid={uid}"),
            &format!("--regid={gid}"),
            "--clear-groups",
            "make",
        ]);
        c
    } else {
        Command::new("make")
    };
    let out = cmd
        .args(["-qp", "install-host-kernel"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run make -qp");
    let db = String::from_utf8_lossy(&out.stdout);
    assert!(
        db.contains("install-host-kernel:"),
        "make printed no database: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    for target in ["build", "setup-btrfs"] {
        let rule = db
            .lines()
            .find(|l| l.starts_with(&format!("{target}:")) && !l.contains('='))
            .unwrap_or_else(|| panic!("{target} rule missing from make's database"));
        let order_only = rule.split_once('|').map(|(_, o)| o).unwrap_or("");
        assert!(
            order_only
                .split_whitespace()
                .any(|p| p == "check-host-kernel-build"),
            "{target} must wait for check-host-kernel-build under -j: {rule}"
        );
    }
}

/// The recipe gives the build the directory the check chose, and stops when
/// the repeated check fails (an empty value would send the build to /tmp).
#[test]
fn install_host_kernel_stops_when_the_repeated_check_fails() {
    let makefile =
        std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Makefile"))
            .unwrap();
    let recipe: Vec<&str> = makefile
        .lines()
        .skip_while(|l| !l.starts_with("install-host-kernel:") || l.contains("private"))
        .skip(1)
        .take_while(|l| l.starts_with('\t'))
        .collect();
    let capture = recipe
        .iter()
        .find(|l| l.contains("scripts/check-host-kernel-build.sh"))
        .expect("recipe runs the check");
    assert!(
        capture.trim_start().starts_with("dir=$$(") && capture.contains(") || exit"),
        "the check's result must be captured on its own and fail the recipe: {capture}"
    );
    assert!(
        recipe
            .iter()
            .any(|l| l.contains("FCVM_HOST_KERNEL_BUILD_DIR=\"$$dir\"")
                && l.contains("--install-host-kernel")),
        "recipe must pass the checked directory: {recipe:?}"
    );
}

//! `make setup-pjdfstest` (scripts/setup-pjdfstest.sh) can run in several makes at once.
//!
//! The recipe it replaced removed `/tmp/pjdfstest-check` and cloned into it, so a second make
//! starting meanwhile removed the first one's clone from under it: one run failed with
//! `fatal: cannot copy '.../hooks/push-to-checkout.sample' ... No such file or directory`.

use std::path::Path;
use std::process::Command;

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .current_dir(dir)
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?}");
}

/// Four builds started together, over a half-built tree, all succeed and leave one complete
/// build in place, and no build directory behind. The repository is a local stand-in whose build takes a moment and
/// writes the binary last, so the builds overlap and a half-built tree has no binary.
#[test]
fn concurrent_setups_all_succeed_and_install_one_complete_build() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    std::fs::write(
        repo.join("build.sh"),
        "sleep 1\nmkdir tests\nprintf '#!/bin/sh\\n' > pjdfstest\nchmod +x pjdfstest\n",
    )
    .unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "build.sh"]);
    git(&repo, &["commit", "-q", "-m", "stand-in"]);

    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/setup-pjdfstest.sh");
    let installed = dir.path().join("pjdfstest-check");
    // A tree without the binary, as a failed run of the old recipe left behind: the setups
    // replace it, and only one of them may (a second would move the first one's build aside).
    std::fs::create_dir_all(installed.join(".git")).unwrap();
    let children: Vec<_> = (0..4)
        .map(|_| {
            Command::new(script)
                .env("PJDFSTEST_DIR", &installed)
                .env("PJDFSTEST_REPO", &repo)
                .env("PJDFSTEST_BUILD", "sh build.sh")
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .expect("start setup-pjdfstest.sh")
        })
        .collect();
    for child in children {
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "a concurrent setup failed: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert!(installed.join("pjdfstest").is_file());
    assert!(installed.join("tests").is_dir());
    let left: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".build."))
        .collect();
    assert!(left.is_empty(), "build directories left behind: {left:?}");
    let stale = std::fs::read_dir(dir.path())
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".stale.")
        })
        .count();
    assert_eq!(stale, 1, "the half-built tree is moved aside once");
}

/// The check, the move of a half-built tree and the install run under one lock. Without it, two
/// setups that both found a half-built tree could each move it aside, and the second would move
/// the first one's finished install aside. That interleaving is a few instructions wide and no
/// stand-in build lands in it reliably, so the order is pinned in the script.
#[test]
fn the_install_runs_under_the_lock() {
    let script = include_str!("../scripts/setup-pjdfstest.sh");
    let at = |needle: &str| {
        script
            .find(needle)
            .unwrap_or_else(|| panic!("setup-pjdfstest.sh has no {needle:?}"))
    };
    let locked = at("flock 9");
    assert!(
        locked
            < script
                .rfind("[ -x \"$dest/pjdfstest\" ] && exit 0")
                .unwrap()
    );
    assert!(locked < at("mv -T \"$dest\""));
    assert!(locked < at("mv -T \"$work/src\" \"$dest\""));
}

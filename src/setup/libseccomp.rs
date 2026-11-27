//! Build libseccomp as a static archive for the Firecracker build to link.
//!
//! Firecracker's `seccompiler` crate links libseccomp (`-lseccomp`) into the
//! firecracker crate's build script, which compiles the seccomp policy the binary
//! carries. Upstream Firecracker now calls `seccomp_export_bpf_mem()`, which
//! libseccomp has from 2.6.0 on. Ubuntu 24.04 ships 2.5.5, so a Firecracker with
//! that call does not link against the host's library
//! (`undefined reference to seccomp_export_bpf_mem`). `fcvm setup` builds the
//! release pinned in `[libseccomp]` into the content-addressed assets directory, and
//! the Firecracker build is given its directory as a library search path
//! (`super::kernel::firecracker_build_command`). Nothing is installed on the host.
//!
//! Build and publication rules, as for pasta (`super::pasta`):
//! - The source is a release tarball verified by its SHA-256, from the first of the
//!   configured sources that serves it.
//! - The build needs make and a C compiler, and not gperf. The release tarball
//!   ships the syscall table gperf generates, newer than what it is generated
//!   from, so make has nothing to regenerate. configure still refuses to run
//!   without a gperf, so it is given a guard program that fails loudly if make
//!   ever runs it (`GENERATED_SOURCES`, `gperf_guard_script`).
//! - A store entry is a directory named by the content hash that holds
//!   `libseccomp.a` and nothing else: the linker is given the directory and looks
//!   for that file name in it. With no shared library beside it, the link is
//!   static.
//! - An entry is staged as a directory of the builder's own and renamed into
//!   place. Builds are serialized by an exclusive flock, double-checked after
//!   acquisition. That flock does not hold between VMs that share the store over
//!   FUSE; there the unique staging name and the rename keep two builders apart.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use tokio::process::Command;
use tracing::{debug, info};

use crate::paths;

/// The pinned libseccomp release (`[libseccomp]` in rootfs-config.toml).
#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
pub struct LibseccompConfig {
    /// Release version, e.g. "2.6.1".
    pub version: String,
    /// SHA-256 of that version's release tarball.
    pub archive_sha256: String,
    /// Where the tarball can be downloaded, tried in order. Any copy that
    /// matches `archive_sha256` is the same source.
    pub archives: Vec<String>,
}

/// The file the linker looks for when it is given `-lseccomp` and a directory
/// without a shared library.
const ARCHIVE_NAME: &str = "libseccomp.a";

/// `configure` arguments besides `--prefix`, hashed into the store entry's name.
/// Only the static archive is built. `--with-pic` because Rust links build scripts
/// as position-independent executables, and a non-PIC archive does not link into
/// one on x86_64.
const CONFIGURE_ARGS: &[&str] = &["--enable-static", "--disable-shared", "--with-pic"];

/// Content hash of the archive `config` builds on this host: the Firecracker
/// binary's name carries it (`super::kernel`).
pub(crate) fn content_hash(config: &LibseccompConfig) -> String {
    content_hash_for(
        config,
        std::env::consts::ARCH,
        &super::kernel::libc_version_tag(),
    )
}

/// The release, how it is configured, the architecture it is compiled for and the
/// C library whose headers it is compiled against. The download sources are not
/// part of it: the checksum names the source.
fn content_hash_for(config: &LibseccompConfig, arch: &str, libc_tag: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"fcvm-libseccomp-v1\0");
    let fields = [
        config.version.as_str(),
        config.archive_sha256.as_str(),
        arch,
        libc_tag,
    ];
    // Length-prefixed, so a byte cannot move across a field boundary and name
    // another build.
    for field in fields.into_iter().chain(CONFIGURE_ARGS.iter().copied()) {
        hasher.update((field.len() as u64).to_le_bytes());
        hasher.update(field.as_bytes());
    }
    hex::encode(&hasher.finalize()[..6])
}

/// Directory holding one directory per built archive.
pub(crate) fn store_dir() -> PathBuf {
    paths::assets_dir().join("libseccomp")
}

fn entry_dir_in(store: &Path, config: &LibseccompConfig) -> PathBuf {
    store.join(content_hash(config))
}

/// The directory to give the linker for a build that links libseccomp.
///
/// With a `[libseccomp]` section the built archive is required: a missing one is
/// an error naming `fcvm setup`, not a link against whatever libseccomp the host
/// has. Without the section there is no directory, and the build links the
/// host's. A pure lookup: it builds nothing and works offline.
pub fn libseccomp_dir_for_config(config: Option<&LibseccompConfig>) -> Result<Option<PathBuf>> {
    let Some(config) = config else {
        return Ok(None);
    };
    libseccomp_dir_in(&store_dir(), config).map(Some)
}

/// [`libseccomp_dir_for_config`] for a configured pin, in the store `store`.
pub(crate) fn libseccomp_dir_in(store: &Path, config: &LibseccompConfig) -> Result<PathBuf> {
    let dir = entry_dir_in(store, config);
    let archive = dir.join(ARCHIVE_NAME);
    if !archive.is_file() {
        bail!(
            "pinned libseccomp {} not found at {}. Run 'fcvm setup' (or 'make setup-fcvm') \
             to build it",
            config.version,
            archive.display()
        );
    }
    Ok(dir)
}

/// Ensure the pinned libseccomp archive exists, building it if needed, and
/// return its directory. `Ok(None)` when no `[libseccomp]` section is configured.
pub async fn ensure_libseccomp(config: Option<&LibseccompConfig>) -> Result<Option<PathBuf>> {
    let Some(config) = config else {
        return Ok(None);
    };
    let dir = entry_dir_in(&store_dir(), config);
    let archive = dir.join(ARCHIVE_NAME);
    if archive.is_file() {
        info!(path = %archive.display(), "libseccomp archive exists");
        return Ok(Some(dir));
    }

    // Serialize concurrent builds of the same archive (parallel `fcvm setup`).
    let flock = super::lock_store_dir(&dir.with_extension("lock"), "libseccomp build").await?;

    // Another process may have finished the build while we waited.
    if archive.is_file() {
        debug!(path = %archive.display(), "libseccomp exists (built by another process)");
        flock.unlock().map_err(|(_, err)| err)?;
        return Ok(Some(dir));
    }

    println!(
        "  → Building libseccomp {} as a static archive (sha: {})...",
        config.version,
        content_hash(config)
    );

    // A build tree of this builder's own: the flock above does not keep a VM
    // that shares the store from building the same archive at the same time.
    let build_dir = build_dir_for(&dir, uuid::Uuid::new_v4());
    if let Err(e) = build_and_publish(config, &build_dir, &dir).await {
        // A failed build keeps its tree for debugging. The store holds nothing
        // of it: the staged entry is removed when it is not published.
        let _ = flock.unlock();
        return Err(e);
    }

    let _ = tokio::fs::remove_dir_all(&build_dir).await;
    flock.unlock().map_err(|(_, err)| err)?;
    Ok(Some(dir))
}

/// The files `src/Makefile.am` regenerates, each with what it is generated from:
/// `syscalls.perf` by a script the tarball carries, and `syscalls.perf.c` from it
/// by gperf. make regenerates a file that is missing or older than one of its
/// sources. The release tarball ships both, each newer than its sources (tar
/// restores the times), so a build of it runs neither rule and needs no gperf.
const GENERATED_SOURCES: &[(&str, &[&str])] = &[
    (
        "src/syscalls.perf",
        &["src/syscalls.csv", "src/syscalls.perf.template"],
    ),
    ("src/syscalls.perf.c", &["src/syscalls.perf"]),
];

/// Check that make will find the generated sources of the extracted tarball in
/// `tree` up to date, so the build does not run gperf. `tarball` names the
/// archive in the error.
///
/// Nothing is touched to make this true: a tarball that lacks a generated file,
/// or carries it older than its source, is not the release fcvm pinned, and is
/// refused before configure runs.
fn check_generated_sources(tree: &Path, tarball: &str) -> Result<()> {
    let modified = |file: &str| -> Result<std::time::SystemTime> {
        let path = tree.join(file);
        let meta = std::fs::metadata(&path).map_err(|err| {
            anyhow::anyhow!(
                "{tarball} has no {file} ({err}). fcvm builds libseccomp without gperf, which \
                 needs the generated syscall table a release tarball ships; this archive is \
                 not a release tarball"
            )
        })?;
        if !meta.is_file() || meta.len() == 0 {
            bail!(
                "{file} in {tarball} is empty or not a file. fcvm builds libseccomp without \
                 gperf, which needs the generated syscall table a release tarball ships"
            );
        }
        meta.modified()
            .with_context(|| format!("reading the modification time of {}", path.display()))
    };
    for (generated, sources) in GENERATED_SOURCES {
        let generated_at = modified(generated)?;
        for source in *sources {
            if modified(source)? > generated_at {
                bail!(
                    "{generated} in {tarball} is older than {source}, so make would regenerate \
                     it, and fcvm builds libseccomp without gperf. A release tarball ships it \
                     newer than its sources; this archive, or how it was extracted, differs"
                );
            }
        }
    }
    Ok(())
}

/// The program configure is given as gperf (`GPERF=`). configure refuses to run
/// without one, and the build has no use for it (`GENERATED_SOURCES`). If make
/// runs it anyway, it fails the build with the reason on stderr. A no-op such as
/// `true` would let the rule succeed and leave an empty syscall table behind: the
/// rule redirects the program's output into the generated file.
fn gperf_guard_script(tarball: &str) -> String {
    // The name goes into a shell script; a version is digits and dots.
    let tarball: String = tarball
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        .collect();
    format!(
        "#!/bin/sh\n\
         # Written by fcvm setup as configure's GPERF (src/setup/libseccomp.rs).\n\
         echo 'fcvm: the libseccomp build tried to run gperf to regenerate its syscall table \
         (src/syscalls.perf.c).' >&2\n\
         echo 'fcvm: the pinned release tarball {tarball} ships that file newer than its \
         sources, so the build should not need gperf. The extracted tree is not what fcvm \
         pinned.' >&2\n\
         exit 1\n"
    )
}

/// Write the guard program to `path`, which must not exist.
fn write_gperf_guard(path: &Path, tarball: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    // create_new: the build directory belongs to the sudo invoker, and this
    // process may be root. An existing path, a symlink included, is refused.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    file.write_all(gperf_guard_script(tarball).as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    // Set after creation, so the umask does not decide whether the build, which
    // runs as the sudo invoker, can execute it.
    file.set_permissions(std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("making {} executable", path.display()))
}

/// The configure command for the extracted tarball in `tree`: a static,
/// position-independent archive installed under `prefix`, with `guard` as gperf.
fn configure_command(tree: &Path, prefix: &Path, guard: &Path) -> Command {
    let mut gperf = OsString::from("GPERF=");
    gperf.push(guard);
    let mut prefix_arg = OsString::from("--prefix=");
    prefix_arg.push(prefix);
    let mut command = Command::new("./configure");
    command
        .arg(gperf)
        .arg(prefix_arg)
        .args(CONFIGURE_ARGS)
        .current_dir(tree);
    command
}

/// A build tree for one build of the archive published at `dir`.
fn build_dir_for(dir: &Path, nonce: uuid::Uuid) -> PathBuf {
    let hash = dir
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| "libseccomp".into());
    PathBuf::from(format!("/tmp/libseccomp-{hash}-build-{nonce}"))
}

/// Run one step of the build as the sudo invoker (see
/// `run_build_as_sudo_invoker`), so a failed root setup leaves no root-owned
/// build tree. The step's output is kept out of setup's own: configure alone
/// prints several hundred lines. A failed step's last lines go in the error.
async fn run_build_step(command: &mut Command, what: &str) -> Result<()> {
    let output = super::run_build_as_sudo_invoker(command)
        .output()
        .await
        .with_context(|| {
            format!("running a command to {what} (it needs tar, make and a C toolchain)")
        })?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let lines: Vec<&str> = stderr.lines().collect();
    bail!(
        "could not {what} ({}):\n{}",
        output.status,
        lines[lines.len().saturating_sub(20)..].join("\n")
    )
}

/// Download the pinned tarball to `dest`.
async fn fetch_source(
    config: &LibseccompConfig,
    dest: &Path,
    retry_delay: std::time::Duration,
) -> Result<()> {
    super::fetch_verified_archive(
        "libseccomp",
        &config.archives,
        &config.archive_sha256,
        dest,
        retry_delay,
    )
    .await
}

/// Build the archive in `build_dir`, which must not exist yet, and return its path
/// in that tree.
async fn build_archive(config: &LibseccompConfig, build_dir: &Path) -> Result<PathBuf> {
    let tarball = format!("libseccomp-{}.tar.gz", config.version);
    // The extracted tarball gets a directory of its own, beside the download,
    // the guard program and the install prefix. Not `mkdir -p`: a path that
    // exists is not this builder's.
    let tree = build_dir.join("tree");
    run_build_step(
        Command::new("mkdir").arg(build_dir).arg(&tree),
        "create the libseccomp build directory",
    )
    .await?;
    let download = build_dir.join("libseccomp.tar.gz");
    fetch_source(config, &download, super::ARCHIVE_RETRY_DELAY).await?;
    run_build_step(
        Command::new("tar")
            .arg("-xzf")
            .arg(&download)
            .arg("--strip-components=1")
            .current_dir(&tree),
        "extract the libseccomp source archive",
    )
    .await?;

    check_generated_sources(&tree, &tarball)?;
    let guard = build_dir.join("gperf-guard");
    write_gperf_guard(&guard, &tarball)?;

    let prefix = build_dir.join("install");
    run_build_step(
        &mut configure_command(&tree, &prefix, &guard),
        "configure libseccomp",
    )
    .await?;
    let jobs = std::thread::available_parallelism().map_or(1, |n| n.get());
    run_build_step(
        Command::new("make")
            .arg(format!("-j{jobs}"))
            .current_dir(&tree),
        "build libseccomp",
    )
    .await?;
    run_build_step(
        Command::new("make").arg("install").current_dir(&tree),
        "install libseccomp into its build tree",
    )
    .await?;

    let built = prefix.join("lib").join(ARCHIVE_NAME);
    if !built.is_file() {
        bail!("{} not found after the libseccomp build", built.display());
    }
    Ok(built)
}

async fn build_and_publish(config: &LibseccompConfig, build_dir: &Path, dir: &Path) -> Result<()> {
    let built = build_archive(config, build_dir).await.with_context(|| {
        format!(
            "building libseccomp {} (build tree: {})",
            config.version,
            build_dir.display()
        )
    })?;
    let staged = super::StoreTemp::new(dir)?;
    stage_archive(&staged, &built).await?;
    staged.publish(dir, "libseccomp archive").await?;
    // A rename that found the entry in place kept it; an entry is whole only
    // with the archive in it.
    if !dir.join(ARCHIVE_NAME).is_file() {
        bail!(
            "{} exists without {ARCHIVE_NAME}; remove it and run 'fcvm setup' again",
            dir.display()
        );
    }
    Ok(())
}

/// Copy a built archive into `staged`, a store entry of this builder's own beside
/// the final directory, ready to be renamed onto it. The entry is a whole
/// directory before the rename, so the final directory never exists without its
/// archive.
async fn stage_archive(staged: &super::StoreTemp, built: &Path) -> Result<()> {
    tokio::fs::create_dir(staged.path())
        .await
        .with_context(|| format!("creating {}", staged.path().display()))?;
    let archive = staged.path().join(ARCHIVE_NAME);
    tokio::fs::copy(built, &archive)
        .await
        .context("staging the libseccomp archive")?;
    // The publish hands the directory back to the sudo invoker; the file in it
    // goes with it.
    super::give_store_entry_to_invoker(&archive);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup::rootfs::{Plan, EMBEDDED_CONFIG};

    fn pin(version: &str, sha256: &str) -> LibseccompConfig {
        LibseccompConfig {
            version: version.to_string(),
            archive_sha256: sha256.to_string(),
            archives: vec!["https://example.invalid/libseccomp.tar.gz".to_string()],
        }
    }

    /// The pin's first source serves other bytes (or nothing) and its second
    /// serves the pinned tarball: the download takes the second, and a pin none of
    /// whose sources serves the checksum fails naming libseccomp.
    #[tokio::test]
    async fn the_libseccomp_archive_falls_through_to_the_next_source() {
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("good.tar.gz");
        let other = dir.path().join("other.tar.gz");
        std::fs::write(&good, b"the pinned tarball").unwrap();
        std::fs::write(&other, b"some other tarball").unwrap();
        let url = |path: &Path| format!("file://{}", path.display());
        let absent = url(&dir.path().join("absent.tar.gz"));
        let dest = dir.path().join("dest.tar.gz");
        let no_wait = std::time::Duration::ZERO;
        let mut config = pin("2.6.1", &hex::encode(Sha256::digest(b"the pinned tarball")));

        for first in [url(&other), absent.clone()] {
            config.archives = vec![first.clone(), url(&good)];
            fetch_source(&config, &dest, no_wait)
                .await
                .unwrap_or_else(|e| panic!("{first} then the pinned tarball: {e:#}"));
            assert_eq!(std::fs::read(&dest).unwrap(), b"the pinned tarball");
        }

        config.archives = vec![url(&other), absent];
        let err = fetch_source(&config, &dest, no_wait)
            .await
            .expect_err("no source served the pinned tarball");
        let text = format!("{err:#}");
        assert!(
            text.contains("libseccomp") && text.contains(&config.archive_sha256),
            "{text}"
        );
        assert!(!dest.exists(), "a rejected download was left behind");
    }

    /// The store can be a directory several VMs share over FUSE, where the build
    /// flock does not keep two of them from building one archive at once. Each
    /// stages an entry of its own, the first rename publishes one, and the second
    /// builder finds it in place and removes its own: one entry, holding the
    /// archive and nothing else.
    #[tokio::test]
    async fn two_builders_of_one_archive_stage_to_separate_directories() {
        let store = tempfile::tempdir().unwrap();
        let records = tempfile::tempdir().unwrap();
        let trees = tempfile::tempdir().unwrap();
        let owners = crate::reclaim::owner_dir_in(records.path()).unwrap();
        let dir = store.path().join("0123456789ab");
        let first_built = trees.path().join("first.a");
        let second_built = trees.path().join("second.a");
        std::fs::write(&first_built, b"first builder").unwrap();
        std::fs::write(&second_built, b"second builder").unwrap();

        // The second builder stages after the first has staged and before the
        // first has published.
        let first = crate::setup::StoreTemp::new_in(&owners, &dir).unwrap();
        let second = crate::setup::StoreTemp::new_in(&owners, &dir).unwrap();
        stage_archive(&first, &first_built).await.unwrap();
        stage_archive(&second, &second_built).await.unwrap();
        let second_path = second.path().to_path_buf();
        assert_ne!(first.path(), second.path());
        assert_eq!(first.path().parent(), dir.parent());
        assert_eq!(second.path().parent(), dir.parent());
        assert!(!dir.exists(), "an entry was visible before its rename");

        first.publish(&dir, "libseccomp archive").await.unwrap();
        assert_eq!(
            std::fs::read(second_path.join(ARCHIVE_NAME)).unwrap(),
            b"second builder",
            "the first builder published the entry the second builder was writing"
        );
        second.publish(&dir, "libseccomp archive").await.unwrap();

        assert_eq!(
            std::fs::read(dir.join(ARCHIVE_NAME)).unwrap(),
            b"first builder"
        );
        let names = |path: &Path| -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(path)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        };
        assert_eq!(
            names(&dir),
            [ARCHIVE_NAME],
            "the linker is given this directory"
        );
        assert_eq!(
            names(store.path()),
            ["0123456789ab"],
            "the losing builder's staged entry was left behind"
        );
    }

    /// A build that needs the archive and does not find it names the setup step.
    /// The alternative is a link against the host's libseccomp: an undefined
    /// symbol ten minutes into a Firecracker build, or an older library linked
    /// without a word.
    #[test]
    fn a_lookup_without_the_archive_names_fcvm_setup() {
        let store = tempfile::tempdir().unwrap();
        let config = pin("2.6.1", &"ab".repeat(32));

        let err = libseccomp_dir_in(store.path(), &config).expect_err("nothing is built");
        let text = format!("{err:#}");
        assert!(
            text.contains("fcvm setup") && text.contains("libseccomp 2.6.1"),
            "{text}"
        );

        // A directory without the archive is not an entry.
        let dir = entry_dir_in(store.path(), &config);
        std::fs::create_dir(&dir).unwrap();
        libseccomp_dir_in(store.path(), &config).expect_err("an entry without its archive");

        std::fs::write(dir.join(ARCHIVE_NAME), b"!<arch>\n").unwrap();
        assert_eq!(libseccomp_dir_in(store.path(), &config).unwrap(), dir);

        // Another pin is another entry.
        libseccomp_dir_in(store.path(), &pin("2.6.2", &"ab".repeat(32)))
            .expect_err("the archive of another version");
    }

    /// Without a `[libseccomp]` section there is nothing to build or to look up,
    /// and a build links the host's library.
    #[tokio::test]
    async fn no_section_means_no_archive() {
        assert_eq!(libseccomp_dir_for_config(None).unwrap(), None);
        assert_eq!(ensure_libseccomp(None).await.unwrap(), None);
    }

    #[test]
    fn the_content_hash_tracks_what_is_compiled() {
        let config = pin("2.6.1", &"ab".repeat(32));
        let hash = |config: &LibseccompConfig, arch: &str, libc: &str| {
            content_hash_for(config, arch, libc)
        };
        let base = hash(&config, "x86_64", "libc-2.39");
        assert_eq!(base.len(), 12);
        assert_eq!(base, hash(&config.clone(), "x86_64", "libc-2.39"));

        let mut mirrored = config.clone();
        mirrored.archives = vec!["https://example.invalid/mirror.tar.gz".to_string()];
        assert_eq!(
            base,
            hash(&mirrored, "x86_64", "libc-2.39"),
            "the checksum names the source, not where it was downloaded"
        );

        for (what, other) in [
            (
                "version",
                hash(&pin("2.6.0", &"ab".repeat(32)), "x86_64", "libc-2.39"),
            ),
            (
                "tarball",
                hash(&pin("2.6.1", &"cd".repeat(32)), "x86_64", "libc-2.39"),
            ),
            ("architecture", hash(&config, "aarch64", "libc-2.39")),
            ("C library", hash(&config, "x86_64", "libc-2.41")),
        ] {
            assert_ne!(base, other, "another {what} named the same archive");
        }
        assert_eq!(
            content_hash(&config),
            hash(
                &config,
                std::env::consts::ARCH,
                &crate::setup::kernel::libc_version_tag()
            )
        );
    }

    /// bench/chromium/reqbench.py computes the same hash (`libseccomp_identity`)
    /// on its way to a Firecracker binary's name. Its test
    /// `test_the_name_fcvm_gives_these_fields` holds this literal too.
    #[test]
    fn the_content_hash_is_the_one_the_bench_harness_computes() {
        assert_eq!(
            content_hash_for(&pin("2.6.1", &"ab".repeat(32)), "aarch64", "libc-2.39"),
            "22445fa649f2"
        );
    }

    /// A fixture of the files make regenerates and their sources, each written
    /// `age` seconds before now. `None` leaves the file out.
    fn source_tree(ages: &[(&str, Option<u64>)]) -> tempfile::TempDir {
        let tree = tempfile::tempdir().unwrap();
        std::fs::create_dir(tree.path().join("src")).unwrap();
        let now = std::time::SystemTime::now();
        for (file, age) in ages {
            let Some(age) = age else { continue };
            let path = tree.path().join(file);
            std::fs::write(&path, b"contents").unwrap();
            std::fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(now - std::time::Duration::from_secs(*age))
                .unwrap();
        }
        tree
    }

    /// The release tarball's order: the sources, then `syscalls.perf`, then
    /// `syscalls.perf.c`, as tar restores their times.
    const RELEASE_AGES: [(&str, Option<u64>); 4] = [
        ("src/syscalls.csv", Some(400)),
        ("src/syscalls.perf.template", Some(400)),
        ("src/syscalls.perf", Some(200)),
        ("src/syscalls.perf.c", Some(100)),
    ];

    fn with_age(file: &str, age: Option<u64>) -> Vec<(&'static str, Option<u64>)> {
        let mut ages = RELEASE_AGES.to_vec();
        let entry = ages.iter_mut().find(|(name, _)| *name == file).unwrap();
        entry.1 = age;
        ages
    }

    /// The build runs without gperf because make finds the generated syscall
    /// table up to date. That is checked before configure: a tree in the release
    /// tarball's order passes, with equal times too, which make also accepts.
    #[test]
    fn a_release_tree_needs_nothing_regenerated() {
        let tree = source_tree(&RELEASE_AGES);
        check_generated_sources(tree.path(), "libseccomp-2.6.1.tar.gz").unwrap();

        let same_second = source_tree(&with_age("src/syscalls.perf.c", Some(200)));
        check_generated_sources(same_second.path(), "libseccomp-2.6.1.tar.gz").unwrap();
    }

    /// A tree without the generated table (a git archive, not a release tarball)
    /// is refused naming the missing file and why it is needed. So is one whose
    /// table is older than what it is generated from, which make would hand to
    /// gperf.
    #[test]
    fn a_tree_make_would_regenerate_is_refused() {
        let refusal = |ages: &[(&str, Option<u64>)]| {
            let tree = source_tree(ages);
            let err = check_generated_sources(tree.path(), "libseccomp-2.6.1.tar.gz")
                .expect_err("make would run gperf on this tree");
            format!("{err:#}")
        };

        let missing = refusal(&with_age("src/syscalls.perf.c", None));
        assert!(
            missing.contains("libseccomp-2.6.1.tar.gz has no src/syscalls.perf.c")
                && missing.contains("without gperf")
                && missing.contains("not a release tarball"),
            "{missing}"
        );
        let no_intermediate = refusal(&with_age("src/syscalls.perf", None));
        assert!(
            no_intermediate.contains("has no src/syscalls.perf "),
            "{no_intermediate}"
        );

        let stale_table = refusal(&with_age("src/syscalls.perf.c", Some(300)));
        assert!(
            stale_table.contains("src/syscalls.perf.c in libseccomp-2.6.1.tar.gz is older than")
                && stale_table.contains("src/syscalls.perf,")
                && stale_table.contains("make would regenerate"),
            "{stale_table}"
        );
        let newer_source = refusal(&with_age("src/syscalls.csv", Some(150)));
        assert!(
            newer_source.contains("src/syscalls.perf in libseccomp-2.6.1.tar.gz is older than")
                && newer_source.contains("src/syscalls.csv"),
            "{newer_source}"
        );

        let empty = source_tree(&RELEASE_AGES);
        std::fs::write(empty.path().join("src/syscalls.perf.c"), b"").unwrap();
        let err = check_generated_sources(empty.path(), "libseccomp-2.6.1.tar.gz")
            .expect_err("an empty syscall table");
        assert!(format!("{err:#}").contains("empty"), "{err:#}");
    }

    /// The guard stands in for gperf in the rule
    /// `${GPERF} ... syscalls.perf > syscalls.perf.c`. Run that way it fails,
    /// says on stderr what happened and which tarball should not have needed it,
    /// and prints nothing: its stdout is the generated file.
    #[tokio::test]
    async fn the_gperf_guard_fails_the_build_that_runs_it() {
        let dir = tempfile::tempdir().unwrap();
        let guard = dir.path().join("gperf-guard");
        write_gperf_guard(&guard, "libseccomp-2.6.1.tar.gz").unwrap();

        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&guard).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o755,
            "make runs the guard as the build's user"
        );

        // Through sh: executing a file this process has just written can fail
        // with ETXTBSY while another test forks.
        let output = Command::new("sh")
            .arg(&guard)
            .args(["-m", "100", "--null-strings", "--pic", "-tCEG", "-T", "-S1"])
            .arg("syscalls.perf")
            .output()
            .await
            .unwrap();
        assert!(!output.status.success(), "the guard reported success");
        assert!(output.stdout.is_empty(), "the guard wrote a syscall table");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("tried to run gperf")
                && stderr.contains("src/syscalls.perf.c")
                && stderr.contains("libseccomp-2.6.1.tar.gz"),
            "{stderr}"
        );

        // Nothing already at the path is written through.
        write_gperf_guard(&guard, "libseccomp-2.6.1.tar.gz").expect_err("the path exists");

        // The name is text in a shell script.
        let hostile = gperf_guard_script("x'; touch /tmp/y; echo '.tar.gz");
        assert!(!hostile.contains("touch /tmp") && hostile.contains("xtouchtmpyecho.tar.gz"));
    }

    /// configure is told to use the guard as gperf, and to build the static,
    /// position-independent archive the content hash names.
    #[test]
    fn configure_is_given_the_guard_as_gperf() {
        let command = configure_command(
            Path::new("/b/tree"),
            Path::new("/b/install"),
            Path::new("/b/gperf-guard"),
        );
        let command = command.as_std();
        assert_eq!(command.get_program(), "./configure");
        assert_eq!(command.get_current_dir(), Some(Path::new("/b/tree")));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [
                "GPERF=/b/gperf-guard",
                "--prefix=/b/install",
                "--enable-static",
                "--disable-shared",
                "--with-pic"
            ]
        );
    }

    /// A `[libseccomp]` section missing a field is refused when the config loads,
    /// as `[pasta]` is, not accepted until a build needs the field.
    #[test]
    fn a_libseccomp_section_missing_a_field_is_refused() {
        let fields = [
            ("version", "version = \"2.6.1\"\n"),
            ("archive_sha256", "archive_sha256 = \"00\"\n"),
            (
                "archives",
                "archives = [\"https://example.invalid/a.tar.gz\"]\n",
            ),
        ];
        let whole: String = fields.iter().map(|(_, line)| *line).collect();
        toml::from_str::<LibseccompConfig>(&whole).expect("a section with every field");
        for (missing, _) in fields {
            let partial: String = fields
                .iter()
                .filter(|(name, _)| *name != missing)
                .map(|(_, line)| *line)
                .collect();
            let error = toml::from_str::<LibseccompConfig>(&partial)
                .expect_err("a [libseccomp] section missing a field");
            assert!(error.to_string().contains(missing), "{missing}: {error}");
        }
    }

    /// The config fcvm ships pins a release tarball by a whole SHA-256. Upstream's
    /// release is a source, and one source is enough; any other is a copy of the
    /// same file (scripts/publish-libseccomp-archive.sh uploads one to this
    /// repository's releases).
    #[test]
    fn the_embedded_libseccomp_section_pins_a_verified_release() {
        let plan: Plan = toml::from_str(EMBEDDED_CONFIG).unwrap();
        let config = plan
            .libseccomp
            .expect("the embedded config has [libseccomp]");
        assert_eq!(config.archive_sha256.len(), 64, "{}", config.archive_sha256);
        assert!(
            config
                .archive_sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "{} is not lowercase hex",
            config.archive_sha256
        );
        let file = format!("libseccomp-{}.tar.gz", config.version);
        let upstream = format!(
            "https://github.com/seccomp/libseccomp/releases/download/v{}/{file}",
            config.version
        );
        assert!(
            config.archives.contains(&upstream),
            "{:?} does not list upstream's release {upstream}",
            config.archives
        );
        for url in &config.archives {
            assert!(
                url.starts_with("https://") && url.ends_with(&format!("/{file}")),
                "{url} is not an https copy of {file}"
            );
        }
    }
}

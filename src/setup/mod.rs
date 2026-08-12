pub mod kernel;
pub mod pasta;
pub mod rootfs;
pub mod storage;

use anyhow::{Context, Result};
use nix::fcntl::{Flock, FlockArg, OFlag};
use std::os::fd::AsRawFd;
use std::path::Path;
use tracing::warn;

/// The user who invoked sudo, resolved from SUDO_USER via passwd.
///
/// `None` when not running as root, when SUDO_USER is unset (a genuine root
/// login, container CI), or when the name does not resolve. Memoized: euid,
/// SUDO_USER, and the passwd entry are process-constant, and the store paths
/// consult this on every entry they create.
pub(crate) fn sudo_invoker() -> Option<&'static nix::unistd::User> {
    static INVOKER: std::sync::OnceLock<Option<nix::unistd::User>> = std::sync::OnceLock::new();
    INVOKER
        .get_or_init(|| {
            if !nix::unistd::Uid::effective().is_root() {
                return None;
            }
            let sudo_user = std::env::var("SUDO_USER").ok()?;
            match nix::unistd::User::from_name(&sudo_user) {
                Ok(Some(user)) => Some(user),
                Ok(None) => {
                    warn!(%sudo_user, "SUDO_USER not found in passwd; store entries stay root-owned");
                    None
                }
                Err(err) => {
                    warn!(%sudo_user, %err, "passwd lookup for SUDO_USER failed; store entries stay root-owned");
                    None
                }
            }
        })
        .as_ref()
}

/// Open a store entry for an ownership hand-back, refusing to follow symlinks.
///
/// The hand-back runs as root inside a user-owned tree, so a path-based chown
/// would follow a symlink the user planted and change the ownership of
/// whatever it points at. Opening with O_NOFOLLOW and chowning the descriptor
/// makes that impossible: a symlink fails the open (ELOOP) and is skipped.
fn open_store_entry_nofollow(path: &Path) -> nix::Result<std::os::fd::OwnedFd> {
    let raw = nix::fcntl::open(
        path,
        OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        nix::sys::stat::Mode::empty(),
    )?;
    // SAFETY: open just returned this descriptor; nothing else owns it.
    Ok(unsafe { std::os::fd::FromRawFd::from_raw_fd(raw) })
}

/// Hand a content-addressed store entry back to the user who invoked sudo.
///
/// The stores under assets_dir (kernels, pasta, firecracker, cloud-hypervisor,
/// initrd, rootfs, cache; the list lives in src/paths.rs and in
/// scripts/normalize-store-ownership.sh) belong to the operator; root only
/// writes there on the operator's behalf (`sudo fcvm setup`). Anything root
/// leaves behind blocks the next rootless run from writing: a failed root
/// build left a root-owned firecracker/ directory and 0600 lock file on a CI
/// runner, and every rootless setup on that machine then died with EACCES
/// opening the lock.
///
/// No-op unless running as root with SUDO_USER set. Failure is logged rather
/// than propagated: a store on a fuse-pipe mapped directory (nested runs map
/// the host store into the guest) legitimately refuses chown while everything
/// else works, and a genuinely lost hand-back surfaces at the next rootless
/// write with a clear EACCES rather than silently.
pub(crate) fn give_store_entry_to_invoker(path: &Path) {
    let Some(user) = sudo_invoker() else {
        return;
    };
    let fd = match open_store_entry_nofollow(path) {
        Ok(fd) => fd,
        Err(err) => {
            warn!(
                path = %path.display(),
                %err,
                "not handing store entry back (symlink or unreadable)"
            );
            return;
        }
    };
    if let Err(err) = nix::unistd::fchown(fd.as_raw_fd(), Some(user.uid), Some(user.gid)) {
        warn!(
            path = %path.display(),
            invoker = %user.name,
            %err,
            "failed to hand store entry back to the invoking user"
        );
    }
}

/// Publish a completed store artifact: atomically rename the staged temp onto
/// the content-addressed final path, then hand it back to the sudo invoker.
///
/// Every store builder stages to a temp name and renames, so an interrupted
/// build never leaves a partial artifact behind; this owns the rename half of
/// that ritual so the ownership hand-back cannot be forgotten at a call site.
pub(crate) async fn publish_store_entry(temp: &Path, dest: &Path, what: &str) -> Result<()> {
    tokio::fs::rename(temp, dest)
        .await
        .with_context(|| format!("renaming {what} into {}", dest.display()))?;
    give_store_entry_to_invoker(dest);
    Ok(())
}

/// Run a build child as the user who invoked sudo instead of as root.
///
/// The store builders clone repos and run the operator's toolchains. As root
/// those misbehave: the rustup shim keys its toolchain lookup off $HOME, so it
/// tries to download a whole toolchain into /root/.rustup (observed live), and
/// everything the build writes — checkout, target/, registry cache — lands
/// root-owned. Dropping the child to the invoking user makes the build behave
/// exactly as if the operator ran it themselves; root keeps only the store
/// lock and the install into the shared store.
///
/// The drop is complete: supplementary groups, gid, then uid, in one
/// pre-exec, so the child holds none of root's group privileges. CARGO_HOME
/// and RUSTUP_HOME are cleared so a value inherited from root's environment
/// cannot point the build at root-owned trees.
///
/// No-op unless running as root with a resolvable SUDO_USER.
pub(crate) fn run_build_as_sudo_invoker(
    cmd: &mut tokio::process::Command,
) -> &mut tokio::process::Command {
    let Some(user) = sudo_invoker() else {
        return cmd;
    };
    cmd.env("HOME", &user.dir);
    cmd.env_remove("CARGO_HOME");
    cmd.env_remove("RUSTUP_HOME");
    let uid = user.uid;
    let gid = user.gid;
    unsafe {
        cmd.pre_exec(move || {
            let errno_to_io = |e: nix::errno::Errno| std::io::Error::from_raw_os_error(e as i32);
            nix::unistd::setgroups(&[gid]).map_err(errno_to_io)?;
            nix::unistd::setgid(gid).map_err(errno_to_io)?;
            nix::unistd::setuid(uid).map_err(errno_to_io)?;
            Ok(())
        });
    }
    cmd
}

/// Create the store directory holding `lock_path` and take an exclusive flock
/// on it.
///
/// Every store builder (kernel, pasta, firecracker, cloud-hypervisor, rootfs,
/// initrd) serializes concurrent builds of the same artifact this way. The
/// directory and lock file are handed back to the sudo invoker immediately on
/// creation, so a root-invoked build that later fails cannot leave entries a
/// rootless run is unable to open.
pub(crate) async fn lock_store_dir(lock_path: &Path, what: &str) -> Result<Flock<std::fs::File>> {
    let dir = lock_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("store lock {} has no parent", lock_path.display()))?;
    tokio::fs::create_dir_all(dir)
        .await
        .with_context(|| format!("creating {what} directory"))?;
    give_store_entry_to_invoker(dir);

    use std::os::unix::fs::OpenOptionsExt;
    let lock_fd = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(lock_path)
        .with_context(|| format!("opening {what} lock file"))?;
    give_store_entry_to_invoker(lock_path);

    Flock::lock(lock_fd, FlockArg::LockExclusive)
        .map_err(|(_, err)| err)
        .with_context(|| format!("acquiring exclusive lock for {what}"))
}

pub use kernel::{
    ensure_cloud_hypervisor, ensure_kernel, ensure_profile_firecracker,
    get_configured_firecracker_for_profile, get_firecracker_for_profile, get_kernel_path,
    get_kernel_url_hash, get_profile_firecracker_path, install_host_kernel,
    newest_cached_cloud_hypervisor, rebuild_kernel_from_source,
};
pub use pasta::{ensure_pasta, get_pasta_for_config};
pub use rootfs::{
    ensure_fc_agent_initrd, ensure_rootfs, get_kernel_profile, resolve_rootfs_type, KernelProfile,
};
pub use storage::ensure_storage;

#[cfg(test)]
mod store_ownership_tests {
    use super::*;

    #[test]
    fn give_store_entry_is_a_no_op_without_root() {
        // Must never error or alter anything when running unprivileged —
        // this is the path every rootless setup takes.
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("entry");
        std::fs::write(&file, b"x").unwrap();
        let before = std::fs::metadata(&file).unwrap();
        give_store_entry_to_invoker(&file);
        let after = std::fs::metadata(&file).unwrap();
        use std::os::unix::fs::MetadataExt;
        assert_eq!(before.uid(), after.uid());
        assert_eq!(before.gid(), after.gid());
    }

    #[test]
    fn ownership_handback_refuses_symlinks() {
        // A path-based chown would follow a symlink planted in the
        // user-owned store and change the ownership of its target; the
        // hand-back must open the entry itself or skip it.
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("target");
        std::fs::write(&target, b"x").unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(open_store_entry_nofollow(&link).is_err());
        assert!(open_store_entry_nofollow(&target).is_ok());
        assert!(open_store_entry_nofollow(tmp.path()).is_ok());
    }

    #[tokio::test]
    async fn lock_store_dir_creates_dir_and_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("firecracker");
        let flock = lock_store_dir(&dir.join("x.lock"), "test store")
            .await
            .unwrap();
        assert!(dir.is_dir());
        assert!(dir.join("x.lock").is_file());
        flock.unlock().map_err(|(_, e)| e).unwrap();
    }

    #[tokio::test]
    async fn publish_store_entry_renames_into_place() {
        let tmp = tempfile::tempdir().unwrap();
        let staged = tmp.path().join("artifact.tmp");
        std::fs::write(&staged, b"bytes").unwrap();
        let dest = tmp.path().join("artifact.bin");
        publish_store_entry(&staged, &dest, "test artifact")
            .await
            .unwrap();
        assert!(!staged.exists());
        assert_eq!(std::fs::read(&dest).unwrap(), b"bytes");
    }
}

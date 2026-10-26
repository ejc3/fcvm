pub mod kernel;
pub mod nvidia;
pub mod pasta;
pub mod rootfs;
pub mod storage;

use anyhow::{Context, Result};
use nix::fcntl::{Flock, FlockArg, OFlag};
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
    nix::fcntl::open(
        path,
        OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        nix::sys::stat::Mode::empty(),
    )
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
    give_store_entry_to(path, sudo_invoker());
}

/// Hand a store entry to `user`, or leave it alone when there is none.
///
/// [`give_store_entry_to_invoker`] passes the sudo invoker. Tests pass a user
/// directly, because this crate's tests never set `SUDO_USER` (see
/// `test_env`).
pub(crate) fn give_store_entry_to(path: &Path, user: Option<&nix::unistd::User>) {
    if user.is_none() {
        return;
    }
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
    give_store_fd_to(&fd, path, user);
}

/// [`give_store_entry_to`] for an entry already open, which the pmem store hands to the
/// invoker it was opened for before it publishes the entry. `path` names it in the log.
pub(crate) fn give_store_fd_to(
    fd: &impl std::os::fd::AsFd,
    path: &Path,
    user: Option<&nix::unistd::User>,
) {
    let Some(user) = user else {
        return;
    };
    if let Err(err) = nix::unistd::fchown(fd, Some(user.uid), Some(user.gid)) {
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

/// A sibling of `final_path` that is unique to this builder, to stage a store entry
/// in before renaming it onto `final_path`.
///
/// The stores are shared across VMs: nested runs `--map` the host store into every
/// L1. The store lock ([`lock_store_dir`]) is a `flock`, and a fuse-pipe mount
/// negotiates no `FUSE_FLOCK_LOCKS`, so each guest kernel grants it locally and two
/// VMs building the same entry both hold it. A temp name shared by builders lets
/// one VM write, remove or rename the other's in-flight file. The name carries a
/// uuid, not a pid, because separate PID namespaces reuse pids. It sits in the
/// final path's directory, so the rename stays on one filesystem and is atomic, and
/// two builders of one content-addressed entry renaming in turn leave a complete
/// entry either way.
pub(crate) fn store_temp_path(final_path: &Path) -> std::path::PathBuf {
    let mut name = final_path
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_default();
    name.push(format!(".{}.tmp", uuid::Uuid::new_v4()));
    final_path.with_file_name(name)
}

/// A store temp ([`store_temp_path`]), file or directory, that is removed when
/// dropped unless it was published.
///
/// A fixed temp name was overwritten by the next build, so a failed build left at
/// most one behind. A unique name is never reused, so every failure path (including
/// a `?` return and a cancelled future) has to remove its own, and dropping the
/// guard does that.
pub(crate) struct StoreTemp {
    path: std::path::PathBuf,
    published: bool,
}

impl StoreTemp {
    /// Name a unique temp beside `final_path`. Nothing is created.
    pub(crate) fn new(final_path: &Path) -> Self {
        Self {
            path: store_temp_path(final_path),
            published: false,
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Rename the temp onto `dest` and hand it back to the sudo invoker.
    ///
    /// A rename replaces a file or an empty directory atomically, but fails on a
    /// directory that holds entries (ENOTEMPTY or EEXIST). For a content-addressed
    /// directory that means another builder published the same entry first: theirs
    /// is kept, this temp is removed, and the publish succeeds.
    pub(crate) async fn publish(mut self, dest: &Path, what: &str) -> Result<()> {
        match tokio::fs::rename(&self.path, dest).await {
            Ok(()) => {
                self.published = true;
                give_store_entry_to_invoker(dest);
                Ok(())
            }
            Err(err)
                if matches!(err.raw_os_error(), Some(libc::ENOTEMPTY | libc::EEXIST))
                    && dest.is_dir() =>
            {
                tracing::info!(
                    path = %dest.display(),
                    "{what} was published by another builder; keeping it"
                );
                Ok(())
            }
            Err(err) => {
                Err(err).with_context(|| format!("renaming {what} into {}", dest.display()))
            }
        }
    }
}

impl Drop for StoreTemp {
    fn drop(&mut self) {
        if self.published {
            return;
        }
        let removed = match std::fs::symlink_metadata(&self.path) {
            Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(&self.path),
            Ok(_) => std::fs::remove_file(&self.path),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err),
        };
        if let Err(err) = removed {
            warn!(path = %self.path.display(), %err, "removing an unpublished store temp");
        }
    }
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

    match Flock::lock(lock_fd, FlockArg::LockExclusiveNonblock) {
        Ok(lock) => Ok(lock),
        Err((lock_fd, nix::errno::Errno::EWOULDBLOCK)) => {
            failpoint::note("lock.wait", lock_path.display());
            Flock::lock(lock_fd, FlockArg::LockExclusive)
                .map_err(|(_, err)| err)
                .with_context(|| format!("acquiring exclusive lock for {what}"))
        }
        Err((_, err)) => Err(err).with_context(|| format!("acquiring exclusive lock for {what}")),
    }
}

pub use kernel::{
    ensure_cloud_hypervisor, ensure_kernel, ensure_profile_firecracker,
    get_configured_firecracker_for_profile, get_firecracker_for_profile, get_kernel_path,
    get_kernel_url_hash, get_profile_firecracker_path, install_host_kernel,
    newest_cached_cloud_hypervisor, rebuild_kernel_from_source,
};
pub use nvidia::{
    ensure_nvidia_driver_image, get_nvidia_driver_image_path, profile_module_type,
    profile_nvidia_driver_image, ModuleType,
};
pub use pasta::{ensure_pasta, get_pasta_for_config};
pub use rootfs::{
    ensure_fc_agent_initrd, ensure_rootfs, firecracker_args_enable_nv2, get_kernel_profile,
    resolve_rootfs_type, KernelProfile,
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

    #[test]
    fn store_temps_are_unique_uuid_keyed_siblings() {
        let final_path = Path::new("/store/rootfs/layer2-abc.raw");
        let a = store_temp_path(final_path);
        let b = store_temp_path(final_path);
        assert_ne!(a, b, "two builders of one entry must not share a temp");
        for temp in [&a, &b] {
            assert_eq!(temp.parent(), final_path.parent(), "{}", temp.display());
            let name = temp.file_name().unwrap().to_str().unwrap();
            let key = name
                .strip_prefix("layer2-abc.raw.")
                .and_then(|rest| rest.strip_suffix(".tmp"))
                .unwrap_or_else(|| panic!("unexpected temp name {name}"));
            // A uuid, not the pid: separate PID namespaces reuse pids.
            uuid::Uuid::parse_str(key)
                .unwrap_or_else(|e| panic!("temp key {key} is not a uuid: {e}"));
        }
    }

    #[tokio::test]
    async fn an_unpublished_store_temp_is_removed_on_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let file = StoreTemp::new(&tmp.path().join("kernel.bin"));
        std::fs::write(file.path(), b"partial").unwrap();
        let dir = StoreTemp::new(&tmp.path().join("packages-abc"));
        std::fs::create_dir_all(dir.path().join("nested")).unwrap();
        std::fs::write(dir.path().join("nested/a.deb"), b"x").unwrap();
        let never_created = StoreTemp::new(&tmp.path().join("missing"));
        let paths = [file.path().to_path_buf(), dir.path().to_path_buf()];
        drop((file, dir, never_created));
        for path in paths {
            assert!(!path.exists(), "{} was left behind", path.display());
        }
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn store_temp_publish_renames_into_place() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("artifact.bin");
        std::fs::write(&dest, b"old").unwrap();
        let temp = StoreTemp::new(&dest);
        let staged = temp.path().to_path_buf();
        std::fs::write(&staged, b"new").unwrap();
        temp.publish(&dest, "test artifact").await.unwrap();
        assert!(!staged.exists());
        assert_eq!(std::fs::read(&dest).unwrap(), b"new");
    }

    /// Two builders in different kernels download the same package set into their
    /// own temps and publish them in turn: the second finds the first's directory
    /// in place, keeps it, and removes its own temp.
    #[tokio::test]
    async fn two_builders_publish_one_directory_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("packages-abc");
        let first = StoreTemp::new(&dest);
        let second = StoreTemp::new(&dest);
        for (temp, body) in [(&first, b"first"), (&second, b"other")] {
            std::fs::create_dir_all(temp.path()).unwrap();
            std::fs::write(temp.path().join("a.deb"), body).unwrap();
        }
        let second_path = second.path().to_path_buf();
        first.publish(&dest, "packages").await.unwrap();
        second.publish(&dest, "packages").await.unwrap();
        assert_eq!(std::fs::read(dest.join("a.deb")).unwrap(), b"first");
        assert!(
            !second_path.exists(),
            "the losing builder's temp was left behind"
        );
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 1);
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

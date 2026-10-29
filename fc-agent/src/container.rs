use anyhow::{Context, Result};
use std::path::Path;
use std::process::Stdio;
use std::sync::OnceLock;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
    time::{sleep, Duration},
};

use crate::output::OutputHandle;
use crate::types::Plan;
use crate::vsock;

/// Where podman finds the overlay image store.
pub const IMAGE_STORE_PATH: &str = "/mnt/image-store";
/// Where the store's device is mounted for a run with a user, under the bind mount at
/// [`IMAGE_STORE_PATH`]. It is in /run, a tmpfs, so no map of the guest's /mnt can make
/// its mount point impossible to create.
pub const IMAGE_STORE_RAW_PATH: &str = "/run/fcvm-image-store-raw";
/// The store's layer list as a run with a user shows it, each layer carrying the id
/// mapping the store is shown through. It is bound over the store's own list.
pub const IMAGE_STORE_LAYERS_COPY: &str = "/run/fcvm-image-store-layers.json";
/// Where podman reads a store's layer list, relative to the store.
const STORE_LAYER_LIST: &str = "overlay-layers/layers.json";

/// The layer list podman reads from the image store.
pub fn image_store_layers_path() -> String {
    format!("{IMAGE_STORE_PATH}/{STORE_LAYER_LIST}")
}

/// The container user's ids, which the overlay store is shown with. Rootless podman's
/// user namespace has the user's own id as its id 0 and the user's subordinate ids from
/// `sub_start` as its ids 1 to `sub_count`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreUser {
    pub uid: u32,
    pub gid: u32,
    pub sub_start: u64,
    pub sub_count: u64,
    pub sub_gid_start: u64,
    pub sub_gid_count: u64,
}

/// Who a store's files belong to on disk: the ids container uid and gid 0 were written
/// as, and where container uids and gids 1 and up begin. Root podman on the host writes
/// the container ids themselves (0, 0, 1, 1). Rootless podman on the host writes its
/// builder's uid and gid, and its subordinate ids from the subuid and subgid starts the
/// host passes in the plan, which are also the container user's ranges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StoreOwner {
    uid: u32,
    gid: u32,
    sub_start: u64,
    sub_gid_start: u64,
}

impl StoreOwner {
    /// The owner a store has, from who owns its `overlay-images` directory.
    fn of(meta: &std::fs::Metadata, user: &StoreUser) -> Self {
        use std::os::unix::fs::MetadataExt;
        let root_built = meta.uid() == 0;
        Self {
            uid: meta.uid(),
            gid: meta.gid(),
            sub_start: if root_built { 1 } else { user.sub_start },
            sub_gid_start: if root_built { 1 } else { user.sub_gid_start },
        }
    }
}

/// One range of an id mapping: `size` ids from `container` show as ids from `host`.
/// The field names are the ones containers/storage writes into a layer's `uidmap` and
/// `gidmap`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
struct IdRange {
    #[serde(rename = "container_id")]
    container: u64,
    #[serde(rename = "host_id")]
    host: u64,
    size: u64,
}

/// The mapping rootless podman's `--userns=keep-id` gives a container, from container ids
/// to ids in the user's namespace, in podman's order (`GetKeepIDMapping` in podman's
/// pkg/util): container ids below the user's id are namespace ids from 1, the user's id
/// is the user (namespace id 0), and the ones above it are themselves. `count` is the
/// user's subordinate range, which podman reads from /etc/subuid or /etc/subgid.
fn keep_id_map(id: u64, count: u64) -> Vec<IdRange> {
    let mut map = Vec::new();
    if count > 0 {
        map.push(IdRange {
            container: 0,
            host: 1,
            size: id.min(count),
        });
    }
    map.push(IdRange {
        container: id,
        host: 0,
        size: 1,
    });
    if count > id {
        map.push(IdRange {
            container: id + 1,
            host: id + 1,
            size: count - id,
        });
    }
    map
}

/// The `X-mount.idmap` ranges (on-disk id, guest id, count) that show a store's files as
/// a keep-id container of `user` sees them. `keep_id` takes container ids to namespace
/// ids. Namespace id 0 is the guest id `user`, and namespace id k >= 1 is `sub_start` +
/// k - 1. On disk, container id 0 is `owner` and container id c >= 1 is `owner_sub` +
/// c - 1. Each range is split where it meets container id 0 or namespace id 0, the two
/// ids the formulas treat apart.
fn store_idmap(
    keep_id: &[IdRange],
    user: u64,
    sub_start: u64,
    owner: u64,
    owner_sub: u64,
) -> Vec<(u64, u64, u64)> {
    let on_disk = |c: u64| if c == 0 { owner } else { owner_sub + c - 1 };
    let in_guest = |k: u64| if k == 0 { user } else { sub_start + k - 1 };
    let mut ranges = Vec::new();
    for range in keep_id {
        let (mut c, mut k, mut left) = (range.container, range.host, range.size);
        while left > 0 {
            let n = if c == 0 || k == 0 { 1 } else { left };
            ranges.push((on_disk(c), in_guest(k), n));
            c += n;
            k += n;
            left -= n;
        }
    }
    ranges
}

impl StoreUser {
    /// The uid and gid mappings a keep-id container of this user gets, as container
    /// ids to ids in the user's namespace.
    fn keep_id_maps(&self) -> (Vec<IdRange>, Vec<IdRange>) {
        (
            keep_id_map(self.uid.into(), self.sub_count),
            keep_id_map(self.gid.into(), self.sub_gid_count),
        )
    }

    /// The `X-mount.idmap` option (util-linux 2.39) that shows a store `owner` wrote
    /// with the ownership a keep-id container of this user gives its files. That is the
    /// ownership podman would otherwise give the container's layer by copying every file
    /// of the image into it to change its owner.
    fn idmap_option(&self, owner: StoreOwner) -> String {
        let (uids, gids) = self.keep_id_maps();
        let uid_ranges = store_idmap(
            &uids,
            self.uid.into(),
            self.sub_start,
            owner.uid.into(),
            owner.sub_start,
        );
        let gid_ranges = store_idmap(
            &gids,
            self.gid.into(),
            self.sub_gid_start,
            owner.gid.into(),
            owner.sub_gid_start,
        );
        let entries = uid_ranges
            .iter()
            .map(|(disk, guest, n)| format!("u:{disk}:{guest}:{n}"))
            .chain(
                gid_ranges
                    .iter()
                    .map(|(disk, guest, n)| format!("g:{disk}:{guest}:{n}")),
            );
        format!("X-mount.idmap={}", entries.collect::<Vec<_>>().join(" "))
    }

    /// The store's layer list `layers_json` with this user's keep-id mappings as every
    /// layer's `uidmap` and `gidmap`. containers/storage compares a container's mapping
    /// with its image's top layer's, and copies the whole image into the container's
    /// layer to change its owners when they differ; with the store shown through
    /// [`Self::idmap_option`], its files already have the ownership that mapping wants.
    /// A layer that already carries a mapping is refused: the id-mapped mount assumes
    /// the store holds plain container ids.
    fn layers_with_keep_id(&self, layers_json: &str) -> Result<String> {
        let (uids, gids) = self.keep_id_maps();
        let mut layers: Vec<serde_json::Map<String, serde_json::Value>> =
            serde_json::from_str(layers_json).context("parsing the store's layers.json")?;
        for layer in &mut layers {
            for key in ["uidmap", "gidmap"] {
                if layer
                    .get(key)
                    .and_then(|map| map.as_array())
                    .is_some_and(|map| !map.is_empty())
                {
                    anyhow::bail!(
                        "layer {} in the image store already has a {key}; a run with a \
                         user needs a store whose layers carry no mapping, so rebuild the \
                         image's overlay storage image",
                        layer
                            .get("id")
                            .and_then(|id| id.as_str())
                            .unwrap_or("(no id)")
                    );
                }
            }
            layer.insert("uidmap".to_string(), serde_json::to_value(&uids)?);
            layer.insert("gidmap".to_string(), serde_json::to_value(&gids)?);
        }
        Ok(serde_json::to_string(&layers)?)
    }
}

/// Mount a pre-built overlay storage image and configure as additionalImageStore.
///
/// The storage image is an ext4 filesystem containing a podman overlay storage tree
/// (overlay/, overlay-images/, overlay-layers/). It is mounted read-only and podman
/// finds the image there without needing `podman load`. The mounts go into `record`.
///
/// With a user, the device is mounted at [`IMAGE_STORE_RAW_PATH`] and bound at
/// [`IMAGE_STORE_PATH`] through an id-mapped mount that shows the store's files with the
/// ownership the container's `--userns=keep-id` gives them, and a copy of the store's
/// layer list that records that mapping on every layer is bound over the store's own.
/// Rootless podman then runs the container on the store's layers as they are. That
/// covers a store root podman built and one rootless podman built as any uid.
pub fn mount_overlay_image(
    device: &str,
    image_name: &str,
    username: Option<&str>,
    store_user: Option<StoreUser>,
    record: &mut crate::mounts::MountRecord,
) -> Result<String> {
    eprintln!("[fc-agent] mounting overlay storage image: {}", device);

    let mount_path = IMAGE_STORE_PATH;
    std::fs::create_dir_all(mount_path).context("creating image store mount point")?;

    wait_for_device(device)?;

    let device_path = if store_user.is_some() {
        std::fs::create_dir_all(IMAGE_STORE_RAW_PATH)
            .context("creating the raw image store mount point")?;
        IMAGE_STORE_RAW_PATH
    } else {
        mount_path
    };
    let output = std::process::Command::new("mount")
        .args(["-o", "ro", device, device_path])
        .output()
        .context("running mount for the overlay storage image")?;
    mount_succeeded(&output)
        .with_context(|| format!("mounting overlay storage image {device} at {device_path}"))?;
    record.record(format!("image store {device}"), device_path)?;

    if let Some(user) = store_user {
        let meta = std::fs::metadata(format!("{IMAGE_STORE_RAW_PATH}/overlay-images"))
            .context("reading who owns the overlay storage image")?;
        let owner = StoreOwner::of(&meta, &user);
        let options = format!("ro,{}", user.idmap_option(owner));
        let output = std::process::Command::new("mount")
            .args(["--bind", "-o", &options, IMAGE_STORE_RAW_PATH, mount_path])
            .output()
            .context("running mount for the image store bind")?;
        mount_succeeded(&output).with_context(|| {
            format!("binding the image store at {mount_path} with options {options}")
        })?;
        record.record(format!("image store bind of {device}"), mount_path)?;

        let layers = std::fs::read_to_string(format!("{IMAGE_STORE_RAW_PATH}/{STORE_LAYER_LIST}"))
            .context("reading the image store's layers.json")?;
        let layers_path = image_store_layers_path();
        write_readable(IMAGE_STORE_LAYERS_COPY, &user.layers_with_keep_id(&layers)?)?;
        let output = std::process::Command::new("mount")
            .args(["--bind", "-o", "ro", IMAGE_STORE_LAYERS_COPY, &layers_path])
            .output()
            .context("running mount for the image store's layer list")?;
        mount_succeeded(&output)
            .with_context(|| format!("binding {IMAGE_STORE_LAYERS_COPY} over {layers_path}"))?;
        record.record(format!("image store layer list of {device}"), &layers_path)?;
        eprintln!(
            "[fc-agent] image store owned by uid {} on disk, shown through an id-mapped \
             mount as a keep-id container of uid {} sees it",
            owner.uid, user.uid
        );
    }

    // Configure podman to use this as an additional image store.
    let (conf_path, runroot, graphroot) = storage_paths(username);

    let storage_conf = format!(
        "[storage]\ndriver = \"overlay\"\nrunroot = \"{runroot}\"\ngraphroot = \"{graphroot}\"\n\n[storage.options]\nadditionalimagestores = [\"{mount_path}\"]\n"
    );
    std::fs::write(&conf_path, &storage_conf).context("writing storage.conf")?;

    // Write containers.conf to disable netavark (VM uses --network=host)
    let containers_conf_path = if username.is_some() {
        // User-level containers.conf lives alongside storage.conf
        conf_path.replace("storage.conf", "containers.conf")
    } else {
        "/etc/containers/containers.conf".to_string()
    };
    write_containers_conf(&containers_conf_path);

    eprintln!(
        "[fc-agent] overlay image mounted at {}, configured as additional image store (conf: {})",
        mount_path, conf_path
    );

    // Discover the actual image reference in the overlay store.
    // The overlay cache is keyed by digest (content-addressed), but the image
    // inside is tagged with the name from the original `podman save`. When the
    // same content is built under a different name, the cached overlay has the
    // old name. We ask podman what's actually there instead of trusting the
    // expected name from the Plan.
    let discover_output = if let Some(name) = username {
        let user_pw = nix::unistd::User::from_name(name).ok().flatten();
        let uid = user_pw.map(|u| u.uid.as_raw()).unwrap_or(0);
        std::process::Command::new("env")
            .args([
                &format!("HOME=/home/{}", name),
                &format!("XDG_RUNTIME_DIR=/run/user/{}", uid),
                "runuser",
                "-u",
                name,
                "--",
                "podman",
                "images",
                "--format",
                "{{.Repository}}:{{.Tag}}",
            ])
            .output()
    } else {
        std::process::Command::new("podman")
            .args(["images", "--format", "{{.Repository}}:{{.Tag}}"])
            .output()
    };

    if let Ok(output) = discover_output {
        if output.status.success() {
            let images_out = String::from_utf8_lossy(&output.stdout);
            // Find first image that isn't <none>
            if let Some(actual_ref) = images_out
                .lines()
                .find(|line| !line.contains("<none>") && !line.trim().is_empty())
            {
                let actual_ref = actual_ref.trim().to_string();
                if actual_ref != image_name {
                    eprintln!(
                        "[fc-agent] overlay store contains '{}' (expected '{}'), using discovered name",
                        actual_ref, image_name
                    );
                }
                return Ok(actual_ref);
            }
        }
    }

    // Fallback: return expected name (downstream will fail with clear error if wrong)
    Ok(image_name.to_string())
}

/// Write `contents` to `path` with mode 0644 whatever the umask. The layer list copy
/// belongs to root, which the user's namespace does not map, so rootless podman reads it
/// through its "other" permission bits.
fn write_readable(path: &str, contents: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let mut file = std::fs::File::create(path).with_context(|| format!("creating {path}"))?;
    // The mode given at creation is masked by the umask, so it is set after.
    file.set_permissions(std::fs::Permissions::from_mode(0o644))
        .with_context(|| format!("setting the mode of {path}"))?;
    file.write_all(contents.as_bytes())
        .with_context(|| format!("writing {path}"))
}

/// Whether a mount(8) run succeeded; a failure carries its stderr.
fn mount_succeeded(output: &std::process::Output) -> Result<()> {
    if !output.status.success() {
        anyhow::bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(())
}

/// Wait for a block device to appear (up to 5 seconds).
fn wait_for_device(device: &str) -> Result<()> {
    let device_path = std::path::Path::new(device);
    for attempt in 1..=10 {
        if device_path.exists() {
            return Ok(());
        }
        if attempt == 10 {
            anyhow::bail!("Device {} not found after 10 attempts", device);
        }
        eprintln!(
            "[fc-agent] waiting for device {} (attempt {}/10)",
            device, attempt
        );
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    Ok(())
}

/// Get storage.conf path, runroot, and graphroot for the given user.
fn storage_paths(username: Option<&str>) -> (String, String, String) {
    if let Some(name) = username {
        let home = format!("/home/{}", name);
        let config_dir = format!("{}/.config", home);
        let conf_dir = format!("{}/containers", config_dir);
        let _ = std::fs::create_dir_all(&conf_dir);
        // Look up user once and reuse for chown and UID extraction
        let user_pw = nix::unistd::User::from_name(name).ok().flatten();
        if let Some(ref pw) = user_pw {
            let _ = nix::unistd::chown(config_dir.as_str(), Some(pw.uid), Some(pw.gid));
            let _ = nix::unistd::chown(conf_dir.as_str(), Some(pw.uid), Some(pw.gid));
        }
        let uid = user_pw.map(|u| u.uid.as_raw()).unwrap_or(0);
        (
            format!("{}/storage.conf", conf_dir),
            format!("/run/user/{}/containers", uid),
            format!("{}/.local/share/containers/storage", home),
        )
    } else {
        (
            "/etc/containers/storage.conf".to_string(),
            "/run/containers/storage".to_string(),
            "/var/lib/containers/storage".to_string(),
        )
    }
}

/// Write a minimal storage.conf that names the overlay graph driver.
///
/// Podman records the graph driver in its database the first time it runs. If
/// the default storage.conf has `driver = ""` (auto-detect), a podman command
/// exec'd before `mount_overlay_image()` records what it detected, and the
/// storage.conf that function writes later does not match it: "database graph
/// driver '' does not match our graph driver 'overlay'". Naming overlay here keeps
/// every podman invocation consistent with the final configuration.
pub fn write_early_storage_conf() {
    let (conf_path, runroot, graphroot) = storage_paths(None);
    let conf = format!(
        "[storage]\ndriver = \"overlay\"\nrunroot = \"{runroot}\"\ngraphroot = \"{graphroot}\"\n"
    );
    if let Err(e) = std::fs::write(&conf_path, conf) {
        eprintln!(
            "[fc-agent] WARNING: failed to write early storage.conf: {}",
            e
        );
    }
}

/// Write containers.conf to disable netavark requirement.
///
/// The VM always uses `--network=host` for containers, so podman never needs
/// netavark for container networking. However, podman 4.x checks for netavark
/// at startup even with `--network=host`. The rootfs may not have netavark
/// installed (it's not required for our use case), so we configure podman to
/// skip the check by setting `network_backend = "cni"`.
fn write_containers_conf(conf_path: &str) {
    let containers_conf = "[network]\nnetwork_backend = \"cni\"\n";
    let conf_dir = std::path::Path::new(conf_path).parent();
    if let Some(dir) = conf_dir {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(conf_path, containers_conf) {
        eprintln!(
            "[fc-agent] WARNING: failed to write containers.conf at {}: {}",
            conf_path, e
        );
    }
}

/// Global command prefix for running podman commands as the target user.
/// Set once during setup, used by exec server and health checks.
/// Empty when running as root (no user mapping).
static PODMAN_CMD_PREFIX: OnceLock<Vec<String>> = OnceLock::new();

/// Where [`set_podman_cmd_prefix`] leaves the prefix for one-shot `fc-agent` processes
/// (`--gpu-park`, `--gpu-unpark`), which run apart from the agent and need the same
/// container user context. On /run, which is guest memory, so a restored clone keeps it.
pub const PODMAN_CMD_PREFIX_RECORD: &str = "/run/fcvm-podman-cmd-prefix.json";

/// Store the command prefix for use by exec and other podman commands, and record it
/// at [`PODMAN_CMD_PREFIX_RECORD`] for the one-shot processes.
pub fn set_podman_cmd_prefix(prefix: Vec<String>) {
    if let Err(e) = store_podman_cmd_prefix(Path::new(PODMAN_CMD_PREFIX_RECORD), &prefix) {
        eprintln!("[fc-agent] WARNING: recording the podman command prefix failed: {e:#}");
    }
    let _ = PODMAN_CMD_PREFIX.set(prefix);
}

fn store_podman_cmd_prefix(path: &Path, prefix: &[String]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec(prefix)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("renaming to {}", path.display()))
}

fn load_podman_cmd_prefix(path: &Path) -> Result<Vec<String>> {
    let bytes = std::fs::read(path).with_context(|| {
        format!(
            "reading {} (the agent writes it once the container user is set up)",
            path.display()
        )
    })?;
    serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))
}

/// In a one-shot process: take the agent's command prefix from
/// [`PODMAN_CMD_PREFIX_RECORD`], so podman commands run as the container's user. A
/// missing record is an error, since assuming root would look in another user's
/// container store.
pub fn restore_podman_cmd_prefix() -> Result<()> {
    let prefix = load_podman_cmd_prefix(Path::new(PODMAN_CMD_PREFIX_RECORD))?;
    let _ = PODMAN_CMD_PREFIX.set(prefix);
    Ok(())
}

/// Get the command prefix (empty vec if running as root).
pub fn podman_cmd_prefix() -> &'static [String] {
    PODMAN_CMD_PREFIX.get().map(|v| v.as_slice()).unwrap_or(&[])
}

/// Write a btrfs storage.conf for root podman.
pub fn write_btrfs_storage_conf(conf_path: &str, graphroot: &str, runroot: &str) {
    let _ = std::fs::create_dir_all(
        std::path::Path::new(conf_path)
            .parent()
            .unwrap_or(std::path::Path::new("/etc/containers")),
    );
    let storage_conf = format!(
        "[storage]\ndriver = \"btrfs\"\nrunroot = \"{}\"\ngraphroot = \"{}\"\n",
        runroot, graphroot
    );
    if let Err(e) = std::fs::write(conf_path, &storage_conf) {
        eprintln!(
            "[fc-agent] WARNING: failed to write storage.conf at {}: {}",
            conf_path, e
        );
    }
    let containers_conf = conf_path.replace("storage.conf", "containers.conf");
    write_containers_conf(&containers_conf);
}

/// Get the total size of the filesystem containing `path` in bytes.
fn get_filesystem_size_bytes(path: &str) -> Option<u64> {
    let output = std::process::Command::new("df")
        .args(["--output=size", "-B1", path])
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    // df output: header line then value line with size in bytes
    stdout
        .lines()
        .nth(1)?
        .trim()
        .parse()
        .ok()
        .filter(|&b: &u64| b > 0)
}

/// Grow a btrfs root filesystem to fill its disk, which the host extends for
/// `--rootfs-size`, and return whether the root is btrfs. An ext4 root is grown on the
/// host. Every image mode needs it: the container's writable layer is on the root.
/// A failed resize is a warning; the VM still runs on the smaller filesystem.
pub fn grow_btrfs_root() -> bool {
    let root_is_btrfs = std::process::Command::new("findmnt")
        .args(["-n", "-o", "FSTYPE", "/"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "btrfs")
        .unwrap_or(false);
    if !root_is_btrfs {
        return false;
    }
    match std::process::Command::new("btrfs")
        .args(["filesystem", "resize", "max", "/"])
        .output()
    {
        Ok(output) if output.status.success() => {
            eprintln!("[fc-agent] root filesystem is btrfs, resized to fill disk");
        }
        Ok(output) => {
            eprintln!(
                "[fc-agent] WARNING: btrfs resize failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Err(e) => {
            eprintln!("[fc-agent] WARNING: btrfs resize command failed: {}", e);
        }
    }
    true
}

/// Set up btrfs storage if the kernel supports it.
/// Creates a sparse loopback btrfs filesystem sized to the root disk capacity
/// and configures podman to use it.
/// This avoids overlay's idmap issues that cause expensive chown-copy on rootless podman.
/// Errors when a PROVISIONED disk's existing storage loopback fails to mount —
/// continuing would silently detach the captured container (the boot would fall back
/// to an empty store and `podman run` a fresh container). Fresh-boot setup problems
/// before the loopback is mounted remain best-effort warnings (the VM can still run
/// with the default driver). A loopback it mounts goes into `record`, and failing to
/// record it is an error on either path.
pub fn setup_btrfs_storage_if_available(
    record: &mut crate::mounts::MountRecord,
) -> anyhow::Result<()> {
    // Check if kernel has btrfs support via /proc/filesystems.
    // Note: /sys/fs/btrfs only appears after a btrfs filesystem is mounted,
    // so it can't detect built-in (CONFIG_BTRFS_FS=y) support before first mount.
    let has_btrfs = std::fs::read_to_string("/proc/filesystems")
        .map(|content| content.lines().any(|line| line.trim().ends_with("btrfs")))
        .unwrap_or(false);
    if !has_btrfs {
        eprintln!("[fc-agent] btrfs not available in kernel, using default storage driver");
        return Ok(());
    }

    // A native btrfs root is grown to fill its disk and holds the storage itself.
    if grow_btrfs_root() {
        let storage_dir = "/var/lib/containers/storage";
        let _ = std::fs::create_dir_all(storage_dir);
        write_btrfs_storage_conf(
            "/etc/containers/storage.conf",
            storage_dir,
            "/run/containers/storage",
        );
        return Ok(());
    }

    let storage_dir = "/var/lib/containers/storage";
    let loopback_path = "/var/lib/containers/btrfs.img";

    // Skip if already btrfs (either native btrfs root or pre-existing loopback mount)
    let already_btrfs = std::process::Command::new("findmnt")
        .args(["-n", "-o", "FSTYPE", storage_dir])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "btrfs")
        .unwrap_or(false);

    if already_btrfs {
        eprintln!(
            "[fc-agent] btrfs storage already mounted at {}",
            storage_dir
        );
        write_btrfs_storage_conf(
            "/etc/containers/storage.conf",
            storage_dir,
            "/run/containers/storage",
        );
        return Ok(());
    }

    // Disk-only clone: the loopback was formatted and populated on the source's
    // first boot and reflinked into this disk. Mount it as-is — NEVER mkfs, which
    // would erase the captured image + container. Mounts don't survive a reboot,
    // so the file exists but isn't mounted yet.
    if is_provisioned() && std::path::Path::new(loopback_path).exists() {
        match std::process::Command::new("mount")
            .args(["-o", "loop", loopback_path, storage_dir])
            .output()
        {
            Ok(o) if o.status.success() => {
                record.record(format!("container storage {loopback_path}"), storage_dir)?;
                use std::os::unix::fs::PermissionsExt;
                if let Some(parent) = std::path::Path::new(storage_dir).parent() {
                    let _ =
                        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o755));
                }
                let _ =
                    std::fs::set_permissions(storage_dir, std::fs::Permissions::from_mode(0o755));
                write_btrfs_storage_conf(
                    "/etc/containers/storage.conf",
                    storage_dir,
                    "/run/containers/storage",
                );
                eprintln!("[fc-agent] mounted existing btrfs storage loopback (clone)");
                return Ok(());
            }
            Ok(o) => {
                anyhow::bail!(
                    "mounting existing provisioned btrfs loopback failed: {} — refusing \
                     to continue (the captured container storage would be silently lost)",
                    String::from_utf8_lossy(&o.stderr).trim()
                );
            }
            Err(e) => {
                anyhow::bail!(
                    "mount of existing provisioned btrfs loopback failed: {e} — refusing \
                     to continue (the captured container storage would be silently lost)"
                );
            }
        }
    }

    // Size the sparse loopback to the root filesystem capacity.
    // Since it's sparse, only written blocks use real space — share full disk capacity.
    let loopback_size = get_filesystem_size_bytes("/")
        .map(|b| b.to_string())
        .unwrap_or_else(|| "8G".to_string());

    let _ = std::fs::create_dir_all(storage_dir);
    let truncate = std::process::Command::new("truncate")
        .args(["-s", &loopback_size, loopback_path])
        .output();
    match truncate {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            eprintln!(
                "[fc-agent] WARNING: truncate failed: {}",
                String::from_utf8_lossy(&o.stderr)
            );
            return Ok(());
        }
        Err(e) => {
            eprintln!("[fc-agent] WARNING: failed to create btrfs loopback: {}", e);
            return Ok(());
        }
    }

    // Format as btrfs
    let mkfs = std::process::Command::new("mkfs.btrfs")
        .args(["-f", loopback_path])
        .output();
    match mkfs {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            eprintln!(
                "[fc-agent] WARNING: mkfs.btrfs failed: {}",
                String::from_utf8_lossy(&o.stderr)
            );
            return Ok(());
        }
        Err(e) => {
            eprintln!("[fc-agent] WARNING: mkfs.btrfs not found: {}", e);
            return Ok(());
        }
    }

    // Mount
    let mount = std::process::Command::new("mount")
        .args(["-o", "loop", loopback_path, storage_dir])
        .output();
    match mount {
        Ok(o) if o.status.success() => {
            record.record(format!("container storage {loopback_path}"), storage_dir)?;
        }
        Ok(o) => {
            eprintln!(
                "[fc-agent] WARNING: mount btrfs failed: {}",
                String::from_utf8_lossy(&o.stderr)
            );
            return Ok(());
        }
        Err(e) => {
            eprintln!("[fc-agent] WARNING: mount failed: {}", e);
            return Ok(());
        }
    }

    // Make the btrfs mount and parent traversable by non-root users.
    // Rootless podman needs to traverse this path to reach its graphroot subdirectory.
    {
        use std::os::unix::fs::PermissionsExt;
        if let Some(parent) = std::path::Path::new(storage_dir).parent() {
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o755));
        }
        let _ = std::fs::set_permissions(storage_dir, std::fs::Permissions::from_mode(0o755));
    }

    write_btrfs_storage_conf(
        "/etc/containers/storage.conf",
        storage_dir,
        "/run/containers/storage",
    );

    eprintln!(
        "[fc-agent] btrfs storage configured at {} ({} sparse loopback)",
        storage_dir, loopback_size
    );
    Ok(())
}

/// Marker written on the rootfs once first-boot provisioning (storage + image +
/// container) completes. A disk-only clone cold-boots from a reflink of that
/// rootfs, so the marker is already present — the signal that says "the work is
/// already here, don't redo (or destroy) it; just regenerate the identity."
const PROVISIONED_MARKER: &str = "/var/lib/fcvm/provisioned";

/// True if this boot is from an already-provisioned disk (a disk-only clone).
pub fn is_provisioned() -> bool {
    std::path::Path::new(PROVISIONED_MARKER).exists()
}

/// Record that first-boot provisioning completed. Idempotent.
pub fn write_provisioned_marker() {
    if let Some(parent) = std::path::Path::new(PROVISIONED_MARKER).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(PROVISIONED_MARKER, b"1\n") {
        eprintln!("[fc-agent] WARNING: failed to write provisioned marker: {e}");
    }
}

/// Whether a container named `fcvm-container` already exists in podman storage.
/// On a clone boot the container is already created — we start it rather than
/// `podman run` a fresh one, preserving its writable layer (the captured work).
pub fn container_exists(cmd_prefix: &[String]) -> bool {
    let mut cmd = if cmd_prefix.is_empty() {
        std::process::Command::new("podman")
    } else {
        let mut c = std::process::Command::new(&cmd_prefix[0]);
        c.args(&cmd_prefix[1..]);
        c.arg("podman");
        c
    };
    cmd.args(["container", "exists", "fcvm-container"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Build the argv to start (and attach to) the captured `fcvm-container`.
/// Mirrors `build_podman_args`' rootless prefixing so the same run_tty/run_async
/// path drives it.
pub fn build_start_args(plan: &Plan, user_info: Option<(&str, &str)>) -> Vec<String> {
    let mut args = Vec::new();
    if let Some((username, runtime_dir)) = user_info {
        args.extend(run_as_user_prefix(username, runtime_dir));
    }
    args.push("podman".to_string());
    args.push("start".to_string());
    // Attach stdout/stderr (and stdin when interactive) so fc-agent forwards
    // the container's output exactly as it does for `podman run`.
    if plan.interactive {
        args.push("--attach".to_string());
        args.push("--interactive".to_string());
    } else {
        args.push("--attach".to_string());
    }
    args.push("fcvm-container".to_string());
    args
}

/// Regenerate per-machine identity on a clone so concurrently-running clones of
/// the same disk don't collide. Hostname is set separately from the Plan.
pub fn regenerate_identity() {
    // machine-id: a fresh random id (systemd/dbus and many apps key off it).
    if let Ok(uuid) = std::fs::read_to_string("/proc/sys/kernel/random/uuid") {
        let id = format!("{}\n", uuid.trim().replace('-', ""));
        let _ = std::fs::write("/etc/machine-id", &id);
        if std::path::Path::new("/var/lib/dbus").is_dir() {
            let _ = std::fs::write("/var/lib/dbus/machine-id", &id);
        }
        eprintln!("[fc-agent] regenerated machine-id for clone");
    }
    // SSH host keys: regenerate if sshd is present (best-effort).
    if std::path::Path::new("/etc/ssh").is_dir() {
        if let Ok(entries) = std::fs::read_dir("/etc/ssh") {
            for entry in entries.flatten() {
                if entry.file_name().to_string_lossy().starts_with("ssh_host_") {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        let _ = std::process::Command::new("ssh-keygen").arg("-A").output();
    }
}

/// Import a Docker archive into podman storage. Returns image reference.
/// If cmd_prefix is provided, prepend it to the podman command (e.g., for runuser).
pub async fn import_image(
    archive_path: &str,
    image_name: &str,
    output: &OutputHandle,
    cmd_prefix: &[String],
) -> Result<String> {
    eprintln!("[fc-agent] importing Docker archive: {}", archive_path);

    if archive_path.starts_with("/dev/") {
        let _ = std::process::Command::new("chmod")
            .args(["444", archive_path])
            .output();
    }

    let (cmd, args) = if cmd_prefix.is_empty() {
        (
            "podman".to_string(),
            vec![
                "load".to_string(),
                "-i".to_string(),
                archive_path.to_string(),
            ],
        )
    } else {
        let mut all_args: Vec<String> = cmd_prefix[1..].to_vec();
        all_args.extend([
            "podman".to_string(),
            "load".to_string(),
            "-i".to_string(),
            archive_path.to_string(),
        ]);
        (cmd_prefix[0].clone(), all_args)
    };

    // Use btrfs tmpdir if available so podman extracts layers on the same
    // filesystem (avoids ext4→btrfs cross-fs copy during podman load).
    // For rootless: extract uid from cmd_prefix (runuser -u <username>).
    // For root: use uid 0.
    let target_uid = cmd_prefix
        .windows(2)
        .find(|w| w[0] == "-u")
        .and_then(|w| {
            nix::unistd::User::from_name(&w[1])
                .ok()
                .flatten()
                .map(|u| u.uid.as_raw())
        })
        .unwrap_or(0);
    let btrfs_tmpdir = format!("/var/lib/containers/storage/tmp-{}", target_uid);
    let mut cmd_builder = Command::new(&cmd);
    cmd_builder.args(&args);
    if std::path::Path::new(&btrfs_tmpdir).is_dir() {
        eprintln!("[fc-agent] using btrfs tmpdir: {}", btrfs_tmpdir);
        cmd_builder.env("TMPDIR", &btrfs_tmpdir);
    }
    let mut load_child = cmd_builder
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning podman load")?;

    // Drain stdout/stderr concurrently while waiting (same pattern as pull_image).
    // podman load can emit per-layer progress lines; if neither pipe is read until
    // after exit, the child blocks once the 64KB pipe buffer fills and wait() never
    // returns — the import hangs forever with only heartbeats.
    let stdout_task = load_child.stdout.take().map(|stdout| {
        tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            let mut captured = Vec::new();
            while let Ok(Some(line)) = lines.next_line().await {
                captured.push(line);
            }
            captured
        })
    });

    let stderr_task = load_child.stderr.take().map(|stderr| {
        tokio::spawn(async move {
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();
            let mut captured = Vec::new();
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("[fc-agent] [podman] {}", line);
                captured.push(line);
            }
            captured
        })
    });

    let status = loop {
        tokio::select! {
            result = load_child.wait() => {
                break result.context("waiting for podman load")?;
            }
            _ = sleep(Duration::from_secs(30)) => {
                output.try_send_line("heartbeat", "importing image");
                eprintln!("[fc-agent] heartbeat: still importing image...");
            }
        }
    };

    let stdout_lines = if let Some(task) = stdout_task {
        task.await.unwrap_or_default()
    } else {
        Vec::new()
    };
    let stderr_lines = if let Some(task) = stderr_task {
        task.await.unwrap_or_default()
    } else {
        Vec::new()
    };

    if !status.success() {
        anyhow::bail!("podman load failed: {}", stderr_lines.join("\n"));
    }

    eprintln!("[fc-agent] podman load: {}", stdout_lines.join(" "));
    eprintln!("[fc-agent] image imported as: {}", image_name);
    Ok(image_name.to_string())
}

/// Pull image from registry with retries.
pub async fn pull_image(plan: &Plan) -> Result<String> {
    const MAX_RETRIES: u32 = 3;
    const RETRY_DELAY_SECS: u64 = 2;

    let mut last_error = String::new();

    for attempt in 1..=MAX_RETRIES {
        eprintln!(
            "[fc-agent] PULLING IMAGE: {} (attempt {}/{})",
            plan.image, attempt, MAX_RETRIES
        );

        let prefix = podman_cmd_prefix();
        let mut cmd = if prefix.is_empty() {
            let mut c = Command::new("podman");
            c.arg("pull");
            c
        } else {
            let mut c = Command::new(&prefix[0]);
            c.args(&prefix[1..]);
            c.arg("podman").arg("pull");
            c
        };
        cmd.arg(&plan.image);
        if let Some(ref proxy) = plan.http_proxy {
            cmd.env("http_proxy", proxy).env("HTTP_PROXY", proxy);
        }
        if let Some(ref proxy) = plan.https_proxy {
            cmd.env("https_proxy", proxy).env("HTTPS_PROXY", proxy);
        }
        if let Some(ref no_proxy) = plan.no_proxy {
            cmd.env("no_proxy", no_proxy).env("NO_PROXY", no_proxy);
        }

        let mut child = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("spawning podman pull")?;

        let stdout_task = child.stdout.take().map(|stdout| {
            tokio::spawn(async move {
                let reader = BufReader::new(stdout);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    eprintln!("[fc-agent] [podman] {}", line);
                }
            })
        });

        let stderr_task = child.stderr.take().map(|stderr| {
            tokio::spawn(async move {
                let reader = BufReader::new(stderr);
                let mut lines = reader.lines();
                let mut captured = Vec::new();
                while let Ok(Some(line)) = lines.next_line().await {
                    eprintln!("[fc-agent] [podman] {}", line);
                    captured.push(line);
                }
                captured
            })
        });

        let status = child.wait().await.context("waiting for podman pull")?;

        if let Some(task) = stdout_task {
            let _ = task.await;
        }
        let stderr_lines = if let Some(task) = stderr_task {
            task.await.unwrap_or_default()
        } else {
            Vec::new()
        };

        if status.success() {
            eprintln!("[fc-agent] image pulled successfully");
            return Ok(plan.image.clone());
        }

        last_error = stderr_lines.join("\n");
        eprintln!(
            "[fc-agent] IMAGE PULL FAILED (attempt {}/{}), exit code: {:?}",
            attempt,
            MAX_RETRIES,
            status.code()
        );

        if attempt < MAX_RETRIES {
            eprintln!("[fc-agent] retrying in {} seconds...", RETRY_DELAY_SECS);
            sleep(Duration::from_secs(RETRY_DELAY_SECS)).await;
        }
    }

    anyhow::bail!(
        "Failed to pull image after {} attempts:\n{}",
        MAX_RETRIES,
        last_error
    )
}

/// What fc-agent reads from the image before the container starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageFacts {
    /// The image digest, the snapshot cache key the host is told about.
    pub digest: String,
    /// Whether the image defines a HEALTHCHECK. Sent with the start notification, so
    /// the host runs `podman healthcheck` only for images that have one.
    pub healthcheck: bool,
}

/// Get the digest of a pulled image and whether it defines a HEALTHCHECK, in one
/// `podman image inspect`.
pub async fn get_image_facts(image: &str, cmd_prefix: &[String]) -> Result<ImageFacts> {
    let (cmd, mut args) = if cmd_prefix.is_empty() {
        ("podman".to_string(), vec![])
    } else {
        let mut a: Vec<String> = cmd_prefix[1..].to_vec();
        a.push("podman".to_string());
        (cmd_prefix[0].clone(), a)
    };
    args.extend([
        "image".to_string(),
        "inspect".to_string(),
        "--format".to_string(),
        "{{.Digest}}\n{{json .HealthCheck}}".to_string(),
        image.to_string(),
    ]);

    let output = Command::new(&cmd)
        .args(&args)
        .output()
        .await
        .context("running podman image inspect")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("podman image inspect failed: {}", stderr);
    }

    parse_image_facts(&String::from_utf8_lossy(&output.stdout))
}

/// Parse the output of `podman image inspect --format '{{.Digest}}\n{{json .HealthCheck}}'`.
///
/// `HEALTHCHECK NONE` is stored as a test of `["NONE"]`, and podman runs no check for it.
fn parse_image_facts(stdout: &str) -> Result<ImageFacts> {
    let mut lines = stdout.trim().splitn(2, '\n');
    let digest = lines.next().unwrap_or_default().trim().to_string();
    if digest.is_empty() {
        anyhow::bail!("podman image inspect printed no digest: {stdout:?}");
    }
    let health: serde_json::Value = match lines.next().map(str::trim) {
        Some(json) if !json.is_empty() => serde_json::from_str(json)
            .with_context(|| format!("parsing the image's HealthCheck: {json:?}"))?,
        _ => anyhow::bail!("podman image inspect printed no HealthCheck: {stdout:?}"),
    };
    let healthcheck = health
        .get("Test")
        .and_then(|test| test.as_array())
        .and_then(|test| test.first())
        .and_then(|first| first.as_str())
        .is_some_and(|first| first != "NONE");
    Ok(ImageFacts {
        digest,
        healthcheck,
    })
}

/// Result of notify_cache_ready_and_wait: what this VM is, per the host.
#[derive(Debug, PartialEq)]
pub enum CacheResult {
    /// Host answered "cache-ack" — continue cold in this VM and launch the
    /// container.
    ColdStart,
    /// This VM was restored from a snapshot: the owning host process answered
    /// "cache-restored", or the restore-epoch watcher fired. The restore
    /// machinery in that same process drives (or fails closed) the readiness
    /// this VM waits on, so the WarmStart wait is guaranteed to resolve.
    WarmStart,
    /// Host answered "cache-doomed" — this VM produced a pre-start snapshot
    /// and is being replaced by a restore of it. The container must never
    /// launch here; the host tears this VM down momentarily.
    Doomed,
    /// Handshake failed: console quiesce failure (cache-ready deliberately not
    /// sent so the host cannot snapshot a mid-transmit UART), the first
    /// connect/send failing (the host never learned we were ready, so it will
    /// never pause us), a host that went silent AND unreachable, or the
    /// absolute deadline. The VM continues cold.
    Failed,
}

/// One handshake connection's terminal state.
///
/// `Severed` is deliberately NOT a classification. The VMM queues a
/// VIRTIO_VSOCK_EVENT_TRANSPORT_RESET into the guest's event queue during
/// snapshot SAVE (`Vsock::prepare_save` in Firecracker), so the event is
/// processed when vCPUs next run — which happens both when the SOURCE resumes
/// and when a CLONE is restored. The resumed source and the restored clone
/// therefore observe byte-identical connection death, and no timer can tell
/// them apart (guessing hung the source for its whole health deadline, #799).
/// The only party that knows which one we are is the host process that owns
/// this VM, so a severed session always leads back to asking it again.
#[derive(Debug, PartialEq)]
enum SessionOutcome {
    /// "cache-ack": continue cold.
    Ack,
    /// "cache-restored" from the host, or the restore-epoch watcher fired.
    Restored,
    /// "cache-doomed": this VM is being replaced; never launch.
    Doomed,
    /// Transport severed with no verdict read — a snapshot boundary passed
    /// over this connection (or its close raced an unread probe, #627).
    Severed,
    /// No keepalive within the deadline: the host is silent.
    Silent,
    /// Unrecoverable local error (poll error, buffer overflow, absolute cap).
    Fatal,
}

/// Opens connections to the host status port. A seam: the handshake protocol
/// below is fd-generic (nix poll/read/write), so unit tests drive it over
/// Unix socketpairs with a scripted host instead of a vsock device.
pub(crate) trait StatusConnector {
    fn connect(&mut self) -> Result<std::os::fd::OwnedFd, nix::errno::Errno>;
}

/// The production connector: vsock to the host's status listener.
struct VsockStatusConnector;

impl StatusConnector for VsockStatusConnector {
    fn connect(&mut self) -> Result<std::os::fd::OwnedFd, nix::errno::Errno> {
        use nix::sys::socket::{connect, socket, AddressFamily, SockFlag, SockType, VsockAddr};
        use std::os::fd::AsRawFd;
        let sock = socket(
            AddressFamily::Vsock,
            SockType::Stream,
            SockFlag::empty(),
            None,
        )?;
        connect(
            sock.as_raw_fd(),
            &VsockAddr::new(vsock::HOST_CID, vsock::STATUS_PORT),
        )?;
        Ok(sock)
    }
}

/// Notify host that image is cached, wait for snapshot ack.
///
/// The `restore_flag` is set by the restore-epoch watcher when it detects
/// a snapshot restore. This breaks the poll loop early when POLLHUP is not
/// detected (e.g., after pre-start snapshot restore in rootless mode).
/// Compact the cache-handshake read buffer after checking for "cache-ack":
/// drop all COMPLETE lines (host "cache-wait" keepalives — the only complete
/// lines the host sends before the ack) and keep any partial tail, so repeated
/// keepalives can't overflow the small buffer while a "cache-ack" split across
/// reads (e.g. "cache-a" + "ck\n") still assembles correctly from the tail.
///
/// Returns (new_total_read, saw_cache_wait_keepalive).
fn consume_cache_wait_lines(buf: &mut [u8; 64], total_read: usize) -> (usize, bool) {
    let received = std::str::from_utf8(&buf[..total_read]).unwrap_or("");
    let saw = received.contains("cache-wait");
    if !saw {
        return (total_read, false);
    }
    match received.rfind('\n') {
        Some(pos) => {
            let tail_start = pos + 1;
            let tail_len = total_read - tail_start;
            buf.copy_within(tail_start..total_read, 0);
            (tail_len, true)
        }
        None => (total_read, true),
    }
}

pub fn notify_cache_ready_and_wait(
    digest: &str,
    restore_flag: &std::sync::atomic::AtomicBool,
) -> CacheResult {
    // Receiving "cache-ready" makes the host PAUSE this VM for the pre-start
    // snapshot. A snapshot that captures the UART mid-transmit is poisoned:
    // EVERY restore of it has a dead serial console (the guest 8250 driver
    // waits forever for a TX interrupt the re-created serial device never
    // delivers). So: announce FIRST, then make the console provably quiet
    // (flush + gate + TIOCOUTQ drain — structural, not probabilistic), and
    // only THEN let the host know we are ready to be paused. On success the
    // gate holds until the pause is behind us: the first severed session
    // (the snapshot boundary reset the transport) or the handshake's return,
    // whichever comes first. No concurrent task can put a byte in UART TX
    // while the pause can happen — lines logged meanwhile are buffered and
    // flushed when the guard drops. If the console CANNOT be proven quiet,
    // the handshake is aborted BEFORE cache-ready is sent: the host never
    // pauses us, no poisoned snapshot artifact can be created, and the VM
    // continues cold.
    eprintln!(
        "[fc-agent] image digest {} loaded; quiescing console before cache-ready notification",
        digest
    );
    let console_quiesce = match crate::console::quiesce_for_snapshot() {
        Ok(guard) => guard,
        Err(e) => {
            eprintln!(
                "[fc-agent] WARNING: {}; not sending cache-ready — continuing cold \
                 without a pre-start snapshot",
                e
            );
            return CacheResult::Failed;
        }
    };

    run_cache_handshake(
        &mut VsockStatusConnector,
        digest,
        restore_flag,
        Some(console_quiesce),
    )
}

/// The cache handshake protocol: ask the host what this VM is, and keep
/// asking until the owner answers.
///
/// Wire protocol on the status port, all lines newline-terminated:
///   guest -> host  "cache-ready:<digest>"   the (idempotent, re-sendable) ask
///   host  -> guest "cache-wait"             keepalive while the verdict is pending
///   host  -> guest "cache-ack"              verdict: continue cold, launch the container
///   host  -> guest "cache-restored"         verdict: this VM is a restored clone
///   host  -> guest "cache-doomed"           verdict: this VM is being replaced — never launch
///
/// A connection severed without a verdict is a snapshot boundary, not an
/// answer (see [`SessionOutcome::Severed`]): reconnect and re-send the same
/// ask. Every fcvm process that can own this VM keeps a verdict for it — the
/// run loop (Pending until the snapshot decision, then Continue or Doomed)
/// and the restore path (Restored, bound before the clone resumes) — so the
/// re-ask is answered by whichever process actually owns the guest now, from
/// state that process KNOWS. No transport event or timer is ever read as a
/// classification.
///
/// Failure exits (all continue cold, preserving pre-protocol semantics):
/// the FIRST connect/send failing (the host never saw the ask, so it will
/// never pause us), the host going silent for 30s AND then refusing the
/// reconnect (host process gone), or the 10-minute absolute cap.
///
/// `console_gate` is the console quiesce guard taken before the first ask. It
/// keeps the UART idle across the snapshot pause, so it is released at the
/// first severed session: the host pauses the VM with the session open and
/// closes one only after a verdict, so a session that dies without a verdict
/// means the pause is behind us or none is coming. Held to the handshake's
/// return, it also hid every line logged while a restored clone kept failing
/// to reach the host, for up to the 10-minute cap (#1207: a clone that never
/// ACKed printed nothing from fc-agent for two minutes).
fn run_cache_handshake<G>(
    connector: &mut dyn StatusConnector,
    digest: &str,
    restore_flag: &std::sync::atomic::AtomicBool,
    mut console_gate: Option<G>,
) -> CacheResult {
    use nix::unistd::write;

    let mut release_console = |why: &str| {
        if console_gate.take().is_some() {
            eprintln!("[fc-agent] cache handshake: {why}; console released");
        }
    };

    let started = std::time::Instant::now();
    let absolute_deadline = started + std::time::Duration::from_secs(600);
    let mut asked_before = false;
    let mut went_silent = false;
    let mut failed_reconnects: u64 = 0;

    loop {
        if restore_flag.load(std::sync::atomic::Ordering::Acquire) {
            eprintln!("[fc-agent] cache handshake: restore detected via epoch watcher");
            return CacheResult::WarmStart;
        }
        if std::time::Instant::now() >= absolute_deadline {
            eprintln!("[fc-agent] cache handshake absolute deadline expired (10 min)");
            return CacheResult::Failed;
        }

        let sock = match connector.connect() {
            Ok(sock) => sock,
            Err(e) if !asked_before => {
                eprintln!(
                    "[fc-agent] WARNING: failed to connect vsock for cache: {}",
                    e
                );
                return CacheResult::Failed;
            }
            Err(e) if went_silent => {
                // Silent for 30s AND unreachable: the host process is gone
                // (crash/teardown without a verdict). Continue cold — if the
                // VM is in fact being torn down, it dies before the container
                // matters; if the host crashed, cold is the only useful state.
                eprintln!(
                    "[fc-agent] cache handshake: host silent and unreachable ({}); continuing cold",
                    e
                );
                return CacheResult::Failed;
            }
            Err(e) => {
                // Mid-protocol reconnect refused: the owner may still be
                // binding its listener (a restore binds before resume, but a
                // teardown-in-progress can also present this way). Retry; the
                // restore_flag/absolute-deadline checks above bound the loop.
                // Once the console gate is released these lines reach the
                // serial console, and the loop retries ten times a second for
                // up to the 10-minute cap: log the first few and every 100th.
                failed_reconnects += 1;
                if failed_reconnects <= 5 || failed_reconnects.is_multiple_of(100) {
                    eprintln!(
                        "[fc-agent] cache handshake: reconnect failed ({}, attempt {}), retrying",
                        e, failed_reconnects
                    );
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
                continue;
            }
        };

        // failpoint: hold AFTER the console quiesce completed and immediately
        // BEFORE announcing cache-ready (which triggers the host's pre-start
        // snapshot pause) — proves the quiesce gate keeps the UART idle across
        // an arbitrarily long pre-pause window. Sync context (this fn is
        // blocking). Only the first ask holds: re-asks happen after the
        // boundary, where the hold would prove nothing. Note: the marker line
        // is buffered by the quiesced console and reaches the host log only
        // after the guard drops — a hold, not a host-visible sync point.
        if !asked_before {
            failpoint::hit("cache_ready.pre_send");
        }

        let msg = format!("cache-ready:{}\n", digest);
        match write(&sock, msg.as_bytes()) {
            Ok(n) if n == msg.len() => {}
            other => {
                if !asked_before {
                    eprintln!(
                        "[fc-agent] WARNING: failed to send cache-ready message: {:?}",
                        other
                    );
                    return CacheResult::Failed;
                }
                // Re-ask send failed: the transport died again under us.
                // Treat like a severed session and go around. Delay like the
                // refused-connect path so a host that accepts and immediately
                // resets cannot drive this loop hot for the whole absolute cap.
                release_console("re-ask send failed after the snapshot boundary");
                eprintln!(
                    "[fc-agent] cache handshake: re-ask send failed ({:?}), reconnecting",
                    other
                );
                std::thread::sleep(std::time::Duration::from_millis(100));
                continue;
            }
        }
        if asked_before {
            eprintln!(
                "[fc-agent] re-sent cache-ready:{} after severed session, waiting for verdict...",
                digest
            );
        } else {
            eprintln!("[fc-agent] sent cache-ready:{}, waiting for ack...", digest);
        }
        asked_before = true;
        went_silent = false;

        match wait_for_verdict(&sock, restore_flag, absolute_deadline) {
            SessionOutcome::Ack => {
                // Positive close handshake: close our end FIRST, before any
                // logging, so the host's drain sees EOF as early as possible.
                // The host holds this connection open until then precisely so
                // it never closes with one of our 500ms liveness probes still
                // unread — an unread byte at close becomes a vsock RST that
                // flushes this receive buffer, and the ack we just read would
                // have been lost to a spurious severed session (#627). Our
                // close is what tells the host the ack landed.
                drop(sock);
                eprintln!("[fc-agent] received cache-ack from host (handshake closed)");
                return CacheResult::ColdStart;
            }
            SessionOutcome::Restored => {
                drop(sock);
                eprintln!("[fc-agent] cache handshake verdict: restored clone (warm start)");
                return CacheResult::WarmStart;
            }
            SessionOutcome::Doomed => {
                drop(sock);
                eprintln!(
                    "[fc-agent] cache handshake verdict: VM is being replaced by a restore; \
                     the container will not be launched"
                );
                return CacheResult::Doomed;
            }
            SessionOutcome::Severed => {
                release_console("session severed by the snapshot boundary");
                // A snapshot boundary passed over this connection. Ask again —
                // the owner (resumed-source run loop, restored clone's host, or
                // a teardown that will kill us) answers from what it knows.
                // Delay like the refused-connect path: a host that accepts and
                // closes at once returns Severed with no poll timeout consumed,
                // and an undelayed loop would burn a guest vCPU until the
                // absolute deadline.
                std::thread::sleep(std::time::Duration::from_millis(100));
                continue;
            }
            SessionOutcome::Silent => {
                // No keepalive for 30s. One reconnect distinguishes a wedged
                // host (answers or keeps keepaliving) from a dead one (refuses
                // the connect -> Failed above).
                went_silent = true;
                continue;
            }
            SessionOutcome::Fatal => return CacheResult::Failed,
        }
    }
}

/// Wait on one handshake connection until a verdict, a severed transport, or
/// a deadline. See [`run_cache_handshake`] for the wire protocol.
fn wait_for_verdict(
    sock: &std::os::fd::OwnedFd,
    restore_flag: &std::sync::atomic::AtomicBool,
    absolute_deadline: std::time::Instant,
) -> SessionOutcome {
    use nix::fcntl::{fcntl, FcntlArg, OFlag};
    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
    use nix::unistd::{read, write};
    use std::os::fd::AsFd;

    if let Ok(flags) = fcntl(sock, FcntlArg::F_GETFL) {
        let new_flags = OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK;
        let _ = fcntl(sock, FcntlArg::F_SETFL(new_flags));
    }

    let mut buf = [0u8; 64];
    let mut total_read = 0;

    // 30s keepalive deadline with 500ms poll intervals. The host emits a
    // "cache-wait" keepalive every 5s while the verdict is pending (queued on
    // the global snapshot semaphore, or writing the snapshot), and each
    // keepalive extends this deadline (#627): under the SnapshotEnabled CI
    // matrix the semaphore queue alone can exceed a fixed 30s while the host
    // is perfectly alive. The deadline expires only when the host has gone
    // genuinely silent; the caller then probes with a reconnect. The absolute
    // cap bounds a wedged host that keeps ticking keepalives forever.
    //
    // Ordering matters: deadlines are checked AFTER each poll/drain cycle,
    // never before. Keepalives can queue while the VM is paused for the
    // snapshot, and the guest clock can jump forward across the pause (the
    // ARM virtual counter keeps running) — checking expiry before draining
    // would discard extensions already sitting in the socket buffer.
    let mut deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);

    loop {
        // The restore-epoch watcher is an equally authoritative source of the
        // Restored verdict (the host publishes the epoch over its restore
        // control plane). Checked every cycle so a clone whose re-ask is stuck
        // behind a slow listener still classifies promptly.
        if restore_flag.load(std::sync::atomic::Ordering::Acquire) {
            return SessionOutcome::Restored;
        }

        let poll_ms = 500u16;
        let mut poll_fds = [PollFd::new(sock.as_fd(), PollFlags::POLLIN)];

        match poll(&mut poll_fds, PollTimeout::from(poll_ms)) {
            Err(e) => {
                eprintln!("[fc-agent] cache-ack poll error: {}", e);
                return SessionOutcome::Fatal;
            }
            Ok(0) => {
                // Poll timeout — actively probe the connection. After a
                // snapshot boundary the vsock transport is reset but the
                // kernel may not deliver POLLHUP on the old fd (observed in
                // rootless mode). A write to a dead connection fails
                // immediately, which reliably detects the severance.
                match write(sock, b"\n") {
                    Err(nix::errno::Errno::EPIPE)
                    | Err(nix::errno::Errno::ECONNRESET)
                    | Err(nix::errno::Errno::ENOTCONN)
                    | Err(nix::errno::Errno::ECONNREFUSED) => {
                        eprintln!(
                            "[fc-agent] cache handshake connection dead (write probe): \
                             snapshot boundary crossed"
                        );
                        return SessionOutcome::Severed;
                    }
                    Err(nix::errno::Errno::EAGAIN) => {
                        // Connection alive but can't write now — keep polling.
                    }
                    _ => {
                        // Write succeeded or other error — connection alive.
                    }
                }
                // No data this cycle — judge expiry now (never before a
                // drain: queued keepalives must be consumed first).
                let now = std::time::Instant::now();
                if now >= absolute_deadline {
                    eprintln!("[fc-agent] cache handshake absolute deadline expired (10 min)");
                    return SessionOutcome::Fatal;
                }
                if now >= deadline {
                    eprintln!("[fc-agent] cache handshake deadline expired (host silent for 30s)");
                    return SessionOutcome::Silent;
                }
                continue;
            }
            Ok(_) => {}
        }

        // Drain readable data BEFORE acting on POLLHUP/POLLERR. Linux can
        // report POLLIN|POLLHUP together when the host writes a verdict and
        // immediately closes the connection. Treating POLLHUP as EOF before
        // reading would discard a buffered verdict and turn a decided
        // handshake into a spurious severed session.
        let hung_up = poll_fds[0]
            .revents()
            .is_some_and(|r| r.contains(PollFlags::POLLHUP) || r.contains(PollFlags::POLLERR));

        match read(sock, &mut buf[total_read..]) as Result<usize, nix::errno::Errno> {
            Ok(n) if n > 0 => {
                total_read += n;
                let received = std::str::from_utf8(&buf[..total_read]).unwrap_or("");
                // Verb scan before keepalive compaction, so a verdict
                // coalesced into the same read as a trailing keepalive is
                // never compacted away. The three verbs share no substring.
                if received.contains("cache-ack") {
                    return SessionOutcome::Ack;
                }
                if received.contains("cache-restored") {
                    return SessionOutcome::Restored;
                }
                if received.contains("cache-doomed") {
                    return SessionOutcome::Doomed;
                }
                // Host keepalive: it is alive but the verdict is still
                // pending. Extend the deadline (bounded by the absolute cap)
                // and compact the buffer so repeated keepalives can't
                // overflow it.
                let (new_total, saw_keepalive) = consume_cache_wait_lines(&mut buf, total_read);
                total_read = new_total;
                if saw_keepalive {
                    deadline = (std::time::Instant::now() + std::time::Duration::from_secs(30))
                        .min(absolute_deadline);
                    eprintln!("[fc-agent] cache-wait keepalive from host, extending deadline");
                }
                if total_read >= buf.len() {
                    eprintln!("[fc-agent] cache handshake buffer overflow, giving up");
                    return SessionOutcome::Fatal;
                }
                // Partial data and the peer has hung up: no verdict is coming
                // on this connection.
                if hung_up {
                    eprintln!("[fc-agent] cache handshake connection reset after partial read");
                    return SessionOutcome::Severed;
                }
                // Otherwise keep reading for the rest of the message.
            }
            Ok(_) => {
                // Orderly EOF with no buffered verdict — the transport was
                // severed by a snapshot boundary.
                eprintln!("[fc-agent] cache handshake connection closed without a verdict");
                return SessionOutcome::Severed;
            }
            Err(nix::errno::Errno::EAGAIN) => {
                // No data pending. If the peer hung up, the severance is real.
                if hung_up {
                    eprintln!("[fc-agent] cache handshake connection reset (no verdict)");
                    return SessionOutcome::Severed;
                }
                // Spurious wakeup, continue polling.
                continue;
            }
            // A reset/torn-down connection with no buffered verdict — either a
            // genuine snapshot boundary, or a handshake where the host closed
            // with our write-probe bytes still unread, turning its close into
            // a RST that flushed our receive buffer (#627). Both re-ask: the
            // owner's verdict state answers the retry correctly either way,
            // which is exactly the recovery #627's one-shot protocol lacked.
            Err(nix::errno::Errno::ECONNRESET)
            | Err(nix::errno::Errno::EPIPE)
            | Err(nix::errno::Errno::ENOTCONN)
            | Err(nix::errno::Errno::ECONNREFUSED) => {
                eprintln!("[fc-agent] cache handshake connection reset on read");
                return SessionOutcome::Severed;
            }
            Err(e) => {
                eprintln!("[fc-agent] cache handshake read error: {}", e);
                return SessionOutcome::Fatal;
            }
        }
    }
}

/// Build podman run args from the plan.
/// If user_info is Some, the container runs as the specified user (rootless podman).
pub fn build_podman_args(
    plan: &Plan,
    image_ref: &str,
    user_info: Option<(&str, &str)>, // (username, runtime_dir)
    cdi_devices: &[String],
) -> Vec<String> {
    let mut args = vec![
        "podman".to_string(),
        "run".to_string(),
        "--name".to_string(),
        "fcvm-container".to_string(),
    ];

    // Always use host networking inside the VM. The VM already has its own
    // network namespace (via pasta). A second pasta inside the VM would create
    // double-NAT that breaks port forwarding and health checks.
    args.push("--network=host".to_string());

    args.extend([
        "--cgroups=split".to_string(),
        "--ulimit".to_string(),
        "nofile=65536:65536".to_string(),
        "--pids-limit=-1".to_string(),
        // Disable conmon log capture. fc-agent captures container output through
        // podman's stdout/stderr pipes directly. conmon's default k8s-file log
        // driver adds a redundant buffering layer that blocks under burst output
        // with rootless podman (--user mode), causing the container to deadlock
        // on stdout/stderr pipe write.
        "--log-driver=none".to_string(),
        // podman sends READY=1 to $NOTIFY_SOCKET (START_NOTIFY_SOCKET) once the
        // container is running; that is the host's signal that it started.
        "--sdnotify=conmon".to_string(),
    ]);

    if let Some((username, runtime_dir)) = user_info {
        setup_user_mapping(&mut args, username, runtime_dir);
    }

    if plan.privileged {
        eprintln!("[fc-agent] privileged mode enabled");
        if user_info.is_some() {
            // Rootless podman: --privileged gives zero effective capabilities with
            // --userns=keep-id. Instead, use explicit --cap-add for the caps we need.
            // These match the host podman's cap-add list exactly.
            for cap in &[
                "net_bind_service",
                "net_admin",
                "sys_nice",
                "sys_resource",
                "sys_ptrace",
                "sys_admin",
            ] {
                args.push(format!("--cap-add={}", cap));
            }
            args.push("--security-opt".to_string());
            args.push("seccomp=unconfined".to_string());
            args.push("--device".to_string());
            args.push("/dev/fuse".to_string());
        } else {
            // Root podman: full device cgroup access + privileged.
            args.push("--device-cgroup-rule=b *:* rwm".to_string());
            args.push("--device-cgroup-rule=c *:* rwm".to_string());
            args.push("--privileged".to_string());
        }
    }

    if plan.interactive {
        args.push("-i".to_string());
    }
    if plan.tty {
        args.push("-t".to_string());
    }

    for (key, val) in &plan.env {
        args.push("-e".to_string());
        args.push(format!("{}={}", key, val));
    }

    // Add FUSE/disk/NFS mounts as bind mounts
    for vol in &plan.volumes {
        let spec = if vol.read_only {
            format!("{}:{}:ro", vol.guest_path, vol.guest_path)
        } else {
            format!("{}:{}", vol.guest_path, vol.guest_path)
        };
        args.push("-v".to_string());
        args.push(spec);
    }
    for disk in &plan.extra_disks {
        let spec = if disk.read_only {
            format!("{}:{}:ro", disk.mount_path, disk.mount_path)
        } else {
            format!("{}:{}", disk.mount_path, disk.mount_path)
        };
        args.push("-v".to_string());
        args.push(spec);
    }
    for pmem in &plan.pmem_mounts {
        args.push("-v".to_string());
        args.push(format!("{}:{}:ro", pmem.mount_path, pmem.mount_path));
    }
    for share in &plan.nfs_mounts {
        let spec = if share.read_only {
            format!("{}:{}:ro", share.mount_path, share.mount_path)
        } else {
            format!("{}:{}", share.mount_path, share.mount_path)
        };
        args.push("-v".to_string());
        args.push(spec);
    }

    // GPUs, by CDI name (`gpu::attach` wrote the spec).
    for device in cdi_devices {
        args.push("--device".to_string());
        args.push(device.clone());
    }

    args.push(image_ref.to_string());

    if let Some(cmd_args) = &plan.cmd {
        args.extend(cmd_args.iter().cloned());
    }

    args
}

/// The VM user's subordinate id range: the host's from /etc/subuid when the plan
/// carries it (a store built by rootless podman on the host holds those ids),
/// otherwise a default range.
pub fn resolve_subuid_range(subuid_range: Option<(u64, u64)>) -> (u64, u64) {
    subuid_range.unwrap_or((100000, 65536))
}

/// The VM user's subordinate gid range: the host's from /etc/subgid when the plan
/// carries it, which can differ from its uid range, otherwise the uid range.
pub fn resolve_subgid_range(
    subgid_range: Option<(u64, u64)>,
    subuid_range: Option<(u64, u64)>,
) -> (u64, u64) {
    subgid_range.unwrap_or_else(|| resolve_subuid_range(subuid_range))
}

/// Create the VM user and set up rootless podman prerequisites.
/// Call this BEFORE importing images so podman load runs as the target user.
/// Returns (username, uid, runtime_dir) for use by run_as_user_prefix().
///
/// `subuid_range`: optional (start, count) from the host's /etc/subuid.
/// When the storage image is built by rootless podman on the host, it contains
/// files with UIDs from the host's subuid range. The VM must use the same range
/// so those UIDs are valid in the VM's user namespace.
///
pub fn create_vm_user(
    user_spec: &str,
    desired_name: &str,
    subuid_range: Option<(u64, u64)>,
    subgid_range: Option<(u64, u64)>,
    provisioned: bool,
) -> (String, String, String) {
    let parts: Vec<&str> = user_spec.split(':').collect();
    let uid = parts[0].to_string();
    let gid = parts.get(1).unwrap_or(&"100").to_string();
    let username = desired_name.to_string();

    eprintln!(
        "[fc-agent] setting up user mapping: uid={} gid={}",
        uid, gid
    );

    let _ = std::process::Command::new("groupadd")
        .args(["-g", &gid, &username])
        .output();
    let _ = std::process::Command::new("useradd")
        .args(["-u", &uid, "-g", &gid, "-m", "-s", "/bin/sh", &username])
        .output();

    let (subuid_start, subuid_count) = resolve_subuid_range(subuid_range);
    let (subgid_start, subgid_count) = resolve_subgid_range(subgid_range, subuid_range);
    eprintln!(
        "[fc-agent] subuid {}:{}, subgid {}:{}",
        subuid_start, subuid_count, subgid_start, subgid_count
    );
    let _ = std::fs::write(
        "/etc/subuid",
        format!("{username}:{subuid_start}:{subuid_count}\n"),
    );
    let _ = std::fs::write(
        "/etc/subgid",
        format!("{username}:{subgid_start}:{subgid_count}\n"),
    );

    let runtime_dir = format!("/run/user/{}", uid);
    let _ = std::fs::create_dir_all(&runtime_dir);
    let _ = std::process::Command::new("chown")
        .args([&format!("{}:{}", uid, gid), &runtime_dir])
        .output();

    // Clean stale podman storage from previous VM runs. The run root may have
    // changed between runs (e.g., /tmp vs /run/user), causing "database
    // configuration mismatch" errors.
    // HOME is set so podman finds user-level config (if it exists at this point).
    // NEVER on a provisioned boot (disk-only clone / in-place reboot): the user's
    // storage holds the captured container — a reset would erase it.
    if provisioned {
        eprintln!(
            "[fc-agent] skipping user podman reset (provisioned disk — captured storage preserved)"
        );
    } else {
        let _ = std::process::Command::new("env")
            .args([
                &format!("HOME=/home/{}", username),
                &format!("XDG_RUNTIME_DIR={}", runtime_dir),
                "runuser",
                "-u",
                &username,
                "--",
                "podman",
                "system",
                "reset",
                "--force",
            ])
            .output();
    }

    let cgroup_dir = format!("/sys/fs/cgroup/user.slice/user-{}.slice", uid);
    let _ = std::fs::create_dir_all(&cgroup_dir);
    let _ = std::process::Command::new("chown")
        .args(["-R", &format!("{}:{}", uid, gid), &cgroup_dir])
        .output();
    for path in &[
        "/sys/fs/cgroup/cgroup.subtree_control".to_string(),
        format!("{}/cgroup.subtree_control", cgroup_dir),
    ] {
        let _ = std::fs::write(path, "+cpu +memory +pids");
    }

    if let Ok(cgroup_path) = std::fs::read_to_string("/proc/self/cgroup") {
        if let Some(path) = cgroup_path.trim().strip_prefix("0::") {
            let full_path = format!("/sys/fs/cgroup{}", path);
            let _ = std::process::Command::new("chown")
                .args(["-R", &format!("{}:{}", uid, gid), &full_path])
                .output();
            eprintln!("[fc-agent] delegated cgroup {} to user {}", full_path, uid);
        }
    }

    // Set up user-level btrfs storage if root btrfs is available
    setup_user_btrfs_storage(&uid, &gid, &username);

    (username, uid, runtime_dir)
}

/// Set up btrfs storage for a rootless user (loopback mode).
///
/// Creates a user-specific subdirectory on the loopback btrfs mount
/// so rootless podman has its own btrfs graphroot.
fn setup_user_btrfs_storage(uid: &str, gid: &str, username: &str) {
    let root_mnt = "/var/lib/containers/storage";

    // Check if root btrfs storage is mounted (loopback from setup_btrfs_storage_if_available)
    let is_btrfs = std::process::Command::new("findmnt")
        .args(["-n", "-o", "FSTYPE", root_mnt])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "btrfs")
        .unwrap_or(false);

    if !is_btrfs {
        return;
    }

    // Loopback mode: create user-specific subdirectory and temp dir.
    // The temp dir is on btrfs so podman extracts layers on the same filesystem
    // (avoids ext4→btrfs cross-filesystem copy during podman load).
    let user_graphroot = format!("{}/user-{}", root_mnt, uid);
    let user_tmpdir = format!("{}/tmp-{}", root_mnt, uid);
    for dir in [&user_graphroot, &user_tmpdir] {
        let _ = std::fs::create_dir_all(dir);
        let _ = std::process::Command::new("chown")
            .args([&format!("{}:{}", uid, gid), dir.as_str()])
            .output();
    }

    // Create user-level runroot
    let user_runroot = format!("/run/user/{}/containers", uid);
    let _ = std::fs::create_dir_all(&user_runroot);
    let _ = std::process::Command::new("chown")
        .args([&format!("{}:{}", uid, gid), &user_runroot])
        .output();

    // Write user-level storage.conf
    let user_config_dir = format!("/home/{}/.config/containers", username);
    let _ = std::fs::create_dir_all(&user_config_dir);
    let storage_conf = format!(
        "[storage]\ndriver = \"btrfs\"\ngraphroot = \"{}\"\nrunroot = \"{}\"\n",
        user_graphroot, user_runroot
    );
    if let Err(e) = std::fs::write(format!("{}/storage.conf", user_config_dir), &storage_conf) {
        eprintln!(
            "[fc-agent] WARNING: failed to write user storage.conf: {}",
            e
        );
        return;
    }
    write_containers_conf(&format!("{}/containers.conf", user_config_dir));
    // Ensure the user owns their config
    let _ = std::process::Command::new("chown")
        .args([
            "-R",
            &format!("{}:{}", uid, gid),
            &format!("/home/{}/.config", username),
        ])
        .output();

    eprintln!(
        "[fc-agent] user btrfs storage configured at {}",
        user_graphroot
    );
}

/// Build the env + runuser prefix for running commands as the VM user.
///
/// Sets HOME so podman finds user-level config at ~/.config/containers/,
/// and XDG_RUNTIME_DIR for the user's runtime state.
pub fn run_as_user_prefix(username: &str, runtime_dir: &str) -> Vec<String> {
    vec![
        "env".to_string(),
        format!("HOME=/home/{}", username),
        format!("XDG_RUNTIME_DIR={}", runtime_dir),
        "runuser".to_string(),
        "-u".to_string(),
        username.to_string(),
        "--".to_string(),
    ]
}

fn setup_user_mapping(args: &mut Vec<String>, username: &str, runtime_dir: &str) {
    // Rootless podman: remove split cgroups, add keep-id + cgroupfs manager.
    // No systemd user session in the VM, so use cgroupfs directly.
    args.retain(|a| a != "--cgroups=split");
    args.push("--userns=keep-id".to_string());
    args.push("--cgroup-manager=cgroupfs".to_string());

    // Wrap with env + runuser to set XDG_RUNTIME_DIR (rootless podman needs it)
    let prefix = run_as_user_prefix(username, runtime_dir);
    for (i, arg) in prefix.into_iter().enumerate() {
        args.insert(i, arg);
    }
}

/// The uid and gid podman runs the container as, or None when it runs as root. Matches
/// how `agent::run` decides on a container user: a uid that is 0 or does not parse means
/// root, and a spec without a gid takes group 100, as [`create_vm_user`] does.
pub fn podman_user_ids(user_spec: Option<&str>) -> Option<(u32, u32)> {
    let mut parts = user_spec?.split(':');
    let uid: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    if uid == 0 {
        return None;
    }
    let gid = parts.next().and_then(|s| s.parse().ok()).unwrap_or(100);
    Some((uid, gid))
}

/// Where podman reports that the container is running.
///
/// fc-agent runs podman with `--sdnotify=conmon` and this path in `NOTIFY_SOCKET`, and
/// podman sends `READY=1` here once the OCI runtime has started the container. The
/// container itself is not given the socket. podman records the path when it creates the
/// container, so a `podman start` of a captured container reports here too.
pub const START_NOTIFY_SOCKET: &str = "/run/fcvm-container-start.sock";

/// Whether the container has started in this boot: [`NOT_STARTED`], or what the start
/// notification said ([`STARTED`] or [`STARTED_WITH_HEALTHCHECK`]). A restore re-sends it
/// to the clone's host process, whose status listener has never heard it.
static CONTAINER_START: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
const NOT_STARTED: u8 = 0;
const STARTED: u8 = 1;
const STARTED_WITH_HEALTHCHECK: u8 = 2;

/// Whether one sd_notify datagram says the service is ready.
fn notify_says_ready(datagram: &[u8]) -> bool {
    datagram
        .split(|b| *b == b'\n')
        .any(|line| line == b"READY=1")
}

/// The socket podman reports the container's start to, bound before podman runs.
pub struct StartNotify {
    socket: tokio::net::UnixDatagram,
}

impl StartNotify {
    /// Bind [`START_NOTIFY_SOCKET`]. `owner` is the uid and gid rootless podman runs as;
    /// podman sends from that user, and a datagram socket takes writes only from users
    /// with write permission on it.
    pub fn bind(owner: Option<(u32, u32)>) -> Result<Self> {
        Self::bind_at(std::path::Path::new(START_NOTIFY_SOCKET), owner)
    }

    fn bind_at(path: &std::path::Path, owner: Option<(u32, u32)>) -> Result<Self> {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("removing a stale {path:?}")),
        }
        let socket = tokio::net::UnixDatagram::bind(path)
            .with_context(|| format!("binding the container start socket {path:?}"))?;
        if let Some((uid, gid)) = owner {
            std::os::unix::fs::chown(path, Some(uid), Some(gid))
                .with_context(|| format!("giving {path:?} to uid {uid}"))?;
        }
        Ok(Self { socket })
    }

    /// Wait for podman's `READY=1`, then tell the host the container is running. Other
    /// datagrams (podman sends `MAINPID=` and, while it pulls, `EXTEND_TIMEOUT_USEC=`)
    /// are read and ignored.
    pub fn spawn(self, healthcheck: bool) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            if let Err(e) = self.wait_ready().await {
                eprintln!(
                    "[fc-agent] WARNING: reading the container start socket failed: {e}; \
                     the host will not learn that the container started"
                );
                return;
            }
            let started = if healthcheck {
                STARTED_WITH_HEALTHCHECK
            } else {
                STARTED
            };
            CONTAINER_START.store(started, std::sync::atomic::Ordering::Release);
            eprintln!("[fc-agent] container running (podman READY=1)");
            send_container_started(healthcheck).await;
        })
    }

    /// Return once a datagram says `READY=1`.
    async fn wait_ready(&self) -> std::io::Result<()> {
        let mut buf = [0u8; 4096];
        loop {
            let n = self.socket.recv(&mut buf).await?;
            if notify_says_ready(&buf[..n]) {
                return Ok(());
            }
        }
    }
}

/// Tell the host the container is running, off the runtime's worker threads: the send
/// is a blocking vsock connect, retried.
async fn send_container_started(healthcheck: bool) {
    if let Err(e) =
        tokio::task::spawn_blocking(move || vsock::notify_container_started(healthcheck)).await
    {
        eprintln!("[fc-agent] WARNING: the container start notification task failed: {e}");
    }
}

/// After a restore, tell the clone's host process that the container is running, if it
/// was when the snapshot was taken. The host process that took the snapshot heard it;
/// the clone's never did. Before the container starts this sends nothing: the start
/// notification goes to the clone's host when it happens.
pub async fn resend_container_started_after_restore() {
    match CONTAINER_START.load(std::sync::atomic::Ordering::Acquire) {
        NOT_STARTED => {}
        started => send_container_started(started == STARTED_WITH_HEALTHCHECK).await,
    }
}

/// Run container in TTY mode. Never returns: powers the VM off when the container exits.
pub async fn run_tty(
    podman_args: &[String],
    plan: &Plan,
    mounted_fuse_paths: &[String],
    start_notify: StartNotify,
    healthcheck: bool,
) -> ! {
    let start_watch = start_notify.spawn(healthcheck);

    let exit_code = crate::tty::run_with_pty(
        podman_args,
        vec![("NOTIFY_SOCKET".to_string(), START_NOTIFY_SOCKET.to_string())],
        plan.tty,
        plan.interactive,
    )
    .await;
    start_watch.abort();

    vsock::notify_container_exit(exit_code);

    crate::mounts::unmount_paths(mounted_fuse_paths, "FUSE volume");

    eprintln!("[fc-agent] powering off VM");
    let _ = std::process::Command::new("poweroff").arg("-f").spawn();

    std::process::exit(exit_code);
}

/// Run container in non-TTY async mode. Returns exit code.
pub async fn run_async(
    podman_args: &[String],
    output: &OutputHandle,
    non_blocking_output: bool,
    start_notify: StartNotify,
    healthcheck: bool,
) -> Result<i32> {
    let mut cmd = Command::new(&podman_args[0]);
    cmd.args(&podman_args[1..]);
    cmd.env("NOTIFY_SOCKET", START_NOTIFY_SOCKET);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd.spawn().context("spawning Podman container")?;
    // The socket is already bound, so a READY=1 sent before this task first reads it
    // waits in the socket's queue.
    let start_watch = start_notify.spawn(healthcheck);

    // Stream stdout via OutputHandle.
    // In non-blocking mode, use try_send_line (drops on full) to prevent
    // backpressure from cascading into the container and deadlocking
    // FUSE-based services like configerator_fuse.
    let out = output.clone();
    let nb = non_blocking_output;
    let stdout_task = child.stdout.take().map(|stdout| {
        tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if nb {
                    out.try_send_line("stdout", &line);
                } else {
                    out.send_line("stdout", &line).await;
                }
            }
        })
    });

    let out = output.clone();
    let nb = non_blocking_output;
    let stderr_task = child.stderr.take().map(|stderr| {
        tokio::spawn(async move {
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if nb {
                    out.try_send_line("stderr", &line);
                } else {
                    out.send_line("stderr", &line).await;
                }
            }
        })
    });

    let status = child.wait().await?;
    start_watch.abort();
    let exit_code = status.code().unwrap_or(1);

    if let Some(task) = stdout_task {
        let _ = task.await;
    }
    if let Some(task) = stderr_task {
        let _ = task.await;
    }

    if status.success() {
        eprintln!("[fc-agent] container exited successfully");
    } else {
        eprintln!(
            "[fc-agent] container exited with error: {} (code {})",
            status, exit_code
        );

        // podman's own logs are unavailable here: --log-driver=none is
        // required to prevent a conmon deadlock under burst output. That is
        // exactly why the guest's own vitals are printed instead, and why they
        // must be collected without forking (issue #841).
        eprintln!("[fc-agent] VITALS guest begin (container exited {exit_code})");
        eprint!(
            "{}",
            crate::vitals::snapshot_bounded(std::time::Duration::from_secs(3))
        );
        eprintln!("[fc-agent] VITALS guest end");
        // Note: podman logs are unavailable because we use --log-driver=none
        // (required to prevent conmon deadlock under burst output).
        // Container output was already captured through fc-agent's pipe-based
        // output forwarding and sent to the host via vsock.
    }

    // Clean up the container (use user prefix for rootless podman)
    let prefix = podman_cmd_prefix();
    if prefix.is_empty() {
        let _ = std::process::Command::new("podman")
            .args(["rm", "-f", "fcvm-container"])
            .output();
    } else {
        let _ = std::process::Command::new(&prefix[0])
            .args(&prefix[1..])
            .args(["podman", "rm", "-f", "fcvm-container"])
            .output();
    }

    Ok(exit_code)
}

#[cfg(test)]
mod tests {
    /// A one-shot process recovers the agent's prefix, and a missing record is an error
    /// rather than an empty (root) prefix.
    #[test]
    fn podman_cmd_prefix_record_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("prefix.json");
        assert!(load_podman_cmd_prefix(&path).is_err());
        let prefix = run_as_user_prefix("example-user", "/run/user/1000");
        store_podman_cmd_prefix(&path, &prefix).unwrap();
        assert_eq!(load_podman_cmd_prefix(&path).unwrap(), prefix);
        store_podman_cmd_prefix(&path, &[]).unwrap();
        assert!(load_podman_cmd_prefix(&path).unwrap().is_empty());
    }

    use super::*;

    /// podman's `--userns=keep-id` mapping, as `GetKeepIDMapping` in podman 4.9 builds
    /// it: ids below the user's are namespace ids from 1, the user's id is namespace id
    /// 0, and ids above it up to the subordinate count are themselves.
    #[test]
    fn keep_id_map_is_podmans() {
        let range = |container, host, size| IdRange {
            container,
            host,
            size,
        };
        assert_eq!(
            keep_id_map(1001, 65536),
            [
                range(0, 1, 1001),
                range(1001, 0, 1),
                range(1002, 1002, 64535)
            ]
        );
        // Id 0 (a --user with group 0): podman 4.9.3 still emits its first range, with
        // size 0, as `min(gid, maxGID)`, so the layer's map carries it too. (crun then
        // refuses that map: "writing file /proc/<pid>/gid_map: Invalid argument".)
        assert_eq!(
            keep_id_map(0, 65536),
            [range(0, 1, 0), range(0, 0, 1), range(1, 1, 65536)]
        );
        // An id beyond the subordinate range has nothing above it.
        assert_eq!(
            keep_id_map(70000, 65536),
            [range(0, 1, 65536), range(70000, 0, 1)]
        );
    }

    /// A store's files show with the ownership a keep-id container of the user gives
    /// them: container root as the user's first subordinate id, the user's own id as the
    /// user, and the ids around it shifted as keep-id shifts them. Root podman wrote
    /// container ids (0, and 1 up); rootless podman on the host wrote its builder's uid
    /// and the host's subordinate ranges, which are also the user's.
    #[test]
    fn store_user_shows_the_store_as_a_keep_id_container_sees_it() {
        let user = StoreUser {
            uid: 1001,
            gid: 100,
            sub_start: 100000,
            sub_count: 65536,
            sub_gid_start: 200000,
            sub_gid_count: 65536,
        };
        let root_built = StoreOwner {
            uid: 0,
            gid: 0,
            sub_start: 1,
            sub_gid_start: 1,
        };
        assert_eq!(
            user.idmap_option(root_built),
            "X-mount.idmap=u:0:100000:1 u:1:100001:1000 u:1001:1001:1 u:1002:101001:64535 \
             g:0:200000:1 g:1:200001:99 g:100:100:1 g:101:200100:65436"
        );
        let built_by_another_uid = StoreOwner {
            uid: 2000,
            gid: 100,
            sub_start: 100000,
            sub_gid_start: 200000,
        };
        assert_eq!(
            user.idmap_option(built_by_another_uid),
            "X-mount.idmap=u:2000:100000:1 u:100000:100001:1000 u:101000:1001:1 \
             u:101001:101001:64535 g:100:200000:1 g:200000:200001:99 g:200099:100:1 \
             g:200100:200100:65436"
        );
    }

    /// Every layer of the store's list carries the keep-id mappings, in the field names
    /// and order containers/storage reads and compares, and everything else in the list
    /// is kept. A layer that already has a mapping is refused.
    #[test]
    fn layers_carry_the_keep_id_mappings() {
        let user = StoreUser {
            uid: 1000,
            gid: 1000,
            sub_start: 100000,
            sub_count: 65536,
            sub_gid_start: 100000,
            sub_gid_count: 65536,
        };
        let layers = r#"[{"id":"a","created":"x"},{"id":"b","parent":"a"}]"#;
        let mapped: serde_json::Value =
            serde_json::from_str(&user.layers_with_keep_id(layers).unwrap()).unwrap();
        let keep_id = serde_json::json!([
            {"container_id": 0, "host_id": 1, "size": 1000},
            {"container_id": 1000, "host_id": 0, "size": 1},
            {"container_id": 1001, "host_id": 1001, "size": 64536},
        ]);
        assert_eq!(
            mapped,
            serde_json::json!([
                {"id": "a", "created": "x", "uidmap": keep_id, "gidmap": keep_id},
                {"id": "b", "parent": "a", "uidmap": keep_id, "gidmap": keep_id},
            ])
        );
        let mapped_already = r#"[{"id":"a","uidmap":[{"container_id":0,"host_id":1,"size":1}]}]"#;
        let error = user.layers_with_keep_id(mapped_already).unwrap_err();
        assert!(format!("{error:#}").contains("layer a"), "{error:#}");
    }

    /// The layer list copy is readable by every user even under a umask of 077: root
    /// owns it, and the user's namespace does not map root.
    #[test]
    fn the_layer_list_copy_is_world_readable_whatever_the_umask() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("layers.json");
        // SAFETY: umask only sets this process's file mode creation mask; nextest runs
        // each test in a process of its own.
        unsafe { libc::umask(0o077) };
        write_readable(path.to_str().unwrap(), "[]").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644, "mode {mode:o}");
    }

    /// The guest's /etc/subgid takes the host's gid range when the plan carries one,
    /// so a rootless-built store's group ids are the user's; without one it is the uid
    /// range, as before.
    #[test]
    fn the_subgid_range_is_the_hosts_and_falls_back_to_the_subuid_range() {
        assert_eq!(
            resolve_subgid_range(Some((200000, 65536)), Some((100000, 65536))),
            (200000, 65536)
        );
        assert_eq!(
            resolve_subgid_range(None, Some((100000, 65536))),
            (100000, 65536)
        );
        assert_eq!(resolve_subgid_range(None, None), (100000, 65536));
    }

    use super::{consume_cache_wait_lines, run_cache_handshake, CacheResult, StatusConnector};

    /// An attached GPU reaches the container as a CDI device, before the image
    /// name (podman reads everything after it as the command); without one the
    /// arguments carry no --device.
    #[test]
    fn cdi_devices_are_passed_before_the_image() {
        let plan: crate::types::Plan =
            serde_json::from_str(r#"{"image": "alpine", "cmd": ["nvidia-smi"]}"#).unwrap();
        let args =
            super::build_podman_args(&plan, "alpine", None, &["nvidia.com/gpu=all".to_string()]);
        let device = args
            .windows(2)
            .position(|w| w[0] == "--device" && w[1] == "nvidia.com/gpu=all")
            .expect("the CDI device must be passed");
        let image = args.iter().position(|a| a == "alpine").unwrap();
        assert!(device < image, "{args:?}");
        assert_eq!(args.last().unwrap(), "nvidia-smi");

        let args = super::build_podman_args(&plan, "alpine", None, &[]);
        assert!(!args.iter().any(|a| a == "--device"), "{args:?}");
    }

    /// podman is asked to report the container's start, the host's only signal that it
    /// is running.
    #[test]
    fn podman_run_reports_the_start_over_sd_notify() {
        let plan: crate::types::Plan = serde_json::from_str(r#"{"image": "alpine"}"#).unwrap();
        for user in [None, Some(("fcvm-user", "/run/user/1000"))] {
            let args = super::build_podman_args(&plan, "alpine", user, &[]);
            let sdnotify = args.iter().position(|a| a == "--sdnotify=conmon");
            let image = args.iter().position(|a| a == "alpine").unwrap();
            assert!(sdnotify.is_some_and(|i| i < image), "{args:?}");
        }
    }

    /// The image's HEALTHCHECK is read from the same inspect as its digest; a missing
    /// one, and `HEALTHCHECK NONE`, are no healthcheck.
    #[test]
    fn image_facts_carry_the_digest_and_whether_there_is_a_healthcheck() {
        let digest = "sha256:4b9a4e0196b9ec48c209f98d6e7cea3241bfc402262bf1b0500ea4dbb1002c45";
        for (health, expected) in [
            (
                r#"{"Test":["CMD-SHELL","true"],"Interval":5000000000}"#,
                true,
            ),
            (r#"{"Test":["CMD","/bin/check"]}"#, true),
            ("null", false),
            (r#"{"Test":["NONE"]}"#, false),
            (r#"{"Test":[]}"#, false),
        ] {
            let facts = super::parse_image_facts(&format!("{digest}\n{health}\n")).unwrap();
            assert_eq!(facts.digest, digest);
            assert_eq!(facts.healthcheck, expected, "{health}");
        }
        assert!(super::parse_image_facts("\n").is_err());
        assert!(super::parse_image_facts(&format!("{digest}\n")).is_err());
        assert!(super::parse_image_facts(&format!("{digest}\nnot json")).is_err());
    }

    #[test]
    fn container_user_ids_follow_the_plans_user() {
        assert_eq!(super::podman_user_ids(None), None);
        assert_eq!(super::podman_user_ids(Some("0")), None);
        assert_eq!(super::podman_user_ids(Some("0:0")), None);
        assert_eq!(
            super::podman_user_ids(Some("1000:1000")),
            Some((1000, 1000))
        );
        assert_eq!(super::podman_user_ids(Some("1000")), Some((1000, 100)));
    }

    /// The start watch returns on podman's READY=1 and not before: podman sends
    /// MAINPID= and, while it pulls, EXTEND_TIMEOUT_USEC= on the same socket.
    #[tokio::test]
    async fn the_start_watch_waits_for_ready() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("start.sock");
        let watch = super::StartNotify::bind_at(&path, None).unwrap();
        let podman = std::os::unix::net::UnixDatagram::unbound().unwrap();

        podman
            .send_to(b"EXTEND_TIMEOUT_USEC=30000000", &path)
            .unwrap();
        podman.send_to(b"MAINPID=42", &path).unwrap();
        let early =
            tokio::time::timeout(std::time::Duration::from_millis(200), watch.wait_ready()).await;
        assert!(early.is_err(), "returned before READY=1");

        podman.send_to(b"MAINPID=42\nREADY=1", &path).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), watch.wait_ready())
            .await
            .expect("READY=1 was not seen")
            .unwrap();

        // A stale socket from an earlier boot is replaced, not an error.
        drop(watch);
        super::StartNotify::bind_at(&path, None).unwrap();
    }

    /// A pmem mount reaches the container read-only at the same path, as a
    /// read-only --disk does.
    #[test]
    fn pmem_mounts_are_bind_mounted_into_the_container_read_only() {
        let plan: crate::types::Plan = serde_json::from_str(
            r#"{"image": "alpine",
                "pmem_mounts": [{"device": "/dev/pmem0", "mount_path": "/mnt/cache"}]}"#,
        )
        .unwrap();
        let args = super::build_podman_args(&plan, "alpine", None, &[]);
        assert!(
            args.windows(2)
                .any(|pair| pair[0] == "-v" && pair[1] == "/mnt/cache:/mnt/cache:ro"),
            "{args:?}"
        );
    }

    fn buf_with(data: &[u8]) -> ([u8; 64], usize) {
        let mut buf = [0u8; 64];
        buf[..data.len()].copy_from_slice(data);
        (buf, data.len())
    }

    #[test]
    fn keepalive_lines_are_consumed_and_flagged() {
        let (mut buf, n) = buf_with(b"cache-wait\ncache-wait\n");
        let (rest, saw) = consume_cache_wait_lines(&mut buf, n);
        assert!(saw);
        assert_eq!(rest, 0, "complete keepalive lines must be drained");
    }

    #[test]
    fn split_cache_ack_tail_survives_compaction() {
        // "cache-ack" arriving split across reads must not be destroyed by the
        // keepalive drain: the partial tail is preserved so the next read
        // completes it.
        let (mut buf, n) = buf_with(b"cache-wait\ncache-a");
        let (rest, saw) = consume_cache_wait_lines(&mut buf, n);
        assert!(saw);
        assert_eq!(&buf[..rest], b"cache-a");
        // Simulate the next read appending the remainder.
        buf[rest..rest + 3].copy_from_slice(b"ck\n");
        let total = rest + 3;
        let received = std::str::from_utf8(&buf[..total]).unwrap();
        assert!(
            received.contains("cache-ack"),
            "split ack must assemble: {received}"
        );
    }

    #[test]
    fn no_keepalive_means_no_compaction() {
        let (mut buf, n) = buf_with(b"cache-a");
        let (rest, saw) = consume_cache_wait_lines(&mut buf, n);
        assert!(!saw);
        assert_eq!(rest, n, "partial ack untouched when no keepalive present");
        assert_eq!(&buf[..rest], b"cache-a");
    }

    #[test]
    fn repeated_keepalives_never_overflow_buffer() {
        let mut buf = [0u8; 64];
        let mut total = 0usize;
        for _ in 0..50 {
            let msg = b"cache-wait\n";
            assert!(
                total + msg.len() <= buf.len(),
                "buffer overflow before drain"
            );
            buf[total..total + msg.len()].copy_from_slice(msg);
            total += msg.len();
            let (rest, saw) = consume_cache_wait_lines(&mut buf, total);
            assert!(saw);
            total = rest;
            assert_eq!(total, 0);
        }
    }

    // -----------------------------------------------------------------------
    // Cache-handshake protocol tests: a scripted fake host over socketpairs.
    //
    // The protocol functions are fd-generic (nix poll/read/write), so Unix
    // stream pairs stand in for vsock exactly. Each `Session` script runs on
    // its own thread holding the host end; `ScriptedConnector` hands the guest
    // the matching ends in order. No sleeps anywhere except the single test
    // that exercises the 500ms write-probe path — every other script answers
    // or severs immediately, so orderings are forced by the script shape, not
    // by timing.
    //
    // The pre-protocol behaviour (classify a severed session as WarmStart)
    // cannot host these tests — the connector seam did not exist. Its red is
    // end-to-end: on the unfixed binary, 4/4 forced snapshot misses hung with
    // the host's ack sent into the severed session (2026-08-10, x86 KVM,
    // /tmp/missrace-logs), and the lifecycle-interleave failpoint test pins
    // the same ordering in-repo.
    // -----------------------------------------------------------------------

    use std::io::{Read as _, Write as _};
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    /// One scripted host action after reading the guest's ask.
    #[derive(Clone, Debug)]
    enum HostScript {
        /// Write these chunks (possibly split mid-verb), then keep the
        /// connection open until the guest drops its end.
        Answer(Vec<&'static [u8]>),
        /// Like `Answer`, but waits for the guest's hang-up after the first
        /// chunk. The guest acts on a verb the instant it is unambiguous, so
        /// a chunk boundary between a verb and its newline leaves the rest of
        /// the script writing into a closed peer. Reading to EOF is what
        /// sequences this — no sleeps.
        AnswerAcrossHangup(Vec<&'static [u8]>),
        /// Read the ask, then close without answering (a snapshot boundary).
        Sever,
        /// Never read at all; close after the guest's first extra byte
        /// arrives (the 500ms liveness probe) — leaves probe bytes unread so
        /// the close can surface as a reset (#627's flush mode).
        CloseOnProbe,
    }

    /// Runs one script on the host end of a socketpair. Returns the ask line
    /// it read (empty for CloseOnProbe).
    fn spawn_host(mut host: UnixStream, script: HostScript) -> std::thread::JoinHandle<String> {
        std::thread::spawn(move || match script {
            HostScript::Answer(chunks) => answer(&mut host, chunks, false),
            HostScript::AnswerAcrossHangup(chunks) => answer(&mut host, chunks, true),
            HostScript::Sever => {
                let ask = read_ask(&mut host);
                drop(host);
                ask
            }
            HostScript::CloseOnProbe => {
                let mut byte = [0u8; 1];
                // First read: the ask's first byte. Swallow the whole ask,
                // then wait for one probe byte and close with it unread-ish.
                let _ = read_ask(&mut host);
                let _ = host.read(&mut byte);
                drop(host);
                String::new()
            }
        })
    }

    /// Reads the ask, writes the script's chunks, then holds the connection
    /// open (mirroring the real listener's positive-close drain) until the
    /// guest closes.
    fn answer(
        host: &mut UnixStream,
        chunks: Vec<&'static [u8]>,
        hang_up_after_first: bool,
    ) -> String {
        let ask = read_ask(host);
        for (i, chunk) in chunks.iter().enumerate() {
            if !write_chunk(host, chunk) {
                break;
            }
            if hang_up_after_first && i == 0 {
                drain_to_close(host);
            }
        }
        drain_to_close(host);
        ask
    }

    /// Writes one chunk, reporting whether the guest is still on the other end.
    /// A verdict is complete as soon as its verb is unambiguous, so the guest
    /// may hang up before the host has written the rest of the script. That is
    /// the guest behaving correctly, not a broken host.
    fn write_chunk(host: &mut UnixStream, chunk: &[u8]) -> bool {
        match host.write_all(chunk) {
            Ok(()) => true,
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => false,
            Err(e) => panic!("scripted host write: {e:?}"),
        }
    }

    fn drain_to_close(host: &mut UnixStream) {
        let mut sink = [0u8; 64];
        while matches!(host.read(&mut sink), Ok(n) if n > 0) {}
    }

    fn read_ask(host: &mut UnixStream) -> String {
        let mut ask = Vec::new();
        let mut byte = [0u8; 1];
        while let Ok(1) = host.read(&mut byte) {
            if byte[0] == b'\n' {
                break;
            }
            ask.push(byte[0]);
        }
        String::from_utf8_lossy(&ask).into_owned()
    }

    /// Hands out pre-scripted sessions in order; connects fail once exhausted.
    struct ScriptedConnector {
        sessions: std::collections::VecDeque<OwnedFd>,
        handles: Vec<std::thread::JoinHandle<String>>,
    }

    impl ScriptedConnector {
        fn new(scripts: Vec<HostScript>) -> Self {
            let mut sessions = std::collections::VecDeque::new();
            let mut handles = Vec::new();
            for script in scripts {
                let (guest, host) = UnixStream::pair().expect("socketpair");
                handles.push(spawn_host(host, script));
                sessions.push_back(OwnedFd::from(guest));
            }
            Self { sessions, handles }
        }

        fn asks(self) -> Vec<String> {
            self.handles
                .into_iter()
                .map(|h| h.join().expect("host thread"))
                .collect()
        }
    }

    impl StatusConnector for ScriptedConnector {
        fn connect(&mut self) -> Result<OwnedFd, nix::errno::Errno> {
            self.sessions
                .pop_front()
                .ok_or(nix::errno::Errno::ECONNREFUSED)
        }
    }

    fn handshake(scripts: Vec<HostScript>, flag: &AtomicBool) -> (CacheResult, Vec<String>) {
        let mut connector = ScriptedConnector::new(scripts);
        let result = run_cache_handshake(&mut connector, "sha256:test", flag, None::<()>);
        (result, connector.asks())
    }

    #[test]
    fn ack_on_the_first_session_is_a_cold_start() {
        let flag = AtomicBool::new(false);
        let (result, asks) = handshake(vec![HostScript::Answer(vec![b"cache-ack\n"])], &flag);
        assert_eq!(result, CacheResult::ColdStart);
        assert_eq!(asks, vec!["cache-ready:sha256:test"]);
    }

    /// THE core case of the re-ask protocol: a session severed by a snapshot
    /// boundary is not a classification. The guest asks again and the owner's
    /// answer decides. (Pre-protocol code returned WarmStart here and hung
    /// the resumed source forever.)
    #[test]
    fn a_severed_session_re_asks_and_the_late_ack_is_still_cold() {
        let flag = AtomicBool::new(false);
        let (result, asks) = handshake(
            vec![HostScript::Sever, HostScript::Answer(vec![b"cache-ack\n"])],
            &flag,
        );
        assert_eq!(result, CacheResult::ColdStart);
        assert_eq!(
            asks,
            vec!["cache-ready:sha256:test", "cache-ready:sha256:test"],
            "the re-ask must be the identical idempotent message"
        );
    }

    #[test]
    fn a_severed_session_answered_restored_is_a_warm_start() {
        let flag = AtomicBool::new(false);
        let (result, asks) = handshake(
            vec![
                HostScript::Sever,
                HostScript::Answer(vec![b"cache-restored\n"]),
            ],
            &flag,
        );
        assert_eq!(result, CacheResult::WarmStart);
        assert_eq!(asks.len(), 2);
    }

    #[test]
    fn a_doomed_verdict_is_surfaced_not_misread_as_cold() {
        let flag = AtomicBool::new(false);
        let (result, _) = handshake(vec![HostScript::Answer(vec![b"cache-doomed\n"])], &flag);
        assert_eq!(result, CacheResult::Doomed);
    }

    #[test]
    fn two_boundaries_in_a_row_still_converge_on_the_verdict() {
        let flag = AtomicBool::new(false);
        let (result, asks) = handshake(
            vec![
                HostScript::Sever,
                HostScript::Sever,
                HostScript::Answer(vec![b"cache-ack\n"]),
            ],
            &flag,
        );
        assert_eq!(result, CacheResult::ColdStart);
        assert_eq!(asks.len(), 3);
    }

    #[test]
    fn a_verdict_split_across_writes_still_assembles() {
        let flag = AtomicBool::new(false);
        let (result, _) = handshake(vec![HostScript::Answer(vec![b"cache-a", b"ck\n"])], &flag);
        assert_eq!(result, CacheResult::ColdStart);
    }

    #[test]
    fn keepalives_before_the_verdict_are_consumed_not_misread() {
        let flag = AtomicBool::new(false);
        let (result, _) = handshake(
            vec![HostScript::Answer(vec![
                b"cache-wait\n",
                b"cache-wait\n",
                b"cache-wait\n",
                b"cache-restored\n",
            ])],
            &flag,
        );
        assert_eq!(result, CacheResult::WarmStart);
    }

    #[test]
    fn the_restore_epoch_watcher_is_an_equal_authority() {
        // The flag is set before the boundary severs the first session, so
        // the guest must classify WarmStart without ever needing session 2.
        let flag = Arc::new(AtomicBool::new(false));
        let (guest, mut host) = UnixStream::pair().expect("socketpair");
        let flag_host = flag.clone();
        let handle = std::thread::spawn(move || {
            let _ = read_ask(&mut host);
            flag_host.store(true, Ordering::Release);
            drop(host); // sever AFTER publishing the flag
        });
        struct One(Option<OwnedFd>);
        impl StatusConnector for One {
            fn connect(&mut self) -> Result<OwnedFd, nix::errno::Errno> {
                self.0.take().ok_or(nix::errno::Errno::ECONNREFUSED)
            }
        }
        let mut connector = One(Some(OwnedFd::from(guest)));
        let result = run_cache_handshake(&mut connector, "sha256:test", &flag, None::<()>);
        handle.join().unwrap();
        assert_eq!(result, CacheResult::WarmStart);
    }

    /// The console quiesce guard covers the snapshot pause and nothing after
    /// it. A restored clone whose reconnects keep failing loops in this
    /// handshake until the restore flag or the 10-minute cap; with the guard
    /// held for the whole loop, every line fc-agent logged meanwhile stayed in
    /// the gated buffer, and #1207's clone showed nothing for two minutes.
    /// The guard must be gone by the first reconnect after the severed session.
    #[test]
    fn the_console_gate_is_released_at_the_first_severed_session() {
        struct Gate(Arc<AtomicBool>);
        impl Drop for Gate {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        /// One scripted session that the host severs, then refused reconnects.
        /// Each reconnect records whether the gate was already released; the
        /// third one publishes the restore flag so the handshake returns.
        struct SeverThenRefuse {
            first: Option<OwnedFd>,
            released: Arc<AtomicBool>,
            restore_flag: Arc<AtomicBool>,
            reconnects_seen_released: Vec<bool>,
        }
        impl StatusConnector for SeverThenRefuse {
            fn connect(&mut self) -> Result<OwnedFd, nix::errno::Errno> {
                if let Some(first) = self.first.take() {
                    return Ok(first);
                }
                self.reconnects_seen_released
                    .push(self.released.load(Ordering::SeqCst));
                if self.reconnects_seen_released.len() == 3 {
                    self.restore_flag.store(true, Ordering::Release);
                }
                Err(nix::errno::Errno::ECONNREFUSED)
            }
        }

        let (guest, host) = UnixStream::pair().expect("socketpair");
        let host = spawn_host(host, HostScript::Sever);
        let released = Arc::new(AtomicBool::new(false));
        let restore_flag = Arc::new(AtomicBool::new(false));
        let mut connector = SeverThenRefuse {
            first: Some(OwnedFd::from(guest)),
            released: released.clone(),
            restore_flag: restore_flag.clone(),
            reconnects_seen_released: Vec::new(),
        };

        let result = run_cache_handshake(
            &mut connector,
            "sha256:test",
            &restore_flag,
            Some(Gate(released.clone())),
        );
        host.join().expect("host thread");

        assert_eq!(result, CacheResult::WarmStart);
        assert_eq!(
            connector.reconnects_seen_released,
            vec![true, true, true],
            "the console gate was still held after the snapshot boundary severed the session"
        );
    }

    #[test]
    fn a_first_connect_refusal_fails_cold_like_before() {
        let flag = AtomicBool::new(false);
        let (result, asks) = handshake(vec![], &flag);
        assert_eq!(result, CacheResult::Failed);
        assert!(asks.is_empty());
    }

    /// #627's flush mode, now HEALED instead of merely tolerated: the host
    /// closes with a liveness probe unread, the guest's session dies with no
    /// verdict, and the re-ask recovers the answer. Exercises the 500ms
    /// write-probe path, so this is the one deliberately slow test (~1s).
    #[test]
    fn a_close_racing_an_unread_probe_recovers_via_re_ask() {
        let flag = AtomicBool::new(false);
        let (result, _) = handshake(
            vec![
                HostScript::CloseOnProbe,
                HostScript::Answer(vec![b"cache-ack\n"]),
            ],
            &flag,
        );
        assert_eq!(result, CacheResult::ColdStart);
    }

    /// The guest acts on a verb the instant it is unambiguous, which is before
    /// the newline whenever the host's write splits there. Every verdict must
    /// still map exactly, and the host must survive discovering the guest
    /// already hung up on the rest of its script.
    #[test]
    fn a_verdict_split_before_its_newline_maps_and_survives_the_hang_up() {
        for (verb, expected) in [
            (b"cache-ack".as_slice(), CacheResult::ColdStart),
            (b"cache-restored".as_slice(), CacheResult::WarmStart),
            (b"cache-doomed".as_slice(), CacheResult::Doomed),
        ] {
            let flag = AtomicBool::new(false);
            let (result, asks) = handshake(
                vec![HostScript::AnswerAcrossHangup(vec![verb, b"\n"])],
                &flag,
            );
            assert_eq!(
                result,
                expected,
                "verb {} lost its verdict when the newline landed after the hang-up",
                String::from_utf8_lossy(verb)
            );
            assert_eq!(asks, vec!["cache-ready:sha256:test"]);
        }
    }

    /// Seeded protocol fuzz: random host scripts, exact-verdict invariants.
    ///
    /// Every script is a chain of severed sessions (with random keepalive and
    /// garbage prefixes) ending in exactly one verdict, possibly split at a
    /// random byte. Invariants: the result maps 1:1 to the scripted verdict,
    /// the guest asked exactly once per session it was given, and every ask
    /// carried the identical message. No timing dependence: every script
    /// answers or severs immediately, so a hang would be a real protocol bug
    /// (and fails the suite's timeout rather than passing vacuously).
    ///
    /// Deterministic and replayable: a failure prints its seed; re-run with
    /// FCVM_FUZZ_SEED=<n> to reproduce, FCVM_FUZZ_SEEDS=<n> to widen (the
    /// hardware sweep runs thousands; default keeps `cargo test` fast).
    #[test]
    fn fuzz_random_host_scripts_map_verdicts_exactly_and_never_hang() {
        let seeds: u64 = std::env::var("FCVM_FUZZ_SEEDS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(500);
        let fixed: Option<u64> = std::env::var("FCVM_FUZZ_SEED")
            .ok()
            .and_then(|v| v.parse().ok());

        for seed in fixed.map(|s| s..s + 1).unwrap_or(0..seeds) {
            // xorshift64* — tiny, deterministic, no dependency.
            let mut state = seed.wrapping_mul(2685821657736338717).max(1);
            let mut next = move || {
                state ^= state >> 12;
                state ^= state << 25;
                state ^= state >> 27;
                state.wrapping_mul(2685821657736338717)
            };

            let boundaries = (next() % 4) as usize; // 0..=3 severed sessions
            let mut scripts = Vec::new();
            for _ in 0..boundaries {
                scripts.push(HostScript::Sever);
            }
            let verdict_idx = next() % 3;
            let verdict: &'static [u8] = match verdict_idx {
                0 => b"cache-ack\n",
                1 => b"cache-restored\n",
                _ => b"cache-doomed\n",
            };
            let mut chunks: Vec<&'static [u8]> = Vec::new();
            chunks.extend(std::iter::repeat_n(
                b"cache-wait\n".as_slice(),
                (next() % 3) as usize,
            ));
            // Random split point inside the verdict verb.
            let split = (next() as usize) % verdict.len();
            if split == 0 {
                chunks.push(verdict);
            } else {
                let (a, b) = verdict.split_at(split);
                chunks.push(a);
                chunks.push(b);
            }
            scripts.push(HostScript::Answer(chunks));

            let flag = AtomicBool::new(false);
            let expected_asks = scripts.len();
            let (result, asks) = handshake(scripts, &flag);
            let expected = match verdict_idx {
                0 => CacheResult::ColdStart,
                1 => CacheResult::WarmStart,
                _ => CacheResult::Doomed,
            };
            assert_eq!(
                result, expected,
                "seed {seed}: verdict mapping broke (boundaries={boundaries}, split={split})"
            );
            assert_eq!(
                asks.len(),
                expected_asks,
                "seed {seed}: ask count diverged from session count"
            );
            for ask in &asks {
                assert_eq!(
                    ask, "cache-ready:sha256:test",
                    "seed {seed}: a re-ask mutated the idempotent message"
                );
            }
        }
    }

    /// fc-agent resets podman state only for a container user, inside
    /// create_vm_user(). Root storage has nothing to reset on a fresh boot: no
    /// podman process runs before storage.conf names the final driver, because
    /// exec opens after storage setup; the btrfs loopback is freshly formatted;
    /// and the rootfs images carry no podman database. A root reset that ran
    /// while storage.conf still named overlay could leave a database recording
    /// the overlay driver, which the btrfs pull then rejects.
    #[test]
    fn only_the_container_user_gets_a_podman_reset() {
        let source = include_str!("container.rs");
        let body = &source[..source
            .find("\n#[cfg(test)]\nmod tests {")
            .expect("container.rs has no test module")];
        let resets: Vec<usize> = body.match_indices("\"reset\"").map(|(at, _)| at).collect();
        let user_fn = body
            .find("pub fn create_vm_user(")
            .expect("container.rs no longer defines create_vm_user");
        let user_fn_end = user_fn
            + body[user_fn..]
                .find("\n}\n")
                .expect("create_vm_user has no closing brace");
        assert!(
            resets.len() == 1 && (user_fn..user_fn_end).contains(&resets[0]),
            "fc-agent runs `podman system reset` outside create_vm_user() ({} call(s) at \
             byte offsets {resets:?}). A root reset on a fresh boot runs while \
             storage.conf may still name the overlay driver.",
            resets.len()
        );
    }
}

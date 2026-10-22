//! The NVIDIA driver image for GPU passthrough guests.
//!
//! A kernel profile with an `nvidia` table (the `gpu` profile) gets a
//! read-only ext4 image holding, for that profile's exact kernel build:
//!
//! - `lib/modules/<release>/extra/{nvidia,nvidia-uvm}.ko`, built from the
//!   `kernel-open/` sources inside the pinned driver `.run` against a tree
//!   configured and built from the same inputs as the profile's kernel;
//! - `lib/firmware/nvidia/<version>/gsp_*.bin`, the GSP firmware the open
//!   modules boot on Turing and later;
//! - `usr/lib/<triplet>/lib*.so.<version>` and `usr/bin/nvidia-smi` and
//!   friends, from the same `.run`;
//! - `usr/bin/nvidia-ctk` and `usr/bin/nvidia-cdi-hook` from the pinned
//!   nvidia-container-toolkit release.
//!
//! The modules and the user-space libraries come from one archive, so their
//! versions match by construction, and the build checks it (`modinfo` version
//! and the `libcuda.so.<version>` file name). The host attaches the image as a
//! read-only virtio-blk drive and names it in the boot plan; fc-agent mounts
//! it at `/run/fcvm-gpu/driver` (fc-agent/src/gpu.rs).
//!
//! The image is content-addressed by the kernel's identity (profile, version,
//! architecture, kernel SHA), the `nvidia` table, and the build stage below,
//! so a change to any of them builds a new image and leaves the old one.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tokio::process::Command;
use tracing::{info, warn};

use crate::paths;
use crate::setup::rootfs::{get_kernel_profile, KernelProfile};
use crate::utils::run_streaming;

/// Pinned NVIDIA driver and container toolkit for a kernel profile
/// (`[kernel_profiles.<name>.<arch>.nvidia]`).
#[derive(Debug, Deserialize, Clone, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NvidiaDriverConfig {
    /// Driver version, e.g. "580.196.07". The modules, libraries and firmware
    /// all come from the `.run` of this version.
    pub driver_version: String,
    /// The driver's `.run` installer for this architecture.
    pub driver_url: String,
    /// SHA-256 of the `.run`, lowercase hex.
    pub driver_sha256: String,
    /// nvidia-container-toolkit version, e.g. "1.20.1".
    pub toolkit_version: String,
    /// The toolkit's `deb_<arch>.tar.gz` release archive.
    pub toolkit_url: String,
    /// SHA-256 of that archive, lowercase hex.
    pub toolkit_sha256: String,
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn is_version(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|b| b.is_ascii_digit() || b == b'.')
        && !value.starts_with('.')
        && !value.ends_with('.')
}

impl NvidiaDriverConfig {
    /// Refuse a table that cannot be pinned exactly. Each value is
    /// interpolated into the build script, so this is also what keeps a quote
    /// or a space out of it.
    pub fn validate(&self) -> Result<()> {
        for (name, value) in [
            ("driver_version", &self.driver_version),
            ("toolkit_version", &self.toolkit_version),
        ] {
            if !is_version(value) {
                bail!("nvidia.{name} must be dotted decimal digits, got {value:?}");
            }
        }
        for (name, value) in [
            ("driver_sha256", &self.driver_sha256),
            ("toolkit_sha256", &self.toolkit_sha256),
        ] {
            if !is_sha256(value) {
                bail!("nvidia.{name} must be 64 lowercase hexadecimal characters, got {value:?}");
            }
        }
        for (name, value) in [
            ("driver_url", &self.driver_url),
            ("toolkit_url", &self.toolkit_url),
        ] {
            let safe = value.starts_with("https://")
                && value
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-._~:/%+".contains(&b));
            if !safe {
                bail!("nvidia.{name} must be an https URL without quotes or spaces, got {value:?}");
            }
        }
        if !self.driver_url.contains(&self.driver_version) {
            bail!(
                "nvidia.driver_url {:?} does not name driver_version {}",
                self.driver_url,
                self.driver_version
            );
        }
        if !self.toolkit_url.contains(&self.toolkit_version) {
            bail!(
                "nvidia.toolkit_url {:?} does not name toolkit_version {}",
                self.toolkit_url,
                self.toolkit_version
            );
        }
        Ok(())
    }
}

/// The Debian multiarch triplet the guest's libraries live under.
fn lib_triplet(arch: &str) -> Result<&'static str> {
    match arch {
        "aarch64" => Ok("aarch64-linux-gnu"),
        "x86_64" => Ok("x86_64-linux-gnu"),
        other => bail!("no NVIDIA driver image for architecture {other}"),
    }
}

/// The shell stage that builds the driver image, run in the configured kernel
/// tree after `make <image> modules` (see `generate_vm_kernel_build_script`).
///
/// It reads its inputs from variables the header sets, so its text depends on
/// nothing but the recipe and is hashed into the image's key as it stands.
pub(crate) const DRIVER_IMAGE_STAGE: &str = r##"
# ===== NVIDIA driver image =====
export PATH="$PATH:/usr/sbin:/sbin"
KREL=$(make -s ARCH="$KERNEL_ARCH" kernelrelease)
NV_DIR="$BUILD_DIR/nvidia"
NV_ROOT="$NV_DIR/root"
mkdir -p "$NV_DIR"
echo "=== NVIDIA driver image: driver $NV_DRIVER_VERSION, toolkit $NV_TOOLKIT_VERSION, kernel $KREL ==="

# Download to a unique sibling, check the pinned SHA-256, then rename, so an
# interrupted download never leaves a file that a later run trusts.
fetch_verified() {
    local url="$1" sha="$2" dest="$3"
    if [[ -f "$dest" ]] && echo "$sha  $dest" | sha256sum -c --status; then
        return
    fi
    local tmp
    tmp=$(mktemp "$dest.downloading.XXXXXX")
    if ! curl -fSL "$url" -o "$tmp"; then
        rm -f "$tmp"
        echo "ERROR: failed to download $url" >&2
        exit 1
    fi
    if ! echo "$sha  $tmp" | sha256sum -c --status; then
        echo "ERROR: $url does not have the pinned SHA-256 $sha (got $(sha256sum "$tmp" | cut -d' ' -f1))" >&2
        rm -f "$tmp"
        exit 1
    fi
    mv -f "$tmp" "$dest"
}
fetch_verified "$NV_DRIVER_URL" "$NV_DRIVER_SHA256" "$NV_DIR/driver-$NV_DRIVER_VERSION.run"
fetch_verified "$NV_TOOLKIT_URL" "$NV_TOOLKIT_SHA256" "$NV_DIR/toolkit-$NV_TOOLKIT_VERSION.tar.gz"

rm -rf "$NV_DIR/driver" "$NV_DIR/toolkit" "$NV_ROOT"
sh "$NV_DIR/driver-$NV_DRIVER_VERSION.run" --extract-only --target "$NV_DIR/driver" >/dev/null

# Kernel modules, from the .run's own open sources, against this tree.
make -C "$NV_DIR/driver/kernel-open" SYSSRC="$PWD" SYSOUT="$PWD" ARCH="$KERNEL_ARCH" -j"$NPROC" modules
MODDIR="$NV_ROOT/lib/modules/$KREL/extra"
mkdir -p "$MODDIR"
for module in nvidia nvidia-uvm; do
    ko="$NV_DIR/driver/kernel-open/$module.ko"
    version=$(modinfo -F version "$ko")
    vermagic=$(modinfo -F vermagic "$ko")
    if [[ "$version" != "$NV_DRIVER_VERSION" ]]; then
        echo "ERROR: $module.ko is version $version, expected $NV_DRIVER_VERSION" >&2
        exit 1
    fi
    if [[ "$vermagic" != "$KREL "* ]]; then
        echo "ERROR: $module.ko vermagic '$vermagic' is not for kernel $KREL" >&2
        exit 1
    fi
    install -m 0644 "$ko" "$MODDIR/$module.ko"
done

# GSP firmware, where the firmware loader looks for nvidia/<version>/.
install -d "$NV_ROOT/lib/firmware/nvidia/$NV_DRIVER_VERSION"
install -m 0644 "$NV_DIR"/driver/firmware/gsp_*.bin "$NV_ROOT/lib/firmware/nvidia/$NV_DRIVER_VERSION/"

# User-space driver: every library of this exact version, its soname link,
# and the compute tools.
LIBDIR="$NV_ROOT/usr/lib/$NV_LIB_TRIPLET"
install -d "$LIBDIR" "$NV_ROOT/usr/bin"
for lib in "$NV_DIR"/driver/lib*.so."$NV_DRIVER_VERSION"; do
    name=$(basename "$lib")
    install -m 0755 "$lib" "$LIBDIR/$name"
    soname=$(readelf -d "$lib" | sed -n 's/.*(SONAME).*\[\(.*\)\]/\1/p')
    if [[ -n "$soname" && "$soname" != "$name" ]]; then
        ln -sf "$name" "$LIBDIR/$soname"
    fi
done
for tool in nvidia-smi nvidia-debugdump nvidia-persistenced nvidia-cuda-mps-control nvidia-cuda-mps-server; do
    install -m 0755 "$NV_DIR/driver/$tool" "$NV_ROOT/usr/bin/$tool"
done

# nvidia-ctk and nvidia-cdi-hook, from the toolkit's base package.
mkdir -p "$NV_DIR/toolkit"
tar -xzf "$NV_DIR/toolkit-$NV_TOOLKIT_VERSION.tar.gz" -C "$NV_DIR/toolkit"
mapfile -t debs < <(find "$NV_DIR/toolkit" -name "nvidia-container-toolkit-base_${NV_TOOLKIT_VERSION}-*.deb")
if [[ ${#debs[@]} -ne 1 ]]; then
    echo "ERROR: expected one nvidia-container-toolkit-base $NV_TOOLKIT_VERSION package, found ${#debs[@]}" >&2
    exit 1
fi
dpkg-deb -x "${debs[0]}" "$NV_DIR/toolkit/base"
install -m 0755 "$NV_DIR/toolkit/base/usr/bin/nvidia-ctk" "$NV_DIR/toolkit/base/usr/bin/nvidia-cdi-hook" "$NV_ROOT/usr/bin/"

for required in \
    "$MODDIR/nvidia.ko" "$MODDIR/nvidia-uvm.ko" \
    "$LIBDIR/libcuda.so.$NV_DRIVER_VERSION" "$LIBDIR/libcuda.so.1" \
    "$LIBDIR/libnvidia-ml.so.$NV_DRIVER_VERSION" "$LIBDIR/libnvidia-ml.so.1" \
    "$NV_ROOT/usr/bin/nvidia-smi" "$NV_ROOT/usr/bin/nvidia-ctk" "$NV_ROOT/usr/bin/nvidia-cdi-hook"; do
    if [[ ! -e "$required" ]]; then
        echo "ERROR: driver image is missing $required" >&2
        exit 1
    fi
done
if ! compgen -G "$NV_ROOT/lib/firmware/nvidia/$NV_DRIVER_VERSION/gsp_*.bin" >/dev/null; then
    echo "ERROR: driver image has no GSP firmware" >&2
    exit 1
fi

# ext4 with 4 KiB blocks and a length in whole 2 MiB units: what fcvm's
# read-only disk and pmem paths accept. mkfs.ext4 -d needs no root.
used_kib=$(du -sk --apparent-size "$NV_ROOT" | cut -f1)
size_mib=$(( (used_kib * 5 / 4) / 1024 + 64 ))
size_mib=$(( (size_mib + 1) / 2 * 2 ))
rm -f "$NV_IMAGE_TMP"
truncate -s "${size_mib}M" "$NV_IMAGE_TMP"
mkfs.ext4 -q -F -b 4096 -L fcvm-nvidia -d "$NV_ROOT" "$NV_IMAGE_TMP"

# mkfs.ext4 -d copies the builder's uid and gid. Hand every inode to root so
# the guest sees a root-owned tree, as a package install would leave it.
debugfs_cmds="$NV_DIR/debugfs-owner.cmds"
: > "$debugfs_cmds"
(cd "$NV_ROOT" && find . -mindepth 1 -printf '%P\n') | while IFS= read -r path; do
    printf 'set_inode_field "/%s" uid 0\nset_inode_field "/%s" gid 0\n' "$path" "$path" >> "$debugfs_cmds"
done
printf 'set_inode_field / uid 0\nset_inode_field / gid 0\n' >> "$debugfs_cmds"
debugfs -w -f "$debugfs_cmds" "$NV_IMAGE_TMP" >/dev/null
# debugfs exits 0 when a command fails, so check the result: the root, a
# directory, a file and a symlink.
for path in / /usr/bin /usr/bin/nvidia-smi "/usr/lib/$NV_LIB_TRIPLET/libcuda.so.1"; do
    owner=$(debugfs -R "stat \"$path\"" "$NV_IMAGE_TMP" 2>/dev/null | sed -n 's/^User: *\([0-9]*\) *Group: *\([0-9]*\).*/\1:\2/p')
    if [[ "$owner" != "0:0" ]]; then
        echo "ERROR: $path in the driver image is owned by '$owner', expected 0:0" >&2
        exit 1
    fi
done
e2fsck -fn "$NV_IMAGE_TMP" >/dev/null
echo "Driver image: $NV_IMAGE_TMP (${size_mib} MiB)"
"##;

/// The header that hands the stage its inputs.
fn stage_header(config: &NvidiaDriverConfig, triplet: &str, image_tmp: &Path) -> String {
    format!(
        "NV_DRIVER_VERSION=\"{}\"\nNV_DRIVER_URL=\"{}\"\nNV_DRIVER_SHA256=\"{}\"\n\
         NV_TOOLKIT_VERSION=\"{}\"\nNV_TOOLKIT_URL=\"{}\"\nNV_TOOLKIT_SHA256=\"{}\"\n\
         NV_LIB_TRIPLET=\"{}\"\nNV_IMAGE_TMP=\"{}\"\n",
        config.driver_version,
        config.driver_url,
        config.driver_sha256,
        config.toolkit_version,
        config.toolkit_url,
        config.toolkit_sha256,
        triplet,
        image_tmp.display(),
    )
}

/// The image's content key: the kernel's identity, the pinned versions and
/// hashes, and the stage text. Not the paths the build writes to.
pub fn driver_image_key(
    profile_name: &str,
    kernel_version: &str,
    arch: &str,
    kernel_sha: &str,
    config: &NvidiaDriverConfig,
) -> String {
    let mut hasher = Sha256::new();
    for part in [
        profile_name,
        kernel_version,
        arch,
        kernel_sha,
        &config.driver_version,
        &config.driver_url,
        &config.driver_sha256,
        &config.toolkit_version,
        &config.toolkit_url,
        &config.toolkit_sha256,
        DRIVER_IMAGE_STAGE,
    ] {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part.as_bytes());
    }
    hex::encode(&hasher.finalize()[..6])
}

/// The image's file name in `<assets_dir>/nvidia/`.
pub fn driver_image_filename(
    profile_name: &str,
    kernel_version: &str,
    arch: &str,
    config: &NvidiaDriverConfig,
    key: &str,
) -> String {
    format!(
        "nvidia-{}-{}-{}-{}-{}.ext4",
        config.driver_version, profile_name, kernel_version, arch, key
    )
}

fn nvidia_config(profile: &KernelProfile, profile_name: &str) -> Result<NvidiaDriverConfig> {
    let config = profile.nvidia.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "kernel profile '{profile_name}' has no [kernel_profiles.{profile_name}.<arch>.nvidia] \
             table, so it has no NVIDIA driver image"
        )
    })?;
    config.validate()?;
    if !profile.is_custom() || profile.is_url_based() {
        bail!(
            "kernel profile '{profile_name}' must build its kernel from source to carry an \
             NVIDIA driver image: the modules are built against that kernel's tree"
        );
    }
    Ok(config)
}

/// The driver image every cold boot on `profile_name` attaches, or None for a
/// profile without an NVIDIA table. Builds it when `allow_create` is set.
pub async fn profile_nvidia_driver_image(
    profile_name: &str,
    allow_create: bool,
) -> Result<Option<PathBuf>> {
    match get_kernel_profile(profile_name)? {
        Some(profile) if profile.nvidia.is_some() => {
            ensure_nvidia_driver_image(profile_name, allow_create)
                .await
                .map(Some)
        }
        _ => Ok(None),
    }
}

/// The driver image a restored clone's in-place reboot attaches: the profile's
/// current image when it resolves and exists, else None with a warning. Never an
/// error, because the restore that precedes any reboot does not use it.
pub fn reboot_driver_image(profile_name: &str) -> Option<PathBuf> {
    let has_driver = match get_kernel_profile(profile_name) {
        Ok(Some(profile)) => profile.nvidia.is_some(),
        Ok(None) => false,
        Err(error) => {
            warn!(error = %format!("{error:#}"), profile = profile_name, "reading the kernel profile for a reboot's driver image");
            return None;
        }
    };
    if !has_driver {
        return None;
    }
    match get_nvidia_driver_image_path(profile_name) {
        Ok(path) => present_for_reboot(path),
        Err(error) => {
            warn!(error = %format!("{error:#}"), profile = profile_name, "resolving the NVIDIA driver image for a reboot");
            None
        }
    }
}

fn present_for_reboot(path: PathBuf) -> Option<PathBuf> {
    if path.exists() {
        Some(path)
    } else {
        warn!(
            path = %path.display(),
            "the profile's NVIDIA driver image is not built; a reboot of this clone gets none"
        );
        None
    }
}

/// The driver image path for a profile, without building it.
pub fn get_nvidia_driver_image_path(profile_name: &str) -> Result<PathBuf> {
    Ok(DriverImage::resolve(profile_name)?.path)
}

/// A profile's driver image: its inputs, key and store path.
struct DriverImage {
    profile: KernelProfile,
    config: NvidiaDriverConfig,
    kernel_sha: String,
    key: String,
    path: PathBuf,
}

impl DriverImage {
    fn resolve(profile_name: &str) -> Result<Self> {
        let profile = get_kernel_profile(profile_name)?.ok_or_else(|| {
            anyhow::anyhow!("kernel profile '{profile_name}' not found in config")
        })?;
        let config = nvidia_config(&profile, profile_name)?;
        let kernel_sha = crate::setup::kernel::compute_profile_kernel_sha(&profile)?;
        let arch = std::env::consts::ARCH;
        let key = driver_image_key(
            profile_name,
            &profile.kernel_version,
            arch,
            &kernel_sha,
            &config,
        );
        let path = paths::assets_dir()
            .join("nvidia")
            .join(driver_image_filename(
                profile_name,
                &profile.kernel_version,
                arch,
                &config,
                &key,
            ));
        Ok(Self {
            profile,
            config,
            kernel_sha,
            key,
            path,
        })
    }
}

/// Ensure the NVIDIA driver image for `profile_name` exists, building it when
/// `allow_create` is set. Returns its content-addressed path.
///
/// The build configures and builds the profile's kernel tree (the same inputs
/// as the kernel artifact) so the modules link against `Module.symvers` of
/// that exact configuration, then runs [`DRIVER_IMAGE_STAGE`]. It needs the
/// fcvm source checkout, like a local kernel build.
pub async fn ensure_nvidia_driver_image(profile_name: &str, allow_create: bool) -> Result<PathBuf> {
    let image = DriverImage::resolve(profile_name)?;
    let image_path = image.path.clone();
    if image_path.exists() {
        info!(path = %image_path.display(), "NVIDIA driver image exists");
        return Ok(image_path);
    }
    if !allow_create {
        bail!(
            "NVIDIA driver image not found at {}.\nRun: fcvm setup --kernel-profile {}",
            image_path.display(),
            profile_name
        );
    }

    let dir = image_path
        .parent()
        .expect("image path has a parent")
        .to_path_buf();
    let file_name = image_path
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let flock = super::lock_store_dir(
        &dir.join(format!("{file_name}.lock")),
        "NVIDIA driver image",
    )
    .await?;
    if image_path.exists() {
        flock.unlock().map_err(|(_, err)| err)?;
        return Ok(image_path);
    }

    let result = build_driver_image(&image, profile_name).await;
    flock.unlock().map_err(|(_, err)| err)?;
    result?;
    Ok(image_path)
}

async fn build_driver_image(image: &DriverImage, profile_name: &str) -> Result<()> {
    let DriverImage {
        profile,
        config,
        kernel_sha,
        key,
        path: image_path,
    } = image;
    let repo_root = crate::setup::kernel::find_repo_root().ok_or_else(|| {
        anyhow::anyhow!(
            "building the NVIDIA driver image needs the fcvm source checkout (it builds the \
             '{profile_name}' kernel tree); run fcvm setup from the repository"
        )
    })?;
    let arch = std::env::consts::ARCH;

    // A build tree of this invocation's own: the store lock is per assets directory,
    // so two setups with different assets directories can build the same key at
    // once, and the kernel build of the same profile uses
    // /tmp/kernel-build-<profile>-<sha> under a different lock. The build runs as
    // the invoking user, so it stays in the temp directory that user can write.
    let (build_dir, script_path) =
        build_workspace(&std::env::temp_dir(), key, uuid::Uuid::new_v4());
    let image_tmp = image_path.with_extension(format!("ext4.tmp-{}", uuid::Uuid::new_v4()));
    let kernel_tmp = build_dir.join("kernel-image.discard");
    let _ = std::fs::remove_file(&kernel_tmp);

    let stage = format!(
        "{}{}",
        stage_header(config, lib_triplet(arch)?, &image_tmp),
        DRIVER_IMAGE_STAGE
    );
    let script = crate::setup::kernel::generate_vm_kernel_build_script(
        profile,
        profile_name,
        kernel_sha,
        &kernel_tmp,
        &repo_root,
        Some(&stage),
    )?;
    std::fs::write(&script_path, &script).context("writing NVIDIA driver image build script")?;
    std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))?;

    println!(
        "⚙️  Building NVIDIA driver image (driver {}, toolkit {}, profile {})...",
        config.driver_version, config.toolkit_version, profile_name
    );
    info!(script = %script_path.display(), build_dir = %build_dir.display(), "building NVIDIA driver image");
    let mut cmd = Command::new(&script_path);
    cmd.env("BUILD_DIR", &build_dir);
    super::run_build_as_sudo_invoker(&mut cmd);
    let status = run_streaming(cmd, "nvidia_build").await;
    let _ = std::fs::remove_file(&script_path);
    let status = match status {
        Ok(status) => status,
        Err(error) => {
            let _ = std::fs::remove_file(&image_tmp);
            return Err(error).context("running NVIDIA driver image build");
        }
    };
    if !status.success() || !image_tmp.exists() {
        let _ = std::fs::remove_file(&image_tmp);
        bail!(
            "NVIDIA driver image build failed (exit {:?}); the build tree is kept at {}",
            status.code(),
            build_dir.display()
        );
    }
    let published = super::publish_store_entry(&image_tmp, image_path, "NVIDIA driver image").await;
    if published.is_ok() {
        if let Err(error) = std::fs::remove_dir_all(&build_dir) {
            warn!(build_dir = %build_dir.display(), %error, "removing the NVIDIA build tree");
        }
    }
    published
}

/// The build tree and script of one driver image build, unique to the invocation
/// so concurrent builds never share a file. The script sits beside the tree, which
/// the script itself creates as the invoking user.
fn build_workspace(temp: &Path, key: &str, invocation: uuid::Uuid) -> (PathBuf, PathBuf) {
    (
        temp.join(format!("nvidia-build-{key}-{invocation}")),
        temp.join(format!("fcvm-nvidia-build-{key}-{invocation}.sh")),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pinned() -> NvidiaDriverConfig {
        NvidiaDriverConfig {
            driver_version: "580.196.07".to_string(),
            driver_url: "https://example.invalid/580.196.07/NVIDIA-Linux-aarch64-580.196.07.run"
                .to_string(),
            driver_sha256: "a".repeat(64),
            toolkit_version: "1.20.1".to_string(),
            toolkit_url: "https://example.invalid/v1.20.1/toolkit_1.20.1_deb_arm64.tar.gz"
                .to_string(),
            toolkit_sha256: "b".repeat(64),
        }
    }

    #[test]
    fn concurrent_builds_get_their_own_workspace() {
        let temp = Path::new("/tmp");
        let (a_dir, a_script) = build_workspace(temp, "k1", uuid::Uuid::new_v4());
        let (b_dir, b_script) = build_workspace(temp, "k1", uuid::Uuid::new_v4());
        assert_ne!(a_dir, b_dir);
        assert_ne!(a_script, b_script);
        assert!(
            !a_script.starts_with(&a_dir),
            "the script is written before its tree exists"
        );
    }

    #[test]
    fn a_reboot_without_a_built_image_gets_none() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nvidia-gpu.img");
        assert_eq!(present_for_reboot(missing.clone()), None);
        std::fs::write(&missing, b"").unwrap();
        assert_eq!(present_for_reboot(missing.clone()), Some(missing));
    }

    #[test]
    fn a_pinned_table_validates() {
        pinned().validate().unwrap();
    }

    #[test]
    fn values_that_would_break_the_script_or_the_pin_are_refused() {
        type Mutation = fn(&mut NvidiaDriverConfig);
        let cases: Vec<(&str, Mutation)> = vec![
            ("short sha", |c| c.driver_sha256 = "abc".to_string()),
            ("uppercase sha", |c| c.toolkit_sha256 = "A".repeat(64)),
            ("quote in url", |c| c.driver_url.push('"')),
            ("space in url", |c| c.toolkit_url.push(' ')),
            ("plain http", |c| {
                c.driver_url = c.driver_url.replace("https", "http")
            }),
            ("non-numeric version", |c| {
                c.driver_version = "580.x".to_string()
            }),
            ("url for another version", |c| {
                c.driver_version = "580.196.08".to_string()
            }),
            ("toolkit url for another version", |c| {
                c.toolkit_version = "1.20.2".to_string()
            }),
        ];
        for (what, mutate) in cases {
            let mut config = pinned();
            mutate(&mut config);
            assert!(config.validate().is_err(), "{what} was accepted");
        }
    }

    #[test]
    fn every_input_changes_the_key() {
        let base = driver_image_key("gpu", "6.18.50", "aarch64", "0123456789ab", &pinned());
        let mut keys = std::collections::BTreeSet::from([base.clone()]);
        let mut variants: Vec<String> = vec![
            driver_image_key("gpu2", "6.18.50", "aarch64", "0123456789ab", &pinned()),
            driver_image_key("gpu", "6.18.51", "aarch64", "0123456789ab", &pinned()),
            driver_image_key("gpu", "6.18.50", "x86_64", "0123456789ab", &pinned()),
            driver_image_key("gpu", "6.18.50", "aarch64", "0123456789ac", &pinned()),
        ];
        for mutate in [
            |c: &mut NvidiaDriverConfig| c.driver_version.push('1'),
            |c: &mut NvidiaDriverConfig| c.driver_url.push('1'),
            |c: &mut NvidiaDriverConfig| c.driver_sha256 = "c".repeat(64),
            |c: &mut NvidiaDriverConfig| c.toolkit_version.push('1'),
            |c: &mut NvidiaDriverConfig| c.toolkit_url.push('1'),
            |c: &mut NvidiaDriverConfig| c.toolkit_sha256 = "d".repeat(64),
        ] {
            let mut config = pinned();
            mutate(&mut config);
            variants.push(driver_image_key(
                "gpu",
                "6.18.50",
                "aarch64",
                "0123456789ab",
                &config,
            ));
        }
        for key in &variants {
            assert_eq!(key.len(), 12);
            assert!(keys.insert(key.clone()), "two inputs share key {key}");
        }
        assert_eq!(
            base,
            driver_image_key("gpu", "6.18.50", "aarch64", "0123456789ab", &pinned()),
            "the key is deterministic"
        );
    }

    #[test]
    fn the_stage_reads_every_input_the_header_sets() {
        let header = stage_header(&pinned(), "aarch64-linux-gnu", Path::new("/x/img.tmp"));
        for line in header.lines() {
            let name = line.split('=').next().unwrap();
            assert!(
                DRIVER_IMAGE_STAGE.contains(&format!("${name}"))
                    || DRIVER_IMAGE_STAGE.contains(&format!("${{{name}}}")),
                "the header sets {name} and the stage never reads it"
            );
        }
    }

    /// The modules and the libraries must be the same driver version; the
    /// stage checks both against NV_DRIVER_VERSION rather than trusting the
    /// archive name.
    #[test]
    fn the_stage_checks_module_and_library_versions() {
        assert!(DRIVER_IMAGE_STAGE.contains(r#"[[ "$version" != "$NV_DRIVER_VERSION" ]]"#));
        assert!(DRIVER_IMAGE_STAGE.contains(r#"[[ "$vermagic" != "$KREL "* ]]"#));
        assert!(DRIVER_IMAGE_STAGE.contains(r#""$LIBDIR/libcuda.so.$NV_DRIVER_VERSION""#));
    }

    #[test]
    fn a_profile_without_an_nvidia_table_has_no_image() {
        let profile = KernelProfile {
            kernel_version: "6.18.50".to_string(),
            kernel_repo: "ejc3/fcvm".to_string(),
            ..Default::default()
        };
        let error = nvidia_config(&profile, "default").unwrap_err().to_string();
        assert!(
            error.contains("[kernel_profiles.default.<arch>.nvidia]"),
            "{error}"
        );
    }

    #[test]
    fn an_inherited_kernel_cannot_carry_an_image() {
        let profile = KernelProfile {
            nvidia: Some(pinned()),
            ..Default::default()
        };
        let error = nvidia_config(&profile, "gpu").unwrap_err().to_string();
        assert!(error.contains("build its kernel from source"), "{error}");
    }

    #[test]
    fn the_image_name_carries_version_kernel_and_key() {
        assert_eq!(
            driver_image_filename("gpu", "6.18.50", "aarch64", &pinned(), "0123456789ab"),
            "nvidia-580.196.07-gpu-6.18.50-aarch64-0123456789ab.ext4"
        );
    }
}

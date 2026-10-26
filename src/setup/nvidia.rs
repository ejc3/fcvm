//! The NVIDIA driver image for GPU passthrough guests.
//!
//! A kernel profile with an `nvidia` table (the `gpu` profile) gets a
//! read-only ext4 image holding, for that profile's exact kernel build:
//!
//! - `lib/modules/<release>/extra/{nvidia,nvidia-uvm}.ko`, built from the
//!   `kernel-open/` sources (or, with `module_type = "proprietary"`, the
//!   proprietary `kernel/` sources) inside the pinned driver `.run` against a
//!   tree configured and built from the same inputs as the profile's kernel,
//!   and `nvidia.params`, the parameters fc-agent loads nvidia.ko with;
//! - `lib/firmware/nvidia/<version>/gsp_*.bin`, the GSP firmware the open
//!   modules boot on Turing and later;
//! - `usr/lib/<triplet>/lib*.so.<version>` and `usr/bin/nvidia-smi` and
//!   friends, from the same `.run`;
//! - `usr/bin/nvidia-ctk` and `usr/bin/nvidia-cdi-hook` from the pinned
//!   nvidia-container-toolkit release;
//! - `usr/bin/fcvm-gpu-scrub`, built here from [`GPU_SCRUB_SOURCE`], which
//!   fc-agent runs to zero the GPUs' memory before a container gets them.
//! - `usr/bin/cuda-checkpoint`, NVIDIA's pinned checkpoint utility, which
//!   fc-agent uses to park a GPU's CUDA processes for a warm GPU snapshot
//!   (fc-agent/src/gpu_park.rs).
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
    /// Which of the `.run`'s kernel modules the image carries.
    #[serde(default)]
    pub module_type: ModuleType,
    /// NVIDIA's `cuda-checkpoint` binary for this architecture, at a fixed commit
    /// of github.com/NVIDIA/cuda-checkpoint. It drives the driver's checkpoint API
    /// through `libcuda.so.1`, so the image's driver decides what it supports.
    pub cuda_checkpoint_url: String,
    /// SHA-256 of that binary, lowercase hex.
    pub cuda_checkpoint_sha256: String,
}

/// The NVIDIA kernel modules a driver image carries. Both come from the same
/// `.run` and work with the same user-space driver.
#[derive(Debug, Deserialize, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ModuleType {
    /// `kernel-open/`. The driver's resource manager runs on the GPU's GSP, so
    /// every GPU initialisation boots the GSP firmware first.
    #[default]
    Open,
    /// `kernel/`, the proprietary modules, loaded with `NVreg_EnableGpuFirmware=0`
    /// so the resource manager runs on the CPU and no GSP firmware is booted.
    /// On a T4 in a gpu VM that halved the GPU initialisation (nvidia-persistenced
    /// start 576 to 623 ms against 1,113 to 1,156 ms with the open modules).
    /// Turing and Ampere only: later GPUs have no proprietary module support.
    Proprietary,
}

impl ModuleType {
    /// The `.run`'s directory holding these modules' sources.
    fn source_dir(self) -> &'static str {
        match self {
            ModuleType::Open => "kernel-open",
            ModuleType::Proprietary => "kernel",
        }
    }

    /// The parameters nvidia.ko is loaded with (fc-agent reads them from
    /// `nvidia.params` beside the module).
    fn nvidia_params(self) -> &'static str {
        match self {
            ModuleType::Open => "",
            ModuleType::Proprietary => "NVreg_EnableGpuFirmware=0",
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            ModuleType::Open => "open",
            ModuleType::Proprietary => "proprietary",
        }
    }

    /// Whether these modules can run the NVIDIA device `device` (PCI device ID).
    ///
    /// The open modules run every GPU this driver supports (Turing and later). The
    /// proprietary modules are loaded with the GSP firmware off, which this
    /// configuration was measured with on Turing only; they are allowed on Turing
    /// and Ampere, and refused on anything later (Blackwell has no proprietary
    /// module support at all). The ranges cover exactly the Turing and Ampere
    /// entries of the driver's `supported-gpus.json` (580.196.07): Turing
    /// 0x1E00-0x1FFF and 0x2180-0x21FF, Ampere 0x2080-0x20FF, 0x2200-0x22FF and
    /// 0x2400-0x25FF. Volta (0x1Dxx), Hopper (0x23xx), Ada (0x26xx-0x28xx) and
    /// Blackwell (0x29xx and up) fall outside them.
    pub fn supports_device(self, device: u16) -> bool {
        match self {
            ModuleType::Open => true,
            ModuleType::Proprietary => matches!(
                device,
                0x1E00..=0x1FFF | 0x2180..=0x21FF | 0x2080..=0x20FF | 0x2200..=0x22FF | 0x2400..=0x25FF
            ),
        }
    }
}

/// The module type of `profile_name`'s NVIDIA driver image on this architecture,
/// or `None` when the profile builds no driver image.
pub fn profile_module_type(profile_name: &str) -> Result<Option<ModuleType>> {
    Ok(get_kernel_profile(profile_name)?
        .and_then(|profile| profile.nvidia)
        .map(|nvidia| nvidia.module_type))
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
            ("cuda_checkpoint_sha256", &self.cuda_checkpoint_sha256),
        ] {
            if !is_sha256(value) {
                bail!("nvidia.{name} must be 64 lowercase hexadecimal characters, got {value:?}");
            }
        }
        for (name, value) in [
            ("driver_url", &self.driver_url),
            ("toolkit_url", &self.toolkit_url),
            ("cuda_checkpoint_url", &self.cuda_checkpoint_url),
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
fetch_verified "$NV_CUDA_CHECKPOINT_URL" "$NV_CUDA_CHECKPOINT_SHA256" "$NV_DIR/cuda-checkpoint-$NV_CUDA_CHECKPOINT_SHA256"

rm -rf "$NV_DIR/driver" "$NV_DIR/toolkit" "$NV_ROOT"
sh "$NV_DIR/driver-$NV_DRIVER_VERSION.run" --extract-only --target "$NV_DIR/driver" >/dev/null

# Let nvidia.ko load with no GPU on the bus, as it already does when an NVSwitch
# is present: nvidia_init_module refuses an empty bus only behind this local.
# fc-agent loads the modules at boot, before the pre-start snapshot, so a
# restore that attaches a GPU only probes it (PCI rescan) instead of loading
# and initialising the driver.
NV_C="$NV_DIR/driver/$NV_MODULE_SRC/nvidia/nv.c"
NV_NOGPU_FROM='const NvBool is_nvswitch_present = os_is_nvswitch_present();'
NV_NOGPU_TO='const NvBool is_nvswitch_present = NV_TRUE;'
if [[ $(grep -cF "$NV_NOGPU_FROM" "$NV_C") -ne 1 ]]; then
    echo "ERROR: $NV_C does not declare is_nvswitch_present once as expected; cannot let the driver load without a GPU" >&2
    exit 1
fi
sed -i "s/$NV_NOGPU_FROM/$NV_NOGPU_TO/" "$NV_C"
if [[ $(grep -cF "$NV_NOGPU_TO" "$NV_C") -ne 1 ]]; then
    echo "ERROR: patching $NV_C to load without a GPU failed" >&2
    exit 1
fi

# Kernel modules, from the .run's own sources (kernel-open/ or the proprietary
# kernel/), against this tree.
make -C "$NV_DIR/driver/$NV_MODULE_SRC" SYSSRC="$PWD" SYSOUT="$PWD" ARCH="$KERNEL_ARCH" -j"$NPROC" modules
MODDIR="$NV_ROOT/lib/modules/$KREL/extra"
mkdir -p "$MODDIR"
for module in nvidia nvidia-uvm; do
    ko="$NV_DIR/driver/$NV_MODULE_SRC/$module.ko"
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
# The parameters fc-agent loads nvidia.ko with, one line, possibly empty.
printf '%s\n' "$NV_MODULE_PARAMS" > "$MODDIR/nvidia.params"

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

# The memory scrub fc-agent runs before a container gets the GPUs. It loads
# libcuda.so.1 with dlopen, so it needs only a C compiler. It runs on the guest's
# glibc: refuse a build that needs a version newer than 2.34, where dlopen and
# pthread_create moved into libc, rather than ship one the guest cannot load.
gcc -O2 -Wall -Wextra -Werror -pthread -o "$NV_ROOT/usr/bin/fcvm-gpu-scrub" "$NV_SCRUB_SOURCE"
newest_glibc=$(objdump -T "$NV_ROOT/usr/bin/fcvm-gpu-scrub" | grep -o 'GLIBC_[0-9.]*' | sort -uV | tail -1)
if [[ "$(printf '%s\nGLIBC_2.34\n' "$newest_glibc" | sort -V | tail -1)" != "GLIBC_2.34" ]]; then
    echo "ERROR: fcvm-gpu-scrub needs $newest_glibc, newer than GLIBC_2.34" >&2
    exit 1
fi
# cuda-checkpoint, for warm GPU snapshots.
install -m 0755 "$NV_DIR/cuda-checkpoint-$NV_CUDA_CHECKPOINT_SHA256" "$NV_ROOT/usr/bin/cuda-checkpoint"

for required in \
    "$MODDIR/nvidia.ko" "$MODDIR/nvidia-uvm.ko" \
    "$LIBDIR/libcuda.so.$NV_DRIVER_VERSION" "$LIBDIR/libcuda.so.1" \
    "$LIBDIR/libnvidia-ml.so.$NV_DRIVER_VERSION" "$LIBDIR/libnvidia-ml.so.1" \
    "$NV_ROOT/usr/bin/nvidia-smi" "$NV_ROOT/usr/bin/nvidia-ctk" "$NV_ROOT/usr/bin/nvidia-cdi-hook" \
    "$NV_ROOT/usr/bin/cuda-checkpoint" "$NV_ROOT/usr/bin/fcvm-gpu-scrub"; do
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

/// The source of `usr/bin/fcvm-gpu-scrub`, compiled by the stage. Part of the
/// image's key like the stage text.
pub(crate) const GPU_SCRUB_SOURCE: &str = include_str!("nvidia_gpu_scrub.c");

/// The header that hands the stage its inputs.
fn stage_header(
    config: &NvidiaDriverConfig,
    triplet: &str,
    image_tmp: &Path,
    scrub_source: &Path,
) -> String {
    format!(
        "NV_DRIVER_VERSION=\"{}\"\nNV_DRIVER_URL=\"{}\"\nNV_DRIVER_SHA256=\"{}\"\n\
         NV_TOOLKIT_VERSION=\"{}\"\nNV_TOOLKIT_URL=\"{}\"\nNV_TOOLKIT_SHA256=\"{}\"\n\
         NV_CUDA_CHECKPOINT_URL=\"{}\"\nNV_CUDA_CHECKPOINT_SHA256=\"{}\"\n\
         NV_LIB_TRIPLET=\"{}\"\nNV_IMAGE_TMP=\"{}\"\n\
         NV_MODULE_SRC=\"{}\"\nNV_MODULE_PARAMS=\"{}\"\nNV_SCRUB_SOURCE=\"{}\"\n",
        config.driver_version,
        config.driver_url,
        config.driver_sha256,
        config.toolkit_version,
        config.toolkit_url,
        config.toolkit_sha256,
        config.cuda_checkpoint_url,
        config.cuda_checkpoint_sha256,
        triplet,
        image_tmp.display(),
        config.module_type.source_dir(),
        config.module_type.nvidia_params(),
        scrub_source.display(),
    )
}

/// The image's content key: the kernel's identity, the pinned versions and
/// hashes, the stage text and the scrub tool's source. Not the paths the build
/// writes to.
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
        config.module_type.as_str(),
        &config.cuda_checkpoint_url,
        &config.cuda_checkpoint_sha256,
        DRIVER_IMAGE_STAGE,
        GPU_SCRUB_SOURCE,
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

    // Beside the script, unique to this invocation like it.
    let scrub_source = script_path.with_extension("c");
    write_scrub_source(&scrub_source)?;
    let stage = format!(
        "{}{}",
        stage_header(config, lib_triplet(arch)?, &image_tmp, &scrub_source),
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
    let _ = std::fs::remove_file(&scrub_source);
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

/// Write the scrub source for the build, which may run as the sudo invoker
/// rather than as this process.
fn write_scrub_source(path: &Path) -> Result<()> {
    std::fs::write(path, GPU_SCRUB_SOURCE).context("writing the GPU scrub source")?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644))
        .context("making the GPU scrub source readable")
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
            module_type: ModuleType::Open,
            cuda_checkpoint_url: "https://example.invalid/0123abc/bin/x86_64_Linux/cuda-checkpoint"
                .to_string(),
            cuda_checkpoint_sha256: "e".repeat(64),
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
            ("short cuda-checkpoint sha", |c| {
                c.cuda_checkpoint_sha256 = "e".repeat(63)
            }),
            ("quote in cuda-checkpoint url", |c| {
                c.cuda_checkpoint_url.push('"')
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
            |c: &mut NvidiaDriverConfig| c.module_type = ModuleType::Proprietary,
            |c: &mut NvidiaDriverConfig| c.cuda_checkpoint_url.push('1'),
            |c: &mut NvidiaDriverConfig| c.cuda_checkpoint_sha256 = "f".repeat(64),
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
        let header = stage_header(
            &pinned(),
            "aarch64-linux-gnu",
            Path::new("/x/img.tmp"),
            Path::new("/x/scrub.c"),
        );
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

    /// A stand-in for libcuda.so.1: each GPU has `free` bytes, an allocation
    /// larger than what is left is refused as out of memory, and at exit it
    /// reports per GPU what was allocated, zeroed, freed and left.
    const FAKE_LIBCUDA: &str = r#"
#include <stdio.h>
#include <stdlib.h>
static size_t avail[2] = {(1ul << 30) + (3ul << 20) + 4096, 5ul << 20};
static size_t allocated[2], zeroed[2];
static int allocs[2], frees[2];
static __thread int cur = -1;
int cuInit(unsigned f) { (void)f; return 0; }
int cuDeviceGetCount(int *c) { *c = getenv("FAKE_GPUS") ? atoi(getenv("FAKE_GPUS")) : 2; return 0; }
int cuDeviceGet(int *d, int o) { *d = o; return 0; }
int cuCtxCreate_v2(void **c, unsigned f, int d) { (void)f; cur = d; *c = &avail[d]; return 0; }
int cuCtxDestroy_v2(void *c) { (void)c; return 0; }
int cuCtxSynchronize(void) { return 0; }
int cuMemGetInfo_v2(size_t *f, size_t *t) { *f = avail[cur]; *t = 16ul << 30; return 0; }
int cuMemAlloc_v2(unsigned long long *p, size_t n) {
    if (n > avail[cur]) return 2;
    avail[cur] -= n; allocated[cur] += n; allocs[cur]++;
    *p = ((unsigned long long)cur << 40) | allocated[cur];
    return 0;
}
int cuMemFree_v2(unsigned long long p) { frees[p >> 40]++; return 0; }
int cuMemsetD32Async(unsigned long long p, unsigned v, size_t n, void *s) {
    (void)s;
    if (getenv("FAKE_MEMSET_FAILS")) return 700;
    if (v == 0) zeroed[p >> 40] += 4 * n;
    return 0;
}
int cuGetErrorString(int r, const char **s) { (void)r; *s = "fake error"; return 0; }
__attribute__((destructor)) static void report(void) {
    for (int d = 0; d < 2; d++)
        fprintf(stderr, "fake device=%d allocated=%zu zeroed=%zu allocs=%d frees=%d left=%zu\n",
                d, allocated[d], zeroed[d], allocs[d], frees[d], avail[d]);
}
"#;

    fn gcc(args: &[&str]) {
        let output = std::process::Command::new("gcc")
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "gcc {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Build fcvm-gpu-scrub with the stage's flags and the fake libcuda, and
    /// run it with `env`; returns (exit code, stdout, stderr).
    fn run_scrub_against_fake(env: &[(&str, &str)]) -> (i32, String, String) {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("scrub.c");
        let tool = dir.path().join("fcvm-gpu-scrub");
        std::fs::write(&source, GPU_SCRUB_SOURCE).unwrap();
        assert!(
            DRIVER_IMAGE_STAGE.contains(
                r#"gcc -O2 -Wall -Wextra -Werror -pthread -o "$NV_ROOT/usr/bin/fcvm-gpu-scrub" "$NV_SCRUB_SOURCE""#
            ),
            "the stage builds the scrub with other flags than this test"
        );
        gcc(&[
            "-O2",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-pthread",
            "-o",
            tool.to_str().unwrap(),
            source.to_str().unwrap(),
        ]);
        // The stage refuses a binary that needs a glibc newer than 2.34; check
        // the same here, so a source change that pulls in a newer symbol fails
        // this test rather than the next image build.
        let symbols = std::process::Command::new("objdump")
            .arg("-T")
            .arg(&tool)
            .output()
            .unwrap();
        let symbols = String::from_utf8_lossy(&symbols.stdout);
        let too_new: Vec<&str> = symbols
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '.'))
            .filter_map(|word| word.strip_prefix("GLIBC_"))
            .filter(|version| {
                let mut parts = version.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
                (parts.next().unwrap_or(0), parts.next().unwrap_or(0)) > (2, 34)
            })
            .collect();
        assert!(
            too_new.is_empty(),
            "fcvm-gpu-scrub needs glibc {too_new:?}, newer than the stage allows (2.34)"
        );
        let lib = dir.path().join("lib");
        std::fs::create_dir(&lib).unwrap();
        if !env.iter().any(|(k, _)| *k == "NO_FAKE") {
            let fake = dir.path().join("fake.c");
            std::fs::write(&fake, FAKE_LIBCUDA).unwrap();
            gcc(&[
                "-shared",
                "-fPIC",
                "-o",
                lib.join("libcuda.so.1").to_str().unwrap(),
                fake.to_str().unwrap(),
            ]);
        }
        let output = std::process::Command::new(&tool)
            .env("LD_LIBRARY_PATH", &lib)
            .envs(env.iter().copied())
            .output()
            .unwrap();
        (
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }

    /// Run the stage's no-GPU patch against an `nv.c` holding `source`.
    fn run_no_gpu_patch(module_src: &str, source: &str) -> (bool, String, String) {
        let start = DRIVER_IMAGE_STAGE
            .find("NV_C=")
            .expect("the stage no longer patches nv.c");
        let end = DRIVER_IMAGE_STAGE[start..]
            .find("# Kernel modules,")
            .expect("the no-GPU patch must come before the module build")
            + start;
        let dir = tempfile::tempdir().unwrap();
        let nvidia = dir.path().join("driver").join(module_src).join("nvidia");
        std::fs::create_dir_all(&nvidia).unwrap();
        std::fs::write(nvidia.join("nv.c"), source).unwrap();
        let output = std::process::Command::new("bash")
            .arg("-c")
            .arg(&DRIVER_IMAGE_STAGE[start..end])
            .env("NV_DIR", dir.path())
            .env("NV_MODULE_SRC", module_src)
            .output()
            .unwrap();
        (
            output.status.success(),
            std::fs::read_to_string(nvidia.join("nv.c")).unwrap(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }

    /// The scrub takes every allocation a GPU grants, down to 2 MiB, zeroes
    /// all of it and frees all of it, on every GPU.
    #[test]
    fn the_gpu_scrub_zeroes_everything_it_can_allocate_on_every_gpu() {
        let (code, stdout, stderr) = run_scrub_against_fake(&[]);
        assert_eq!(code, 0, "{stdout}{stderr}");
        assert!(stdout.contains("scrubbed devices=2 "), "{stdout}");
        for (device, allocated) in [(0, (1u64 << 30) + (2 << 20)), (1, 4 << 20)] {
            let line = stderr
                .lines()
                .find(|l| l.starts_with(&format!("fake device={device} ")))
                .unwrap_or_else(|| panic!("no report for device {device}: {stderr}"));
            assert!(
                line.contains(&format!("allocated={allocated} zeroed={allocated} ")),
                "device {device} was not zeroed in full: {line}"
            );
            let field = |name: &str| -> u64 {
                line.split_whitespace()
                    .find_map(|f| f.strip_prefix(&format!("{name}=")))
                    .unwrap()
                    .parse()
                    .unwrap()
            };
            assert_eq!(field("allocs"), field("frees"), "{line}");
            assert!(field("left") < 2 << 20, "{line}");
        }
    }

    /// A GPU it cannot zero, or a missing driver, fails the scrub; what it
    /// allocated is still freed.
    #[test]
    fn the_gpu_scrub_fails_when_it_cannot_zero_a_gpu() {
        let (code, stdout, stderr) = run_scrub_against_fake(&[("FAKE_MEMSET_FAILS", "1")]);
        assert_eq!(code, 1, "{stdout}{stderr}");
        assert!(stderr.contains("cuMemsetD32Async: fake error"), "{stderr}");
        assert!(!stdout.contains("scrubbed devices="), "{stdout}");
        assert!(stderr.contains("allocs=2 frees=2"), "{stderr}");

        let (code, stdout, stderr) = run_scrub_against_fake(&[("FAKE_GPUS", "0")]);
        assert_eq!(code, 1, "{stdout}{stderr}");
        assert!(stderr.contains("reports 0 GPU(s)"), "{stderr}");

        let (code, _, stderr) = run_scrub_against_fake(&[("NO_FAKE", "1")]);
        assert_eq!(code, 1, "{stderr}");
        assert!(stderr.contains("libcuda.so.1"), "{stderr}");
    }

    /// The build drops to the sudo invoker, so the source must be readable by
    /// another user whatever this process's umask.
    #[test]
    fn the_scrub_source_is_readable_by_the_build_user_under_any_umask() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scrub.c");
        // nextest runs each test in its own process, so the umask is ours.
        let old = unsafe { libc::umask(0o077) };
        let written = write_scrub_source(&path);
        unsafe { libc::umask(old) };
        written.unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644, "mode {mode:o}");
    }

    /// fc-agent runs the scrub from the path the stage installs it at.
    #[test]
    fn fc_agent_runs_the_scrub_the_image_carries() {
        let agent = include_str!("../../fc-agent/src/gpu.rs");
        assert!(
            agent.contains(r#"pub const SCRUB_TOOL: &str = "usr/bin/fcvm-gpu-scrub";"#),
            "fc-agent's SCRUB_TOOL no longer names usr/bin/fcvm-gpu-scrub"
        );
        assert!(DRIVER_IMAGE_STAGE.contains(r#""$NV_ROOT/usr/bin/fcvm-gpu-scrub"; do"#));
    }

    /// nvidia.ko must load with no GPU on the bus, so fc-agent can load it
    /// before the pre-start snapshot. The open module's init refuses an empty
    /// bus unless `is_nvswitch_present`; the stage makes that local true, and
    /// fails the build when the driver source no longer has the line.
    #[test]
    fn the_stage_lets_the_driver_load_without_a_gpu() {
        let declared = "    const NvBool is_nvswitch_present = os_is_nvswitch_present();\n";
        let refusal = "    if ((count == 0) && (!is_nvswitch_present))\n";
        for module_type in [ModuleType::Open, ModuleType::Proprietary] {
            let src = module_type.source_dir();
            let (ok, patched, stderr) = run_no_gpu_patch(src, &format!("{declared}{refusal}"));
            assert!(ok, "{src}: {stderr}");
            assert_eq!(
                patched,
                format!("    const NvBool is_nvswitch_present = NV_TRUE;\n{refusal}"),
                "{src}"
            );

            let (ok, unchanged, stderr) = run_no_gpu_patch(src, refusal);
            assert!(
                !ok,
                "{src}: a driver without the declaration must fail the build"
            );
            assert_eq!(unchanged, refusal);
            assert!(stderr.contains("is_nvswitch_present"), "{stderr}");
        }
    }

    /// The module type picks the `.run` directory the stage builds and the
    /// parameters it records for fc-agent; an unset type is the open modules.
    #[test]
    fn the_module_type_picks_the_sources_and_the_load_parameters() {
        let parse = |extra: &str| -> Result<NvidiaDriverConfig, toml::de::Error> {
            toml::from_str(&format!(
                "driver_version = \"1\"\ndriver_url = \"u\"\ndriver_sha256 = \"s\"\n\
                 toolkit_version = \"1\"\ntoolkit_url = \"u\"\ntoolkit_sha256 = \"s\"\n\
                 cuda_checkpoint_url = \"u\"\ncuda_checkpoint_sha256 = \"s\"\n{extra}"
            ))
        };
        assert_eq!(parse("").unwrap().module_type, ModuleType::Open);
        assert_eq!(
            parse("module_type = \"proprietary\"").unwrap().module_type,
            ModuleType::Proprietary
        );
        assert!(parse("module_type = \"closed\"").is_err());

        let mut config = pinned();
        let open = stage_header(
            &config,
            "x86_64-linux-gnu",
            Path::new("/x/img.tmp"),
            Path::new("/x/scrub.c"),
        );
        assert!(open.contains("NV_MODULE_SRC=\"kernel-open\"\n"), "{open}");
        assert!(open.contains("NV_MODULE_PARAMS=\"\"\n"), "{open}");
        config.module_type = ModuleType::Proprietary;
        let closed = stage_header(
            &config,
            "x86_64-linux-gnu",
            Path::new("/x/img.tmp"),
            Path::new("/x/scrub.c"),
        );
        assert!(closed.contains("NV_MODULE_SRC=\"kernel\"\n"), "{closed}");
        assert!(
            closed.contains("NV_MODULE_PARAMS=\"NVreg_EnableGpuFirmware=0\"\n"),
            "{closed}"
        );
        assert!(DRIVER_IMAGE_STAGE.contains(r#"make -C "$NV_DIR/driver/$NV_MODULE_SRC""#));
        assert!(DRIVER_IMAGE_STAGE.contains(r#""$NV_MODULE_PARAMS" > "$MODDIR/nvidia.params""#));
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

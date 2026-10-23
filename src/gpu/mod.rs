//! Host-side checks for passing a PCI device (a GPU) through to a microVM.
//!
//! A GPU is attached to a VM restored from a snapshot that was taken without it:
//! Firecracker's `LoadSnapshot` takes the device list and attaches each device to
//! the paused VM before any vCPU runs. A device's state lives on the device, so a
//! VM that has one can not be snapshotted again. Before handing an address to
//! Firecracker, fcvm proves the host side is ready, so a misconfigured host fails
//! with a message that says what to fix instead of a VFIO error from inside the
//! restore.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

pub mod scrub;

/// The Firecracker argument a kernel profile must carry for passthrough: devices
/// are attached to the PCI bus, which Firecracker builds only with it.
pub const PCI_FIRECRACKER_ARG: &str = "--enable-pci";

/// Canonical `SSSS:BB:DD.F` form of a host PCI address, as lspci -D and sysfs
/// print it. A short `BB:DD.F` gets segment 0000.
pub fn canonical_sbdf(address: &str) -> Result<String> {
    let lowered = address.trim().to_ascii_lowercase();
    let full = match lowered.matches(':').count() {
        1 => format!("0000:{lowered}"),
        2 => lowered,
        _ => bail!("PCI address {address:?} is not SSSS:BB:DD.F or BB:DD.F"),
    };
    let (rest, function) = full
        .rsplit_once('.')
        .with_context(|| format!("PCI address {address:?} has no function (.F)"))?;
    let mut parts = rest.split(':');
    let (segment, bus, device) = match (parts.next(), parts.next(), parts.next()) {
        (Some(s), Some(b), Some(d)) => (s, b, d),
        _ => bail!("PCI address {address:?} is not SSSS:BB:DD.F"),
    };
    let field = |text: &str, width: usize, max: u32, name: &str| -> Result<u32> {
        if text.len() != width || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("PCI address {address:?}: {name} must be {width} hex digits");
        }
        let value = u32::from_str_radix(text, 16).expect("checked hex digits");
        if value > max {
            bail!("PCI address {address:?}: {name} {value:#x} is above {max:#x}");
        }
        Ok(value)
    };
    let segment = field(segment, 4, 0xffff, "segment")?;
    let bus = field(bus, 2, 0xff, "bus")?;
    let device = field(device, 2, 0x1f, "device")?;
    let function = field(function, 1, 0x7, "function")?;
    Ok(format!("{segment:04x}:{bus:02x}:{device:02x}.{function:x}"))
}

/// Canonical, de-duplicated-checked list of the devices a run asked for.
pub fn canonical_sbdfs(addresses: &[String]) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::with_capacity(addresses.len());
    for address in addresses {
        let sbdf = canonical_sbdf(address)?;
        if out.contains(&sbdf) {
            bail!("--gpu {sbdf} is given twice");
        }
        out.push(sbdf);
    }
    Ok(out)
}

/// What a passthrough device needs on the host, checked under `sysfs` and `dev`
/// (normally `/sys` and `/dev`): the device exists, it is bound to vfio-pci (not
/// to a host driver), it is not quarantined by a failed host scrub (records in
/// `gpu_state`, normally [`scrub::state_dir`]), and this process can open its
/// IOMMU group and the VFIO container. Returns the group's device node.
pub fn check_host_device(
    sysfs: &Path,
    dev: &Path,
    gpu_state: &Path,
    sbdf: &str,
) -> Result<PathBuf> {
    let device = sysfs.join("bus/pci/devices").join(sbdf);
    if !device.exists() {
        bail!(
            "no PCI device {sbdf} on this host (looked for {})",
            device.display()
        );
    }
    let driver = std::fs::read_link(device.join("driver"))
        .ok()
        .and_then(|target| target.file_name().map(|n| n.to_string_lossy().into_owned()));
    match driver.as_deref() {
        Some("vfio-pci") => {}
        Some(other) => bail!(
            "PCI device {sbdf} is bound to {other}, not vfio-pci. Hand it to VFIO first: \
             echo {sbdf} | sudo tee /sys/bus/pci/drivers/{other}/unbind; \
             echo vfio-pci | sudo tee {}/driver_override; \
             echo {sbdf} | sudo tee /sys/bus/pci/drivers_probe",
            device.display()
        ),
        None => bail!(
            "PCI device {sbdf} has no driver. Bind it to vfio-pci: sudo modprobe vfio-pci; \
             echo vfio-pci | sudo tee {}/driver_override; \
             echo {sbdf} | sudo tee /sys/bus/pci/drivers_probe",
            device.display()
        ),
    }
    scrub::check_not_quarantined(gpu_state, sbdf)?;
    let group = std::fs::read_link(device.join("iommu_group"))
        .with_context(|| {
            format!(
                "PCI device {sbdf} has no IOMMU group; the host IOMMU is off \
                 (x86: boot with intel_iommu=on or amd_iommu=on)"
            )
        })?
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .with_context(|| format!("PCI device {sbdf}: unreadable IOMMU group link"))?;
    let group_node = dev.join("vfio").join(&group);
    for node in [dev.join("vfio/vfio"), group_node.clone()] {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&node)
            .with_context(|| {
                format!(
                    "cannot open {} for PCI device {sbdf}; give this user access \
                     (e.g. sudo chown $USER {})",
                    node.display(),
                    node.display()
                )
            })?;
    }
    Ok(group_node)
}

/// Whether these Firecracker arguments turn on the PCI transport.
pub fn pci_transport_enabled(firecracker_args: Option<&str>) -> bool {
    firecracker_args
        .map(|args| {
            args.split_whitespace()
                .any(|arg| arg == PCI_FIRECRACKER_ARG)
        })
        .unwrap_or(false)
}

/// A kernel profile's Firecracker arguments must turn on the PCI transport.
pub fn require_pci_transport(firecracker_args: Option<&str>) -> Result<()> {
    if !pci_transport_enabled(firecracker_args) {
        bail!(
            "--gpu needs Firecracker's PCI transport ({PCI_FIRECRACKER_ARG}) and a Firecracker \
             that attaches passthrough devices on restore: use --kernel-profile gpu"
        );
    }
    Ok(())
}

/// Refuse, before `podman run` launches, a passthrough device the profile's NVIDIA
/// driver image cannot run: with `module_type = "proprietary"` only Turing and
/// Ampere GPUs ([`ModuleType::supports_device`]). Without this the restore would load the
/// driver, fail to initialise the GPU and shut the clone down. A device that is
/// not NVIDIA, or a profile without a driver image, is not checked here.
pub fn check_driver_supports_device(
    sysfs: &Path,
    sbdf: &str,
    profile_name: &str,
    module_type: Option<crate::setup::ModuleType>,
) -> Result<()> {
    let Some(module_type) = module_type else {
        return Ok(());
    };
    let device = sysfs.join("bus/pci/devices").join(sbdf);
    let read = |name: &str| -> Result<u16> {
        let file = device.join(name);
        let text = std::fs::read_to_string(&file)
            .with_context(|| format!("reading {}", file.display()))?;
        let text = text.trim();
        u16::from_str_radix(text.strip_prefix("0x").unwrap_or(text), 16)
            .with_context(|| format!("{} is not a 16-bit hex ID: {text:?}", file.display()))
    };
    if read("vendor")? != 0x10de {
        return Ok(());
    }
    let id = read("device")?;
    if !module_type.supports_device(id) {
        bail!(
            "PCI device {sbdf} [10de:{id:04x}] is not a Turing or Ampere GPU, and kernel profile \
             {profile_name:?} builds the proprietary NVIDIA modules with the GSP firmware off \
             (module_type = \"proprietary\"), which run only those. Set module_type = \"open\" \
             in [kernel_profiles.{profile_name}.<arch>.nvidia] to use this GPU"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_are_canonical_and_checked() {
        assert_eq!(canonical_sbdf("0000:01:00.0").unwrap(), "0000:01:00.0");
        assert_eq!(canonical_sbdf("01:00.0").unwrap(), "0000:01:00.0");
        assert_eq!(canonical_sbdf("0000:3B:1F.7").unwrap(), "0000:3b:1f.7");
        for bad in [
            "",
            "01:00",
            "0000:01:00:0.0",
            "0000:01:20.0",
            "0000:01:00.8",
            "000:01:00.0",
            "0000:1:00.0",
            "0000:zz:00.0",
            "/etc/passwd",
        ] {
            assert!(canonical_sbdf(bad).is_err(), "{bad:?} accepted");
        }
        assert!(canonical_sbdfs(&["01:00.0".into(), "0000:01:00.0".into()]).is_err());
    }

    fn fixture(driver: Option<&str>, group: Option<&str>) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        let device = root.path().join("sys/bus/pci/devices/0000:01:00.0");
        std::fs::create_dir_all(&device).unwrap();
        if let Some(driver) = driver {
            let target = root.path().join("sys/bus/pci/drivers").join(driver);
            std::fs::create_dir_all(&target).unwrap();
            std::os::unix::fs::symlink(&target, device.join("driver")).unwrap();
        }
        if let Some(group) = group {
            let target = root.path().join("sys/kernel/iommu_groups").join(group);
            std::fs::create_dir_all(&target).unwrap();
            std::os::unix::fs::symlink(&target, device.join("iommu_group")).unwrap();
            std::fs::create_dir_all(root.path().join("dev/vfio")).unwrap();
            std::fs::write(root.path().join("dev/vfio/vfio"), b"").unwrap();
            std::fs::write(root.path().join("dev/vfio").join(group), b"").unwrap();
        }
        root
    }

    fn check(root: &tempfile::TempDir) -> Result<PathBuf> {
        check_host_device(
            &root.path().join("sys"),
            &root.path().join("dev"),
            &root.path().join("gpu-state"),
            "0000:01:00.0",
        )
    }

    /// A GPU whose last host scrub failed is not handed to a VM; one whose
    /// scrub then succeeded is.
    #[test]
    fn a_quarantined_gpu_is_refused() {
        let root = fixture(Some("vfio-pci"), Some("7"));
        let state = root.path().join("gpu-state");
        scrub::record_outcome(&state, "0000:01:00.0", &Err("reset failed".into())).unwrap();
        let error = format!("{:#}", check(&root).unwrap_err());
        assert!(
            error.contains("quarantined") && error.contains("reset failed"),
            "{error}"
        );
        scrub::record_outcome(&state, "0000:01:00.0", &Ok(Default::default())).unwrap();
        assert!(check(&root).is_ok());
    }

    #[test]
    fn a_device_bound_to_vfio_pci_is_ready() {
        let root = fixture(Some("vfio-pci"), Some("7"));
        assert_eq!(check(&root).unwrap(), root.path().join("dev/vfio/7"));
    }

    #[test]
    fn a_device_on_a_host_driver_is_refused_with_the_rebind_steps() {
        let root = fixture(Some("nvidia"), Some("7"));
        let err = format!("{:#}", check(&root).unwrap_err());
        assert!(err.contains("bound to nvidia, not vfio-pci"), "{err}");
        assert!(err.contains("drivers/nvidia/unbind"), "{err}");
    }

    #[test]
    fn a_device_without_an_iommu_group_is_refused() {
        let root = fixture(Some("vfio-pci"), None);
        let err = format!("{:#}", check(&root).unwrap_err());
        assert!(err.contains("no IOMMU group"), "{err}");
    }

    #[test]
    fn a_missing_device_is_refused() {
        let root = fixture(Some("vfio-pci"), Some("7"));
        let err = check_host_device(
            &root.path().join("sys"),
            &root.path().join("dev"),
            &root.path().join("gpu-state"),
            "0000:02:00.0",
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("no PCI device 0000:02:00.0"));
    }

    fn nvidia_function(device: &str) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("bus/pci/devices/0000:01:00.0");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("vendor"), "0x10de\n").unwrap();
        std::fs::write(dir.join("device"), format!("{device}\n")).unwrap();
        root
    }

    /// The proprietary modules with the GSP firmware off run Turing and Ampere GPUs
    /// only, so a profile that builds them refuses any other NVIDIA GPU before
    /// launch, naming the device and the setting that runs it. The open modules,
    /// and a profile without a driver image, take any GPU.
    #[test]
    fn a_gpu_the_proprietary_modules_cannot_run_is_refused_before_launch() {
        use crate::setup::ModuleType;
        // Tesla T4 (Turing), A10G and A100 (Ampere): allowed.
        for device in ["0x1eb8", "0x2237", "0x20b5"] {
            let root = nvidia_function(device);
            check_driver_supports_device(
                root.path(),
                "0000:01:00.0",
                "gpu",
                Some(ModuleType::Proprietary),
            )
            .unwrap_or_else(|e| panic!("{device}: {e:#}"));
        }
        // V100 (Volta), H100 (Hopper), L4 (Ada), RTX 5090 (Blackwell): refused.
        for device in ["0x1db4", "0x2330", "0x27b8", "0x2b85"] {
            let root = nvidia_function(device);
            let err = check_driver_supports_device(
                root.path(),
                "0000:01:00.0",
                "gpu",
                Some(ModuleType::Proprietary),
            )
            .expect_err(device);
            let err = format!("{err:#}");
            assert!(err.contains(&format!("[10de:{}]", &device[2..])), "{err}");
            assert!(err.contains("module_type = \"open\""), "{err}");
            check_driver_supports_device(
                root.path(),
                "0000:01:00.0",
                "gpu",
                Some(ModuleType::Open),
            )
            .unwrap();
            check_driver_supports_device(root.path(), "0000:01:00.0", "gpu", None).unwrap();
        }
    }

    /// `podman run --gpu` checks against the profile's module type, which is the
    /// driver its clone gets: the snapshot key holds the driver image's path, and
    /// the image's key holds the module type. `snapshot run --gpu` restores the
    /// image recorded in the snapshot, which the current profile does not name, so
    /// it must not refuse a GPU on the profile's say (the guest's attach fails
    /// closed for a GPU the recorded driver cannot run).
    #[test]
    fn only_podman_run_checks_the_gpu_against_the_profiles_driver() {
        let call = "crate::gpu::check_driver_supports_device(";
        let code = |source: &'static str| source.split("#[cfg(test)]").next().unwrap();
        assert!(
            code(include_str!("../commands/podman/mod.rs")).contains(call),
            "podman run does not check that the driver image supports the GPU"
        );
        assert!(
            !code(include_str!("../commands/snapshot.rs")).contains(call),
            "snapshot run checks the GPU against the current profile, not the driver image \
             the snapshot recorded"
        );
    }

    #[test]
    fn the_profile_must_turn_on_pci() {
        assert!(require_pci_transport(Some("--enable-pci")).is_ok());
        assert!(require_pci_transport(Some("--enable-nv2 --enable-pci")).is_ok());
        assert!(require_pci_transport(Some("--enable-nv2")).is_err());
        assert!(require_pci_transport(Some("--enable-pcix")).is_err());
        assert!(require_pci_transport(None).is_err());
    }
}

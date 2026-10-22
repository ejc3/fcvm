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
/// to a host driver), and this process can open its IOMMU group and the VFIO
/// container. Returns the group's device node.
pub fn check_host_device(sysfs: &Path, dev: &Path, sbdf: &str) -> Result<PathBuf> {
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
            "0000:01:00.0",
        )
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
            "0000:02:00.0",
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("no PCI device 0000:02:00.0"));
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

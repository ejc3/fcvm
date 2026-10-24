//! The host's GPUs: NVIDIA display functions bound to vfio-pci, and whether any process
//! still holds one.

use anyhow::{bail, Context, Result};
use fleet_core::state::Gpu;
use std::path::Path;

const NVIDIA: &str = "0x10de";

fn gpu_name(device: &str) -> String {
    match device {
        "0x1eb8" => "Tesla T4".into(),
        other => format!("NVIDIA 10de:{}", other.trim_start_matches("0x")),
    }
}

/// NVIDIA display-class functions bound to vfio-pci, in address order.
pub fn vfio_gpus(sysfs: &Path) -> Result<Vec<Gpu>> {
    let dir = sysfs.join("bus/pci/drivers/vfio-pci");
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    let mut gpus = Vec::new();
    for entry in entries {
        let entry = entry?;
        let sbdf = entry.file_name().to_string_lossy().to_string();
        // Device links look like 0000:01:00.0; the driver directory also holds bind,
        // unbind, new_id and the module link.
        if sbdf.len() != 12 || sbdf.as_bytes()[4] != b':' {
            continue;
        }
        let dev = sysfs.join("bus/pci/devices").join(&sbdf);
        let read = |f: &str| std::fs::read_to_string(dev.join(f)).map(|s| s.trim().to_string());
        let (Ok(vendor), Ok(class)) = (read("vendor"), read("class")) else {
            continue;
        };
        if vendor != NVIDIA || !class.starts_with("0x03") {
            continue;
        }
        let device = read("device").unwrap_or_default();
        gpus.push(Gpu {
            name: gpu_name(&device),
            sbdf,
        });
    }
    gpus.sort_by(|a, b| a.sbdf.cmp(&b.sbdf));
    Ok(gpus)
}

/// `--gpus <sbdf>=<name>,...`
pub fn parse_gpus(spec: &str) -> Result<Vec<Gpu>> {
    let mut out: Vec<Gpu> = Vec::new();
    for item in spec.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let (sbdf, name) = item.split_once('=').unwrap_or((item, "GPU"));
        if out.iter().any(|g| g.sbdf == sbdf) {
            bail!("--gpus names {sbdf} twice");
        }
        out.push(Gpu {
            sbdf: sbdf.to_string(),
            name: name.to_string(),
        });
    }
    if out.is_empty() {
        bail!("--gpus names no GPU");
    }
    Ok(out)
}

/// The `/dev/vfio/<group>` node of a PCI function, or `None` when it has no IOMMU group
/// (not a passthrough device, or a test host).
pub fn vfio_node(sysfs: &Path, sbdf: &str) -> Option<std::path::PathBuf> {
    let link =
        std::fs::read_link(sysfs.join("bus/pci/devices").join(sbdf).join("iommu_group")).ok()?;
    let group = link.file_name()?.to_string_lossy().to_string();
    Some(Path::new("/dev/vfio").join(group))
}

/// Process ids holding `node` open, from `/proc/<pid>/fd` links. Only links are read
/// (never `cmdline`, which faults the target's memory), and processes this user cannot
/// inspect are skipped.
pub fn holders(node: &Path) -> Vec<u32> {
    let mut out = Vec::new();
    let Ok(procs) = std::fs::read_dir("/proc") else {
        return out;
    };
    for p in procs.flatten() {
        let Ok(pid) = p.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(fds) = std::fs::read_dir(p.path().join("fd")) else {
            continue;
        };
        if fds
            .flatten()
            .any(|fd| std::fs::read_link(fd.path()).is_ok_and(|t| t == node))
        {
            out.push(pid);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inventory_lists_nvidia_display_functions_bound_to_vfio() {
        let root = tempfile::tempdir().unwrap();
        let sys = root.path();
        let drv = sys.join("bus/pci/drivers/vfio-pci");
        std::fs::create_dir_all(&drv).unwrap();
        for (sbdf, vendor, class, device) in [
            ("0000:02:00.0", "0x10de", "0x030200", "0x1eb8"),
            ("0000:01:00.0", "0x10de", "0x030000", "0x2236"),
            ("0000:03:00.0", "0x10de", "0x040300", "0x10f8"), // audio function
            ("0000:04:00.0", "0x8086", "0x030000", "0x1234"),
        ] {
            let dev = sys.join("bus/pci/devices").join(sbdf);
            std::fs::create_dir_all(&dev).unwrap();
            std::fs::write(dev.join("vendor"), format!("{vendor}\n")).unwrap();
            std::fs::write(dev.join("class"), format!("{class}\n")).unwrap();
            std::fs::write(dev.join("device"), format!("{device}\n")).unwrap();
            std::os::unix::fs::symlink(&dev, drv.join(sbdf)).unwrap();
        }
        std::fs::write(drv.join("bind"), "").unwrap();
        let gpus = vfio_gpus(sys).unwrap();
        assert_eq!(
            gpus,
            vec![
                Gpu {
                    sbdf: "0000:01:00.0".into(),
                    name: "NVIDIA 10de:2236".into()
                },
                Gpu {
                    sbdf: "0000:02:00.0".into(),
                    name: "Tesla T4".into()
                },
            ]
        );
        assert!(vfio_gpus(&sys.join("missing")).unwrap().is_empty());
    }

    #[test]
    fn gpus_flag_parses_and_refuses_repeats() {
        let g = parse_gpus("0000:01:00.0=Tesla T4, 0000:02:00.0").unwrap();
        assert_eq!(g[1].name, "GPU");
        assert!(parse_gpus("a=x,a=y").is_err());
        assert!(parse_gpus(" , ").is_err());
    }

    #[test]
    fn holders_finds_this_process_holding_a_file() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let path = f.path().canonicalize().unwrap();
        let _open = std::fs::File::open(&path).unwrap();
        assert!(holders(&path).contains(&std::process::id()));
    }
}

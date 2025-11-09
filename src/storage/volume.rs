use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Volume mount configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeMount {
    pub host_path: PathBuf,
    pub guest_path: PathBuf,
    pub read_only: bool,
}

impl VolumeMount {
    /// Parse volume mount from string: HOST:GUEST[:ro]
    pub fn parse(s: &str) -> Result<Self> {
        let parts: Vec<&str> = s.split(':').collect();

        if parts.len() < 2 {
            anyhow::bail!("invalid volume mount format: {}", s);
        }

        let host_path = PathBuf::from(parts[0]);
        let guest_path = PathBuf::from(parts[1]);
        let read_only = parts.get(2).map(|&s| s == "ro").unwrap_or(false);

        Ok(Self {
            host_path,
            guest_path,
            read_only,
        })
    }

    /// Convert to Firecracker drive configuration
    pub fn to_drive_config(&self, drive_id: String) -> crate::firecracker::api::Drive {
        crate::firecracker::api::Drive {
            drive_id: drive_id.clone(),
            path_on_host: self.host_path.display().to_string(),
            is_root_device: false,
            is_read_only: self.read_only,
            partuuid: None,
            rate_limiter: None,
        }
    }
}

/// Manages volume mounts for VMs
pub struct VolumeManager {
    mounts: Vec<VolumeMount>,
}

impl VolumeManager {
    pub fn new(mounts: Vec<VolumeMount>) -> Self {
        Self { mounts }
    }

    /// Parse volume mounts from CLI arguments
    pub fn from_args(mount_args: Vec<String>) -> Result<Self> {
        let mut mounts = Vec::new();

        for arg in mount_args {
            let mount = VolumeMount::parse(&arg)
                .with_context(|| format!("parsing volume mount: {}", arg))?;
            mounts.push(mount);
        }

        Ok(Self { mounts })
    }

    /// Get all mounts
    pub fn mounts(&self) -> &[VolumeMount] {
        &self.mounts
    }

    /// Validate that host paths exist
    pub fn validate(&self) -> Result<()> {
        for mount in &self.mounts {
            if !mount.host_path.exists() {
                anyhow::bail!("host path does not exist: {}", mount.host_path.display());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_volume_mount() {
        let mount = VolumeMount::parse("/host/data:/data").unwrap();
        assert_eq!(mount.host_path, PathBuf::from("/host/data"));
        assert_eq!(mount.guest_path, PathBuf::from("/data"));
        assert!(!mount.read_only);

        let mount = VolumeMount::parse("/host/data:/data:ro").unwrap();
        assert!(mount.read_only);
    }
}

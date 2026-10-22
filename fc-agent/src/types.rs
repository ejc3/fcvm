use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Deserialize)]
pub struct Plan {
    pub image: String,
    #[serde(default)]
    pub env: HashMap<String, String>,
    pub cmd: Option<Vec<String>>,
    #[serde(default)]
    pub volumes: Vec<VolumeMount>,
    #[serde(default)]
    pub extra_disks: Vec<ExtraDiskMount>,
    #[serde(default)]
    pub nfs_mounts: Vec<NfsMount>,
    /// Read-only virtio-pmem images, mounted with DAX
    #[serde(default)]
    pub pmem_mounts: Vec<PmemMount>,
    /// Device path for localhost image (e.g., "/dev/vdb")
    #[serde(default)]
    pub image_device: Option<String>,
    /// Image delivery mode: "overlay", "btrfs", or "archive"
    #[serde(default)]
    pub image_mode: Option<String>,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub forward_localhost: Vec<String>,
    /// Guest TCP ports the host publishes. Each is DNAT'd to 127.0.0.1 so a
    /// service bound to guest loopback ONLY is still reachable — see
    /// `network::publish_to_loopback`.
    #[serde(default)]
    pub published_guest_ports: Vec<String>,
    #[serde(default)]
    pub privileged: bool,
    #[serde(default)]
    pub interactive: bool,
    #[serde(default)]
    pub tty: bool,
    #[serde(default)]
    pub http_proxy: Option<String>,
    #[serde(default)]
    pub https_proxy: Option<String>,
    #[serde(default)]
    pub no_proxy: Option<String>,
    /// Host user's subordinate UID range start (from /etc/subuid).
    /// When set, the VM user is created with the same subuid range so that
    /// the storage image (built by rootless podman on the host) has valid UIDs.
    #[serde(default)]
    pub subuid_start: Option<u64>,
    #[serde(default)]
    pub subuid_count: Option<u64>,
    /// Use non-blocking output: drop container stdout/stderr when the output
    /// channel is full instead of blocking. Prevents backpressure from
    /// deadlocking FUSE-based services in the container.
    #[serde(default)]
    pub non_blocking_output: bool,
    /// Enable vsock egress proxy: transparent TCP proxy over vsock bypassing
    /// the TAP/bridge/pasta data path for outbound connections.
    #[serde(default)]
    pub egress_proxy: bool,
    /// The host's NTP servers, already resolved to addresses by fcvm. Configured
    /// into the guest's chronyd at boot; empty means the host had none to give.
    #[serde(default)]
    pub ntp_servers: Vec<String>,
    /// The NVIDIA driver image (src/setup/nvidia.rs), attached read-only.
    /// Mounted at boot at `gpu::DRIVER_ROOT`, so a snapshot carries the mount
    /// and a GPU can be attached to any clone of it.
    #[serde(default)]
    pub gpu_driver: Option<GpuDriver>,
    /// This run's GPUs are attached by the host's restore of the pre-start
    /// snapshot, so a VM that was not restored must not launch the container
    /// (`gpu::check_gpu_restore`). Read only before the cache handshake, so the
    /// value a snapshot captures never applies to its restores.
    #[serde(default)]
    pub await_gpu_restore: bool,
}

/// The block device holding the NVIDIA driver image.
#[derive(Debug, Clone, Deserialize)]
pub struct GpuDriver {
    pub device: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct VolumeMount {
    pub guest_path: String,
    pub vsock_port: u32,
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExtraDiskMount {
    pub device: String,
    pub mount_path: String,
    #[serde(default)]
    pub read_only: bool,
}

/// A read-only virtio-pmem device (`/dev/pmemN`) holding an ext4 image.
#[derive(Debug, Clone, Deserialize)]
pub struct PmemMount {
    pub device: String,
    pub mount_path: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NfsMount {
    pub host_ip: String,
    pub host_path: String,
    pub mount_path: String,
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Debug, Deserialize)]
pub struct LatestMetadata {
    #[serde(rename = "host-time")]
    pub host_time: String,
    #[serde(rename = "restore-epoch")]
    pub restore_epoch: Option<String>,
    /// For routed mode clones: unique IPv6 to replace the snapshot's shared guest IPv6.
    #[serde(rename = "clone-ipv6", default)]
    pub clone_ipv6: Option<String>,
    /// The command to run inside the container before this restore is published
    /// (`--on-restore`). See `snapshot_network::RestoreHook`.
    #[serde(rename = "restore-hook", default)]
    pub restore_hook: Option<String>,
    /// GPUs the host attached to this clone before resuming it, as host PCI
    /// addresses. Non-empty means: rescan, load the driver and write the CDI
    /// spec before this restore is acknowledged (`gpu::attach`).
    #[serde(default)]
    pub gpus: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plan_minimal() {
        let json = r#"{"image": "alpine:latest"}"#;
        let plan: Plan = serde_json::from_str(json).unwrap();
        assert_eq!(plan.image, "alpine:latest");
        assert!(plan.volumes.is_empty());
        assert!(plan.cmd.is_none());
        assert!(!plan.tty);
        assert!(!plan.privileged);
    }

    #[test]
    fn test_plan_full() {
        let json = r#"{
            "image": "nginx:alpine",
            "env": {"FOO": "bar"},
            "cmd": ["echo", "hello"],
            "volumes": [{"guest_path": "/mnt/data", "vsock_port": 5000}],
            "extra_disks": [{"device": "/dev/vdb", "mount_path": "/mnt/disk"}],
            "tty": true,
            "privileged": true,
            "user": "1000:1000",
            "http_proxy": "http://proxy:8080"
        }"#;
        let plan: Plan = serde_json::from_str(json).unwrap();
        assert_eq!(plan.image, "nginx:alpine");
        assert_eq!(plan.env["FOO"], "bar");
        assert_eq!(plan.cmd.as_ref().unwrap(), &["echo", "hello"]);
        assert_eq!(plan.volumes.len(), 1);
        assert_eq!(plan.volumes[0].vsock_port, 5000);
        assert!(!plan.volumes[0].read_only);
        assert!(plan.tty);
        assert!(plan.privileged);
    }

    #[test]
    fn test_plan_pmem_mounts() {
        let plan: Plan = serde_json::from_str(
            r#"{"image": "alpine:latest",
                "pmem_mounts": [{"device": "/dev/pmem0", "mount_path": "/mnt/cache"}]}"#,
        )
        .unwrap();
        assert_eq!(plan.pmem_mounts.len(), 1);
        assert_eq!(plan.pmem_mounts[0].device, "/dev/pmem0");
        assert_eq!(plan.pmem_mounts[0].mount_path, "/mnt/cache");

        let bare: Plan = serde_json::from_str(r#"{"image": "alpine:latest"}"#).unwrap();
        assert!(bare.pmem_mounts.is_empty());
    }

    /// The NTP list rides in the boot plan because nothing mounts the host's /etc
    /// into a guest. A plan without the field must still parse (older/minimal plans).
    #[test]
    fn test_plan_ntp_servers() {
        let plan: Plan = serde_json::from_str(
            r#"{"image": "alpine:latest", "ntp_servers": ["10.0.0.1", "fd00::1"]}"#,
        )
        .unwrap();
        assert_eq!(plan.ntp_servers, vec!["10.0.0.1", "fd00::1"]);

        let bare: Plan = serde_json::from_str(r#"{"image": "alpine:latest"}"#).unwrap();
        assert!(bare.ntp_servers.is_empty());
    }

    #[test]
    fn test_plan_with_overlay_image() {
        let json = r#"{
            "image": "localhost/myapp",
            "image_device": "/dev/vdb",
            "image_mode": "overlay"
        }"#;
        let plan: Plan = serde_json::from_str(json).unwrap();
        assert_eq!(plan.image, "localhost/myapp");
        assert_eq!(plan.image_device.as_deref(), Some("/dev/vdb"));
        assert_eq!(plan.image_mode.as_deref(), Some("overlay"));
    }

    #[test]
    fn test_plan_with_btrfs_image() {
        let json = r#"{
            "image": "localhost/myapp",
            "image_device": "/dev/vdb",
            "image_mode": "btrfs"
        }"#;
        let plan: Plan = serde_json::from_str(json).unwrap();
        assert_eq!(plan.image_device.as_deref(), Some("/dev/vdb"));
        assert_eq!(plan.image_mode.as_deref(), Some("btrfs"));
    }

    #[test]
    fn test_plan_with_archive_image() {
        let json = r#"{
            "image": "localhost/myapp",
            "image_device": "/dev/vdb",
            "image_mode": "archive"
        }"#;
        let plan: Plan = serde_json::from_str(json).unwrap();
        assert_eq!(plan.image_device.as_deref(), Some("/dev/vdb"));
        assert_eq!(plan.image_mode.as_deref(), Some("archive"));
    }

    #[test]
    fn test_latest_metadata() {
        let json = r#"{"host-time": "1731301800", "restore-epoch": "abc123"}"#;
        let meta: LatestMetadata = serde_json::from_str(json).unwrap();
        assert_eq!(meta.host_time, "1731301800");
        assert_eq!(meta.restore_epoch.as_deref(), Some("abc123"));
    }

    #[test]
    fn gpu_fields_parse_and_default_to_none() {
        let plan: Plan = serde_json::from_str(
            r#"{"image": "alpine:latest", "gpu_driver": {"device": "/dev/vdc"}}"#,
        )
        .unwrap();
        assert_eq!(plan.gpu_driver.unwrap().device, "/dev/vdc");
        let bare: Plan = serde_json::from_str(r#"{"image": "alpine:latest"}"#).unwrap();
        assert!(bare.gpu_driver.is_none());

        let meta: LatestMetadata = serde_json::from_str(
            r#"{"host-time": "1", "restore-epoch": "e", "gpus": ["0000:01:00.0", "0000:02:00.0"]}"#,
        )
        .unwrap();
        assert_eq!(meta.gpus.len(), 2);
        let meta: LatestMetadata = serde_json::from_str(r#"{"host-time": "1"}"#).unwrap();
        assert!(meta.gpus.is_empty());
    }

    #[test]
    fn test_latest_metadata_no_epoch() {
        let json = r#"{"host-time": "1731301800"}"#;
        let meta: LatestMetadata = serde_json::from_str(json).unwrap();
        assert!(meta.restore_epoch.is_none());
    }
}

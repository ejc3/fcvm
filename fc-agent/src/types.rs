use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// How the guest handles podman storage on boot. Defaults to `Provision`
/// (today's behavior: wipe + rebuild). A disk-only clone sets `Preserve` to keep
/// the captured store. See docs/disk-only-clone.html.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoragePolicy {
    #[default]
    Provision,
    Preserve,
}

/// How the guest brings up the container on boot. Defaults to `RunNew` (today's
/// `podman run`). A disk-only clone uses `StartCaptured` (replay the captured
/// container) or `FreshContainer` (new container, keep image + volumes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContainerStatePolicy {
    #[default]
    RunNew,
    StartCaptured,
    FreshContainer,
}

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
    /// Disk-only clone: how to treat captured podman storage. Default `Provision`
    /// (today's wipe + rebuild). See docs/disk-only-clone.html.
    #[serde(default)]
    pub storage_policy: StoragePolicy,
    /// Disk-only clone: how to bring up the container. Default `RunNew` (today's
    /// `podman run`).
    #[serde(default)]
    pub container_state_policy: ContainerStatePolicy,
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
}

#[derive(Debug, Deserialize)]
pub struct ExecRequest {
    pub command: Vec<String>,
    #[serde(default)]
    pub in_container: bool,
    #[serde(default)]
    pub interactive: bool,
    #[serde(default)]
    pub tty: bool,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", content = "data")]
pub enum ExecResponse {
    #[serde(rename = "stdout")]
    Stdout(String),
    #[serde(rename = "stderr")]
    Stderr(String),
    #[serde(rename = "exit")]
    Exit(i32),
    #[serde(rename = "error")]
    Error(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plan_policies_default_to_today_behavior() {
        // Every existing plan has no policy fields; they must deserialize to the
        // current behavior so disk-only is purely additive.
        let plan: Plan = serde_json::from_str(r#"{ "image": "alpine" }"#).unwrap();
        assert_eq!(plan.storage_policy, StoragePolicy::Provision);
        assert_eq!(plan.container_state_policy, ContainerStatePolicy::RunNew);
    }

    #[test]
    fn test_plan_disk_only_policies_parse() {
        let json = r#"{
            "image": "alpine",
            "storage_policy": "preserve",
            "container_state_policy": "start_captured"
        }"#;
        let plan: Plan = serde_json::from_str(json).unwrap();
        assert_eq!(plan.storage_policy, StoragePolicy::Preserve);
        assert_eq!(plan.container_state_policy, ContainerStatePolicy::StartCaptured);
    }

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
    fn test_latest_metadata_no_epoch() {
        let json = r#"{"host-time": "1731301800"}"#;
        let meta: LatestMetadata = serde_json::from_str(json).unwrap();
        assert!(meta.restore_epoch.is_none());
    }

    #[test]
    fn test_exec_response_serialization() {
        let resp = ExecResponse::Exit(0);
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"type\":\"exit\""));
        assert!(json.contains("\"data\":0"));
    }
}

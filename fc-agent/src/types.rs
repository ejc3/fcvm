use serde::{Deserialize, Serialize};
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
    #[serde(default)]
    pub image_archive: Option<String>,
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

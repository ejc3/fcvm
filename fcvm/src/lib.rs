pub mod firecracker;
pub mod network;
pub mod storage;
pub mod readiness;
pub mod state;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum Mode { Auto, Privileged, Rootless }

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum MapMode { Block, Sshfs, Nfs }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Proto { Tcp, Udp }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Publish {
    pub host_ip: Option<String>,
    pub host_port: u16,
    pub guest_port: u16,
    pub proto: Proto,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadyGate {
    pub mode: String,         // "vsock" | "http" | "log" | "exec"
    pub arg: Option<String>,  // url | pattern | cmd
}

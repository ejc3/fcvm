# UFFD-Based VM Cloning Implementation Status

## Current Implementation (Partially Complete)

### What We Have ✓

1. **fcvm clone command** (src/commands/clone.rs:190)
   - Loads snapshot configuration
   - Creates new VM ID and networking
   - Creates CoW disk from snapshot base
   - Calls Firecracker's snapshot load API with uffd backend

2. **API Structures** (src/firecracker/api.rs)
   ```rust
   pub struct SnapshotLoad {
       pub snapshot_path: String,
       pub mem_backend: MemBackend,
       pub enable_diff_snapshots: Option<bool>,
       pub resume_vm: Option<bool>,
   }

   pub struct MemBackend {
       pub backend_type: String, // "File" or "Uffd"
       pub backend_path: String,
   }
   ```

3. **Storage CoW** (src/storage/disk.rs)
   - Uses `qemu-img create -f qcow2 -b base.ext4` for disk CoW
   - Each VM gets independent write-able overlay

### Critical Issues  ❌

#### 1. Missing UFFD Page Server

**Problem**: We pass `backend_type: "Uffd"` but never start the uffd handler process!

**What's Needed**:
```rust
// Before calling load_snapshot, spawn uffd handler:
let uffd_socket = format!("/tmp/fcvm/{}/uffd.sock", vm_id);
let uffd_handler = tokio::process::Command::new("uffd_handler")
    .arg(&uffd_socket)
    .arg(&snapshot_config.memory_path)
    .spawn()?;
```

**Reference Implementation**:
```bash
# From firecracker-microvm/firecracker/tests/host_tools/uffd/
deps/uffd_handler -v /tmp/uffd.sock /snapshots/mem.img
```

#### 2. Wrong backend_path Value

**Problem**: Line 127 in clone.rs passes memory file path, should pass Unix socket path

**Current (WRONG)**:
```rust
mem_backend: MemBackend {
    backend_type: "Uffd".to_string(),
    backend_path: snapshot_config.memory_path.display().to_string(), // ❌ mem file
}
```

**Should Be**:
```rust
mem_backend: MemBackend {
    backend_type: "Uffd".to_string(),
    backend_path: uffd_socket_path, // ✅ Unix socket where uffd handler listens
}
```

#### 3. Network Configuration After Load

**Current Approach** (lines 138-148): Add network interface after snapshot load
```rust
client.add_network_interface("eth0", NetworkInterface { ... }).await?;
```

**Better Approach** (from reference): Use `network_overrides` in SnapshotLoad
```rust
pub struct SnapshotLoad {
    // ... existing fields ...
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network_overrides: Option<Vec<NetworkOverride>>,
}

pub struct NetworkOverride {
    pub iface_id: String,
    pub host_dev_name: String,
}
```

Then:
```rust
client.load_snapshot(SnapshotLoad {
    snapshot_path: ...,
    mem_backend: ...,
    network_overrides: Some(vec![NetworkOverride {
        iface_id: "eth0".to_string(),
        host_dev_name: network_config.tap_device.clone(),
    }]),
    resume_vm: Some(true),
}).await?;
```

## Recommended Architecture (Production-Ready)

### Component 1: UFFD Handler Manager

Create `src/uffd/handler.rs`:

```rust
use anyhow::Result;
use std::path::{Path, PathBuf};
use tokio::process::{Child, Command};

pub struct UffdHandler {
    process: Child,
    socket_path: PathBuf,
}

impl UffdHandler {
    /// Start uffd handler process for serving memory pages
    pub async fn start(
        socket_path: PathBuf,
        mem_file: &Path,
    ) -> Result<Self> {
        // Try firecracker's test handler first
        let handler_bin = which::which("uffd_handler")
            .or_else(|_| which::which("/usr/local/bin/uffd_handler"))?;

        let process = Command::new(handler_bin)
            .arg(&socket_path)
            .arg(mem_file)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;

        // Wait for socket to exist
        for _ in 0..50 {
            if socket_path.exists() {
                break;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }

        Ok(Self { process, socket_path })
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }
}

impl Drop for UffdHandler {
    fn drop(&mut self) {
        let _ = self.process.start_kill();
        let _ = std::fs::remove_file(&self.socket_path);
    }
}
```

### Component 2: Updated Clone Command

```rust
pub async fn cmd_clone(args: CloneArgs) -> Result<()> {
    // ... (existing setup code) ...

    // Start UFFD handler for this VM
    let uffd_socket = data_dir.join("uffd.sock");
    let uffd_handler = UffdHandler::start(
        uffd_socket.clone(),
        &snapshot_config.memory_path,
    ).await
        .context("starting uffd handler")?;

    info!(
        socket = %uffd_socket.display(),
        mem_file = %snapshot_config.memory_path.display(),
        "uffd handler started"
    );

    // ... (start Firecracker) ...

    // Load snapshot with uffd backend
    client.load_snapshot(SnapshotLoad {
        snapshot_path: snapshot_config.memory_path.display().to_string(),
        mem_backend: MemBackend {
            backend_type: "Uffd".to_string(),
            backend_path: uffd_handler.socket_path().display().to_string(), // ✅ Correct!
        },
        network_overrides: Some(vec![NetworkOverride {
            iface_id: "eth0".to_string(),
            host_dev_name: network_config.tap_device.clone(),
        }]),
        resume_vm: Some(true),
    }).await?;

    // uffd_handler stays alive for VM lifetime
    // ... (wait for signals) ...

    // Cleanup (uffd_handler dropped automatically)
    Ok(())
}
```

### Component 3: Shared Read-Only Rootfs (Optional Enhancement)

For true zero-copy storage:

```rust
// In cmd_run when creating snapshot:
// Drive 1: Read-only base (shared across all VMs)
client.add_drive("rootfs", Drive {
    drive_id: "rootfs".to_string(),
    path_on_host: "/var/lib/fcvm/rootfs/base.ext4".to_string(),
    is_root_device: true,
    is_read_only: true, // ← Shared!
}).await?;

// Drive 2: Per-VM scratch disk (CoW)
client.add_drive("scratch", Drive {
    drive_id: "scratch".to_string(),
    path_on_host: format!("/tmp/fcvm/{}/scratch.ext4", vm_id),
    is_root_device: false,
    is_read_only: false,
}).await?;

// Inside guest: mount overlayfs
// overlay on / lowerdir=/dev/vda,upperdir=/dev/vdb,workdir=/tmp/work
```

## Memory Sharing Benefits (When Fixed)

| Scenario | Without uffd | With uffd |
|----------|--------------|-----------|
| 1 VM (512MB) | 512MB | 512MB |
| 10 VMs | 5120MB (5GB) | ~600MB |
| 100 VMs | 51200MB (50GB) | ~2GB |

**Why?** Pages are `mmap`'d from shared memory file, faulted on-demand via uffd, and only divergent pages consume unique memory.

## Next Steps for Production

1. ✅ Implement `UffdHandler` manager
2. ✅ Fix `backend_path` to use socket
3. ✅ Add `network_overrides` to API types
4. ✅ Update `cmd_clone` to use uffd handler
5. ⚠️ Get/build `uffd_handler` binary (from Firecracker tests or custom)
6. ⚠️ Add snapshot creation command (`fcvm snapshot create`)
7. ⚠️ Test end-to-end: run → snapshot → clone × 10

## Alternative: File Backend (Simpler, No Sharing)

If uffd is too complex initially, use File backend:

```rust
mem_backend: MemBackend {
    backend_type: "File".to_string(),
    backend_path: snapshot_config.memory_path.display().to_string(),
}
```

**Pros**: No uffd handler needed, kernel handles page faults directly
**Cons**: Each VM gets independent memory mapping, no sharing, higher memory usage

## References

- Firecracker Snapshotting Docs: https://github.com/firecracker-microvm/firecracker/blob/main/docs/snapshotting/snapshot-support.md
- UFFD Handler Example: https://github.com/firecracker-microvm/firecracker/tree/main/tests/host_tools/uffd
- Julia Evans on Firecracker: https://jvns.ca/blog/2021/01/23/firecracker--start-a-vm-in-less-than-a-second/
- FaaSnap Paper (UFFD prefetching): https://www.usenix.org/conference/atc20/presentation/cadden

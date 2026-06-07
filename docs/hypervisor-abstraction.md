# Design: hypervisor-agnostic abstraction (Firecracker + Cloud Hypervisor)

Status: **proposal / RFC** (epic: #632). Canonical design doc; #632 holds the full proven
capability mapping with source citations.

**Scope: the two microVMs — Firecracker and Cloud Hypervisor.** QEMU is explicitly out of
scope (it was the outlier on every hard axis: no host vsock UDS proxy, QMP vs REST, a much
larger attack surface, `microvm` x86-only, ~3× boot / ~2× RSS). Restricting to the two lean
Rust/KVM microVMs makes the abstraction small and clean.

## Goal
Make fcvm's VMM **pluggable** behind one `Hypervisor` trait so the same features work on
**Firecracker** (today) and **Cloud Hypervisor**. Backends declare **capabilities**; the
orchestration layer degrades gracefully where a backend lacks one. No behavior change for
the Firecracker path.

## Why this pair is a clean fit
The research proved FC and CH are closely aligned:
- **vsock is byte-for-byte identical** — both use the hybrid `CONNECT <port>` proxy over a
  host Unix socket, guest→host via CID 2 with the host listening on `<socket>_<port>` (CH:
  "based on the Firecracker implementation"). fcvm's exec/volume/status/tty layer ports
  **unchanged** — and there's effectively **one `GuestChannel` impl**, not two.
- Both are **REST-over-UDS** control planes (FC `/actions`,`PATCH /vm`; CH `vm.boot`,
  `vm.pause`…), so the API client shape is shared.
- Both support **direct kernel boot**, **UFFD lazy restore**, **static single binaries**,
  and **seccomp** — similar security posture (no QEMU large-surface concern).

## Current Firecracker coupling (~5k LOC / ~17%)
`src/firecracker/` (~1.7k LOC: api/config/vm) is the trait boundary; snapshot/restore lives
in `src/commands/common.rs` (~2.3k LOC); the UFFD server in `src/uffd/` (~0.9k LOC); guest
comms = vsock ports + **MMDS** (the boot plan to fc-agent).

## Capability mapping (FC vs CH) — load-bearing conclusions
Full table + citations in #632.

| capability | Firecracker | Cloud Hypervisor | abstraction handling |
|---|---|---|---|
| lifecycle / boot / machine / block+net add / console / process model | ✅ | ✅ | trait covers directly |
| vsock host↔guest (`CONNECT`) | ✅ | ✅ identical | single `GuestChannel` (UdsConnect) |
| full snapshot + restore-into-fresh-process | ✅ | ✅ | trait |
| UFFD lazy restore | ✅ | ✅ (`memory_restore_mode=ondemand`, v52) | trait |
| **cross-clone memory sharing** | ✅ (`File` backend mmaps `memory.bin` MAP_PRIVATE ⇒ CoW) | ⚠️ CoW via `--memory file=`,`shared=off` (MAP_PRIVATE); restore-path wiring to verify | cap `shared_memory_clones`; it's **CoW, not UFFD-exclusive** |
| diff / incremental snapshots | ✅ | ❌ | cap `diff_snapshots` (FC-only; CH takes full) |
| drive retarget on restore (`patch_drive`) | ✅ | ❌ | use **bind-mount redirect** (already VMM-agnostic) everywhere |
| metadata service (boot plan) | ✅ MMDS | ❌ no MMDS | **boot-plan over vsock** (portable; keep MMDS as FC fast path) |
| nested ARM64 (FEAT_NV2) | ✅ (custom fork + DSB kernel patches) | ❌ (CH nested is x86-only) | cap `nested_arm64` (FC-only) |

Net: only **five** capability gates separate the two (memory-share verification, diff
snapshots, drive retarget, native metadata, ARM64 nesting) — everything else is shared.
`uffd_lazy_restore` is a capability (both support it) to future-proof the trait if a
backend without UFFD is ever added.

## Proposed abstraction
Move `src/firecracker/` → `src/hypervisor/{firecracker,cloud_hypervisor}/`.

```rust
pub enum Backend { Firecracker, CloudHypervisor }

pub struct Capabilities {
    pub diff_snapshots: bool,          // FC: true,  CH: false
    pub shared_memory_clones: bool,    // FC: true,  CH: verify (CoW file backing)
    pub uffd_lazy_restore: bool,       // both: true
    pub drive_retarget: bool,          // FC: true,  CH: false (bind-mount redirect)
    pub native_metadata_service: bool, // FC: MMDS,  CH: false (vsock boot-plan)
    pub nested_arm64: bool,            // FC: true,  CH: false
}

#[async_trait]
pub trait Hypervisor: Send + Sync {
    fn backend(&self) -> Backend;
    fn capabilities(&self) -> Capabilities;
    async fn configure(&mut self, spec: &VmSpec) -> Result<()>;
    async fn start(&mut self) -> Result<()>;
    async fn pause(&self) -> Result<()>;
    async fn resume(&self) -> Result<()>;
    async fn snapshot(&self, out: &Path, kind: SnapshotKind) -> Result<()>;
    async fn restore(&mut self, spec: &VmSpec, src: &RestoreSource) -> Result<()>;
    async fn kill(&mut self) -> Result<()>;
    fn guest_channel(&self) -> &dyn GuestChannel; // UdsConnect for both
}

pub trait GuestChannel { fn connect(&self, port: u32) -> Result<UnixStream>; fn host_listen_path(&self, port: u32) -> PathBuf; }
```

`VmSpec`/`RestoreSource` are VMM-neutral; each backend translates to its REST API.

## Hard problems & resolution
1. **Boot plan (MMDS → vsock).** Add a boot-plan vsock channel fc-agent reads at boot
   (required for CH; keep MMDS as the FC fast path). fc-agent is a second porting surface
   (also: vsock CID convention, the restore-reset event — CH also resets vsock on restore).
2. **Drive retarget.** Make the **bind-mount namespace redirect** primary (VMM-agnostic);
   `patch_drive` becomes an FC optimization.
3. **Memory-share clones.** It's CoW of MAP_PRIVATE file-backed guest RAM, not UFFD-only.
   FC's `File` backend gives it directly; **verify CH's restore can back guest RAM with a
   MAP_PRIVATE mapping of the shared snapshot** (else CH clones fall back to per-clone
   `ondemand`/copy — correct but less dense).
4. **Diff snapshots / ARM64 nesting / native metadata.** Capability-gated to Firecracker.
5. **Snapshot format/version is per-VMM** — the cache key must encode
   `(backend, binary, version)` and refuse cross-backend/version restores.

## Why not adopt Kata's trait
Kata's `Hypervisor` trait is hotplug-first and coupled to kata-agent/containerd-shim with
**no snapshot/clone/memory-sharing** — opposite emphasis. Borrow the pattern + `Capabilities`
idea + their FC/CH backend code as reference; write fcvm's own snapshot/clone-first trait.

## Risks / further angles (full list in #632)
Security/jailing model (FC `jailer`+seccomp vs CH seccomp — both lean, but the trait should
own a `Sandbox` responsibility); CI combinatorics (`{FC,CH}×{x86,arm64}×snapshot` →
capability-gated test selection + per-backend mock; interacts with #630); perf/footprint
benchmarking to pick the default; CH packaging (static binary, min v52.0 for ondemand);
optional future: evaluate embedding a VMM via rust-vmm/libkrun.

## Phased plan
- **P0** Extract the trait around existing Firecracker (no behavior change). Green CI = done.
- **P0.5** Boot-plan-over-vsock in fc-agent (removes the MMDS dependency for CH).
- **P1** Cloud Hypervisor backend: cold boot + run a container via the vsock boot-plan (no snapshots).
- **P2** CH snapshot/restore + UFFD `ondemand`; capability-gate diff + memory-share; verify CoW sharing.
- **P3** Parity polish: capability-driven degradation everywhere, `--hypervisor {firecracker,cloud-hypervisor}` in profiles/state/cache-key, docs, CI matrix dimension on both backends.
Each phase: tests + `/code-review` + Codex review per repo convention.

## Open research items
- Verify CH cross-clone CoW sharing (`--memory file=` + restore path maps snapshot
  MAP_PRIVATE as guest RAM).
- Per-backend security/jailing model.

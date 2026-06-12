# Design: hypervisor-agnostic abstraction (Firecracker + Cloud Hypervisor)

Status: **proposal / RFC** (epic: #632). Canonical design doc; #632 holds the full proven
capability mapping with source citations, a code-verified seam audit (per-call-site routing
table + P0 checklist), and **measured experiment results** (2026-06-12: CH v52 boots the full
fcvm stack; FC File-backend sharing quantified; CH v52 snapshot creation fails on ARM64).

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
| **cross-clone memory sharing** | ✅ **measured**: `File` backend mmaps `memory.bin` MAP_PRIVATE; 3× 1GiB clones ≈ 230MiB total PSS, with dirty tracking on OR off. UFFD path is **lazy population, not sharing** (UFFDIO_COPY makes per-VM copies) | ❓ blocked: CH v52 snapshot create fails on ARM64 (`GetAarchCoreRegister` EINVAL), so density unmeasured | split caps: `file_backed_cow_restore` / `external_uffd_lazy_restore` / `internal_uffd_lazy_restore` |
| diff / incremental snapshots | ✅ | ❌ | cap `diff_snapshots` (FC-only; CH takes full) |
| drive retarget on restore (`patch_drive`) | ✅ | ❌ | use **bind-mount redirect** (already VMM-agnostic) everywhere |
| metadata service (boot plan) | ✅ MMDS | ❌ no MMDS | **boot-plan over vsock** (portable; keep MMDS as FC fast path) |
| nested ARM64 (FEAT_NV2) | ✅ (custom fork + DSB kernel patches) | ❌ (CH nested is x86-only) | cap `nested_arm64` (FC-only) |

Net: the capability gates separating the two are memory-sharing semantics (split into
`file_backed_cow_restore` / `external_uffd_lazy_restore` / `internal_uffd_lazy_restore` —
FC and CH implement *different* lazy-restore mechanisms and neither is a superset), diff
snapshots, drive retarget, native metadata, and ARM64 nesting — everything else is shared.

## Proposed abstraction
Move `src/firecracker/` → `src/hypervisor/{firecracker,cloud_hypervisor}/`.

```rust
pub enum Backend { Firecracker, CloudHypervisor }

pub struct Capabilities {
    pub diff_snapshots: bool,             // FC: true,  CH: false
    pub file_backed_cow_restore: bool,    // FC: true (measured, #632), CH: unverified (snapshot blocked)
    pub external_uffd_lazy_restore: bool, // FC: true (fork handshake w/ page_size), CH: false
    pub internal_uffd_lazy_restore: bool, // FC: n/a,   CH: ondemand (v52; unverified on ARM64)
    pub drive_retarget: bool,             // FC: true,  CH: false (bind-mount redirect)
    pub native_metadata_service: bool,    // FC: MMDS,  CH: false (vsock boot-plan)
    pub nested_arm64: bool,               // FC: true,  CH: false
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
3. **Memory-share clones — measured (#632, 2026-06-12).** FC's `File` backend mmaps the
   snapshot MAP_PRIVATE: 3× 1GiB clones cost ~230MiB total PSS, and `track_dirty_pages`
   does NOT erode the sharing (ON vs OFF within 1MiB — the flag's costs are KVM logging
   overhead and the 2M→4K hugepage split). FC's UFFD path is **lazy population, not
   sharing**: guest RAM is MAP_ANONYMOUS and UFFDIO_COPY makes a private per-VM copy of
   each faulted page — density there comes from only working sets materializing. CH's
   density is **unmeasured**: CH v52 snapshot creation fails on ARM64
   (`GetAarchCoreRegister` EINVAL; FC snapshots work on the same host), blocking both the
   `ondemand` and CoW-backing experiments.
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
- **P1** Cloud Hypervisor backend: cold boot + run a container via the vsock boot-plan (no
  snapshots). De-risked by experiment (#632): CH v52 boots fcvm's kernel+initrd+rootfs to
  fc-agent today — needs `console=ttyAMA0` (custom profile kernels lack PL011: add
  `CONFIG_SERIAL_AMBA_PL011=y` or use `hvc0`), `--cpus boot=`, `image_type=raw` on disks,
  no `pci=off`. fc-agent starts ALL vsock channels only after the boot-plan fetch, so P0.5
  is a hard prerequisite for any guest-channel function on CH.
- **P2** CH snapshot/restore + UFFD `ondemand`; capability-gate diff + memory-share; verify
  CoW sharing. **Currently blocked**: CH v52 snapshot creation fails on ARM64
  (`GetAarchCoreRegister` EINVAL) — isolate vs the `kvm-arm.mode=nested` host kernel, test
  newer CH, file upstream. Until resolved, realistic CH scope is P1 only.
- **P3** Parity polish: capability-driven degradation everywhere, `--hypervisor {firecracker,cloud-hypervisor}` in profiles/state/cache-key, docs, CI matrix dimension on both backends.
Each phase: tests + `/code-review` + Codex review per repo convention.

## Open research items
- Root-cause CH v52 ARM64 snapshot failure (`GetAarchCoreRegister` EINVAL): stock host
  kernel? newer CH? upstream bug? Blocks all CH snapshot/restore work.
- Verify CH cross-clone CoW sharing (`--memory file=` + restore path maps snapshot
  MAP_PRIVATE as guest RAM) — blocked on the above.
- CH vsock `CONNECT` parity with a live guest listener (testable only after P0.5 lands,
  since fc-agent binds vsock channels only post-boot-plan; CH docs confirm the
  `<socket>_<port>` naming).
- Per-backend security/jailing model.

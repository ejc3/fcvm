# Plan: Disk-only clone (cold-boot from a captured disk) + free hugepage choice

## Goal (user-agreed)
Capture a running VM's **disk only** (no memory state), then **cold-boot** fresh,
independent VMs from that disk. Each clone boots its kernel from scratch and fc-agent
runs the container against the disk's already-populated podman storage. No UFFD, no
`vmstate.bin`, no shared memory. A clone may pick **any `--memory`/`--cpu`/`--hugepages`**
regardless of how the source booted (cold boot ⇒ fresh memory ⇒ no UFFD/backing-match
constraint — that constraint only exists on the memory-restore path).

## Interface (user choice: extend snapshot commands)
```
fcvm snapshot create --pid <vm_pid> --tag base --disk-only      # capture disk only
fcvm snapshot run --tag base --name c1 --hugepages --memory 8192 --cpu 4   # cold boot
fcvm snapshot run --tag base --name c2                          # another fresh copy
```
- `--disk-only` snapshot stores only `disk.raw` (+ `:ro` extra disks) + `config.json`
  (`kind=DiskOnly`); no `memory.bin`/`vmstate.bin`; no `serve` step.
- `snapshot run` on a DiskOnly tag (no `--pid` serve) = cold boot, not memory restore.

## SUCCESS CRITERIA (user-specified)
1. Implemented end to end (capture + cold-boot run), compiles, tests run locally.
2. **Run `/code-review`** on the feature PR and address findings.
3. **Run a Codex adversarial review** (`@codex review`) on the PR and address findings.
4. **Documentation kept coherent**: update `README.md` (snapshot/clone section), the
   snapshot workflow docs, and `.claude/CLAUDE.md` (snapshot/UFFD + the "Memory Sharing
   (UFFD)" / snapshot-workflow sections) to describe disk-only capture + cold-boot clone
   and the hugepage-on-clone capability; `DESIGN.md` if it covers snapshots.

## Reuse seam (code-grounded — from Plan agent)
- Cold boot is driven by `DiskManager::create_cow_disk()` (`src/storage/disk.rs:41-102`),
  which reflinks `self.base_rootfs` → `<vm_dir>/rootfs.raw`. `base_rootfs` is supplied via
  `DiskManager::new(vm_id, base_rootfs, vm_dir)` (`disk.rs:26`), set in
  `run_vm_setup_inner` (`src/commands/podman/vm_config.rs:805-806`) from
  `VmSetupParams.base_rootfs` ← `prepare_vm`'s `ensure_rootfs(...)` (`mod.rs:293`, the
  `layer2-{sha}.raw`).
- **Seam:** point `base_rootfs` at the snapshot's `disk.raw` and the identical cold-boot
  sequence (create_cow_disk → ensure_free_space → Firecracker start → MMDS plan →
  InstanceStart) clones the captured disk as rootfs. Thread via a new
  `RunArgs.rootfs_override: Option<PathBuf>` + `RunArgs.disk_only: bool`.

## CRITICAL constraints / gotchas (must handle)
1. **fc-agent wipes podman storage on cold boot.** `agent.rs:246-248` calls
   `reset_podman_state()` (`podman system reset --force`, `container.rs:451-468`) for root.
   A disk-only clone MUST skip this (and `write_early_storage_conf` override) or it
   destroys the baked-in image. **Requires a new fc-agent disk-only boot mode.**
2. **Overlay image mode (the DEFAULT) does not bake the image into the rootfs.** Overlay
   keeps the image on a separate `.storage-v2.img` device mounted as
   `additionalimagestores` (`mod.rs:571-591`, `container.rs:50-51`). So a disk-only
   capture of an overlay VM yields a rootfs with no image. **v1: reject `--disk-only`
   capture of an overlay-mode VM** (require btrfs/archive mode, where fc-agent
   `podman load`s the image into the rootfs at boot). (Future: also capture+reattach the
   store device for overlay.) Capture reads the source VM's persisted `image_mode`.
3. **Disk consistency at capture:** reflink the rootfs while the VM is **paused** (mirror
   `create_snapshot_core` pause→reflink→resume, `common.rs:1559/1653/1683`). A live reflink
   risks an inconsistent FS (the corruption class the existing code warns about at
   `common.rs:1645-1648`).
4. **Env vars are intentionally NOT persisted to host state** (`mod.rs:643-644`, secrets).
   For disk-only run, re-supply env at run time (`--env` on `snapshot run`) rather than
   baking secrets into the world-readable snapshot dir.

## Ordered implementation sequence (from Plan agent)
1. `SnapshotKind { Full, DiskOnly }` + `#[serde(default)] kind` on `SnapshotConfig`
   (`storage/snapshot.rs`). Update all 7 `SnapshotConfig { … }` literals (1 prod in
   `common.rs:1261` build_snapshot_config → `kind: Full`; 6 test fixtures).
2. Persist `image_mode`, `container_cmd`, `privileged`, `rootfs_type`,
   `non_blocking_output` into `VmState.config` in `prepare_vm` (`mod.rs:645-673`); check
   `src/state/types.rs` for existing fields first.
3. Extend `SnapshotMetadata` (`snapshot.rs:67`) + `build_snapshot_config`
   (`common.rs:1271`) with: `container_cmd`, `privileged`, `image_mode`, `rootfs_type`,
   `non_blocking_output` (all `#[serde(default)]`).
4. `create_disk_only_snapshot_core` in `common.rs` (factor pause→reflink→resume→finalize
   from `create_snapshot_core`; skip Firecracker `create_snapshot` + the memory-size disk
   check). Add `--disk-only` to `SnapshotCreateArgs` + branch in `cmd_snapshot_create`
   (`snapshot.rs:88`) incl. overlay-mode rejection.
5. fc-agent: `#[serde(default)] Plan.disk_only` (`types.rs`); in `agent.rs` when set, skip
   `reset_podman_state`/`write_early_storage_conf` override, add a disk-only arm in the
   image-prep match (`agent.rs:251-274`) that sets `image_ref=plan.image` and verifies it
   exists locally (`podman image inspect`), skip overlay unmount at cleanup. **Rebuild
   initrd** (fc-agent changed) for local testing.
6. `RunArgs.rootfs_override` + `RunArgs.disk_only`; branches in `prepare_vm` (skip the
   localhost export block `mod.rs:445-611`, `image_disk_path=None`, use override as
   `base_rootfs`, mark MMDS disk-only) and `to_mmds_json`/`FirecrackerConfig.disk_only`
   (`config.rs:357-395`).
7. `cmd_snapshot_run_disk_only` in `snapshot.rs`: dispatch from `cmd_snapshot_run` on
   `kind==DiskOnly` (reject `--pid`/UFFD); synthesize `RunArgs` from
   `snapshot_config.metadata` + CLI overrides (`--memory`/`--cpu`/`--hugepages`/`--env`),
   set `rootfs_override=Some(disk.raw)`, `disk_only=true`, `no_snapshot=true`; call
   `prepare_vm`→`run_vm_loop`→`cleanup_vm_context`; patch `process_type=Clone` +
   `snapshot_name` into state.
8. **Docs** (success criterion #4): README snapshot section, CLAUDE.md, DESIGN.md.
9. **Tests**: cold-boot-from-disk-clone (write marker in source → capture → cold-boot →
   marker present + container runs); hugepage upgrade (source no-hugepages → clone
   `--hugepages` healthy); independence (two clones don't share memory writes); reject
   overlay-mode capture. Run locally (rootless, no sudo; needs initrd rebuild).
10. **Reviews** (success criteria #2/#3): `/code-review` + `@codex review` on the PR;
    address findings.

## Top risks
1. fc-agent `reset_podman_state()` wiping the baked-in image (most important change).
2. Overlay-mode default not baking the image into rootfs → v1 rejects overlay capture.
3. Disk consistency: reflink must be while paused.

## Critical files
`src/storage/snapshot.rs`, `src/commands/common.rs`, `src/commands/snapshot.rs`,
`src/commands/podman/mod.rs`, `src/commands/podman/vm_config.rs`, `src/cli/args.rs`,
`src/state/types.rs`, `src/firecracker/config.rs`, `fc-agent/src/agent.rs`,
`fc-agent/src/types.rs`; reuse seam `src/storage/disk.rs:41`. Docs: `README.md`,
`.claude/CLAUDE.md`, `DESIGN.md`.

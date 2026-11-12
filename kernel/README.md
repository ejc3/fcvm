# fcvm guest kernels

fcvm builds guest kernels from a pinned Linux release and Firecracker microVM
base config. `rootfs-config.toml` defines the source profiles for both supported
architectures; `fcvm setup` normally downloads their content-addressed release
artifacts, while `--build-kernels` builds the selected profile locally.

The default profile is part of snapshot correctness. Every shipped guest config
must enable:

- `CONFIG_FUSE_FS=y` for host-backed volumes
- `CONFIG_INET_DIAG=y` and `CONFIG_INET_DIAG_DESTROY=y`, to enumerate sockets by
  kernel cookie and retire exactly that set with `SOCK_DESTROY`
- `CONFIG_PACKET=y`, whose AF_PACKET receive path supplies the grace period that
  closes the capture boundary
- the directional NEW-flow REJECT gate that holds the boundary shut while
  cookies are captured and retired: `CONFIG_NETFILTER_XTABLES=y`,
  `CONFIG_NF_TABLES=y`, `CONFIG_NFT_COMPAT=y`, `CONFIG_NF_CONNTRACK=y`,
  `CONFIG_NETFILTER_XT_MATCH_CONNTRACK=y`, and both address families
  (`CONFIG_IP_NF_IPTABLES=y`, `CONFIG_IP_NF_TARGET_REJECT=y`,
  `CONFIG_IP6_NF_IPTABLES=y`, `CONFIG_IP6_NF_TARGET_REJECT=y`) — the guest runs
  iptables and ip6tables, so a missing family means the gate cannot install and
  snapshot creation fails closed

`tests/test_default_kernel_release.rs` asserts this exact list against every
`kernel/*.conf`, so it is the contract rather than a summary of one.

The capture side runs at snapshot time, before memory is persisted: the guest
holds the boundary shut, enumerates its sockets, and retires them. The captured
sockets are destroyed again on the RESTORE side, before the clone is published,
because the restored memory image still contains them. A kernel without these
options cannot safely create a restorable network snapshot.

## Patches

The default profiles apply `kernel/patches-default`, which holds upstream fixes
the pinned release lacks. Today that is Linux commit 0d0eff39ceb3, without which
about one guest shutdown in 200 spins forever in `virtnet_poll_cleantx()`
(#990). The nested and btrfs patch directories link the same files, so every
shipped guest kernel carries them. Drop a patch once the pinned release contains
it: the build then fails when the patch no longer applies.

`kernel/patches-default` also holds one change Linux does not have:
`0002-fuse-trust-server-attributes-without-writers.patch`. With the FUSE writeback
cache, upstream keeps the kernel's own size and times of a cached regular file for the
life of the inode, so a file the host changed under a read-write `--map` was read cut off
at the old size (#1041). The patch keeps them only while the file is open for writing.
Every guest patch directory links it; the nested ones link it as a `.vm.patch`, so the
host kernel built from the same directory does not apply it.

The nested profiles, the only ones with an NFS client, also apply
`kernel/patches-arm64/0006-nfs-give-restored-clients-identities-of-their-own.vm.patch`
(linked from `kernel/patches-x86`). Two clones restored from one snapshot hold the
snapshot's NFSv4 clients, identity and server state included, and the server keeps one copy
of that state: when one clone closed a file it held across the snapshot, the other's next
write to it failed with EIO (#1737). Writing a salt in hex to
`/sys/fs/nfs/net/nfs_client/renew_identities` gives each NFSv4.0 client the salt (the next
values for further clients) on its owner identifier, a new client ID and its opens and locks
again under it. fc-agent writes it in every restore of a guest with NFS shares, with a salt
from the restore epoch the host makes for that restore, since twin clones resume with the
same random state. Linux has no such interface.

x86 patches live in `kernel/patches-default-x86`, which the x86 default profile
applies in place of `kernel/patches-default` and which links the shared patches.
`kernel/patches-btrfs-x86` and `kernel/patches-x86` link them for the btrfs and
nested x86 kernels. The directory held an x86-only fix for an async page fault
taken inside an NMI ("Host injected async #PF in interrupt disabled region")
until upstream took it as 81d99abe2bd8, which 6.18.59 carries; it holds no
x86-only patch today.

The host kernels (`make install-host-kernel`) apply the `patches_dir` of their
own `host_kernel` table, minus the `.vm.patch` files: `kernel/patches-x86` on
x86_64, shared with the nested guest, and `kernel/patches-arm64-host` on arm64,
where the host runs 7.3.3 ahead of the 6.18.60 guests (an L1 guest kernel newer
than its host wedges L2, #659). Edit a host patch with
`scripts/kernel-patch.sh edit nested.host_kernel <patch>`. The host kernels pin
6.18.60 or a later 6.18 release, or 7.2 or later: the first releases with Linux commit
25ae123db10b (6.18 stable 39679927fa7a), which removes an `rcu_barrier()` from
`br_multicast_dev_del()`. Each fcvm VM has a bridge in its own network
namespace, and the kernel tears dying namespaces down in batches under one RTNL
hold, so the barrier ran once per bridge inside that hold: with 100 VMs stopping
together on 6.18.50, `cleanup_net` held RTNL for 1.9 s and a namespace creation
waited up to 1.86 s; without the barrier no hold passed 50 ms and the longest
creation took 180 ms (#1563). fcvm carried the commit as a patch until the pin
reached 6.18.60. `every_host_kernel_pins_a_release_without_the_bridge_rcu_barrier`
fails if a host kernel pins an earlier release.

`kernel/patches-arm64-host/vgic-v3-keep-hcr-on-quiet-exits.patch` gives back most of what
6.19's vgic rework added to every KVM exit (#659). Since Linux 6.19 (the vgic-lr-overflow
series) every entry and exit runs the vgic flush and sync in full when no interrupt is in
flight: the empty ap_list is walked under its lock to fill and prune it, ICH_HCR_EL2 is
cleared and ICH_MISR_EL2 read on exit, and ICH_HCR_EL2 is written again on entry. The patch
skips the walk and the lock for an empty ap_list, as 6.18 did, and on VHE leaves
ICH_HCR_EL2 programmed across an exit when no LR is in use and the value cannot raise a
maintenance interrupt before the next entry. `quiet_vgic_exits_leave_ich_hcr_el2_programmed_without_a_host_maintenance_interrupt`
runs the release's flush, sync, load and put against a model of the CPU interface: it
fails without the patch on the work a quiet exit does, and fails any change that leaves
the host holding a register state that raises a maintenance interrupt.

The gpu profile applies `kernel/patches-gpu` (`kernel/patches-gpu-x86` on
x86_64): the default patches, linked, plus one Kconfig patch that gives
`CONFIG_MMU_NOTIFIER` a prompt. NVIDIA's nvidia-uvm needs MMU notifiers, the
option has no prompt upstream, and nothing in this kernel selects it. The gpu
profile is not published as a release yet; `make setup-gpu` builds it.

## Release identity

Published default profiles carry `kernel_sha`, the first 12 hexadecimal digits
of the concatenated `build_inputs`. The inputs include:

1. an architecture-specific build recipe with an immutable Firecracker commit,
   config path, patch policy, and build-spec version;
2. the architecture-specific kernel config fragment;
3. each patch the recipe's `patches_dir` applies, named as an exact file.

A source checkout recomputes and verifies the manifest SHA before building. An
installed binary uses the recorded SHA to locate the release even though the
`kernel/` source files are no longer present. Artifact names are:

```text
vmlinux-{profile}-{kernel_version}-{runtime_arch}-{kernel_sha}.bin
```

Changing a build input requires updating `kernel_sha`; the deterministic tests
reject stale manifests. If the generated kernel build procedure changes in a
way that can alter the binary, bump `build_spec` in both default build recipes.

`.github/workflows/kernels.yml` builds and publishes the default, nested and
btrfs kernels, each on the self-hosted ARM64 and X64 runners, so each of those
profiles has a release for both architectures. The three build jobs run
independently; none waits for another. Setup fetches the default kernel before
it builds a named profile, so a nested or btrfs leg that starts before the
default release exists builds the default kernel locally (`--build-kernels`).
`scripts/kernel-release-identity.py` derives each leg's version, SHA, tag and
file name the way `fcvm setup` does. A leg builds when its tag has no release
yet or when a manual run sets `force_build`. The nested and btrfs legs then
always build from source (`--force-build-kernels`) and never reuse a kernel
file already on the runner. Each release's tag is created at the commit that
was built. `verify-releases` runs after the build jobs of every run and fails
when a published leg's release asset is missing
(`scripts/verify-kernel-releases.py`). `verify-kernel-releases.yml` runs the
same check weekly and on demand. It is a workflow of its own because `ci.yml`
and `build-runner-ami.yml` start on every completed Build Kernels run on main,
and a scheduled run under that name would start them every week.

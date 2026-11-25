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
same random state. A renewal that fails or runs past 10 s leaves the clients on the snapshot's
identity; the restore's `guest_phases` then carry `"nfs_renew_failed":true` beside
`nfs_renew_ms`, and the host logs a warning. Linux has no such interface.

x86 patches live in `kernel/patches-default-x86`, which the x86 default profile
applies in place of `kernel/patches-default` and which links the shared patches.
`kernel/patches-btrfs-x86` and `kernel/patches-x86` link them for the btrfs and
nested x86 kernels. The directory held an x86-only fix for an async page fault
taken inside an NMI ("Host injected async #PF in interrupt disabled region")
until upstream took it as 81d99abe2bd8, which 6.18.59 carries; it holds no
x86-only patch today.

The host kernels (`make install-host-kernel`) apply the `patches_dir` of their
own `host_kernel` table, minus the `.vm.patch` files, both on 7.3.3:
`kernel/patches-x86-host` on x86_64, which links the 7.3 fuse patches and holds the KVM
patch below, and
`kernel/patches-arm64-host` on arm64, which links the patches it shares with the
7.3.3 nested guest from `kernel/patches-arm64` and holds the host-only KVM patches
below. Each host also sets the Kconfig lines of its `config_fragment` on top of the
running kernel's config, and its build stops if one does not hold after `olddefconfig`:
`kernel/host-x86.conf` on x86_64 (ENA built in, wireless extensions off) and
`kernel/host-arm64.conf` on arm64 (wireless extensions off, #2034). A host must not
be older than its L1 guest kernel:
an arm64 guest newer than its host wedges L2 (#659). Edit a host patch with
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
`scripts/check-bridge-teardown-kernel.sh` applies the same rule to a `uname -r` release.
Both runner images (`scripts/build-ami.sh`, arm64 and x64) install the host kernel of their
architecture and run that check on the release GRUB will boot (`scripts/grub-boot-kernel.sh`).
The image is then created as a candidate no launcher selects, one bare-metal instance boots
from it and runs the check on its running kernel, and only an image that passes is tagged
`Purpose=github-runner`. The x64 runners ran a stock
6.14 kernel before the x64 image existed: on 2026-11-24 one of them stalled every network
operation for 6.6 s and 5.9 s while namespaces died, and nine tests lost their namespace
holder at its 5 s readiness deadline.

Every self-hosted job that starts VMs runs `scripts/check-runner-kernel.py` before it builds
anything. The script computes the host kernel release the checkout's `host_kernel` table gives
for the runner's architecture (`<kernel_version>-fcvm-<hash>`, the hash `fcvm setup` computes
from the table's `kernel_version` and `build_inputs`) and compares it with `uname -r`, so a
patch added to a host set changes what the script expects with no edit to it. An x86-only
patch changes the x86_64 release alone. The job fails on a kernel fcvm did not build, and on
an fcvm build that is neither the checkout's nor one main's inputs gave in the last 24 hours.
The 24 hours cover the image that follows a host kernel change: the change starts Build
Kernels (it is under `kernel/` or in `rootfs-config.toml`), which starts Build Runner AMI,
which waits for approval, and runners then boot the new image as they are replaced. That took
about 4 h 20 min on 2026-11-25, and a runner lives at most 13 h 30 min. Inside the window a job on the
previous build passes with a warning that names the commit and the deadline. A checkout whose
own inputs differ from main's (a pull request that changes a host patch, or a branch behind
main) passes on main's build with a notice, since images are built from main only.
`weekly.yml`'s benchmark job passes `--current-only`, which accepts the checkout's build and
no other. The kernel build jobs do not run the check: they build what the next image needs,
on whatever image the runners have. `every_self_hosted_job_checks_the_runner_kernel` lists the
jobs that run it and the ones that do not, and fails for a new self-hosted job in neither list.

`kernel/patches-x86-host/0003-kvm-x86-keep-a-pending-pv-kick-across-async-page-ready.patch`
keeps a paravirtual spinlock kick that KVM erased (#2125). A PV spinlock waiter checks the
lock and then executes HLT, with interrupts off when the lock is taken from interrupt
context; a kick (`KVM_HC_KICK_CPU`) that arrives between the two sets `pv_unhalted`, and the
HLT must then not block. Since Linux 6.15 (e9cb61055fee) `kvm_set_mp_state()` clears
`pv_unhalted` on every move to runnable, and `kvm_arch_async_page_present()` goes through it
for a vCPU that is already runnable, so a page-ready completion in that window erased the
kick: the HLT blocked, only an NMI could wake the vCPU, and every CPU that sent it an IPI and
waited spun for ever. On an oversubscribed x86_64 host 15 of 900 two-vCPU guests restored
through userfaultfd hung that way; the trace of each showed the kick hypercall, its
`kvm_apic_accept_irq`, a `kvm_async_pf_ready` for the kicked vCPU 40 to 104 us later and then
its HLT with no wakeup. Guest `no-kvmapf` does not avoid it: without the guest interface KVM
halts the vCPU artificially and completes through the same function. The patch sets the run
state directly in that one function, as 6.14 did. Upstream has the same code today (7.4-rc4);
there is no upstream commit to take. arm64 KVM has no async page faults, so the arm64 host
does not carry it. `a_page_ready_completion_keeps_a_pending_pv_kick`
(tests/test_kernel_patch_invariants.rs) runs the release's two functions in a C harness and
fails without the patch.
The kick a snapshot load gives the runnable vCPUs (#2131, DESIGN.md "A Restore Gives Runnable
vCPUs a PV Kick") needs this patch as well: a restored vCPU's first `KVM_RUN` completes the
page-ready wake-up that restoring `MSR_KVM_ASYNC_PF_EN` queued, through the same function,
before the guest runs an instruction (#2132). Only this host kernel carries the patch. The
guest kernels that run KVM themselves (`nested.amd64`, `phone.amd64`, the `mountinfo` pair)
do not, so a VM restored inside one of those guests has its kick cleared.

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

`kernel/patches-arm64-host/nv-guest-hypervisor-exits-without-calls.patch` takes back what 7.3
added to every exit of a guest hypervisor's vCPU (#659). 3fc7867a95c0 moved HCR_EL2 out of the
VNCR page and copies it to the page's NVHCR_EL2 slot and back around every run in hyp context,
each copy a masked read and a masked write. 62ed180231f4 grew the VHE world switch until GCC
stopped inlining `___ctxt_sys_reg()`, so every exit calls out-of-line copies of it,
`vcpu_has_nv()`, `is_hyp_ctxt()` and `vcpu_el2_e2h_is_set()`. On a host without FEAT_NV3 the
patch copies HCR_EL2 as stored and copies it back, masked, only when the guest changed it; it
makes those accessors `__always_inline`, and applies the register masks inline in the VHE world
switch. `a_guest_hypervisors_quiet_exit_does_not_mask_its_hcr_el2_copies` runs the release's
`__compute_hcr()` and `fixup_nv_guest_exit()` in a harness and fails without the patch on the
masking; `the_world_switch_inlines_the_nv_accessors_it_calls_on_every_exit` fails without it on
the `static inline` declarations.

The gpu and phone profiles apply `kernel/patches-gpu-7.3` on both arches: the
fuse attribute patch, linked, plus one Kconfig patch that gives
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

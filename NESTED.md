# Nested Virtualization Guide

fcvm supports running VMs inside VMs using ARM64 FEAT_NV2. Host → L1 → L2 nesting works. L3+ is blocked by FUSE-over-FUSE latency (~5x per level).

## Requirements

| Requirement | Details |
|-------------|---------|
| **Hardware** | ARM64 with FEAT_NV2 (Graviton3+: c7g.metal, c7gn.metal, r7g.metal) |
| **Host kernel** | 6.18+ with `kvm-arm.mode=nested` boot parameter |
| **Nested kernel** | Pre-built from releases or `fcvm setup --kernel-profile nested --build-kernels` |
| **Firecracker** | Fork with NV2 support (configured via kernel profile) |

## Setting Up an EC2 Instance

**Step 1: Launch a metal instance**

```bash
# Must be a metal instance for FEAT_NV2 hardware support
aws ec2 run-instances \
    --instance-type c7g.metal \
    --image-id ami-0xyz...  # Ubuntu 24.04 ARM64
```

**Step 2: Install fcvm and set up host kernel**

```bash
cargo install fcvm

# Download nested kernel profile and install as host kernel
# Configures GRUB with kvm-arm.mode=nested
sudo ./fcvm setup --kernel-profile nested --install-host-kernel
sudo reboot
```

**Step 3: Verify nested KVM is enabled**

```bash
uname -r                                  # Should show 6.18-nested
cat /sys/module/kvm/parameters/mode       # Should show "nested"
ls -la /dev/kvm
```

<details>
<summary>Manual kernel build (alternative)</summary>

```bash
sudo apt-get install -y build-essential flex bison bc libelf-dev libssl-dev

cd /tmp
wget https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-6.18.2.tar.xz
tar xf linux-6.18.2.tar.xz && cd linux-6.18.2

make defconfig
./scripts/config --enable VIRTUALIZATION
./scripts/config --enable KVM
./scripts/config --enable CONFIG_FUSE_FS

make -j$(nproc)
sudo make modules_install && sudo make install

sudo sed -i 's/GRUB_CMDLINE_LINUX_DEFAULT="/GRUB_CMDLINE_LINUX_DEFAULT="kvm-arm.mode=nested /' /etc/default/grub
sudo update-grub && sudo reboot
```

</details>

## Getting the Nested Kernel

If you already have a host with nested KVM enabled:

```bash
# Download pre-built kernel (~20MB)
./fcvm setup --kernel-profile nested

# Or build locally (~10-20 minutes)
./fcvm setup --kernel-profile nested --build-kernels
```

The nested kernel (6.18) includes CONFIG_KVM=y, EL2 support, MMFR4 patch for NV2 capability, FUSE, and TUN/VETH/netfilter for nested networking.

## Running Nested VMs

```bash
# Start outer VM with nested kernel profile
sudo ./fcvm podman run \
    --name outer-vm \
    --network bridged \
    --kernel-profile nested \
    --privileged \
    --map /mnt/fcvm-btrfs:/mnt/fcvm-btrfs \
    --map /path/to/fcvm/binary:/opt/fcvm \
    nginx:alpine

# Verify nested KVM works
./fcvm exec --pid <outer_pid> --vm -- dmesg | grep -i kvm
# Should show: "kvm [1]: VHE mode initialized successfully"

# Run inner VM
./fcvm exec --pid <outer_pid> --vm -- \
    /opt/fcvm/fcvm podman run --name inner --network bridged alpine:latest echo "nested!"
```

## How It Works

1. `FCVM_NV2=1` (auto-set with `--kernel-profile nested`) passes `--enable-nv2` to Firecracker
2. HAS_EL2 + HAS_EL2_E2H0 vCPU features enabled (virtual EL2, nVHE mode)
3. vCPU boots at EL2h — guest kernel sees `is_hyp_mode_available() == true`
4. EL2 registers initialized: HCR_EL2, CNTHCTL_EL2, VMPIDR_EL2, VPIDR_EL2
5. Guest kernel initializes KVM, nested fcvm creates VMs using guest's KVM

## Performance

See [PERFORMANCE.md](PERFORMANCE.md#nested-virtualization) for full benchmarks. Summary:

- ~5-7x FUSE overhead at L2 due to FUSE-over-FUSE chaining
- Local disk ~4-7x overhead (virtio block only)
- L1: 40-53 Gbps network, L2: 8-13 Gbps (~4-5x overhead)
- L2 VMs limited to single vCPU (NV2 multi-vCPU interrupt issue)

## Testing

```bash
make test-root FILTER=kvm

# Tests:
# - test_kvm_available_in_vm: Verifies /dev/kvm in guest
# - test_nested_run_fcvm_inside_vm: Full fcvm-in-fcvm test
# - test_nested_l2: Full L1→L2 nesting with benchmarks
```

## Limitations

- ARM64 only (x86_64 uses different mechanism)
- Requires bare-metal instance (c7g.metal)
- L3+ blocked by FUSE-over-FUSE latency (~5x per level)
- L2 limited to single vCPU (NV2 multi-vCPU interrupt delivery issue)

## Known Issues

- **Nested tests disabled in CI**: L2/L3 tests pass individually but are slow (~5 min each) and occasionally timeout. Run manually with `make test-root FILTER=nested`.

## Related

- [PERFORMANCE.md](PERFORMANCE.md#nested-virtualization) — Nested benchmarks
- [.claude/CLAUDE.md](.claude/CLAUDE.md) — NV2 technical details, cache coherency fixes, kernel patches

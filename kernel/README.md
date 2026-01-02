# Custom Kernel Build for fcvm Nested Virtualization

This directory contains the build infrastructure for a custom Linux kernel
with both FUSE and KVM support, enabling fcvm-in-fcvm (nested virtualization).

## Requirements

Base: Firecracker's microvm kernel config for ARM64
Additions:
- CONFIG_FUSE_FS=y (required for fuse-pipe volumes)
- CONFIG_VIRTUALIZATION=y
- CONFIG_KVM=y (required for nested virtualization)

## Build Process

1. Download kernel source
2. Apply base config from Firecracker
3. Enable FUSE and KVM via scripts/config
4. Build kernel

## Output

The kernel binary is named based on:
- Linux kernel version (e.g., 6.12.10)
- SHA of the build script (for cache invalidation)

Format: `vmlinux-{version}-{build_sha}.bin`

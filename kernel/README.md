# fcvm guest kernels

fcvm builds guest kernels from a pinned Linux release and Firecracker microVM
base config. `rootfs-config.toml` defines the source profiles for both supported
architectures; `fcvm setup` normally downloads their content-addressed release
artifacts, while `--build-kernels` builds the selected profile locally.

The default profile is part of snapshot correctness. Every shipped guest config
must enable:

- `CONFIG_FUSE_FS=y` for host-backed volumes
- `CONFIG_INET_DIAG=y`
- `CONFIG_INET_DIAG_DESTROY=y` for selective `SOCK_DESTROY`
- `CONFIG_PACKET=y` for the guest agent's netlink transport

The last three options let the guest capture the pre-snapshot socket set and
destroy exactly that set before memory is persisted. A kernel without them
cannot safely create a restorable network snapshot.

## Release identity

Published default profiles carry `kernel_sha`, the first 12 hexadecimal digits
of the concatenated `build_inputs`. The inputs include:

1. an architecture-specific build recipe with an immutable Firecracker commit,
   config path, patch policy, and build-spec version;
2. the architecture-specific kernel config fragment.

A source checkout recomputes and verifies the manifest SHA before building. An
installed binary uses the recorded SHA to locate the release even though the
`kernel/` source files are no longer present. Artifact names are:

```text
vmlinux-{profile}-{kernel_version}-{runtime_arch}-{kernel_sha}.bin
```

Changing a build input requires updating `kernel_sha`; the deterministic tests
reject stale manifests. If the generated kernel build procedure changes in a
way that can alter the binary, bump `build_spec` in both default build recipes.

`.github/workflows/kernels.yml` builds and publishes default artifacts on the
self-hosted ARM64 and X64 runners. The nested and btrfs release jobs wait for
the default matrix because their setup path boots with the released default
kernel before building the requested named profile.

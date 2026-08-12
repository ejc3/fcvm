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

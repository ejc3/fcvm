# Windows sandboxes: `?mode=windows`

Status: **design**, the first PR of its stack. The owner, 2026-11-07: a demo of Windows
sandboxes in fcvm, "disposable Windows desktops restored from a snapshot, shown on the fleet's
staging wall the way the phones mode shows Android, with a time-to-desktop readout", chosen
over adding Windows lanes to the EC2 race. The phones mode
(`docs/fleet/ARCHITECTURE.md`, "Phones mode") and the potato mode are the models for the wall
side; this document covers what is new: a guest fcvm does not control, on a VMM fcvm has not
yet run in production, with no container and no fc-agent.

## What a viewer sees

- `?mode=windows` opens it; without the parameter the wall is unchanged.
- **The desktops.** Every sandbox is a tile showing its Windows desktop live, at 1280 x 720,
  laid out like the phones (one row of up to four, then rows of equal length). Each tile is
  labelled with its slot (`win-3`), the edition, and "Desktop in X.XX s": the time from the
  click that asked for a fresh sandbox to the first frame of its desktop decoded on the host.
- **The header.** Sandboxes up, the median and the slowest time to desktop of the last reset
  of each, and the last "Reset all".
- **Reset.** Clicking a tile throws its sandbox away and restores a new one from the
  snapshot; the tile goes dark and its timer runs until the new desktop's first frame.
  "Reset all" does every sandbox at once, which is the number the demo is tuned for, as the
  wall's "Relaunch all" is.
- **Take over.** A viewer borrows one sandbox like a phone (`claim_windows`) and gets a panel
  of fixed actions (below). Nothing a viewer types or points at reaches Windows: an action is a
  fixed sequence of key presses and clicks the host sends for it. There are no free clicks,
  because a click on Windows' on-screen keyboard is typing (Licensing, below). When the borrow
  ends the sandbox is reset, so the next viewer finds the same idle desktop.
- **The label.** Every tile and the take-over panel say "Windows Server 2025 evaluation, for
  demonstration only" (Licensing, below).
- **Proof that it is live.** The taskbar clock is Windows' own, set from the host at every
  restore before the sandbox is reported ready, so every tile reads the same minute as the
  viewer's own clock. "About" opens `winver` with the build and the evaluation's expiry date.

## The guest

### Which VMM

Firecracker cannot boot Windows: it loads a Linux kernel directly and has no UEFI or BIOS
firmware, no VGA and no way to load a boot loader from a disk. Cloud Hypervisor boots Windows
through UEFI firmware (`CLOUDHV.fd`, an EDK II build for it) and runs it with virtio devices
and the virtio-win drivers; its documentation lists Windows Server 2019, Server Core 2004 and
Windows 11 IoT Enterprise LTSC 2024 as tested. fcvm already drives Cloud Hypervisor for cold
boot, snapshot create and restore (`src/hypervisor/cloud_hypervisor`,
`restore_from_snapshot_ch`), so Windows needs no third VMM. Cloud Hypervisor's nested and
passthrough paths are not needed: a sandbox has no GPU and no KVM inside.

The sandboxes are x86_64 only: Microsoft publishes no arm64 evaluation media for Windows
Server, and Windows 11's arm64 builds are not offered for evaluation. They run on the fleet's
x86 hosts (g4dn.metal), as CPU-only VMs beside the wall's GPU slots and the phones; a sandbox
takes no GPU.

### Edition and licensing

**Windows Server 2025 Datacenter Evaluation, Desktop Experience** (build 26100.1742, the
Evaluation Center's ISO `26100.1742.240906-0331.ge_release_svc_refresh_SERVER_EVAL_x64FRE_en-us.iso`,
6,014,152,704 bytes). Chosen over Windows 11 Enterprise Evaluation because:

- the evaluation runs 180 days from installation (Windows 11 Enterprise's runs 90);
- it needs no TPM (Cloud Hypervisor's documentation records Windows 11 with TPM 2.0 as not
  working), no Microsoft account and no consumer setup screens;
- Remote Desktop is built in; a sandbox has one session, opened by the bridge (but see the
  client access licence point below).

The terms. Windows Server 2025's license terms say:

> "Evaluation. For evaluation (or test or demonstration) use, you may not sell the software,
> use it in a production environment, or use it after the evaluation period."

and its general restrictions forbid using the software for "commercial software hosting
services". The owner's reading (2026-11-07): a demonstration on evaluation media is fine,
public included, as long as viewers cannot do anything real with it and it never runs past the
evaluation period. The design holds to that so a public launch stays possible, and every point
below is enforced by code with a test, not by convention:

- **No accounts and no persistence.** A viewer signs in to nothing. The desktop is the session
  of a standard local user (`sandbox`), not an administrator, and nobody types its password.
  Every borrowed sandbox is discarded when the borrow ends (restored from the snapshot), so
  nothing a viewer did survives them.
- **A hard session limit.** A borrow lasts `windows_claim_secs` (120 s by default, the
  phones' value), and the coordinator ends it then whatever the viewer does.
- **No file transfer in or out, and no egress.** The bridge negotiates no clipboard, drive,
  printer, audio or smart-card channel, and the viewer has no free input (only the fixed
  actions, no free clicks: a click on the on-screen keyboard is typing, and a viewer who can
  type in a session can turn off anything inside the guest). Egress is closed outside the
  guest, where nothing in it can reopen it: a raw VM's namespace has no pasta and no route
  out (below), so the guest can reach only the namespace's own address. The guest firewall
  also blocks outbound traffic, as a second layer.
- **Never sold or metered.** No price, quota or billing is attached to a sandbox; the cap on
  borrows is the only limit and it is the same for everyone.
- **Rebuilt from fresh evaluation media well before expiry.** The evaluation runs 180 days
  from installation and must be activated online within 10 days of it, or it shuts down every
  hour; the image build activates it while the builder has a network. The expiry is fixed by
  the gold image's installation date, not by when a clone runs: an image installed on day D
  expires on D + 180, and every sandbox restored from it shows that date. The build records
  the installation date and `slmgr /xpr`'s expiry in the image's manifest. The agent raises an
  alarm (a `windows_image_age` warning in its report, shown in the coordinator's state and its
  log) once the image is 150 days old, and refuses to start sandboxes from an image 170 days
  old or within 7 days of its recorded expiry, whichever comes first. The rebuild downloads
  the ISO from Microsoft again, so a new image is a new installation.
- **Client access licences for a public launch.** Codex's review of this design (2026-11-07)
  points out that the terms' two-user exception (section 4(a)) covers administering the server,
  that section 4(b) counts indirect access through a graphical interface, and that section
  4(e) says pooling connections through one client does not reduce what is needed. Viewers who
  watch a desktop and press fixed buttons on it may count as indirect users needing Remote
  Desktop Services client access licences, whatever the evaluation clause allows. Staging
  behind Access is the licensee's own evaluation; whether a public launch needs RDS CALs is
  for the owner to decide before it, and is not settled here.
- **A visible "demonstration" label.** Every tile and the take-over panel carry "Windows Server
  2025 evaluation, for demonstration only", and Windows' own desktop shows the evaluation's
  watermark with its expiry.

Staging behind Cloudflare Access comes first. **A public, ungated launch still needs the
owner's explicit go-ahead**; if the owner later wants more than a demonstration (anything a
viewer can use for real work, or a paid or production service), it needs licensed Windows
Server and Remote Desktop Services client access licenses instead of the evaluation. No
product key, license file or activation token is in this repository: the evaluation needs
none, and the build takes the ISO from Microsoft's own download link at bake time rather than
storing it.

The virtio-win drivers (virtio-win 0.1.302, from the Fedora project's stable channel) are
signed by Red Hat and redistributable; Cloud Hypervisor's firmware is EDK II under the
BSD-2-Clause-Patent license.

### Devices and drivers

| device | Cloud Hypervisor | Windows driver (virtio-win) |
|---|---|---|
| boot firmware | `CLOUDHV.fd` passed as the payload's firmware (release `ch-97eeb7b09` of cloud-hypervisor/edk2, pinned by SHA-256) | none |
| system disk | virtio-blk, `image_type=raw` | `viostor` |
| network | virtio-net on a tap in the VM's namespace | `NetKVM` |
| memory balloon | virtio-balloon (`deflate_on_oom`; free page reporting stays off, as the backend already refuses it) | `balloon` and its service |
| entropy | virtio-rng | `viorng` |
| serial | 16550 COM1 to a file | in-box; Windows' Emergency Management Services (SAC) writes the boot's progress there |
| console | off | none |
| display | none: Cloud Hypervisor has no graphics device | the desktop exists only inside Remote Desktop sessions |
| clock | `kvm_hyperv=on` (Hyper-V enlightenments: reference counter, reference TSC page, SynIC, synthetic timers) | in-box |

`kvm_hyperv=on` is required by Cloud Hypervisor for Windows, and its synthetic timers are what
keep an idle Windows from waking the vCPUs on every tick.

### The image

QEMU builds the image and nothing else: an unattended install needs the installer's screen to
be debuggable, and Cloud Hypervisor has none. `fleet/windows/build-image.sh` (stack PR 3)
installs from the ISO into a 40 GiB raw disk under QEMU (q35, OVMF, the target disk on
virtio-blk so the installed system boots with `viostor`, the two ISOs and a config ISO on
SATA), with `autounattend.xml` generated from a template in the repository, and two passwords
generated on the builder (the administrator's, which nothing uses after the build, and the
standard user `sandbox`'s, which the bridge logs on with), kept beside the image (mode 0600)
and never written to the repository or a log. `sandbox` is a member of Remote Desktop Users
and of no administrative group. Setup injects every virtio-win driver offline, and a
`SetupComplete.cmd` script configures:

- Remote Desktop on, Network Level Authentication off (the port is reachable only from the
  host, inside the VM's network namespace), 60 frames a second (`DWMFRAMEINTERVAL` 15), the
  H.264 graphics mode preferred, no idle or disconnection time limit;
- no Server Manager at logon, no lock screen, no shutdown event tracker, never sleep or
  hibernate;
- no Windows Update, no Defender (the feature is removed), no diagnostics upload;
- EMS on COM1, a boot menu timeout of 0;
- online activation, then the guest firewall blocks outbound traffic except to the VM's
  namespace;
- the guest agent (below) as a task started at boot.

Then it reboots once so the feature removal finishes and powers off. The disk boots unchanged
under Cloud Hypervisor: the PCI functions are the same virtio-pci devices, Windows binds the
same drivers to them, and the firmware finds `\EFI\Boot\bootx64.efi`. The build's output is
content-addressed like fcvm's other assets: the hash of the ISO, virtio-win, the firmware and
the build's own files names the image.

## A raw VM in fcvm: no container, no fc-agent

Everything fcvm does for a Linux guest after the kernel starts goes through fc-agent: the
boot plan, the container, exec, volumes, health, the snapshot's network boundary, and the
restore epoch that makes a clone reconnect, re-address and set its clock. Windows has none of
it. fcvm gains a raw-VM mode (stack PR 4):

```
fcvm vm run --name win-1 --firmware <CLOUDHV.fd> --disk <image.raw> --cpu 2 --mem 4096 \
    --guest windows --health-port 3389
```

- **Boot.** The Cloud Hypervisor backend boots the firmware as its payload instead of a
  kernel and initrd, with no kernel command line, no vsock and no MMDS. The disk is a
  reflink copy into the VM's directory, as a rootfs is.
- **Network: a namespace with no way out.** A raw VM gets fcvm's per-VM user and network
  namespace, its bridge (`br0`, 10.0.2.1, the fixed `NAMESPACE_MAC`) and the VM's tap, and
  nothing else: no pasta, no default route, no gateway. The guest (static 10.0.2.100/24, set
  by its agent on the virtio NIC) can reach the namespace's address and nothing beyond it, so
  no setting inside Windows can open egress. Every clone of one snapshot has the same address
  inside its own namespace, as Linux clones do.
- **The host side.** fcvm runs one helper inside the namespace (it enters the holder's
  namespaces in its own `pre_exec`, then arms `PR_SET_PDEATHSIG`, so it dies with the VM): it
  relays each connection to the VM's Unix socket `rdp/rdp.sock` in its data directory (the
  directory 0700, the socket 0600, every peer's uid checked against fcvm's) to
  10.0.2.100:3389, and serves the clock line below on 10.0.2.1:4600. A guest restart boots the
  same disk again in place, with the same helper. The bridge reaches Remote Desktop only through
  `rdp.sock` (FreeRDP connects to a Unix socket path directly); no port is published.
- **The restore generation and the clock.** On each connection to 10.0.2.1:4600 fcvm sends
  `<generation> <host time in ms>`, the generation being the VM's id. The guest agent asks ten
  times a second; a generation it has not seen means the VM was restored, and the agent sets
  the wall clock to the host's time plus half the round trip and answers
  `ack <generation> <guest time in ms>` on the same connection. Windows reads the real-time
  clock only at boot, and the Hyper-V reference counter a restore brings back continues from
  the snapshot, so without this every clone's clock would be the snapshot's. The guest pins
  10.0.2.1 to the bridge's fixed MAC, as fc-agent does.
- **Health and the acknowledgement.** No agent reports health, so a raw VM is healthy when
  two things hold: the agent has acknowledged the VM's current generation with a guest time
  within 1 s of the host's, and Remote Desktop answers an X.224 connection request through
  `rdp.sock` with a connection confirm (an answer from Windows' RDP stack, not a SYN-ACK).
  A restored clone is acknowledged at that moment, so the bridge never connects to a desktop
  whose clock is the snapshot's. `fcvm ls` shows it as for any VM, and a VM whose probe fails
  after it was healthy is unhealthy, then stopped when the VMM exits.
- **The guest agent** is a PowerShell script started at boot by a scheduled task as SYSTEM
  (`fleet/windows/fcvm-agent.ps1`): the static address and the clock, nothing else. It runs no
  commands for the host and opens no listener; a sandbox needs no exec, and the take-over
  actions go through Remote Desktop input.
- **Snapshot create** of a raw VM is pause, `vm.snapshot`, resume (or stop, for the gold
  VM). The Linux boundary does not apply: no fc-agent dumps socket cookies and no gate closes,
  because nothing in Windows can be told to. What makes that safe is the gold flow below: the
  only connection that matters (the RDP client's) is closed before the snapshot, and the
  agent's polls are short-lived connections to a namespace that the clone replaces.
- **Restore** is `restore_from_snapshot_ch` with the agent-dependent steps removed: no
  restore epoch over vsock, no wait for fc-agent's acknowledgement. The clone's helper serves
  its new generation from before the vCPUs resume, and the clone is acknowledged when it is
  healthy as above.

`podman run`'s options that need fc-agent (volumes, exec, `--cmd`, image) are refused with
`--guest windows`; `fcvm exec` refuses a raw VM with "a raw VM runs no fc-agent".

### Per-clone identity

Every clone of one snapshot has the snapshot's machine SID, computer name (`FCVM-WIN`),
passwords, Remote Desktop certificate and private key, and network address.
That is acceptable for disposable sandboxes and is stated here so nobody builds on it:

- no sandbox joins a domain or talks to another sandbox (each is alone in a namespace with no
  route out), so no peer can see two machines with one SID or name;
- the RDP certificate is the bridge's to trust, and the bridge pins it by fingerprint from the
  gold image; nobody else connects;
- the password never leaves the host: the bridge reads it from the image's directory.

A per-clone identity (sysprep generalize, a new SID and name at first start) costs a reboot
and minutes, which a restore exists to avoid. If sandboxes ever need distinct identities, the
place is a second agent step at restore, after the clock, not a different snapshot.

## The snapshot point: an idle desktop, logged on

The gold flow, run by the agent once per host and image (stack PR 6):

1. Cold boot the image in a raw VM. Health answers when Remote Desktop listens.
2. The bridge logs on as `sandbox` at the sandbox's size (1280 x 720) and waits for
   the desktop. The first logon creates the profile and runs first-logon tasks.
3. It waits until the desktop is idle: the VM's vCPU threads under 2% of a core for 10 s in a
   row, read from `/proc` on the host.
4. The bridge disconnects. The session stays logged on, in the Disconnected state, with the
   desktop and its windows alive.
5. `fcvm snapshot create --stop`.

A clone's bridge then connects and reconnects to that session: no logon, no profile load, no
shell start. Disconnecting before the snapshot matters: a connection open at the snapshot
would be a half-open connection in every clone, which Windows holds until it times out.

## Memory

Before stack PR 5 a Cloud Hypervisor restore read the whole memory file into anonymous
memory (`memory_restore_mode=copy`): every clone paid a read of the full guest size before it
ran, and held a private copy of every page, including the zeros. Every Cloud Hypervisor
clone now restores with `map` from a symbolic link to the snapshot's memory file
(`prepare_ch_restore_files` in `src/commands/common.rs`). The measurement section gives
both numbers for 1, 4 and 8 clones.

The target is the sharing Firecracker's File backend already gives: map the snapshot's
memory file `MAP_PRIVATE` over guest RAM, so a restore reads nothing before the guest runs, a
page is read from the page cache when the guest first touches it, clean pages are shared by
every clone of the snapshot through the page cache, and a page becomes private to a clone
only when it writes it. Cloud Hypervisor restores by copying (`copy`) or with its own
in-process userfaultfd handler (`ondemand`), which copies each page into the VM's own memory
on its first fault, so it gives laziness but no sharing. The change is a third restore mode
in our Cloud Hypervisor fork, `memory_restore_mode=map`: after the memory manager has created
guest RAM, each saved range of the memory file is mapped `MAP_PRIVATE | MAP_FIXED` over the
RAM that backs it, from its offset in the file. KVM's memory slots already point at those
host addresses and follow the new mapping through its MMU notifiers. It refuses shared and
hugetlbfs guest RAM. Page-cache sharing needs every clone to map the same inode, and a
reflink is a new inode with its own page cache, so each clone's `ch-restore` directory holds a
symbolic link to the snapshot's memory file (immutable once published) and its own copy of
the configuration. A hard link would do the same, but the snapshot directory is a btrfs
subvolume and a link across subvolumes fails with `EXDEV`. Against `copy` on the same host it
cut the first desktop update at 8 clones from 8.67 to 8.77 s to 2.95 to 3.00 s and private
memory per clone from 4,097 to about 840 MiB (the measurement section). A Cloud Hypervisor
without `map` fails the restore with an error naming `fcvm setup --cloud-hypervisor`.

fcvm's UFFD memory server (`src/uffd/`, minor mode with working-set replay) would add
prefetching on top of the same sharing, at the cost of a userfaultfd handshake in the fork.
It is the next step only if page faults from the page cache show up in the time to desktop.

Windows helps both: its zero-page thread zeroes freed memory in the background, so most of an
idle guest's free memory is zero pages, which copy mode stores privately in every clone and a
mapped restore shares through the page cache.

## Getting the desktop to the page

Cloud Hypervisor gives Windows no display, so the desktop exists only as a Remote Desktop
session, and the question is the fastest path from that session to the page's existing frame
protocol (FCVF frames of H.264 over a WebSocket, decoded with WebCodecs).

| option | path | verdict |
|---|---|---|
| RDP with the graphics pipeline, bridged on the host | Windows encodes the session itself (H.264 for most of it); a FreeRDP client on the host decodes it into a desktop surface, which the bridge encodes as FCVF frames | **chosen** |
| Apache Guacamole | guacd is an RDP client too, but it decodes and sends image updates (PNG, JPEG, WebP) in its own protocol to its own JavaScript client, through a servlet | rejected: an extra hop and image coding at a higher bitrate, and it replaces the page's frame path rather than feeding it |
| a capture agent in the guest | needs a virtual monitor (an indirect display driver), Desktop Duplication and a software H.264 encoder inside Windows, competing with the desktop for its 2 vCPUs | rejected: RDP already renders a virtual display and encodes it in the guest, and is built in |

The bridge, `fleet/windows/bridge` (stack PR 6), one per sandbox in a container on the host,
connected to the sandbox's `rdp.sock`:

- connects with FreeRDP 3 at 1280 x 720 with the graphics pipeline and H.264 (FreeRDP built
  with FFmpeg: Ubuntu's FreeRDP packages are built without H.264), no audio, no clipboard, no
  drives;
- **composites, then encodes**: FreeRDP decodes every graphics command into the retained
  desktop surface, applying each H.264 update only inside its region rectangles as
  MS-RDPEGFX requires, and the bridge encodes the surface with x264 (zero latency preset, on
  the host's CPU; the host's GPUs are bound to vfio-pci for the wall) after each frame the
  server ends. Windows' own H.264 cannot be passed through: its updates cover only the
  changed regions (the region rectangles), the stream mixes in other codecs (ClearCodec,
  the progressive codec, cache and surface copies), and AVC444 splits a frame into two
  streams, while an FCVF frame is a whole picture the page decodes on its own;
- serves the renderer's `/frames` WebSocket and `/health` (frames per second, encode time,
  frame age), and stamps each frame with its decode time on the host's clock, so the page's
  frame age works as for a phone;
- reports the time to desktop: the first frame after a restore whose surface is the desktop
  (not black, not the "Please wait" screen; checked on a downscaled copy against the gold
  desktop's), stamped on the host's clock;
- sends the take-over actions' fixed input, and nothing else.

The time to desktop is therefore: restore, the guest's first answer on 3389, the RDP
connection (TCP, TLS, the client and server's capability exchange), the session reconnect, and
the first graphics frame. Each part is in the bridge's `/health` and the agent's report.

## The wall mode

### Components

- **The agent** (`fcvm fleet agent --windows N`): runs N sandboxes on an x86 host, named in
  its hello (`windows`), each a raw-VM clone of the host's gold snapshot with its bridge. It
  builds the gold snapshot at start (the flow above), reports each sandbox's phase
  (`restoring`, `connecting`, `running`, `failed`) with the restore's timings and the bridge's
  time to desktop, and relaunches a sandbox when its desired generation moves, as a slot.
  Reset all restores every sandbox at once from one snapshot.
- **The coordinator.** `state.windows` mirrors `state.phones` (fleet/PROTOCOL.md "Phones
  mode"): tiles with `frame_url`, `phase`, `last_relaunch` (click to first desktop frame), the
  borrow, the action list. Intents: `windows_reset` (one, or all), `claim_windows`,
  `release_windows`, `windows_action`. The borrow rules are `claim_phone`'s: one per client
  address, a cap on sandboxes lent at once, a cooldown, refused to broadcast-only viewers, and
  a reset when it ends. Every event goes through `fleet_core::apply`, so it replays.
- **The page.** A view model in `fleet/web/core` from the snapshot's `windows` section, the
  mock coordinator's sandboxes, the desktop tiles with their timers, the take-over panel, and
  Playwright tests on desktop and phone layouts.

### Actions

`windows_action` takes only these names; anything else is refused before it reaches the
agent. Each is a fixed input sequence the bridge sends, then the bridge reports the action
done when the screen changed.

| action | what the sandbox does |
|---|---|
| `start_menu` | opens the Start menu |
| `notepad` | opens Notepad with a fixed line naming the sandbox and its restore time |
| `explorer` | opens File Explorer on This PC |
| `task_manager` | opens Task Manager on its Performance tab (the 2 vCPUs and 4 GiB) |
| `about` | opens `winver` (edition, build, evaluation expiry) |
| `terminal` | opens PowerShell running `Get-ComputerInfo` for the OS fields |
| `close_all` | closes every window the actions opened |
| `reset` | the coordinator's own: the sandbox is restored from the snapshot |

### Invariants (each becomes a test)

1. A raw VM never waits for fc-agent: `vm run`, `snapshot create` and `snapshot run` of a
   `--guest windows` VM complete with no vsock device configured.
2. A raw clone is reported healthy only after its agent has acknowledged the clone's own
   generation with a guest time within 1 s of the host's (the acknowledgement carries the
   guest's time; a test serves a stale generation and checks the VM stays unhealthy).
3. A raw clone is reported healthy only after an RDP connection confirm, never on a SYN-ACK.
4. Every clone has its own disk and namespace; writes in one never appear in another.
5. Nothing a viewer sends reaches Windows except the enumerated actions' fixed inputs.
6. A sandbox comes back from a borrow, a reset or a failure as the snapshot's idle desktop.
7. The agent warns at 150 days from the image's installation and refuses to start sandboxes
   from an image 170 days old or within 7 days of its recorded expiry.
8. The password and the RDP key never appear in a log, the state or a report.
9. A borrow ends at `windows_claim_secs` and the sandbox is restored from the snapshot; the
   bridge negotiates no clipboard, drive, printer, audio or smart-card channel; the guest
   cannot open an outbound connection beyond its namespace.
10. A raw VM's namespace has no route out: a connection from the guest to anything but
    10.0.2.1 fails (the image test tries one).

### Failure states

| failure | what happens |
|---|---|
| the image build's install does not power off within its deadline | the build fails with the last screenshot and `C:\fcvm\setup.log` read off the disk |
| the gold VM's desktop never goes idle | the gold flow fails after its deadline and names the busiest guest thread's CPU; no snapshot |
| a clone's health probe does not answer within the deadline | the sandbox is failed and relaunched with backoff (1 s, doubling, capped at 30 s) |
| the bridge cannot reconnect the session (Windows logged it off) | the sandbox is failed and relaunched; the gold snapshot is rebuilt if it happens twice in a row |
| Cloud Hypervisor exits | the agent reports the sandbox failed and relaunches it |
| the image is 150 days old | the agent's report carries `windows_image_age`, the coordinator logs it and shows it in the state; sandboxes keep running |
| the image is 170 days old, or within 7 days of its recorded expiry | the agent reports every sandbox failed with the date; the image must be rebuilt from fresh media |

## Stack of PRs

1. This design.
2. The prototype's measurements, added to this document: cold boot to desktop, snapshot
   size, restore to first desktop frame for 1, 4 and 8 clones at once, private memory per
   clone, on one x86 bare-metal host in one session.
3. `fleet/windows`: the image build (QEMU, autounattend template, setup and agent scripts),
   content-addressed, with a test that boots the built image under Cloud Hypervisor and gets
   an RDP connection confirm.
4. fcvm: the raw-VM mode (`vm run --guest windows`, firmware boot, TCP/RDP health, the
   namespace's generation and time line, snapshot create and run without fc-agent), with
   tests that fail first.
5. Our Cloud Hypervisor fork: `memory_restore_mode=map` (the memory file mapped privately
   over guest RAM), and fcvm restoring Cloud Hypervisor clones with it; lands only if it
   measures faster than copy mode.
6. Fleet: the bridge, the agent's Windows slots and gold flow, the coordinator's
   `state.windows` and intents, the page's `?mode=windows`.
7. ejc3/aws: the host image with the Windows image's inputs and the bridge.
8. Staging behind Cloudflare Access, checked from a browser.

## Measurements

The prototype, 2026-11-07, one session on one g4dn.metal launched for it (Xeon Platinum
8259CL, 96 vCPUs, 384 GiB, the fleet host image; nothing else ran on it during a
measurement). The guest: Windows Server 2025 Datacenter evaluation, build 26100.1742, 2 vCPUs
and 4 GiB, installed by the prototype of `fleet/windows/build-image.sh`, logged on over RDP,
the client disconnected, then snapshotted. The prototype's idle check (the Cloud Hypervisor
process's CPU, not its vCPU threads, under 2% of a core for 10 s) never passed: the desktop
stayed at 12 to 13% of a core, the checks gave up after 300 s and 120 s, and the snapshot was
taken anyway. Cloud Hypervisor is fcvm's fork (`fcvm-v52-sve-fix`) for the cold boots and the
ondemand run, and that fork with `memory_restore_mode=map` (ejc3/cloud-hypervisor#1) for the
restore comparison, copy included (the patch leaves the copy path unchanged). The RDP client is FreeRDP 3.17.2 built with FFmpeg, AVC444
graphics, in each VM's network namespace; the scripts are a prototype of the agent's flow
(network namespaces made by hand, a socat line for the clock), not fcvm's raw-VM mode.

Times are from launching `cloud-hypervisor` (boot or `--restore`) on the host's clock:
"RDP answer" is the first X.224 connection confirm from the guest, polled every 50 ms;
"first update" is the first H.264 graphics update the client received, the client being
started at the RDP answer. It is not checked to be the whole desktop: a screenshot of every
client 20 s later shows the desktop, and the bridge (stack PR 6) times the first frame that
matches it. N clones were restored at once, each into its own namespace with
its own reflinked disk; three rounds, the modes interleaved within each round.

### Image, boot and snapshot

| | |
|---|---|
| unattended install under QEMU (8 vCPUs, 8 GiB), to power off after the setup scripts | 713 s |
| first boot under Cloud Hypervisor (new hardware), launch to RDP answer | 17.4 s |
| cold boot of the installed image, launch to RDP answer | 8.2 to 10.2 s (4 boots) |
| cold boot, launch to the desktop (logon of an existing profile, RMSE match at 160x90 against the restored desktop, polled every 100 ms) | 18.8 to 21.2 s (3 boots) |
| snapshot: pause, then `vm.snapshot` | 10 ms, then 2.60 s |
| memory file | 4.00 GiB, dense on disk; 1,801 MiB of it non-zero pages (56% of the pages are zero) |
| disk at the snapshot | 9.6 GiB allocated of 40 GiB |
| idle desktop, session disconnected | 12 to 13% of one host core per sandbox, steady; the cause is not yet attributed (the guest agent's polling is one candidate) |

### Restore to the first graphics update

Each cell is the range of the three rounds' medians over the N clones, then the slowest clone
of any round; "first update" includes the RDP connection (below).

| N at once | | copy | map, memory file reflinked | map, memory file linked |
|---|---|---|---|---|
| 1 | RDP answer | 2.80 to 3.35 s | 1.05 to 1.15 s | 0.074 to 0.076 s |
| 1 | first update | 4.16 to 4.60 s | 3.16 to 3.77 s | **2.19 to 2.58 s** |
| 4 | RDP answer | 3.30 to 3.38 s (max 3.58) | 1.67 to 1.74 s (max 1.78) | 0.39 to 0.43 s (max 0.73) |
| 4 | first update | 5.14 to 5.33 s (max 5.48) | 4.10 to 4.42 s (max 4.81) | **2.62 to 2.75 s (max 3.31)** |
| 8 | RDP answer | 6.65 to 6.80 s (max 7.01) | 2.48 to 2.71 s (max 2.92) | 0.85 to 0.89 s (max 1.69) |
| 8 | first update | 8.67 to 8.77 s (max 9.67) | 5.20 to 5.37 s (max 5.98) | **2.95 to 3.00 s (max 3.72)** |

- **copy** reads the whole memory file into each clone's anonymous memory before the guest
  runs. It took 2.8 to 3.4 s to the RDP answer for one clone and 6.6 to 6.8 s for eight at
  once.
- **ondemand** (Cloud Hypervisor's own userfaultfd handler) was worse on one clone: 3.7 s to
  the RDP answer and 11.0 s to the first update (one run), and it still ended with all 4 GiB
  resident.
- **map** with the memory file reflinked into each clone's directory (as fcvm's restore
  prepares it today) answers RDP after 1.0 to 1.2 s for one clone and 2.5 to 2.7 s for eight:
  a reflink is a new inode with an empty page cache, so every clone reads its pages from disk.
- **map with the memory file linked** (one inode for every clone) answers RDP in 74 to 76 ms
  for one clone: the guest resumes with nothing read, and its pages come from a warm page
  cache. Eight at once answer in 0.85 to 0.89 s (median).

Mapped and linked, most of the time to the first update is after the RDP answer: 2.1 to 2.5 s
for one clone, against 1.2 to 1.5 s with copy, whose guest memory is all resident when the
client connects. That interval holds the RDP connection (TCP, TLS, the capability exchange),
Windows reconnecting the disconnected session, the first update, and the guest's page faults
on pages it has not touched yet; the bridge times each of its own steps, so the next
measurement separates them.

### Memory per clone

Read from `/proc/<cloud-hypervisor>/smaps_rollup` 20 s after each restore, with the desktop
shown: the range over the three rounds of the mean per clone.

| N at once | | copy | map, reflinked | map, linked |
|---|---|---|---|---|
| 1 | resident | 4,102 MiB | 1,663 to 1,690 MiB | 1,699 to 1,703 MiB |
| 1 | proportional (PSS) | 4,101 MiB | 1,661 to 1,688 MiB | 1,697 to 1,702 MiB |
| 4 | PSS | 4,098 MiB | 1,662 to 1,668 MiB | 970 to 979 MiB |
| 8 | PSS | 4,097 MiB | 1,659 to 1,663 MiB | 837 to 845 MiB |
| any | private dirty | 2,023 to 2,073 MiB (and all 4 GiB private) | 692 to 720 MiB | 708 to 733 MiB |

A copy-restored clone holds the whole 4 GiB as its own anonymous memory. A mapped clone
holds only what the guest has touched (1.7 GiB resident), and of that only what it wrote
(about 0.7 GiB) is its own: with the
memory file linked, the clean pages are shared, so eight clones cost 845 MiB each in PSS
against 4,097 MiB with copy, and a ninth costs about 0.7 GiB plus whatever pages no earlier
clone touched.

The design's choice follows: map, with the memory file linked, is stack PR 5.

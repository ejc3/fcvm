# fleet/ami

The fleet's two machine images, built in the fcvm-fleet account (us-east-1) by
`build-ami.sh`, and `fleet/tools/gpubox.sh`, which leases a GPU host for one agent's work.

## Continuous image factory

`Fleet image build request` accepts only successful CI commits on this repository's
protected `main`, a weekly refresh, or a manual main dispatch. Normal requests batch
for six hours and at most one routine build starts per UTC day. A merged pull request
labelled `critical`, `security:critical`, `priority:critical`, `priority:p0`, or `p0`,
or a manual `critical` request, bypasses the batch delay. Critical requests may
preempt a routine build after its exact builders have been terminated.

GitHub's short-lived role can only request work and read its status. The AWS factory
owns the build: one job, a 150-minute deadline, at most one GPU host and one tile
running together, and four deterministic instance slots across build and fresh-image
validation. It does not retry uncertain launches or replace failed builders. An
independent expiry reaper handles a lost or cancelled job. The compute envelope is
bounded separately from EBS, snapshot storage and transfer charges.

The factory uses isolated build/test certificates and its own instance profile.
Production images and warm caches that may contain production TLS material are not
CI build inputs. A pair publishes only after real GPU frames, a restored phone,
certificate rotation, the exact firewall/boot-unit proofs, and cold boot, reboot and
stop/start checks. Both images must use the same renderer. Working artifacts are
private and temporary; the publisher copies the version-pinned validated store into
the Fleet's existing asset bucket before activating the pair.

The coordinator consumes `$Default`, so creating a candidate version cannot deploy
it. The fixed publisher changes only the four template image fields and their
defaults under the Terraform backend lock, retaining the previous pair. Terraform
reads the versioned release pointer and rejects a saved plan if it changed after
planning; it still owns every other template field. Critical publication retires
only the old-image Fleet instances, so streams may briefly reconnect. Routine
publication leaves active machines running and applies on later starts or idle
recycling. No administration or Next.js machine is restarted by the factory.

An accepted request is not a published release. Check the request's AWS status and
the release manifest for validation and promotion results. Infrastructure, the
main-only GitHub environment and the bootstrap enable sequence are owned by
[`ejc3/aws`](https://github.com/ejc3/aws); no Terraform credentials belong in CI.

## A GPU box for testing: `fleet/tools/gpubox.sh`

One g4dn.metal from the newest GPU host AMI: eight T4 on vfio-pci, fcvm built with the gpu
profile set up, the renderer image warmed in the store. Ready about 165 s after `up`
(measured 2026-10-23: SSM online at 113 s, store restored and GPUs bound at 159 s); a GPU VM
then restores to the renderer's first frame in about 9 s.

```bash
id=$(fleet/tools/gpubox.sh up --hours 4 --owner my-task | cut -d' ' -f1)   # prints "<id> us-east-1"
fleet/tools/gpubox.sh run $id 'cat /run/fcvm-fleet/gpus'                   # as root
fleet/tools/gpubox.sh run $id -u 'cd /tmp && fcvm podman run --kernel-profile gpu --gpu 0000:18:00.0 ...'   # as ubuntu
fleet/tools/gpubox.sh copy ./work.tgz $id:/tmp/work.tgz
fleet/tools/gpubox.sh copy $id:/tmp/results.json ./results.json
fleet/tools/gpubox.sh extend $id 2      # lease ends 2 h from now
fleet/tools/gpubox.sh down $id
fleet/tools/gpubox.sh list              # every gpubox, owner, lease end
```

- It assumes `fcvm-fleet-terraform` (the role named in SSM `/infra/fcvm-fleet`) from a dev box whose instance role may assume it; nothing else to set up.
- Access is SSM Run Command only. The fleet's security group admits TCP 443 and nothing
  else, and stays that way: no SSH. One `run` returns at most 24,000 characters of output;
  write more to a file and `copy` it. `-u` runs as `ubuntu` (fcvm's rootless user) with
  VFIO's memlock limit lifted.
- The lease is enforced on the box: a systemd timer powers it off at the lease end (default
  4 h, at most 24), and powering off terminates it. Tested by moving a lease end 90 s ahead:
  the instance shut itself down (`Client.InstanceInitiatedShutdown`).
- Spot first in every us-east-1 AZ, then on demand. Quota, shared with the fleet's own spot
  host: G spot 192 vCPU (two g4dn.metal at once), G on demand 384 vCPU (four).
- It acts only on instances tagged `Purpose=fcvm-fleet-gpubox`; the fleet's hosts, groups
  and services are never touched. The fcvm in the AMI is the commit in its name; build your
  own commit on the box (`/home/ubuntu/fcvm`, `make build`) when you need another.

## The images

| | built by | boots to |
|---|---|---|
| `fcvm-fleet-host-x86_64-<commit>-<date>` (g4dn.metal) | `host/build.sh` prep, build, bake | `fcvm-fleet-vfio.service` binds every NVIDIA function to vfio-pci and fails unless each is passable; `fcvm-fleet-store.service` formats the instance-store NVMe as btrfs at `/mnt/fcvm-btrfs` and restores the store from the asset bucket (`/etc/fcvm-fleet/store.env`), falling back to the AMI's copy (`/var/lib/fcvm-fleet/store/`: the parts `host/store-pack.py` packs at bake, extracted at once); `fcvm-fleet-agent.service` is enabled and runs where the host's boot wrote `/etc/fcvm-fleet/agent.env` (ejc3/aws `fcvm_fleet_host_agent_args`), after the store; the agent dials the coordinator. `fcvm-fleet-prefetch.service` reads, as soon as `/` is mounted, every root volume file a slot's VM launch reads (`/etc/fcvm-fleet/prefetch.list`, written at build by `host/warm-root.sh`, which launches one VM as root exactly as the agent launches a slot and so also leaves the pre-start snapshot the agent's restores hit). Phones (`?mode=phones`): the build also makes the phone profile, the emulator and system image built from source, Robot Park and the phone image, and `host/warm-phone.sh` takes the warm snapshot of a running phone (`phone-warm`) that every phone of a host is a clone of, after checking one clone streams; it writes the agent's phone arguments to `/etc/fcvm-fleet/phone.env`, which the agent unit adds to the host's own (the host's own `--phones N` turns phones on). The Android SDK, used at build only to compile Robot Park, is left out of the store. Units a host does not use (podman's own, apport, fwupd and others, `host/build.sh` prep) are disabled, and the store unit waits for the network only, not for cloud-init's network stage |
| `fcvm-fleet-tile-x86_64-<commit>-<date>` (g4dn.xlarge) | `tile/build.sh` prep, image, bake (`units` and bake revise one launched with `BASE_AMI`) | nvidia-persistenced, the certificate from `fcvm-fleet/tls-wildcard`, the renderer on 127.0.0.1:8080 behind nginx on 443 (`fcvm-fleet-nginx.service`); none of them waits for cloud-init's network stage. `fcvm-fleet-prefetch.service` (wanted by `sysinit.target`, ordered after nothing) reads, 256 at a time, the pieces of root volume files that a first boot read (`/etc/fcvm-fleet/prefetch.list`, 2,825 pieces of 2,422 files, 421 MiB, of which 190 MiB are the NVIDIA modules), with the static `prefetch` binary built from `tile/prefetch.c`. The list is written by `tile/build.sh trace` (`tile/resident.py`: the page cache of a tile that served its first frame, mincore per file), run on a tile launched from the image being revised, and `bake` refuses an image without it |

```bash
H=$(fleet/ami/build-ami.sh launch host)                  # BASE_AMI=<ami> revises an earlier image
fleet/ami/build-ami.sh push $H
fleet/ami/build-ami.sh env $H FCVM_COMMIT=<commit>
fleet/ami/build-ami.sh start $H host/build.sh prep && fleet/ami/build-ami.sh wait $H prep
fleet/ami/build-ami.sh reboot $H                         # IOMMU on
fleet/ami/build-ami.sh start $H host/build.sh build && fleet/ami/build-ami.sh wait $H build
fleet/ami/build-ami.sh env $H STORE_S3_REGION=us-east-2 STORE_S3_BUCKET=<assets bucket> STORE_S3_PREFIX=store/<name>/
fleet/ami/build-ami.sh start $H host/build.sh bake && fleet/ami/build-ami.sh wait $H bake
for f in $(fleet/ami/build-ami.sh sh $H 'cd /var/lib/fcvm-fleet/store && ls *.tar.zst'); do   # each part, under STORE_S3_PREFIX
  U=$(fleet/ami/build-ami.sh put-url store/<name>/$f)
  fleet/ami/build-ami.sh sh $H "curl -sS -o /dev/null -w '%{http_code}\n' -X PUT -T /var/lib/fcvm-fleet/store/$f '$U'"   # must print 200
done
fleet/ami/build-ami.sh image $H fcvm-fleet-host-x86_64-<commit>-<date>
fleet/ami/measure-boot.sh host <ami>                    # and: measure-boot.sh tile <ami>
```

Measured from the launch request (`measure-boot.sh`, on demand): a tile serves its first
frame over HTTPS at 67.7 s and 83.0 s; a host is ready for the agent at 117.9 s, of which
47.5 s is the store fetched from S3 (from the AMI's own volume it took 419 s).

Tile boot, launch request to first frame over HTTPS (`measure-boot.sh tile`, on demand, us-east-1, g4dn.xlarge,
2026-10-27/28): 84.2, 45.5 and 58.6 s on the 728be8c3 image (one spot launch: 86.1 s); 36.6, 32.0 and 33.5 s with the
prefetch. On the 728be8c3 image's first boot, `systemd-modules-load` took 35.4 s (190 MiB of NVIDIA modules, read from a
volume whose blocks are fetched from S3 on first read) and `fcvm-fleet-renderer.service` 8.7 s, with 14 s from podman's
image pull to the container's init; with the prefetch they took 4.0 s and 2.9 s (`systemd-analyze blame`). The root volume's settings were measured by reading the same 421 MiB from a volume
made from the AMI's snapshot (`gp3`, 256 readers): 2.8 to 6.2 s at the default 125 MiB/s and 3000 IOPS, 1.8 s at 500
MiB/s, 0.55 s at 1000 MiB/s and 4000 IOPS, 0.42 s at 1000 MiB/s and 16000 IOPS. A volume initialization rate of 300
MiB/s read it in 6.2 s, no faster than the default. Tile launches with the volume at 1000 MiB/s and 4000 IOPS reached the
first frame in 19.5 s, and at 500 MiB/s and 3000 IOPS in 24.9 s, against 32.0 to 36.6 s at the default.
`measure-boot.sh` takes `VOLUME=Throughput=...,Iops=...` for this.

`bash fleet/ami/check.sh` runs the offline checks of these recipes (shellcheck, `bash -n`,
the phase-marker, security, factory and tile-readiness contracts). CI runs the Python
factory/security tests; run the complete recipe check before changing a recipe.

The wildcard certificate in `fcvm-fleet/tls-wildcard` is issued by
`fleet/tools/issue-cert.sh`, whose header documents the secret's format.

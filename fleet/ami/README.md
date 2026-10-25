# fleet/ami

The fleet's two machine images, built in the fcvm-fleet account (us-east-1) by
`build-ami.sh`, and `fleet/tools/gpubox.sh`, which leases a GPU host for one agent's work.

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
| `fcvm-fleet-host-x86_64-<commit>-<date>` (g4dn.metal) | `host/build.sh` prep, build, bake | `fcvm-fleet-vfio.service` binds every NVIDIA function to vfio-pci and fails unless each is passable; `fcvm-fleet-store.service` formats the instance-store NVMe as btrfs at `/mnt/fcvm-btrfs` and restores the store from the asset bucket (`/etc/fcvm-fleet/store.env`), falling back to the AMI's copy (`/var/lib/fcvm-fleet/store/`: the parts `host/store-pack.py` packs at bake, extracted at once); `fcvm-fleet-agent.service` is enabled and runs where the host's boot wrote `/etc/fcvm-fleet/agent.env` (ejc3/aws `fcvm_fleet_host_agent_args`), after the store; the agent dials the coordinator. Units a host does not use (podman's own, apport, fwupd and others, `host/build.sh` prep) are disabled, and the store unit waits for the network only, not for cloud-init's network stage |
| `fcvm-fleet-tile-x86_64-<commit>-<date>` (g4dn.xlarge) | `tile/build.sh` prep, image, bake | nvidia-persistenced, the certificate from `fcvm-fleet/tls-wildcard`, the renderer on 127.0.0.1:8080 behind nginx on 443 |

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

`bash fleet/ami/check.sh` runs the offline checks of these recipes (shellcheck, `bash -n`,
the phase-marker and tile-readiness contracts); CI does not run it, so run it before
changing a recipe.

The wildcard certificate in `fcvm-fleet/tls-wildcard` is issued by
`fleet/tools/issue-cert.sh`, whose header documents the secret's format.

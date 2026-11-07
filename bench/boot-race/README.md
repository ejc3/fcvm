# boot-race: time to a running workload

Measures the time from asking for compute to one small workload answering an HTTP GET from outside it, for fcvm
snapshot restores and for the AWS ways of starting the same workload, in one interleaved campaign. The page
`docs/boot-race.html` (GitHub Pages) is rendered from that campaign's records.

| lane | t0 | compute |
|---|---|---|
| `fcvm-file` | `fcvm snapshot run --snapshot <tag>` spawned | VM on the campaign host, memory file mapped (the wall's path) |
| `fcvm-uffd` | `fcvm snapshot run --pid <serve>` spawned | same, memory paged in by `fcvm snapshot serve` |
| `ec2-launch` | `RunInstances` | c8g.medium from an AMI holding the workload |
| `ec2-start` | `StartInstances` | the same AMI, stopped |
| `ec2-hibernate` | `StartInstances` | the same AMI, stopped with `Hibernate=true` |
| `ecs-fargate` | `RunTask` (FARGATE), read inside boot-race-launcher | 1 vCPU, 2 GiB, ARM64, no public IP |
| `ecs-ec2` | `RunTask` (EC2), read inside boot-race-launcher | one warm container instance, image cached, host network |
| `lambda-cold` | HTTPS GET to the function URL | the runtime exits after every response |
| `lambda-warm` | HTTPS GET to the function URL | the runtime keeps running |

t1 for every lane: a GET from the campaign host has read a complete `200` answer.

The ECS lanes start through boot-race-launcher (ejc3/aws `boot-race/launcher.py`), the only caller of RunTask, which
fixes the cluster, the task definitions and the Fargate network. It returns its wall-clock time just before and just
after RunTask; t0 is the time before, and the invoke's own cost is recorded beside each sample.

## Files

- `workload/`: the program every lane runs (std-only Rust, static). In HTTP mode it serves `GET /`; as a Lambda
  custom runtime `bootstrap` it runs the Runtime API loop, and exits after each response when
  `BOOT_RACE_EXIT_AFTER_RESPONSE=1`. Each answer carries a random id drawn at process start and the kernel boot id.
- `box-setup.sh` and `scratch.py`: prepare a fresh parallel box (host settings, Btrfs on verified instance storage,
  fcvm at a pinned commit, the workload image). Copy both files into the same directory.
- `fcvm-prep.sh`: boots the workload in a 1 vCPU, 2 GiB VM, snapshots it, starts the memory server.
- `prepare.py`: pushes the image, uploads the Lambda code, makes the AMI, launches the stopped and hibernated
  instances and the ECS container instance, and cleans up.
- `race.py`: the campaign. Each round runs every lane once; the order rotates by one lane per round. Samples are
  appended to `samples.jsonl` as they are taken, failures included.
- `render.py`, `page.html`: build `docs/boot-race.html` from the records; `render.py` refuses page data holding instance,
  image or account ids, ARNs or addresses. The raw samples and REPORT lines go to ejc3/fcvm-bench-records with
  errors scrubbed; `bench/boot-race/results/records-<campaign>.json` lists them.

## Running a campaign

The AWS resources and the `boot-race-driver` role are in ejc3/aws `boot-race.tf`. Once the campaign is
approved and the NVMe launch filter is deployed, use a fresh Ohio pbox with KVM and local instance storage:

```bash
pbox up N kvm-96 --local-nvme
```

That option tries only `c8gd.metal-24xl` and `m8gd.metal-24xl`. Plain `kvm-96` can select a box with
no instance store. After the pbox bootstrap has mounted `/mnt/nvme`, copy `box-setup.sh` and `scratch.py`
together and run `box-setup.sh <fcvm commit>` on the pbox before starting any other jobs there. It verifies the exact mounted block device and
all of its backing disks before installing anything. It converts only the empty pbox ext4 filesystem
labeled `local-nvme` to the benchmark Btrfs mount. An already prepared mount is reused only after the
same storage checks. If the check fails, use a fresh compatible pbox; keep persistent work and root disks
out of the benchmark. Setup restores the original ext4 mount on pre-format failures when the same
filesystem can still be verified. If formatting starts but Btrfs cannot mount, setup stops; a fresh
compatible pbox is the safe retry.

With fcvm built and set up:

```bash
make build-boot-race-workload                      # copy the binary to the box, podman build workload/Containerfile
bench/boot-race/fcvm-prep.sh ./target/release/fcvm $RESULTS   # prints serve_pid
# cfg.json: results, fcvm_bin, fcvm_snapshot, fcvm_serve_pid, workload_bin, workload_unit
for step in image lambda ami instances ecs-host; do python3 bench/boot-race/prepare.py --config cfg.json $step; done
python3 bench/boot-race/race.py --config cfg.json run --rounds 10 --first-round 0   # repeat with fresh credentials
python3 bench/boot-race/race.py --config cfg.json reports                           # Lambda REPORT lines
python3 bench/boot-race/prepare.py --config cfg.json cleanup
python3 bench/boot-race/render.py $RESULTS --records-out bench/boot-race/results/$CAMPAIGN  # page + scrubbed records
git add bench/boot-race/results/$CAMPAIGN      # staged, never committed: records.py moves them out
python3 bench/chromium/records.py move --campaign $CAMPAIGN --records $RECORDS bench/boot-race/results/$CAMPAIGN
python3 bench/chromium/records.py manifest --campaign $CAMPAIGN --records $RECORDS bench/boot-race/results/$CAMPAIGN
```

`make test-boot-race` runs the workload and Python harness tests, including the storage guard regressions.

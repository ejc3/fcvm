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
| `ecs-fargate` | `RunTask` (FARGATE), read inside boot-race-launcher | 1 vCPU, 2 GiB, ARM64, no public IP; **not run** (below) |
| `ecs-ec2` | `RunTask` (EC2), read inside boot-race-launcher | one warm container instance, image cached, host network |
| `lambda-cold` | HTTPS GET to the function URL | a cold start: before each sample, an untimed code upload retires the function's execution environments; a sample without an Init Duration is refused |
| `lambda-warm` | HTTPS GET to the function URL | the runtime keeps running |

t1 for every lane: a GET from the campaign host has read a complete `200` answer.

The published campaign runs eight lanes, every one but `ecs-fargate`. The AWS side (ejc3/aws `boot-race.tf`) keeps
the Fargate lane off: a task without a public IP pulls its image through ECR interface endpoints, which bill by the
hour and change ECR DNS for the whole VPC, and they have no reviewed teardown yet. boot-race-launcher refuses the lane
while its subnet is unset. The page names it as not run, with that reason.

The ECS lanes start through boot-race-launcher (ejc3/aws `boot-race/launcher.py`), the only caller of RunTask, which
fixes the cluster, the task definitions and the Fargate network. It returns its wall-clock time just before and just
after RunTask; t0 is the time before, and the invoke's own cost is recorded beside each sample.

## Files

- `workload/`: the program every lane runs (std-only Rust, static). In HTTP mode it serves `GET /`; as a Lambda
  custom runtime `bootstrap` it runs the Runtime API loop, and exits after each response when
  `BOOT_RACE_EXIT_AFTER_RESPONSE=1`. Each answer carries a random id drawn at process start and the kernel boot id.
- `box-setup.sh` and `scratch.py`: prepare a fresh benchmark worker (host settings, Btrfs on verified instance storage,
  fcvm at a pinned commit, the workload image). Copy both files into the same directory.
- `fcvm-prep.sh`: boots the workload in a 1 vCPU, 2 GiB VM, snapshots it, starts the memory server.
- `prepare.py`: pushes the image, uploads the Lambda code, makes the AMI, launches the stopped and hibernated
  instances and the ECS container instance, cleans up, and checks that nothing is left (`leftovers`).
- `worker_aws.py`: assumes `boot-race-driver` from the worker’s own EC2 instance role, and refreshes both
  roles in memory.
- `race.py`: the campaign. Each round runs every lane once; the order rotates by one lane per round. Samples are
  appended to `samples.jsonl` as they are taken, failures included.
- `render.py`, `page.html`: build `docs/boot-race.html` from the records and `setup.json` (written by
  `race.py setup`); `render.py` refuses page data holding instance,
  image or account ids, ARNs or addresses. The raw samples and REPORT lines go to ejc3/fcvm-bench-records with
  errors scrubbed; `bench/boot-race/results/records-<campaign>.json` lists them.

## Running a campaign

A campaign is 30 rounds (30 samples per lane), run once, on one fresh campaign box, with the AWS side enabled
(ejc3/aws `boot_race_enabled`). The AWS resources and the `boot-race-driver` role are in ejc3/aws
`boot-race.tf`. The reaper there terminates any boot-race instance whose current run began more than 3 hours ago,
so the ECS container instance (launched by `ecs-host`, never restarted) bounds the time from `ecs-host` to the end
of `cleanup`. Lambda recycles an idle environment after some minutes, so `lambda`, which primes the warm function,
runs after `ecs-host` and `run` starts right after it.

1. On primary FCVM ARM, use the dedicated Ohio benchmark worker managed by [ejc3/aws](https://github.com/ejc3/aws).
   From that checkout, run:

   ```bash
   scripts/benchmark-worker.sh up
   scripts/benchmark-worker.sh ssh 'cloud-init status --wait'
   ```

   Save the instance ID printed by `up` for teardown. This is a paid bare-metal Spot worker with local
   instance storage. Copy files by streaming a tar archive over `scripts/benchmark-worker.sh ssh`. An
   ordinary pbox does not have the benchmark worker role.
2. Build the workload on an arm64 machine (`make build-boot-race-workload`) and copy the binary,
   `workload/Containerfile`, `workload/boot-race.service`, `box-setup.sh` and `scratch.py` to `~/boot-race/` on the
   box, with the binary and Containerfile also in `~/boot-race/img/`. Run `box-setup.sh <fcvm commit>` there before
   anything else. It verifies the exact mounted block device and all of its backing disks before installing
   anything, and converts only the empty worker ext4 filesystem labeled `local-nvme` to the benchmark Btrfs mount. An
   already prepared mount is reused only after the same storage checks. If the check fails, use a fresh
   benchmark worker. Setup restores the original ext4 mount on pre-format failures when the same filesystem can still be
   verified; if formatting starts but Btrfs cannot mount, setup stops and a fresh benchmark worker is the safe retry.
3. On the worker, from `/mnt/fcvm-btrfs/src/fcvm`, with `RESULTS=/mnt/fcvm-btrfs/boot-race/results`.
   Every AWS command in the harness uses IMDSv2 on the worker to assume its own `boot-race-driver` session.
   The worker refreshes the session automatically; no credential files, SSH relay, or AWS CLI login are needed.

   ```bash
   bench/boot-race/fcvm-prep.sh ./target/release/fcvm $RESULTS   # prints fcvm_proc_id and serve_pid
   # cfg.json: results, fcvm_bin, fcvm_snapshot (boot-race), fcvm_serve_pid, fcvm_proc_id,
   #           workload_bin (~/boot-race/boot-race-workload), workload_unit (~/boot-race/boot-race.service)
   for step in image ami instances ecs-host lambda; do python3 bench/boot-race/prepare.py --config cfg.json $step; done
   # Fargate is disabled in AWS; resume an interrupted run with --first-round K.
   python3 bench/boot-race/race.py --config cfg.json run --rounds 30 \
     --lanes fcvm-file,fcvm-uffd,ec2-launch,ec2-start,ec2-hibernate,ecs-ec2,lambda-cold,lambda-warm
   python3 bench/boot-race/race.py --config cfg.json reports            # Lambda REPORT lines
   python3 bench/boot-race/race.py --config cfg.json setup              # setup.json, read from the host and AWS
   python3 bench/boot-race/prepare.py --config cfg.json cleanup
   python3 bench/boot-race/prepare.py --config cfg.json leftovers       # fails unless nothing is left billing
   ```

   `cleanup` stops the campaign's ECS tasks directly even if the monthly budget denies the launcher.
   The AWS driver policy must grant `ecs:StopTask` on the boot-race cluster before starting a campaign.

   Copy `$RESULTS` back to the dev box while the run goes; it is on the box's instance store.
4. Stop `fcvm snapshot serve` and copy the results back. On ARM, run
   `scripts/benchmark-worker.sh down i-...` with the instance ID from step 1.
5. On the dev box, render the page and the scrubbed records, then move the raw records out of the repository:

   ```bash
   python3 bench/boot-race/render.py $RESULTS --records-out bench/boot-race/results/$CAMPAIGN  # page + scrubbed records
   git add bench/boot-race/results/$CAMPAIGN      # staged, never committed: records.py moves them out
   python3 bench/chromium/records.py move --campaign $CAMPAIGN --records $RECORDS bench/boot-race/results/$CAMPAIGN
   git -C $RECORDS add bench/boot-race/results/$CAMPAIGN && git -C $RECORDS commit -m "boot-race $CAMPAIGN" && git -C $RECORDS push
   python3 bench/chromium/records.py manifest --campaign $CAMPAIGN --records $RECORDS bench/boot-race/results/$CAMPAIGN
   ```

Every sample records the campaign host's 1-minute load average when it started; the page prints its range.

`make test-boot-race` runs the workload and Python harness tests, including the storage guard regressions.

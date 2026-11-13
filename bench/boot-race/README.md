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
| `lambda-cold` | HTTPS GET to the function URL | the runtime exits after every response; the page counts how often Lambda kept the kernel |
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
  instances and the ECS container instance, cleans up, and checks that nothing is left (`leftovers`).
- `creds.sh`: runs on the dev box and keeps `boot-race-driver` credentials fresh on the campaign box.
- `race.py`: the campaign. Each round runs every lane once; the order rotates by one lane per round. Samples are
  appended to `samples.jsonl` as they are taken, failures included.
- `render.py`, `page.html`: build `docs/boot-race.html` from the records and `setup.json` (written by
  `race.py setup`); `render.py` refuses page data holding instance,
  image or account ids, ARNs or addresses. The raw samples and REPORT lines go to ejc3/fcvm-bench-records with
  errors scrubbed; `bench/boot-race/results/records-<campaign>.json` lists them.

## Running a campaign

A campaign is 30 rounds (30 samples per lane), run once, on one fresh campaign box, with the AWS side enabled for
that one run (ejc3/aws `boot_race_enabled`). The AWS resources and the `boot-race-driver` role are in ejc3/aws
`boot-race.tf`. The reaper there terminates any boot-race instance whose current run began more than 3 hours ago,
so the ECS container instance (launched by `ecs-host`, never restarted) bounds the time from `ecs-host` to the end
of `cleanup`. Lambda recycles an idle environment after some minutes, so `lambda`, which primes the warm function,
runs after `ecs-host` and `run` starts right after it.

1. On the dev box, launch a fresh Ohio pbox with KVM and local instance storage, and carry its instance id:

   ```bash
   pbox up N kvm-96 --local-nvme --lease 6h
   ```

   That option tries only `c8gd.metal-24xl` and `m8gd.metal-24xl`; plain `kvm-96` can select a box with no instance
   store. The lease keeps the idle watchdog off a quiet box (the fcvm lanes need one). Use `pbox ip N --instance
   i-...` for every later step.
2. Build the workload on an arm64 machine (`make build-boot-race-workload`) and copy the binary,
   `workload/Containerfile`, `workload/boot-race.service`, `box-setup.sh` and `scratch.py` to `~/boot-race/` on the
   box, with the binary and Containerfile also in `~/boot-race/img/`. Run `box-setup.sh <fcvm commit>` there before
   anything else. It verifies the exact mounted block device and all of its backing disks before installing
   anything, and converts only the empty pbox ext4 filesystem labeled `local-nvme` to the benchmark Btrfs mount. An
   already prepared mount is reused only after the same storage checks. If the check fails, use a fresh compatible
   pbox. Setup restores the original ext4 mount on pre-format failures when the same filesystem can still be
   verified; if formatting starts but Btrfs cannot mount, setup stops and a fresh pbox is the safe retry.
3. Start `creds.sh <ssh to the box>` on the dev box and leave it running until step 6. It assumes
   `boot-race-driver` (one hour at most) and writes `/dev/shm/boot-race-creds.json` on the box every 30 minutes.
4. On the box, from `/mnt/fcvm-btrfs/src/fcvm`, with `RESULTS=/mnt/fcvm-btrfs/boot-race/results`:

   ```bash
   bench/boot-race/fcvm-prep.sh ./target/release/fcvm $RESULTS   # prints fcvm_proc_id and serve_pid
   # cfg.json: results, fcvm_bin, fcvm_snapshot (boot-race), fcvm_serve_pid, fcvm_proc_id,
   #           workload_bin (~/boot-race/boot-race-workload), workload_unit (~/boot-race/boot-race.service)
   for step in image ami instances ecs-host lambda; do python3 bench/boot-race/prepare.py --config cfg.json $step; done
   python3 bench/boot-race/race.py --config cfg.json run --rounds 30    # resume an interrupted run with --first-round K
   python3 bench/boot-race/race.py --config cfg.json reports            # Lambda REPORT lines
   python3 bench/boot-race/race.py --config cfg.json setup              # setup.json, read from the host and AWS
   python3 bench/boot-race/prepare.py --config cfg.json cleanup
   python3 bench/boot-race/prepare.py --config cfg.json leftovers       # fails unless nothing is left billing
   ```

   Copy `$RESULTS` back to the dev box while the run goes; it is on the box's instance store.
5. Stop `fcvm snapshot serve`, copy the results back, and `pbox down N --instance i-...`.
6. Stop `creds.sh`, and open the ejc3/aws change that sets `boot_race_enabled` back to false for review.
7. On the dev box, render the page and the scrubbed records, then move the raw records out of the repository:

   ```bash
   python3 bench/boot-race/render.py $RESULTS --records-out bench/boot-race/results/$CAMPAIGN  # page + scrubbed records
   git add bench/boot-race/results/$CAMPAIGN      # staged, never committed: records.py moves them out
   python3 bench/chromium/records.py move --campaign $CAMPAIGN --records $RECORDS bench/boot-race/results/$CAMPAIGN
   git -C $RECORDS add bench/boot-race/results/$CAMPAIGN && git -C $RECORDS commit -m "boot-race $CAMPAIGN" && git -C $RECORDS push
   python3 bench/chromium/records.py manifest --campaign $CAMPAIGN --records $RECORDS bench/boot-race/results/$CAMPAIGN
   ```

Every sample records the campaign host's 1-minute load average when it started; the page prints its range.

`make test-boot-race` runs the workload and Python harness tests, including the storage guard regressions.

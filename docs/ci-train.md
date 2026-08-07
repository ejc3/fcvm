# CI Merge Train (Pooled CI)

Full-matrix CI on this repo is expensive: 8 self-hosted runners, ~8 jobs per
PR, with the Host-Root jobs taking ~30 minutes. Running the whole matrix once
per PR is the right default for risky changes — but for a batch of small,
independent, low-risk PRs it is mostly redundant.

The CI merge train pools k PRs onto one branch (`ci-train`), runs the full
matrix **once** on the combined tree, and lands every PR in the batch when
that single run is green. On red, the batch is bisected. This is
[Dorfman pooled testing](https://en.wikipedia.org/wiki/Group_testing) applied
to CI.

## Cost math

Let p be the probability an individual PR would fail the matrix, and k the
batch size. Pooling costs roughly:

```
runs per PR  ≈  1/k  +  p·log2(k) · c        (c ≈ 2: each bisect level runs two halves)
```

versus exactly 1 run per PR without pooling. For low-risk batches:

| k | p    | runs/PR (c=2) | saving |
|---|------|---------------|--------|
| 5 | 0.01 | ~0.25         | ~4x    |
| 5 | 0.05 | ~0.43         | ~2.3x  |
| 8 | 0.01 | ~0.19         | ~5x    |
| 5 | 0.35 | ~1.0          | none   |

Break-even is around p ≈ (1 − 1/k) / (2·log2 k) — about 0.35 for k = 5. So
pooling pays off unless the PRs you batch fail CI more than a third of the
time, in which case they were not low-risk PRs and should not have been
pooled.

## When to pool

Pool PRs that are **independent** and **individually low-risk**:

- dependabot / dependency bumps
- docs and comment-only changes that still trigger CI
- small isolated fixes with no cross-coupling

Do **not** pool:

- PRs with entangled changes (same subsystem, overlapping code): a red train
  cannot attribute the failure to one PR, and a merge conflict between
  batch-mates is a strong signal they belong in a stacked series instead
- risky or large changes that deserve individual attribution and their own
  green checkmark
- anything where you would want to bisect *within* the PR on failure

A train validates the **combined** tree. That is exactly what `main` will
contain after the batch lands (the PRs merge in the same order), but no PR in
the batch gets an individual run — the PR comment left by `land` records
which train run vouched for it.

## Workflow

```bash
make train-create PRS="689 690 691"   # build ci-train = origin/main + each PR, in order
make train-dispatch                    # push ci-train, dispatch ONE full ci.yml matrix
make train-status                      # watch it
make train-land                        # green? merge every batch PR, in order
make train-bisect                      # red?  split into ci-train-a / ci-train-b
```

All state (PR list, head SHAs, base SHA, run id) is recorded in
`.git/ci-train-manifest.json`, so `land` and `bisect` are deterministic: they
operate on what was batched, not on whatever the PRs look like later.

Safety rails (`scripts/ci-train.sh`):

- never touches `main`; only `ci-train*` branches
- refuses to run in a checkout with uncommitted changes
- `create` refuses closed, stacked (non-`main` base), and fork PRs
- `land` refuses unless the green run's head SHA is **exactly** the current
  `ci-train` tip, and every PR's current head SHA is still the one that was
  batched — a force-push after batching means the train validated something
  else, so re-create the train
- `land` explains itself in a comment on every PR it merges, naming the train
  run and the batch

### Merge conflicts during `create`

`create` merges each PR sequentially and **stops** on conflict, naming the
conflicting PR and leaving the tree mid-merge — it never auto-resolves.
Resolve by hand, `git add` + `git commit --no-edit`, then resume with
`make train-create CONTINUE=1`.

Treat a conflict as a warning: if two PRs conflict because they edit the same
logic (not just adjacent lines, as with two dependabot bumps in one workflow
file), they are entangled — abort the train and run them separately.

Note that a conflict resolved on the train must be re-resolved at land time
if GitHub cannot merge the second PR onto the moved `main`; `land` stops with
instructions when that happens.

### Bisect protocol

On a red train, `make train-bisect` splits the batch in half by batch order,
builds `ci-train-a` and `ci-train-b` from the **same recorded base and head
SHAs** (so only batch membership changes), and dispatches both. Then:

- **green half** → land it: `make train-land TRAIN=ci-train-a`
- **red half with one PR** → culprit found: fix or drop it, land the rest
- **red half with >1 PRs** → recurse: `make train-bisect TRAIN=ci-train-a`
  (produces `ci-train-a-a` / `ci-train-a-b`)
- **both halves green** → the failure needed the combination: the PRs are
  entangled; do not pool them

## Alternative: GitHub's native merge queue

GitHub has a built-in merge queue (repo Settings → General → merge queue,
plus a `merge_group:` trigger in `ci.yml`, plus required checks configured
for the `merge_group` event on every PR). Honest comparison:

**Merge queue pros:**

- native and fully automated: click "merge when ready", no scripting, no
  operator driving the batch
- serializes `main` correctly by construction; failed entries are ejected
  and the queue rebuilds automatically

**Merge queue cons:**

- **it does not reduce CI cost.** The queue dispatches checks per queue
  entry (each entry = main + all entries ahead of it + this PR), so k queued
  PRs still cost ~k full runs — more with requeues. It buys automation and
  correctness, not runner-hours, which is the whole point of pooling here.
- batches are opaque: you don't choose or see the batch composition, and
  there is no manifest to bisect deterministically
- no manual conflict resolution: a PR that conflicts is simply ejected
- every PR needs required checks configured for `merge_group`, and the
  workflow needs the `merge_group` trigger before the setting is enabled

**Recommendation:** if the goal is hands-off serialization of `main` and
runner cost does not matter, enable the merge queue. If the goal is cutting
self-hosted runner cost on batches of low-risk PRs — the situation this repo
is in — use the train. The two do not compose on the same PR (a queued PR is
validated by the queue, a trained PR by the train), so pick per-PR.

Enabling the merge queue is a repo-settings change; it is deliberately not
enabled by this tooling.

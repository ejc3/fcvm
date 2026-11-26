# Pooled CI

The self-hosted lanes of `ci.yml` (the Host, Host-Root and Container jobs) run on metal
instances. Testing every pull request alone runs them once per pull request per push.
A pool tests several pull requests in one run of those lanes: if the pool passes, every
member passed, and if it fails, a few further runs of the failed lanes find the member
that broke it. `scripts/ci-pool/ci_pool.py` builds pools and accounts for the cost.

## What a pool is

A pool is a chain of open pull requests of this repository. Each member's base is the
branch of the member below it, the bottom's base is `main`, and each member's branch
contains the branch below it by merge. Nothing else records membership: the pull
requests' bases are the list, so every session reads the same pool from GitHub.

Because each branch contains the ones below, a member's head is the tree of a prefix of
the pool: the bottom's head is the bottom alone, the top's head is every member.
`scripts/ci-pr-scope.sh` already runs the self-hosted lanes only on a chain's top; the
lower members' runs defer to it.

Only a ready pull request whose branch is in this repository and whose author is a
collaborator can be a member. A pool's run has the repository's secrets and runners, so
a fork's pull request never joins one. A draft runs no CI and defers nothing, so it can
sit above a pool's top but not inside the chain.

## Commands

```bash
python3 scripts/ci-pool/ci_pool.py build 2101 2102 2103   # bottom to top
python3 scripts/ci-pool/ci_pool.py build --of 2102        # rebuild the pool #2102 is in
python3 scripts/ci-pool/ci_pool.py build --sync-main --of 2102
python3 scripts/ci-pool/ci_pool.py publish                # this checkout's commits, into its pool
python3 scripts/ci-pool/ci_pool.py status
python3 scripts/ci-pool/ci_pool.py cost --days 7
python3 scripts/ci-pool/ci_pool.py failures --days 7
```

`build` merges each member into the one above it in a temporary work tree, sets each
pull request's base, and pushes every branch that changed in one atomic push. It never
rebases and never forces a push: a branch that moved since the lookup blocks the build
or refuses the push, and the answer to both is to run it again. `--sync-main` also merges
`main` into the bottom, which is what makes the top's tree the tree a merge to `main`
produces.

A member that conflicts with the pool below it is reported with the conflicting files
and left out: it is not merged and not pushed, and the members above it are built on the
member below it. If a later member already contains the conflicting one (the pool was
built before and the conflict is new), leaving it out would keep its commits in the pool
under another member's name, so the build stops and pushes nothing. In both cases the
owner of the conflicting member merges the branch below into theirs, resolves, and
publishes.

`publish` is how a member's owner adds commits: commit locally, then publish. It builds
the pool starting that member from the local commits, so the member and every member
above it move in the same push and only the top runs the lanes. A plain `git push` of a
branch in the middle starts that member's own run of the lanes, because the member above
does not contain the new commit yet.

Fixes to several members go in together: `build --of <top> --pending <branch>=<commit>`,
once per member, starts each named member from that commit (it must contain the member's
remote branch) and pushes every branch in the same atomic push, so the top's run restarts
once however many members changed.

`status` prints each pool bottom to top, and for every lower member the `Stack CI` commit
status its head carries: the top's passing run posts it on each lower head it contains
(`scripts/ci-stack-coverage.sh`). A lower head without it has not been tested, whatever
its own `Summary (deferred)` says. The tool posts no status: whether a head was tested is
something only a run can state.

## What main runs after a landing

`ci_pool.py plan` is the `skip-check` job of `ci.yml`: it decides which self-hosted lanes
a run takes. A pull request takes every arm64 lane and Host-Root-x64-SnapshotEnabled
(and Host-x64 when it touches the VMM's confinement). A dispatch takes the lanes named
in its `lanes` input, by exact job name, all of them by default; a name that is not a
lane fails the job. A push to `main` takes every lane that is not already proven for its
tree, and anything the planner cannot look up counts as not proven.

A lane is proven for a pushed commit by a job of exactly that lane's name that succeeded
on its first attempt, in a run of `.github/workflows/ci.yml`, when the lane never failed
in any counted run on that commit (a second run that passes does not undo a first that
failed), on a commit for which all of this holds:

- It is one of the pushed commit's other parents, or the head of a pull request of this
  repository that GitHub ties to the pushed commit. This is how the evidence is found:
  by the commit, not by searching recent runs.
- Its tree is the pushed commit's tree. The tree is read from the commit, never from
  anything the run reported.
- It contains `main` as it was before the push. "Before the push" is the push event's
  own `before`, not the pushed commit's first parent: after a fast-forward of several
  commits the first parent already holds part of the change. A push that names no
  earlier commit proves nothing.
- The run recorded the pushed commit's tree as the tree it tested. A pull request's run
  tests the merge of its head into its base, and the Actions API records only the head.
  The same head can be run against a base it does not contain: a second pull request of
  the same commit into another branch, or a stack member whose lower branch had moved.
  Such a run tested a tree that never lands, under that base's Makefile. So every run of
  the planner writes the tree of the commit it was given as a notice titled
  `ci-pool tested tree` on the `Skip Check` job, and the planner on `main` reads it back
  from that job and no other (`Skip Check` runs no test, so nothing a test prints can
  write it). A run with no record, or with two that differ, counts for nothing, neither
  for a lane nor against one.
- Between `main` before the push and that commit, no file that defines the lanes changed.
  A pull request's run executes the pull request's own copy of the workflow, the Makefile
  and the scripts, so a change to one of them could produce a job with a lane's name that
  tested nothing. Such a landing runs every lane on `main`, under the files that landed.
  A comparison of 300 files or more may be cut short by the API and proves nothing
  either, and neither does one that says the commit is ahead and lists no file.

Which files define the lanes is derived, not listed by hand: `ci_pool.py lane-files`
reads `ci.yml` and the Makefile and gives

- `ci.yml`, the Makefile, `.config/nextest.toml`, `rust-toolchain.toml`, `.cargo/` and
  every Containerfile;
- the local actions used by the jobs that are lanes or decide which lanes run
  (`changes`, `skip-check`, `host`, `host-root`, `container`);
- every script those jobs name, every script named by the Makefile text that runs when
  a target they make is made (recipes, prerequisites, the targets recipes make in turn,
  the variables all of that uses, and definitions that run a shell command when the
  Makefile is read), and every script those scripts name in turn, by path or, within one
  directory, by file name. A name followed by a glob or a variable, and a directory's
  name, stand for everything that starts with them.

The result is committed as `scripts/ci-pool/lane-defining.txt`, and a test fails when the
committed list is not what the derivation gives, so a script that `ci.yml` or the
Makefile starts to call cannot stay off it. `ci.yml`, the Makefile and everything under
`scripts/ci-pool/` count whatever the list says. When the list cannot be read or is
empty, everything under `.github/` and `scripts/` counts. The derivation cannot follow a
script whose name is built at run time; a new way of calling one is a change to `ci.yml`,
the Makefile or a listed script. A Makefile target that cannot be followed makes the
whole Makefile's scripts count.

Each lane is judged alone. A lane that passed in a run whose `Summary` failed for another
job is proven; a lane that passed only when it was run again is not, and runs on `main`.
The one case where a run speaks for all lanes: a pull request's run that reported
`Summary` (never `Summary (deferred)`) with every self-hosted job skipped by its own
scope means the tree needs no lane.

The record of the tested tree is written by the run's own copy of the tool. A run whose
merge held another copy (a base branch with an edited `scripts/ci-pool/`) could write a
false one; that takes a deliberate edit, which is what review of changes to those files
is for. A base with an older copy writes none, and its runs count for nothing.

In the week of 2026-11-19, 114 first-parent commits on `main` touched code. 17 of them
changed a file on the list (7 the Makefile, 6 `ci.yml`, 4 a listed script) and would run
every lane on `main`. Counting everything under `.github/` and `scripts/` instead, it
would be 31.

## Cost

`cost` lists the workflow runs created in a date range and every job of each (all
attempts), and prints runs, jobs, runner minutes and dollars by workflow, event and
runner type, and by lane. Dollars are job minutes at an hourly price recorded in the
script with its source and date. There is no pricing call, and a host also bills while
it boots and while it waits for its next job, which the Actions API does not report, so
the figure is a lower bound on the bill.

The account fails closed. The runs API returns at most 1000 runs per search and reports
the true count only in `total_count`, so each window's listing is checked against it and
a window at the cap is halved. A finished run whose jobs cannot be read makes the account
`INCOMPLETE` with exit status 3. Job lists of finished runs are cached under the git
directory, without runner names, so a second call asks only for what is new;
`--max-requests` bounds what one call may spend of the account's hourly API limit, and
`--skip-short MINUTES` leaves out first-attempt runs whose whole wall time was under
MINUTES (a deferred member's run, a schedule that found nothing to do), printing how many
and their total wall time. In the week of 2026-11-19 that was 1,758 of 2,914 runs and 783
minutes at 3 minutes.

It also prints how often a pull request's run of the lanes failed on its first attempt,
p, and the expected runs per member for pool sizes 2 to 16:

- two-stage (Dorfman): `1/k + 1 - (1-p)^k`, cheapest near `1/sqrt(p)`;
- with this tool's red path: `(1 + q*f/(1-q))/k + m * (f * (1 + ceil(log2 k)) + 1)`, against
  `(1 + m)/(1 - q)` for a pull request tested alone. f is the share of a run's cost that
  the failed lanes were. q is the rate at which a run fails for a cause of its own (a lost
  runner, a test that does not always pass), which a pool meets once whatever its size.
  m is the rate at which a change is itself broken: each such member costs a rerun of the
  failed lanes, a bisect, and one more pool run after its fix.

p is split into q and m by what reruns showed: a failed run that passed when the same
commit ran again counts toward q, one that failed again toward m. With fewer than ten
reruns in the range there is no split and every failure is charged to the change, the
case least favourable to pooling. Both columns are printed. The recommendation is the smallest
size within 10% of the fewest expected runs under the split, or none when no size beats
testing alone: a larger pool also holds every member back while one culprit is fixed,
which a count of runs does not price. Runs that someone chose to rerun are not a random sample of failures, so the split
is an estimate; once failed pools are always rerun once, it stops being a selected one.

## Failures

`failures` lists, for the same kind of date range, every failed attempt of a self-hosted
lane by cause: the tests nextest lists as not passed at the end of the job's log, or, when
there is none, the error GitHub recorded and the step it ended (a lost runner, a job time
limit, a setup step). For each cause it prints how many attempts failed, how many were run
again in the same run and passed, the first and last day seen, the lanes, and the dollars
of the failed attempts. A job with several failed tests counts once for each and its
dollars are split between them.

It reads the last 3 MB of each job's log, once, and caches only the causes. A runner that
vanishes uploads no log, so a job without one is classified from GitHub's annotation on it
(`runner lost: no contact`). A job with neither is a row of its own (`log not available`) and makes the table
`INCOMPLETE`; a log that shows no failure in its last 3 MB says so. A cause is never
guessed. `CI_POOL_LOGS` names a directory of `<job id>.log` files to read instead.

## When to publish

Every push to a pool's top starts a new run of the lanes and cancels the one in flight.
In the week of 2026-11-19, before pooling, 164 of 265 pull request runs of the lanes were
cancelled by a newer push, $165 of the $478 those runs cost. A pool does the same to
itself if its members publish one at a time while a run is in flight: a lane takes 34 to
57 minutes (medians), so a publish every half hour means no run ever finishes. Until the
tool holds publishes back itself, publish once per round of fixes, and when a run is in
flight let it finish unless what you publish fixes the failure it is about to report.

## Moving an existing stack to this tool

A stack kept by hand or by another script is already a pool if its pull requests' bases
form the chain and each branch contains the one below: `status` reads it as it is, with
nothing to convert. From then on `publish` and `build --of` replace whatever merged and
pushed it before. The tool keeps no list of its own, so there is no state to carry over.

## API use

The account's REST limit is shared by every session. `build`, `publish` and `status` make
one paginated listing of the open pull requests per call and one status read per member.
`cost` and `failures` list runs once per day of the range, and read a finished run's jobs
and a failed job's log once ever: both are cached under the git directory.

## Tests

`make test-ci-pool` (`scripts/test-ci-pool.py`, run by the `actionlint` job) drives the
tool against a fake `gh` and git repositories in a temporary directory.

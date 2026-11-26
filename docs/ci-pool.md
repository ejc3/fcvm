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

`status` prints each pool bottom to top with the `ci/pool` commit status on every
member's head.

## What a run states: the `ci/pool` status

Every pull request run of `ci.yml` ends with the `Pool status` job (`ci_pool.py mark`),
which states what the run showed:

- `pending` on its own head when it tested nothing (a draft, or a lower member whose
  lanes run on the top). The pull request then shows a pending status, not a row of
  green checks under `Summary (deferred)`.
- `success` on its own head, and on the head of every lower member that head contains,
  when its `Summary` passed. The description names the run, and any lane that passed
  only on a rerun. The walk down stops at the first member whose head is not an ancestor
  of the tested head.
- `failure` on its own head only when its `Summary` failed, naming the failed lanes or
  the other job that failed and how many lanes passed. Which member broke a pool is not
  known from one run, so no lower member is marked.

The status is a display for people and sessions. Nothing reads it to decide what to
skip or land: `plan` and `land` read the runs. A success is stated only by the run that
passed; the one status the tool posts from a session is the `failure` that `bisect`
puts on a first failing prefix.

## When the top is red

`ci_pool.py bisect --of <member>` reads the runs on every member's head and prints what
they show and the next step. With `--go` it takes that step. It keeps no state: run it
again after the run it started has finished, from any session.

1. The failure's facts: the failed lanes of the top, and for each the tests its log ends
   with (as `failures` reads them).
2. One rerun of the failed jobs, when a failure has been seen once. Not when the same
   test already failed on two lanes: those were two hosts. A cause that is not a test (a
   lost runner, a setup step, a missing log) is always run again, however many lanes it
   took.
3. If the rerun passes, the pool is not simply green: the lane failed and passed on one
   commit. Each cause is appended to the open issue that owns it (an issue body lists
   the causes it owns as `<!-- ci-pool-cause: X -->`; X also owns causes that start with
   X and a space, so one issue can own every case of a test binary), or a new issue is
   opened. The pool may land with that issue open; `main` runs those lanes again.
4. If it fails again, the failing lanes are run on a lower member's branch
   (`workflow_dispatch` with `lanes`), halving the unknown prefixes each time: about
   log2 of the pool size runs, each of one or two lanes, not of the matrix. The lanes
   are the cheapest that between them show every failing cause. Runs that already exist
   count: a member that was the top earlier has its lanes on record. If exactly one
   member changed the failing test's file, the prefixes around it are run first. That
   only orders the runs.
5. The verdict is the first failing prefix: the member at which the failure appears,
   given the members below it. `bisect --go` puts a `ci/pool` failure on that member's
   head. It does not say the member fails alone, because no run shows that: on
   2026-11-25 a pool of nine failed one test that the member which wrote it and the
   member above it each passed without the other. The fix goes forward: the owner
   publishes a repair, or a revert of the member on its own branch, and the pool's next
   run is the confirmation. Two culprits take two rounds.

A red top may cost 8 extra runs (reruns and probes, counted from the runs themselves).
Past that, or when a prefix both failed and passed, a longer prefix passes above one
that fails, a prefix fails for another cause, or `main` itself fails those lanes, the
outcome is `NOT-ISOLATED`: no member is named and none is called tested.

`bisect` never dispatches on `main`, and it never cancels a run.

## Landing

`ci_pool.py land --of <member>` lands a pool with one push to `main`: it sets every
member's base to `main` (a base edit starts no run), then merges the top pull request
with one merge commit of the tested head, which closes every member as merged. Without
`--go` it only says what it would do.

It lands nothing unless, on the head that would land:

- every lane a pull request takes passed (`bisect` would say GREEN or PASSED ON RERUN),
  in a run that has finished. A run that named some lanes with the `lanes` input does not
  count for the others;
- the head contains `main`'s tip. Then the merge commit has the head's tree, and `main`'s
  planner finds the lanes proven for it. `build --sync-main` gets a pool there; that
  moves the top, so its run has to finish again. If `main` moves between the check and
  the merge, the merge still happens, the tree that lands is one no run tested, `land`
  says so, and `main` runs every lane;
- the head contains every member's head;
- no other job of the head's newest finished run failed. A failed job that is not a lane
  can be accepted by name, with the issue that owns the failure:
  `--despite "Fleet Web=#2177"`. The merge commit's message records it. A lane cannot be
  accepted this way.

A pool that passed only on a rerun lands with an issue filed or appended for each cause,
named in the merge commit's message, and `main` runs those lanes again.

`--through <member>` lands the lower part of a pool up to that member. Its head needs a
run of its own first: as a lower member it deferred, so dispatch `ci.yml` on its branch
and let that finish. The member above it is then based on `main`.

The merge commit's message lists the members, the tested head and the run that shows
each lane.

## What main runs after a landing

`ci_pool.py plan` is the `skip-check` job of `ci.yml`: it decides which self-hosted lanes
a run takes. A pull request takes every arm64 lane and Host-Root-x64-SnapshotEnabled
(and Host-x64 when it touches the VMM's confinement). A dispatch takes the lanes named
in its `lanes` input, by exact job name, all of them by default; a name that is not a
lane fails the job. A push to `main` takes every lane that is not already proven for its
tree, and anything the planner cannot look up counts as not proven.

A lane is proven for a pushed commit by a job of exactly that lane's name that succeeded
on its first attempt, in a run of `.github/workflows/ci.yml`, on a commit for which all
of this holds:

- It is one of the pushed commit's other parents, or the head of a pull request of this
  repository that GitHub ties to the pushed commit. This is how the evidence is found:
  by the commit, not by searching recent runs.
- Its tree is the pushed commit's tree. The tree is read from the commit, never from
  anything the run reported.
- It contains `main` as it was before the push. A pull request's run tests the merge of
  its head into its base, and that merge has the head's tree only when the head already
  holds the base.
- Between `main` before the push and that commit, no file that defines the lanes changed.
  A pull request's run executes the pull request's own copy of the workflow, the Makefile
  and the scripts, so a change to one of them could produce a job with a lane's name that
  tested nothing. Such a landing runs every lane on `main`, under the files that landed.
  A comparison of 300 files or more may be cut short by the API and proves nothing
  either.

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

One assumption is not checked, because GitHub does not record it: that the branch a
pull request was based on did not hold commits its head lacked when the run started. For
a pull request based on `main` it follows from the third condition. For the top of a
pool the base is the member below, which `build` and `publish` keep inside the top; a
member branch pushed by hand, or forced, between a run and the landing breaks it.

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

## When a publish goes in

Every push to a pool's top cancels the top's run and starts another. With publishes
arriving at rate r while a run of length T is in flight (T is 57 minutes, the longest
lane's median), a run finishes with probability e^(-rT), and the runner time spent per
finished run is (e^(rT) - 1)/(rT) runs:

| publishes per hour into the pool | a run finishes | cost per finished run, cancelling | hours to a finished run | cost per publish, queued |
|---|---|---|---|---|
| 0.5 | 0.62 | $3.84 | 1.2 | $3.00 |
| 1 | 0.39 | $5.01 | 1.6 | $3.00 |
| 2 | 0.15 | $8.98 | 2.8 | $1.58 |
| 4 | 0.02 | $34.50 | 10.9 | $0.79 |
| 8 | 0.00 | $788 | 250 | $0.39 |

In the week of 2026-11-19, before pooling, 164 of 265 pull request runs of the lanes
were cancelled by a newer push ($165 of $478): one pull request alone already sat near
the second row. A pool of eight members each publishing at that rate is the last row:
its run never finishes. So a pool cannot take publishes as they come.

`publish` therefore looks at the top's own run first. If it is in flight and no lane of
it has failed, the publish is queued: the commit goes to `refs/ci-pool/pending/<branch>`
on the remote, which starts no run, is visible to every session (`status` lists it) and
is never dropped (a later publish of the same branch must contain the queued commit).
`flush --of <member>` puts everything queued into the pool with one build and one push,
and does nothing while the run is still in flight. So a run that started finishes, each
run costs one run, and a publish gets its verdict within one to two run lengths (57 to
114 minutes at the medians).

A publish goes in at once when there is nothing to wait for: no run in flight, or a
run in which a lane has already failed (its verdict is known, and what is being
published may be the fix). `--now` on `publish` or `flush` goes in regardless.

A new member that joins on top moves no member below it, so the old top's run keeps
going while the new top starts its own. That costs a second run and buys the prefix
evidence a bisect would otherwise have to pay for: on 2026-11-25 the seven-member top's
run finished green while the nine-member top went red, and one probe was left.

Nothing flushes by itself. A job in CI could not: a push made with the workflow's own
token starts no run. Whoever watches the top's run flushes when it ends.

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

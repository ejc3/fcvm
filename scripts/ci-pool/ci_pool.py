#!/usr/bin/env python3
"""Pooled CI for pull requests: test many together, attribute a failure with few runs.

A pool is a chain of open pull requests of this repository: each member's base is the
branch of the member below it, the bottom's base is the default branch, and each member's
head contains the head below it (by merge, never a rebase or a force push). The chain is
the membership: there is no other list. CI runs the self-hosted lanes once, on the top
(scripts/ci-pr-scope.sh), and every member's head is the tree of a prefix of the pool.

  build    make a pool from an ordered list of pull requests, or rebuild the pool a pull
           request is in: merge each member into the one above, set bases, push all
           branches in one atomic push
  publish  put a member's local commits into its pool (build, starting that member there)
  status   the pools, their members and whether each member's head has been tested
  cost     runs, runner-minutes and dollars by workflow, lane and runner type from the
           Actions API, the observed failure rate, and the pool size it implies
  failures every failed attempt of a lane by failing test or other cause, with its cost

docs/ci-pool.md has the rules. Exit codes: 0 done, 2 blocked (a dependency, a lookup or a
precondition failed: nothing was changed), 3 a member conflicted or an account is
incomplete, 4 a push was refused because a branch moved.
"""
import argparse
import datetime as dt
import fcntl
import fnmatch
import json
import math
import os
from pathlib import Path
import re
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

# The commit status the top's passing run puts on each lower member's head
# (scripts/ci-stack-coverage.sh). The top's own evidence is its `Summary` check.
CONTEXT = 'Stack CI'
MAX_MEMBERS = 64

# The self-hosted jobs of .github/workflows/ci.yml, by the names the Actions API reports.
# scripts/test-ci-pool.py fails when this list and the workflow's planner disagree.
LANES = (
    'Host-arm64', 'Host-x64',
    'Host-Root-arm64-SnapshotDisabled', 'Host-Root-arm64-SnapshotEnabled',
    'Host-Root-x64-SnapshotDisabled', 'Host-Root-x64-SnapshotEnabled',
    'Container-arm64', 'Container-x64',
)
# What a pull request's run takes of them (the planner in ci.yml): every arm64 lane and
# one x64 lane. Host-x64 joins when the change touches the VMM's confinement.
PR_LANES = tuple(lane for lane in LANES if 'arm64' in lane) + ('Host-Root-x64-SnapshotEnabled',)

# USD per instance hour, recorded constants: `cost` makes no pricing call.
# The autoscaler (ejc3/aws runner-autoscale.tf) launches arm64 runners as c7gd.metal first
# and x64 runners as r5d.metal first, in us-east-2 first, on spot; a job that asks for the
# `on-demand` label (a rerun) gets an on-demand host.
#   spot: DescribeSpotPriceHistory, us-east-2, Linux/UNIX, 2026-11-25, median of the three
#         availability zones (c7gd.metal 0.6172 to 0.6769, r5d.metal 1.1123 to 1.2013)
#   on-demand: AWS Price List API, us-east-2, Linux, shared tenancy, published 2026-11-24
#   github-hosted: standard GitHub-hosted runners are not billed on a public repository
#         (GitHub Actions billing documentation, read 2026-11-25)
PRICES_USD_PER_HOUR = {
    ('self-hosted', 'arm64', 'spot'): 0.6403,
    ('self-hosted', 'x64', 'spot'): 1.1940,
    ('self-hosted', 'arm64', 'on-demand'): 2.9030,
    ('self-hosted', 'x64', 'on-demand'): 6.9120,
    ('github-hosted', '-', '-'): 0.0,
}
PRICES_SOURCE = ('spot: us-east-2 median of 3 zones, 2026-11-25; on-demand: AWS Price List, '
                 'us-east-2, 2026-11-24; GitHub-hosted: not billed on a public repository')

COLLABORATOR = ('OWNER', 'MEMBER', 'COLLABORATOR')


class Blocked(Exception):
    """A dependency, a lookup or a precondition failed. Nothing may be concluded."""


def note(text=''):
    print(text, flush=True)


# ---------------------------------------------------------------------------- transport

class Gh:
    """`gh api`, one JSON document per line. Every failure raises: a lookup that did not
    answer is never an empty answer."""

    def __init__(self, repo, exe=None, max_requests=None):
        self.repo = repo
        self.exe = exe or os.environ.get('GH', 'gh')
        self.requests = 0
        self.max_requests = max_requests

    def call(self, path, jq='.', paginate=False, method='GET', fields=None, timeout=120):
        if self.max_requests is not None and self.requests >= self.max_requests:
            raise Blocked(f'request budget of {self.max_requests} spent')
        self.requests += 1
        cmd = [self.exe, 'api']
        if paginate:
            cmd.append('--paginate')
        if method != 'GET':
            cmd += ['-X', method]
        for key, value in (fields or {}).items():
            cmd += ['-f', f'{key}={value}']
        cmd += [path, '--jq', f'{jq} | tojson']
        try:
            proc = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)
        except (OSError, subprocess.TimeoutExpired) as error:
            raise Blocked(f'gh api {path}: {error}') from error
        if proc.returncode != 0:
            raise Blocked(f'gh api {path} failed: {proc.stderr.strip()[:300]}')
        out = []
        for line in proc.stdout.splitlines():
            if not line.strip():
                continue
            try:
                out.append(json.loads(line))
            except ValueError as error:
                raise Blocked(f'gh api {path}: unreadable answer: {line[:120]}') from error
        return out

    def one(self, path, jq='.', **kw):
        got = self.call(path, jq, **kw)
        if len(got) != 1:
            raise Blocked(f'gh api {path}: expected one document, got {len(got)}')
        return got[0]


def git(*args, cwd=None, check=True, timeout=300):
    try:
        proc = subprocess.run(['git', *args], cwd=cwd, capture_output=True, text=True, timeout=timeout)
    except subprocess.TimeoutExpired as error:
        raise Blocked(f'git {" ".join(args)}: timed out') from error
    if check and proc.returncode != 0:
        raise Blocked(f'git {" ".join(args)} failed: {proc.stderr.strip()[:400]}')
    return proc.stdout.strip() if check else proc


# --------------------------------------------------------------------------- membership

PR_JQ = ('.[] | {number, draft, base: .base.ref, head: .head.ref, sha: .head.sha, '
         'repo: (.head.repo.full_name // ""), association: .author_association}')


def open_prs(gh):
    return gh.call(f'repos/{gh.repo}/pulls?state=open&per_page=100', PR_JQ, paginate=True)


def eligible(pr, repo):
    """Why this pull request cannot be a pool member, or None. A pool's run has the
    repository's secrets and its self-hosted runners, so a member's branch must be in this
    repository and its author one of the repository's collaborators."""
    if pr['repo'] != repo:
        return f"its branch is in {pr['repo'] or 'a deleted fork'}, not {repo}"
    if pr['association'] not in COLLABORATOR:
        return f"its author is not a collaborator ({pr['association']})"
    if pr['draft'] is not False:
        return 'it is a draft, which runs no CI'
    return None


def find_pr(prs, repo, ref):
    """The open pull request of this repository named by a number or a head branch."""
    ref = str(ref).lstrip('#')
    if ref.isdigit():
        found = [p for p in prs if p['number'] == int(ref)]
    else:
        found = [p for p in prs if p['head'] == ref and p['repo'] == repo]
    if len(found) != 1:
        raise Blocked(f'{ref}: {len(found)} open pull requests match')
    return found[0]


def chain_of(prs, repo, default_branch, member):
    """The pool `member` is in, bottom to top, read from the pull requests' bases.

    Down: the member whose head branch is this one's base, until the default branch. Up:
    the ready pull request of this repository based on this one's branch. Anything that
    makes the chain ambiguous or unsafe blocks: two pull requests on one branch, two ready
    children, a base that is neither the default branch nor a member's branch, a member
    that is not eligible."""
    mine = [p for p in prs if p['repo'] == repo]
    by_head = {}
    for p in mine:
        if p['head'] in by_head:
            raise Blocked(f"#{p['number']} and #{by_head[p['head']]['number']} share the branch {p['head']}")
        by_head[p['head']] = p
    chain = [member]
    while chain[0]['base'] != default_branch:
        below = by_head.get(chain[0]['base'])
        if below is None:
            raise Blocked(f"#{chain[0]['number']} is based on {chain[0]['base']}, which no open pull "
                          f'request of {repo} has as its branch')
        if below in chain or len(chain) > MAX_MEMBERS:
            raise Blocked(f"the bases under #{member['number']} do not reach {default_branch}")
        chain.insert(0, below)
    while True:
        children = [p for p in mine if p['base'] == chain[-1]['head'] and p['draft'] is False]
        if not children:
            break
        if len(children) > 1:
            raise Blocked(f"#{chain[-1]['number']} has more than one ready pull request stacked on it: "
                          + ', '.join(f"#{p['number']}" for p in children))
        if children[0] in chain or len(chain) > MAX_MEMBERS:
            raise Blocked(f"the pull requests above #{member['number']} form a loop")
        chain.append(children[0])
    for p in chain:
        why = eligible(p, repo)
        if why:
            raise Blocked(f"#{p['number']} is in the chain but cannot be a member: {why}")
    return chain


def pools(prs, repo, default_branch):
    """Every pool: (chain, None) or (None, why) per top. A top is a ready pull request of
    this repository that no ready pull request of this repository is based on."""
    mine = [p for p in prs if p['repo'] == repo and p['draft'] is False]
    bases = {p['base'] for p in mine}
    out = []
    for top in sorted((p for p in mine if p['head'] not in bases), key=lambda p: p['number']):
        try:
            out.append((chain_of(prs, repo, default_branch, top), None))
        except Blocked as why:
            out.append(([top], str(why)))
    return out


# --------------------------------------------------------------------------------- build

def is_ancestor(a, b, cwd):
    return git('merge-base', '--is-ancestor', a, b, cwd=cwd, check=False).returncode == 0


class Lock:
    """One build at a time per clone. Two clones racing are settled by the push: it is
    never forced, so the second is refused and told to run again."""

    def __init__(self, path, wait):
        self.path, self.wait = path, wait

    def __enter__(self):
        self.handle = open(self.path, 'w')
        deadline = time.monotonic() + self.wait
        while True:
            try:
                fcntl.flock(self.handle, fcntl.LOCK_EX | fcntl.LOCK_NB)
                return self
            except OSError:
                if time.monotonic() >= deadline:
                    self.handle.close()
                    raise Blocked(f'another build holds {self.path}') from None
                time.sleep(0.2)

    def __exit__(self, *_):
        self.handle.close()


def plan_build(members, starts, main, work, sync_main=False):
    """Merge each member's start commit onto the one below, in `work` (a detached
    worktree). Returns (included, left_out, stop).

    included: [(member, new_head)] bottom to top. left_out: [(member, below, files)] for a
    member that conflicts with the pool below it and that no later member contains: it is
    not merged and not pushed. stop: (member, below, files) when a member conflicts and a
    later member already contains it, so leaving it out would leave its commits in the
    pool: nothing may be pushed."""
    included, left_out = [], []
    prev, prev_name = (main, 'the default branch') if sync_main else (None, None)
    for index, member in enumerate(members):
        start = starts[member['head']]
        git('checkout', '-q', '--detach', start, cwd=work)
        if prev and not is_ancestor(prev, 'HEAD', work):
            merged = git('merge', '-q', '--no-edit', '-m', f"Merge {prev_name} into {member['head']} (pool)",
                         prev, cwd=work, check=False)
            if merged.returncode != 0:
                files = git('diff', '--name-only', '--diff-filter=U', cwd=work).split('\n')
                git('merge', '--abort', cwd=work, check=False)
                git('checkout', '-q', '--detach', main, cwd=work)
                conflict = (member, prev_name, [f for f in files if f])
                if any(is_ancestor(start, starts[later['head']], work) for later in members[index + 1:]):
                    return included, left_out, conflict
                left_out.append(conflict)
                continue
        prev, prev_name = git('rev-parse', 'HEAD', cwd=work), member['head']
        included.append((member, prev))
    git('checkout', '-q', '--detach', main, cwd=work)
    return included, left_out, None


def member_status(gh, sha):
    """(state, description) of the newest CONTEXT status on a commit, or ('none', '')."""
    newest = gh.one(f'repos/{gh.repo}/commits/{sha}/status',
                    f'[.statuses[] | select(.context == "{CONTEXT}")] | .[0] // {{}}')
    return newest.get('state', 'none'), newest.get('description', '')


def cmd_build(args, gh, default_branch):
    prs = open_prs(gh)
    if args.members:
        members = [find_pr(prs, gh.repo, ref) for ref in args.members]
        if len({p['number'] for p in members}) != len(members):
            raise Blocked('a pull request is listed twice')
    else:
        members = chain_of(prs, gh.repo, default_branch, find_pr(prs, gh.repo, args.of))
    if len(members) > MAX_MEMBERS:
        raise Blocked(f'{len(members)} members: a pool holds at most {MAX_MEMBERS}')
    for p in members:
        why = eligible(p, gh.repo)
        if why:
            raise Blocked(f"#{p['number']} cannot be a member: {why}")

    common = git('rev-parse', '--path-format=absolute', '--git-common-dir')
    remote = args.remote
    with Lock(os.path.join(common, 'ci-pool.lock'), args.lock_wait):
        git('fetch', '-q', remote, default_branch, *[p['head'] for p in members], timeout=300)
        main = git('rev-parse', f'refs/remotes/{remote}/{default_branch}')
        old = {p['head']: git('rev-parse', f"refs/remotes/{remote}/{p['head']}") for p in members}
        for p in members:
            if old[p['head']] != p['sha']:
                raise Blocked(f"#{p['number']}'s branch {p['head']} is at {old[p['head']][:9]} but the pull "
                              f"request's head is {p['sha'][:9]}: it moved during the lookup, run again")
        starts = dict(old)
        for spec in args.pending or []:
            branch, _, sha = spec.partition('=')
            if branch not in starts:
                raise Blocked(f'{branch} is not a member of this pool')
            sha = git('rev-parse', '--verify', f'{sha}^{{commit}}')
            if not is_ancestor(old[branch], sha, None):
                raise Blocked(f'the commits for {branch} do not contain {remote}/{branch}: merge it first')
            starts[branch] = sha

        work = tempfile.mkdtemp(prefix='ci-pool-', dir=common)
        try:
            git('worktree', 'add', '-q', '--detach', work, main)
            included, left_out, stop = plan_build(members, starts, main, work, args.sync_main)
        finally:
            git('worktree', 'remove', '--force', work, check=False)
            shutil.rmtree(work, ignore_errors=True)
            git('worktree', 'prune', check=False)

        for member, below, files in left_out + ([stop] if stop else []):
            note(f"CONFLICT merging {below} into {member['head']} (#{member['number']}):")
            for name in files:
                note(f'   {name}')
        if stop:
            note(f"#{stop[0]['number']} is already inside members above it, so it cannot be left out. "
                 f"Its owner merges {stop[1]} into {stop[0]['head']}, resolves, and publishes. Nothing was pushed.")
            return 3
        for member, _, _ in left_out:
            note(f"left out: #{member['number']}. It joins at the top once its owner has merged the top into it.")
        if not included:
            raise Blocked('no member could be built')

        changed = []
        for member, new in included:
            branch = member['head']
            # The tree is what to compare with another tool's result: two merges of the same
            # parents are different commits with the same tree.
            note(f"  #{member['number']} {branch} {old[branch][:9]}" +
                 ('' if new == old[branch] else f" -> {new[:9]} tree {git('rev-parse', new + '^{tree}')[:12]}"))
            if new != old[branch]:
                changed.append((member, new))
        if args.dry_run:
            note('dry run: nothing pushed, no base changed')
            return 3 if left_out else 0

        # Bases first: a lower member's run defers only to a pull request already based on it.
        below = default_branch
        for member, _ in included:
            if member['base'] != below:
                gh.call(f"repos/{gh.repo}/pulls/{member['number']}", method='PATCH', fields={'base': below})
                note(f"  #{member['number']} base {member['base']} -> {below}")
            below = member['head']
        if changed:
            # One atomic, unforced push: every branch moves or none does, and a lower
            # member's run already finds the member above containing its new head.
            pushed = git('push', '--atomic', remote, *[f"{new}:refs/heads/{m['head']}" for m, new in changed],
                         check=False, timeout=300)
            if pushed.returncode != 0:
                note(f'PUSH REFUSED (a branch moved): nothing was pushed. Run again.\n{pushed.stderr.strip()[:400]}')
                return 4
        top, top_sha = included[-1]
        note(f"pool of {len(included)}: top is #{top['number']} {top['head']} at {top_sha[:9]}")
        return 3 if left_out else 0


def cmd_publish(args, gh, default_branch):
    work = args.worktree or '.'
    branch = args.branch or git('rev-parse', '--abbrev-ref', 'HEAD', cwd=work)
    if git('rev-parse', '--abbrev-ref', 'HEAD', cwd=work) != branch:
        raise Blocked(f'{work} is not on {branch}')
    if git('status', '--porcelain', '--untracked-files=no', cwd=work):
        raise Blocked(f'{work} has uncommitted changes')
    head = git('rev-parse', 'HEAD', cwd=work)
    args.members, args.of, args.pending = [], branch, [f'{branch}={head}']
    args.sync_main = False
    return cmd_build(args, gh, default_branch)


# -------------------------------------------------------------------------------- status

def cmd_status(args, gh, default_branch):
    prs = open_prs(gh)
    found = pools(prs, gh.repo, default_branch)
    if not found:
        note('no ready pull request of this repository is open')
    for chain, why in found:
        top = chain[-1]
        if why:
            note(f"#{top['number']} {top['head']}: NOT A POOL: {why}")
            continue
        note(f"pool of {len(chain)}, top #{top['number']} {top['head']} at {top['sha'][:9]}")
        for position, p in enumerate(chain, 1):
            if p is top:
                state, text = 'top', 'its own run of the lanes is the evidence'
            else:
                state, text = member_status(gh, p['sha'])
            note(f"  {position:2d}. #{p['number']:<5d} {p['sha'][:9]} {state:8s} {p['head']}  {text}")
    return 0


# ------------------------------------------------------------------------------ evidence

# A lane is proven for a tree by a job of exactly that name that succeeded in a run of
# ci.yml on a commit with that tree. A pull request's run executes the pull request's own
# workflow, Makefile and scripts, so a change to any of them could produce a job with a
# lane's name that tested nothing. A run therefore proves nothing for a landing that
# changes one of these paths: the lanes run again on the default branch, under the files
# that landed. scripts/test-ci-pool.py fails when a lane job of ci.yml runs a script or a
# local action that no pattern here matches.
LANE_DEFINING = ('.github/*', 'Makefile', '.config/nextest.toml', 'scripts/*', 'Containerfile*',
                 'rust-toolchain.toml', '.cargo/*')
PASS, RERUN_PASS, FAIL, OPEN = 'pass', 'pass-on-rerun', 'fail', 'open'
MAX_EVIDENCE_AGE_DAYS = 7
COMPARE_FILE_CAP = 300
EVIDENCE_EVENTS = ('pull_request', 'workflow_dispatch', 'push')


def lane_defining(path):
    return any(fnmatch.fnmatchcase(path, pattern) for pattern in LANE_DEFINING)


def latest(jobs, name):
    """The newest attempt of the job with exactly this name, or None."""
    tries = [job for job in jobs if job['name'] == name]
    return max(tries, key=lambda job: job['run_attempt']) if tries else None


def lane_states(jobs):
    """Each lane's state from a run's jobs, every attempt: its latest attempt decides,
    whatever the earlier ones were. PASS when it succeeded and no earlier attempt of it
    ended otherwise, RERUN_PASS when it succeeded after one that did, FAIL when it failed
    or timed out, OPEN when it was cancelled or has not finished. A skipped lane is not
    evidence and is absent, and so is every job whose name is not exactly a lane's."""
    states = {}
    for lane in LANES:
        tries = sorted((job for job in jobs if job['name'] == lane and job.get('conclusion') != 'skipped'),
                       key=lambda job: job['run_attempt'])
        if not tries:
            continue
        last = tries[-1].get('conclusion')
        if last == 'success':
            states[lane] = PASS if all(job.get('conclusion') == 'success' for job in tries) else RERUN_PASS
        elif last in ('failure', 'timed_out'):
            states[lane] = FAIL
        else:
            states[lane] = OPEN
    return states


def scope_skipped_the_lanes(jobs):
    """True when a pull request's run found, by its own scope, that its change needs no
    lane: it reported `Summary` (never `Summary (deferred)`, which a draft or a lower
    member reports having tested nothing), the scope job passed, and every self-hosted
    job was skipped before its matrix expanded."""
    summary, changes = latest(jobs, 'Summary'), latest(jobs, 'Changes')
    if not summary or summary.get('conclusion') != 'success' or not changes or changes.get('conclusion') != 'success':
        return False
    unexpanded = [job for job in jobs if 'matrix.' in job['name']]
    return (len(unexpanded) >= 3 and all(job.get('conclusion') == 'skipped' for job in unexpanded)
            and not any(job['name'] in LANES for job in jobs))


def proof_from(commit, runs, landing_tree, compare, accept_rerun=False):
    """What the runs on one commit prove for a landing: (lanes, no_lanes, why_not).

    commit: {'sha', 'tree'}. runs: [{'id', 'event', 'jobs'}]. compare: the comparison of
    the default branch before the landing with this commit: {'status', 'files'} or None
    when it could not be read. Nothing is proven unless all of these hold:
    - the commit's tree is the landing's tree, so its runs tested what landed;
    - the commit contains the default branch as it was before the landing, so a pull
      request's run, which tests the merge of its head into its base, tested that tree;
    - the comparison lists every changed file, and none is lane-defining."""
    if commit['tree'] != landing_tree:
        return {}, None, f"{commit['sha'][:12]} has another tree"
    if compare is None:
        return {}, None, f"could not compare {commit['sha'][:12]} with the default branch"
    if compare['status'] not in ('ahead', 'identical'):
        return {}, None, f"{commit['sha'][:12]} does not contain the default branch ({compare['status']})"
    if len(compare['files']) >= COMPARE_FILE_CAP:
        return {}, None, f"{len(compare['files'])} changed files, more than the comparison lists"
    changed = sorted(path for path in compare['files'] if lane_defining(path))
    if changed:
        return {}, None, f"the landing changes what defines the lanes: {', '.join(changed[:3])}"
    lanes, no_lanes = {}, None
    for run in runs:
        if run['event'] not in EVIDENCE_EVENTS:
            continue
        if run['event'] == 'pull_request' and scope_skipped_the_lanes(run['jobs']):
            no_lanes = run['id']
        for lane, state in lane_states(run['jobs']).items():
            if state == PASS or (state == RERUN_PASS and accept_rerun):
                lanes.setdefault(lane, run['id'])
    return lanes, no_lanes, None


def validate_lanes(text):
    """The lanes a dispatch asked for. Empty means all. Anything that is not a lane's exact
    name blocks: a typo must not turn into a run of nothing that reports success."""
    names = [name for name in re.split(r'[\s,]+', text or '') if name]
    unknown = [name for name in names if name not in LANES]
    if unknown:
        raise Blocked(f"not a lane: {', '.join(unknown)}. The lanes are: {', '.join(LANES)}")
    return tuple(lane for lane in LANES if lane in names) if names else LANES


def landing_evidence(gh, sha, now=None):
    """(tree, [(commit, runs, compare)]) for a commit pushed to the default branch.

    The commits whose runs can have tested its tree: its other parents (a merge commit
    has the tree of the branch it merged when that branch contained the default branch)
    and the heads of the pull requests GitHub ties to it (a squash or a rebase). A lookup
    that fails drops that commit's evidence and nothing else, which can only leave lanes
    unproven."""
    now = now or dt.datetime.now(dt.timezone.utc)
    landing = gh.one(f'repos/{gh.repo}/git/commits/{sha}', '{tree: .tree.sha, parents: [.parents[].sha]}')
    if not landing['parents']:
        return landing['tree'], []
    before, candidates = landing['parents'][0], list(landing['parents'][1:])
    try:
        candidates += gh.call(f'repos/{gh.repo}/commits/{sha}/pulls?per_page=100',
                              f'.[] | select(.head.repo.full_name == "{gh.repo}") | .head.sha', paginate=True)
    except Blocked as why:
        note(f'could not list the pull requests of {sha[:12]}: {why}')
    out = []
    for head in dict.fromkeys(candidates):
        if not re.fullmatch(r'[0-9a-f]{40}', str(head)):
            continue
        try:
            commit = {'sha': head, 'tree': gh.one(f'repos/{gh.repo}/git/commits/{head}', '.tree.sha')}
            if commit['tree'] != landing['tree']:
                out.append((commit, [], None))
                continue
            compare = gh.one(f'repos/{gh.repo}/compare/{before}...{head}',
                             '{status, files: [.files[] | .filename, (.previous_filename // empty)]}')
            runs = []
            for run in gh.call(f'repos/{gh.repo}/actions/runs?head_sha={head}&per_page=100',
                               '.workflow_runs[] | select(.path == ".github/workflows/ci.yml") | {id, event, created_at}',
                               paginate=True):
                if (now - instant(run['created_at'])).days >= MAX_EVIDENCE_AGE_DAYS:
                    continue
                run['jobs'] = gh.call(f"repos/{gh.repo}/actions/runs/{run['id']}/jobs?filter=all&per_page=100",
                                      '.jobs[] | {name, conclusion, run_attempt}', paginate=True)
                runs.append(run)
            out.append((commit, runs, compare))
        except Blocked as why:
            note(f'no evidence taken from {head[:12]}: {why}')
    return landing['tree'], out


def plan_lanes(event, landing_tree=None, evidence=(), confine=False, lanes_input=''):
    """(lanes, reasons): the self-hosted lanes this run takes, and why.

    A pull request takes PR_LANES (and Host-x64 for a change to confinement). A push takes
    every lane not proven for exactly its tree. A dispatch takes the lanes it names, all by default. Evidence is
    applied to a push and to nothing else."""
    if event == 'pull_request':
        return tuple(l for l in LANES if l in PR_LANES or (confine and l == 'Host-x64')), ['a pull request']
    if event == 'workflow_dispatch':
        return validate_lanes(lanes_input), ['the lanes this dispatch asked for' if lanes_input.strip() else 'a dispatch']
    if event != 'push':
        return LANES, [f'event {event}']
    have, reasons = {}, []
    for commit, runs, compare in evidence:
        lanes, no_lanes, why_not = proof_from(commit, runs, landing_tree, compare)
        if why_not:
            reasons.append(f'no proof: {why_not}')
            continue
        if no_lanes:
            return (), [f"run {no_lanes} on {commit['sha'][:12]} found that this tree needs no lane"]
        for lane, run in lanes.items():
            have.setdefault(lane, run)
    if have:
        reasons.append('already passed for this tree: ' + ', '.join(f'{lane} (run {run})' for lane, run in sorted(have.items())))
    return tuple(l for l in LANES if l not in have), reasons or ['no run tested this tree']


def matrix_outputs(lanes):
    """The planner's outputs, as the self-hosted jobs of ci.yml read them. A matrix with
    no combination fails a run instead of skipping the job, so every list has a flag."""
    host = [l.removeprefix('Host-') for l in lanes if l in ('Host-arm64', 'Host-x64')]
    container = [l.removeprefix('Container-') for l in lanes if l.startswith('Container-')]
    host_root = []
    for lane in lanes:
        if lane.startswith('Host-Root-'):
            arch, mode = lane.removeprefix('Host-Root-').split('-')
            disabled = mode == 'SnapshotDisabled'
            # SnapshotDisabled runs the suite once with FCVM_NO_SNAPSHOT=1. SnapshotEnabled
            # runs it twice on one runner: a snapshot miss, then a hit.
            host_root.append({'arch': arch, 'mode': mode, 'fcvm_no_snapshot': '1' if disabled else '',
                              'test_runs': '1' if disabled else '2'})

    def compact(value):
        return json.dumps(value, separators=(',', ':'))

    return {
        'host': compact({'arch': host}), 'host_run': str(bool(host)).lower(),
        'host_root': compact({'include': host_root}), 'host_root_run': str(bool(host_root)).lower(),
        'container': compact({'arch': container}), 'container_run': str(bool(container)).lower(),
        'skip': str(not lanes).lower(),
    }


def cmd_plan(_args, gh, _default_branch):
    """ci.yml's `skip-check` job. Reads EVENT, SHA, CONFINE and LANES and writes
    the matrix to $GITHUB_OUTPUT. Whatever cannot be looked up proves nothing, so the
    lanes run."""
    event = os.environ['EVENT']
    tree, evidence = None, []
    if event == 'push':
        try:
            tree, evidence = landing_evidence(gh, os.environ['SHA'])
        except (Blocked, KeyError) as why:
            note(f'no evidence for this push: {why}')
    lanes, reasons = plan_lanes(event, tree, evidence, os.environ.get('CONFINE') == 'true',
                                os.environ.get('LANES', ''))
    for reason in reasons:
        note(reason)
    note(f"Self-hosted jobs for this {event or 'event'}: {' '.join(lanes) or 'none'}")
    with open(os.environ['GITHUB_OUTPUT'], 'a') as out:
        for key, value in matrix_outputs(lanes).items():
            out.write(f'{key}={value}\n')
    return 0


# ---------------------------------------------------------------------------------- cost

def instant(text):
    return dt.datetime.fromisoformat(text.replace('Z', '+00:00'))


def runner_class(labels):
    """(kind, arch, market) of the runner a job asked for."""
    low = [str(label).lower() for label in labels or []]
    if 'self-hosted' in low:
        arch = 'arm64' if 'arm64' in low else 'x64'
        return ('self-hosted', arch, 'on-demand' if 'on-demand' in low else 'spot')
    return ('github-hosted', '-', '-')


def job_minutes(job):
    """Minutes a job held a runner: zero for a job that was skipped or never got one."""
    if job.get('conclusion') == 'skipped' or not job.get('runner_name'):
        return 0.0
    if not job.get('started_at') or not job.get('completed_at'):
        return 0.0
    return max(0.0, (instant(job['completed_at']) - instant(job['started_at'])).total_seconds()) / 60


def usd(kind, minutes):
    return minutes / 60 * PRICES_USD_PER_HOUR[kind]


def dorfman_two_stage(p, k):
    """Expected runs per member when a failed pool is retested one member at a time."""
    return 1 / k + 1 - (1 - p) ** k


def alone_runs_per_member(member_rate, run_rate):
    """Expected runs to get one pull request green alone. A run fails for a cause of its
    own (a lost runner, a test that does not always pass) with probability run_rate and
    is run again; a change that is itself broken, member_rate of them, takes one more."""
    return (1 + member_rate) / (1 - run_rate)


def pooled_runs_per_member(member_rate, k, failed_share, run_rate=0.0):
    """Expected runs per member, in units of one pool run, with this tool's red path.

    The pool runs once (1/k each). A failure of the run's own costs a rerun of the failed
    lanes, once per pool whatever its size. Every broken member costs a rerun of the
    failed lanes, a bisect of ceil(log2 k) runs of them, and one more pool run after it is
    fixed or moved out, so a pool with two culprits pays that twice. `failed_share` is the
    share of a pool run's cost that the failed lanes are."""
    own = run_rate * failed_share / (1 - run_rate)
    return (1 + own) / k + member_rate * (failed_share * (1 + math.ceil(math.log2(k))) + 1)


# A larger pool also holds every member back while one culprit is fixed, which a count of
# runs does not price. So the recommendation is the smallest size within this much of the
# fewest expected runs, not the size with the very fewest.
SIZE_TOLERANCE = 0.10


def recommend_size(member_rate, failed_share, run_rate=0.0, sizes=range(2, 17)):
    """(size, table): the smallest pool size within SIZE_TOLERANCE of the fewest expected
    runs per member, or None when no size beats testing each pull request alone."""
    table = [(k, pooled_runs_per_member(member_rate, k, failed_share, run_rate)) for k in sizes]
    fewest = min(cost for _, cost in table)
    if fewest >= alone_runs_per_member(member_rate, run_rate):
        return None, table
    return next(k for k, cost in table if cost <= fewest * (1 + SIZE_TOLERANCE)), table


def day_windows(since, until):
    day = since
    while day < until:
        end = min(day + dt.timedelta(days=1), until)
        yield day, end
        day = end


def list_runs(gh, since, until):
    """Every workflow run created in [since, until). The API returns at most 1000 runs per
    search and says so only through total_count, so each window's count is checked and a
    window that reaches the cap is halved."""
    runs = {}
    stack = list(day_windows(since, until))
    while stack:
        start, end = stack.pop()
        span = f"{start.strftime('%Y-%m-%dT%H:%M:%SZ')}..{(end - dt.timedelta(seconds=1)).strftime('%Y-%m-%dT%H:%M:%SZ')}"
        got = gh.call(
            f'repos/{gh.repo}/actions/runs?created={span}&per_page=100',
            '.total_count as $total | .workflow_runs[] | {id, name, path, event, status, conclusion, '
            'run_attempt, created_at, run_started_at, updated_at, total: $total}', paginate=True)
        total = got[0]['total'] if got else gh.one(f'repos/{gh.repo}/actions/runs?created={span}&per_page=1',
                                                   '.total_count')
        if total >= 1000:
            if (end - start).total_seconds() <= 60:
                raise Blocked(f'more than 1000 runs in one minute ({span})')
            middle = start + (end - start) / 2
            middle = middle.replace(microsecond=0)
            stack += [(start, middle), (middle, end)]
            continue
        if len({r['id'] for r in got}) != total:
            raise Blocked(f'{span}: the API reports {total} runs and listed {len(got)}')
        for run in got:
            runs[run['id']] = run
    return list(runs.values())


JOB_JQ = ('.jobs[] | {id, name, conclusion, started_at, completed_at, labels, run_attempt, runner_name}')


def jobs_of(gh, run, cache):
    """A completed run's jobs, every attempt. Cached by run and attempt: a completed
    attempt does not change."""
    path = cache / f"{run['id']}-{run['run_attempt']}.json"
    if path.exists():
        return json.loads(path.read_text())
    jobs = gh.call(f"repos/{gh.repo}/actions/runs/{run['id']}/jobs?filter=all&per_page=100", JOB_JQ, paginate=True)
    for job in jobs:
        job['runner_name'] = bool(job.get('runner_name'))
    tmp = path.with_suffix('.tmp')
    tmp.write_text(json.dumps(jobs))
    tmp.replace(path)
    return jobs


def first_attempt(jobs, names):
    """Each named job's conclusion in the run's first attempt."""
    return {j['name']: j['conclusion'] for j in jobs if j['name'] in names and j['run_attempt'] == 1}


def summarise(runs, jobs_by_run):
    """The account: minutes and dollars by workflow, event and runner class; by lane; the
    cost of one run of the lanes at the median successful duration; and how often a pull
    request's run of the lanes failed."""
    groups, lanes, durations = {}, {}, {}
    passed = failed = rerun = rerun_passed = 0
    failed_cost, failed_runs_cost = [], []
    for run in runs:
        jobs = jobs_by_run.get(run['id'])
        if jobs is None:
            continue
        for job in jobs:
            kind = runner_class(job['labels'])
            minutes = job_minutes(job)
            if minutes == 0:
                continue
            key = (run['name'], run['event'], kind)
            g = groups.setdefault(key, {'jobs': 0, 'minutes': 0.0, 'usd': 0.0, 'runs': set()})
            g['jobs'] += 1
            g['minutes'] += minutes
            g['usd'] += usd(kind, minutes)
            g['runs'].add(run['id'])
            if job['name'] in LANES and kind[0] == 'self-hosted':
                lane = lanes.setdefault((job['name'], kind[2]), {'jobs': 0, 'minutes': 0.0, 'usd': 0.0})
                lane['jobs'] += 1
                lane['minutes'] += minutes
                lane['usd'] += usd(kind, minutes)
                if job['conclusion'] == 'success':
                    durations.setdefault(job['name'], []).append(minutes)
        if run['event'] == 'pull_request' and run['path'].endswith('/ci.yml'):
            first = first_attempt(jobs, LANES)
            ran = {name: c for name, c in first.items() if c in ('success', 'failure')}
            if any(c == 'failure' for c in ran.values()):
                failed += 1
                broke = [name for name, c in ran.items() if c == 'failure']
                failed_runs_cost.append(broke)
                # What a rerun of the same commit said about each lane that failed.
                later = {}
                for job in jobs:
                    if job['name'] in broke and job['run_attempt'] > 1 and job['conclusion'] in ('success', 'failure'):
                        if job['run_attempt'] >= later.get(job['name'], (0, ''))[0]:
                            later[job['name']] = (job['run_attempt'], job['conclusion'])
                if len(later) == len(broke):
                    rerun += 1
                    rerun_passed += all(c == 'success' for _, c in later.values())
            elif ran and all(first.get(lane) == 'success' for lane in PR_LANES):
                passed += 1
    median = {lane: statistics.median(values) for lane, values in durations.items()}

    def run_cost(names):
        if any(lane not in median for lane in names):
            return None
        return sum(usd(('self-hosted', 'arm64' if 'arm64' in lane else 'x64', 'spot'), median[lane]) for lane in names)

    pr_run, full_run = run_cost(PR_LANES), run_cost(LANES)
    share = None
    if failed_runs_cost and pr_run:
        for names in failed_runs_cost:
            cost = run_cost([n for n in names if n in median])
            if cost is not None:
                failed_cost.append(cost / pr_run)
        share = min(1.0, statistics.mean(failed_cost)) if failed_cost else None
    return {
        'groups': groups, 'lanes': lanes, 'median_minutes': median,
        'pr_run_usd': pr_run, 'full_run_usd': full_run,
        'pr_runs_passed': passed, 'pr_runs_failed': failed,
        'failure_rate': failed / (passed + failed) if passed + failed else None,
        'failed_share': share, 'reruns': rerun, 'reruns_passed': rerun_passed,
    }


def print_account(account, since, until, counts):
    note(f"CI cost, runs created {since:%Y-%m-%d %H:%M} to {until:%Y-%m-%d %H:%M} UTC")
    note(f"  {counts['runs']} runs listed, {counts['itemised']} itemised, {counts['unfinished']} not finished, "
         f"{counts['missing']} not itemised")
    if counts['short']:
        note(f"  {counts['short']} runs left out as asked (--skip-short): together they lasted "
             f"{counts['short_minutes']} minutes of wall time, an upper bound on what any one runner did in them")
    note('  dollars are job minutes at the recorded hourly price; a host also bills while it boots and')
    note('  while it waits for its next job, which the Actions API does not report')
    note(f'  prices: {PRICES_SOURCE}')
    note()
    note(f"{'USD':>9} {'minutes':>9} {'jobs':>6} {'runs':>5}  workflow / event / runner")
    total = 0.0
    for key, g in sorted(account['groups'].items(), key=lambda item: (-item[1]['usd'], -item[1]['minutes'])):
        total += g['usd']
        kind = '/'.join(part for part in key[2] if part != '-')
        note(f"{g['usd']:9.2f} {g['minutes']:9.0f} {g['jobs']:6d} {len(g['runs']):5d}  {key[0]} / {key[1]} / {kind}")
    note(f'{total:9.2f}  total')
    note()
    note(f"{'USD':>9} {'minutes':>9} {'jobs':>6}  lane (ci.yml self-hosted jobs)")
    for (lane, market), g in sorted(account['lanes'].items()):
        note(f"{g['usd']:9.2f} {g['minutes']:9.0f} {g['jobs']:6d}  {lane} / {market}")
    note()
    note('one run of the lanes, at each lane\'s median successful duration and the spot price:')
    for lane in LANES:
        if lane in account['median_minutes']:
            note(f"  {account['median_minutes'][lane]:6.1f} min  {lane}")
    if account['pr_run_usd'] is not None:
        note(f"  a pull request's run ({len(PR_LANES)} lanes): ${account['pr_run_usd']:.2f}")
    if account['full_run_usd'] is not None:
        note(f"  the full matrix ({len(LANES)} lanes): ${account['full_run_usd']:.2f}")
    note()
    p = account['failure_rate']
    if p is None:
        note('no pull request run of the lanes finished in this range: no failure rate, no pool size')
        return
    share = account['failed_share'] if account['failed_share'] is not None else 1.0
    note(f"pull request runs of the lanes: {account['pr_runs_passed']} passed, {account['pr_runs_failed']} failed "
         f"a lane on the first attempt: p = {p:.3f}")
    note(f'  the failed lanes were {share:.2f} of a run\'s cost on average')
    for line in size_report(p, share, account['reruns'], account['reruns_passed']):
        note(line)


MIN_RERUNS = 10


def size_report(p, share, reruns, reruns_passed):
    """The pool size the observed failures imply, as lines of text.

    A failed run that passed when the same commit was run again failed for a cause of its
    own, which a pool meets once whatever its size; one that failed again is charged to
    the change. With fewer than MIN_RERUNS reruns observed there is no split, and every
    failure is charged to the change, which is the case least favourable to pooling."""
    lines = []
    if p > 0:
        lines.append(f'  Dorfman: the best pool for two-stage testing is about 1/sqrt(p) = {1 / math.sqrt(p):.1f}')
    transient = reruns_passed / reruns if reruns >= MIN_RERUNS else 0.0
    if reruns >= MIN_RERUNS:
        lines.append(f'  {reruns} failed runs were run again on the same commit and {reruns_passed} of those passed: '
                     f'{transient:.2f} of failures are taken as the run\'s own, {1 - transient:.2f} as the change\'s')
        lines.append('  (runs chosen for a rerun are not a random sample of failures)')
    else:
        lines.append(f'  only {reruns} failed runs were run again on the same commit: every failure is charged to the change')
    run_rate, member_rate = p * transient, p * (1 - transient)
    worst, worst_table = recommend_size(p, share)
    best, table = recommend_size(member_rate, share, run_rate)
    alone = alone_runs_per_member(member_rate, run_rate)
    lines.append(f"  {'size':>4}  {'two-stage':>9}  {'all the change':>14}  {'split':>6}   expected runs per member "
                 f'(alone: {1 + p:.2f} if all the change, {alone:.2f} split)')
    for (k, split), (_, charged) in zip(table, worst_table):
        mark = '   <- recommended' if k == best else ''
        lines.append(f'  {k:4d}  {dorfman_two_stage(p, k):9.2f}  {charged:14.2f}  {split:6.2f}{mark}')
    if best is None:
        lines.append('  no size beats testing each pull request alone at this failure rate')
    if worst is None and best is not None:
        lines.append('  if every failure were the change\'s, no size would beat testing each pull request alone')
    return lines


def cost_range(args):
    now = dt.datetime.now(dt.timezone.utc).replace(microsecond=0)
    until = instant(args.until + 'T00:00:00Z') if args.until else now
    since = instant(args.since + 'T00:00:00Z') if args.since else until - dt.timedelta(days=args.days)
    cache = Path(args.cache or os.path.join(git('rev-parse', '--path-format=absolute', '--git-common-dir'),
                                            'ci-pool-cache'))
    cache.mkdir(parents=True, exist_ok=True)
    return since, until, cache


def wall_minutes(run):
    """From the start of a run's latest attempt to its last update."""
    if not run.get('run_started_at') or not run.get('updated_at'):
        return None
    return (instant(run['updated_at']) - instant(run['run_started_at'])).total_seconds() / 60


def cmd_cost(args, gh, _default_branch):
    since, until, cache = cost_range(args)
    runs = list_runs(gh, since, until)
    jobs_by_run, missing, unfinished, short, short_minutes = {}, 0, 0, 0, 0.0
    for run in sorted(runs, key=lambda r: r['id']):
        if run['status'] != 'completed':
            unfinished += 1
            continue
        # --skip-short leaves out runs too short to matter and says how many and how long:
        # most runs here end in under a minute (a deferred member, a schedule that found
        # nothing to do), and each costs a request to itemise. A run that was rerun is kept:
        # its wall time is its latest attempt's only.
        wall = wall_minutes(run)
        if args.skip_short and run['run_attempt'] == 1 and wall is not None and wall < args.skip_short:
            short += 1
            short_minutes += wall
            continue
        try:
            jobs_by_run[run['id']] = jobs_of(gh, run, cache)
        except Blocked as why:
            missing += 1
            if missing == 1:
                note(f'not itemised: {why}')
    account = summarise(runs, jobs_by_run)
    counts = {'runs': len(runs), 'itemised': len(jobs_by_run), 'unfinished': unfinished, 'missing': missing,
              'short': short, 'short_minutes': round(short_minutes)}
    if args.json:
        best = None
        if account['failure_rate']:
            transient = account['reruns_passed'] / account['reruns'] if account['reruns'] >= MIN_RERUNS else 0.0
            best = recommend_size(account['failure_rate'] * (1 - transient), account['failed_share'] or 1.0,
                                  account['failure_rate'] * transient)[0]
        print(json.dumps({'counts': counts, 'failure_rate': account['failure_rate'],
                          'pr_run_usd': account['pr_run_usd'], 'full_run_usd': account['full_run_usd'],
                          'recommended_size': best,
                          'usd': sum(g['usd'] for g in account['groups'].values())}))
    else:
        print_account(account, since, until, counts)
    if missing:
        note(f'INCOMPLETE: {missing} finished runs are not in this account. Run again to continue '
             f'(what was fetched is cached in {cache}).')
        return 3
    return 0


# ------------------------------------------------------------------------------ failures

LOG_TAIL_BYTES = 3_000_000
ANSI = re.compile(r'\x1b\[[0-9;]*m')
STAMP = re.compile(r'^\d{4}-\d\d-\d\dT[\d:.]+Z ')
# nextest's line for a test that did not pass: "FAIL [ 1.2s] (12/90) crate::binary name",
# with "TRY 2 " in front when it was retried. The run's closing list follows "Summary [".
# Any status word but the ones that mean passed counts, so a status nextest adds later is
# a failure here until it is listed as a pass. TMT is how it abbreviates a retried timeout.
TEST_LINE = re.compile(r'^\s*(?:TRY \d+ )?([A-Z][A-Z-]*) \[\s*[\d.]+s\] (?:\([^)]*\) )?(\S+) (\S+)\s*$')
PASSED_WORDS = ('PASS', 'SLOW', 'LEAK', 'SKIP')
COMPILE_LINE = re.compile(r'^error: could not compile `([^`]+)` \(([^)]+)\)')
NO_LOG = 'log not available'
NO_FAILURE_IN_LOG = f'no failure in the last {LOG_TAIL_BYTES // 1_000_000} MB of the log'


def classify_log(text):
    """Why a job failed, from the end of its log: the tests nextest lists as not passed,
    or, when there is none, the first error GitHub recorded and the step it ended. A log
    that shows neither says so: a cause is never guessed."""
    lines = [STAMP.sub('', ANSI.sub('', line)) for line in text.splitlines()]
    # A host that is shutting down fails whatever was running on it (podman refuses to start
    # a container, VMs die), so tests that failed in a job that lost its runner are not causes.
    for line in lines:
        if line.startswith('##[error]') and runner_lost(line):
            return [runner_lost(line)]
    summary = max((i for i, line in enumerate(lines) if 'Summary [' in line and 'tests run' in line), default=None)
    tests = []
    for line in lines[summary:] if summary is not None else lines:
        match = TEST_LINE.match(line)
        if match and match[1] not in PASSED_WORDS and f'{match[2]} {match[3]}' not in tests:
            tests.append(f'{match[2]} {match[3]}')
    if tests:
        return tests
    builds = []
    for line in lines:
        match = COMPILE_LINE.match(line.strip())
        if match and f'does not compile: {match[1]} ({match[2]})' not in builds:
            builds.append(f'does not compile: {match[1]} ({match[2]})')
    if builds:
        return builds
    error = next((i for i, line in enumerate(lines) if line.startswith('##[error]')), None)
    if error is None:
        return [NO_FAILURE_IN_LOG]
    text = lines[error].removeprefix('##[error]').strip()
    step = next((line.removeprefix('##[group]Run ').strip() for line in reversed(lines[:error])
                 if line.startswith('##[group]Run ')), 'unknown step')
    if 'exceeded the maximum execution time' in text:
        return [f'job time limit, in step: {step[:60]}']
    if 'operation was canceled' in text:
        return [f'cancelled in step: {step[:60]}']
    if text.startswith('Process completed with exit code'):
        return [f'step failed without a failed test: {step[:60]}']
    return [re.sub(r'\d+', 'N', text)[:80]]


def runner_lost(message):
    """The two ways a job loses its runner: the host was told to stop while the job ran, or
    it stopped answering, in which case GitHub usually holds no log for the job."""
    if 'shutdown signal' in message:
        return 'runner lost: shutdown signal'
    if 'lost communication' in message:
        return 'runner lost: no contact'
    return None


def classify_annotations(messages):
    """The cause of a job that left no log, from the annotations GitHub put on it, or None."""
    for message in messages:
        if runner_lost(message):
            return [runner_lost(message)]
        if 'exceeded the maximum execution time' in message:
            return ['job time limit, no log']
    return None


def log_tail(gh, job_id):
    """The last LOG_TAIL_BYTES of a job's log, or None. $CI_POOL_LOGS names a directory of
    <job id>.log files to read instead of asking GitHub."""
    local = os.environ.get('CI_POOL_LOGS')
    if local:
        path = Path(local) / f'{job_id}.log'
        return path.read_text(errors='replace') if path.exists() else None
    class Stay(urllib.request.HTTPRedirectHandler):
        def redirect_request(self, *_args, **_kwargs):
            return None

    try:
        token = subprocess.run([gh.exe, 'auth', 'token'], capture_output=True, text=True, timeout=30).stdout.strip()
        request = urllib.request.Request(f'https://api.github.com/repos/{gh.repo}/actions/jobs/{job_id}/logs',
                                         headers={'Authorization': f'Bearer {token}'})
        gh.requests += 1
        try:
            urllib.request.build_opener(Stay).open(request, timeout=60)
            return None
        except urllib.error.HTTPError as answer:
            where = answer.headers.get('Location') if answer.code in (301, 302, 307) else None
        if not where:
            return None
        ranged = urllib.request.Request(where, headers={'Range': f'bytes=-{LOG_TAIL_BYTES}'})
        return urllib.request.urlopen(ranged, timeout=180).read().decode(errors='replace')
    except (OSError, subprocess.TimeoutExpired):
        return None


def failed_lane_jobs(runs, jobs_by_run):
    """Every failed attempt of a self-hosted lane, with what a later attempt of the same
    lane in the same run said (True passed, False failed again, None not run again)."""
    out = []
    for run in runs:
        jobs = jobs_by_run.get(run['id']) or []
        for job in jobs:
            if job['name'] not in LANES or job['conclusion'] not in ('failure', 'timed_out'):
                continue
            later = [j for j in jobs if j['name'] == job['name'] and j['run_attempt'] > job['run_attempt']
                     and j['conclusion'] in ('success', 'failure', 'timed_out')]
            again = max(later, key=lambda j: j['run_attempt'])['conclusion'] == 'success' if later else None
            minutes = job_minutes(job)
            out.append({'id': job['id'], 'lane': job['name'], 'run': run['id'], 'event': run['event'],
                        'at': job.get('started_at') or '', 'again': again,
                        'usd': usd(runner_class(job['labels']), minutes)})
    return out


def failure_table(failed, causes):
    """Rows by cause, most expensive first. A job with several failed tests counts once for
    each and its dollars are split between them, so the column adds up to what failed
    attempts cost."""
    rows = {}
    for job in failed:
        found = causes.get(job['id']) or [NO_LOG]
        for cause in found:
            row = rows.setdefault(cause, {'cause': cause, 'failed': 0, 'rerun': 0, 'rerun_passed': 0, 'usd': 0.0,
                                          'first': job['at'], 'last': job['at'], 'lanes': {}})
            row['failed'] += 1
            row['usd'] += job['usd'] / len(found)
            row['lanes'][job['lane']] = row['lanes'].get(job['lane'], 0) + 1
            if job['again'] is not None:
                row['rerun'] += 1
                row['rerun_passed'] += job['again']
            if job['at']:
                row['first'] = min(row['first'] or job['at'], job['at'])
                row['last'] = max(row['last'], job['at'])
    return sorted(rows.values(), key=lambda row: (-row['usd'], row['cause']))


def cmd_failures(args, gh, _default_branch):
    since, until, cache = cost_range(args)
    runs = list_runs(gh, since, until)
    jobs_by_run, missing = {}, 0
    for run in sorted(runs, key=lambda r: r['id']):
        wall = wall_minutes(run)
        if args.skip_short and run['run_attempt'] == 1 and wall is not None and wall < args.skip_short:
            continue
        if run['status'] == 'completed' and run['conclusion'] in ('failure', 'cancelled', 'timed_out', 'success'):
            try:
                jobs_by_run[run['id']] = jobs_of(gh, run, cache)
            except Blocked:
                missing += 1
    failed = failed_lane_jobs(runs, jobs_by_run)
    facts = cache / 'failures'
    facts.mkdir(exist_ok=True)
    causes = {}
    for job in failed:
        path = facts / f"{job['id']}.json"
        if path.exists():
            causes[job['id']] = json.loads(path.read_text())
            continue
        text = log_tail(gh, job['id'])
        found = classify_log(text) if text is not None else None
        if found is None or found == [NO_FAILURE_IN_LOG]:
            # A runner that vanished uploads no log, or one that stops mid-test with no
            # error in it; GitHub's annotation on the job says what happened.
            shown = found
            try:
                found = classify_annotations(gh.call(f"repos/{gh.repo}/check-runs/{job['id']}/annotations?per_page=100",
                                                     '.[] | .message', paginate=True))
            except Blocked:
                found = None
            found = found or shown
        if found is not None:
            causes[job['id']] = found
            path.write_text(json.dumps(found))
    rows = failure_table(failed, causes)
    if args.json:
        print(json.dumps(rows))
    else:
        note(f"Failed attempts of the self-hosted lanes, runs created {since:%Y-%m-%d %H:%M} to {until:%Y-%m-%d %H:%M} UTC: "
             f"{len(failed)} jobs, ${sum(job['usd'] for job in failed):.2f}")
        note("  'again' is passed/run again: a later attempt of the same lane in the same run")
        note(f"{'failed':>6} {'again':>6} {'USD':>8}  {'first':10} {'last':10}  cause (lanes)")
        for row in rows:
            lanes = ', '.join(f'{lane} {count}' for lane, count in sorted(row['lanes'].items()))
            note(f"{row['failed']:6d} {row['rerun_passed']:3d}/{row['rerun']:<2d} {row['usd']:8.2f}  "
                 f"{row['first'][:10]:10} {row['last'][:10]:10}  {row['cause']}  ({lanes})")
    unread = sum(1 for job in failed if job['id'] not in causes)
    if missing or unread:
        note(f'INCOMPLETE: {missing} runs without a job list, {unread} failed jobs without a log. Run again to continue.')
        return 3
    return 0


# ---------------------------------------------------------------------------------- main

def main(argv=None):
    parser = argparse.ArgumentParser(prog='ci-pool', description=__doc__.split('\n\n')[0])
    parser.add_argument('--repo', default=os.environ.get('REPO'), help='owner/name (default: the origin remote)')
    parser.add_argument('--remote', default='origin')
    sub = parser.add_subparsers(dest='command', required=True)

    build = sub.add_parser('build', help='make or rebuild a pool')
    build.add_argument('members', nargs='*', help='pull request numbers or branches, bottom to top')
    build.add_argument('--of', help='rebuild the pool this pull request or branch is in')
    build.add_argument('--sync-main', action='store_true', help='merge the default branch into the bottom')
    build.add_argument('--dry-run', action='store_true')
    build.add_argument('--pending', action='append', metavar='BRANCH=COMMIT',
                       help="start this member from a local commit that contains its remote branch; repeat it to "
                            "put several members' commits into the pool in one push")
    build.add_argument('--lock-wait', type=float, default=600)

    publish = sub.add_parser('publish', help="put a member's local commits into its pool")
    publish.add_argument('branch', nargs='?')
    publish.add_argument('worktree', nargs='?')
    publish.add_argument('--dry-run', action='store_true')
    publish.add_argument('--lock-wait', type=float, default=900)

    sub.add_parser('status', help="pools, members and whether each member's head has been tested")
    sub.add_parser('plan', help="the self-hosted lanes of this CI run (ci.yml's skip-check job)")

    for name, text in (('cost', 'runs, runner-minutes and dollars from the Actions API'),
                       ('failures', 'failed lane attempts by failing test or cause, with their cost')):
        cost = sub.add_parser(name, help=text)
        cost.add_argument('--days', type=int, default=7)
        cost.add_argument('--since', metavar='YYYY-MM-DD')
        cost.add_argument('--until', metavar='YYYY-MM-DD')
        cost.add_argument('--cache', help='directory for the job lists of finished runs')
        cost.add_argument('--max-requests', type=int, help='stop asking the API after this many requests')
        cost.add_argument('--skip-short', type=float, default=0, metavar='MINUTES',
                          help='leave out runs whose whole wall time was under MINUTES, and count them')
        cost.add_argument('--json', action='store_true')

    args = parser.parse_args(argv)
    try:
        for tool in ('git', os.environ.get('GH', 'gh'), 'jq'):
            if not shutil.which(tool):
                raise Blocked(f"'{tool}' is missing")
        if args.command == 'build' and bool(args.members) == bool(args.of):
            raise Blocked('give the members bottom to top, or --of one member of an existing pool')
        repo = args.repo
        if not repo:
            url = git('remote', 'get-url', args.remote)
            repo = url.removesuffix('.git').split('github.com')[-1].lstrip(':/')
        if repo.count('/') != 1:
            raise Blocked(f'cannot tell the repository from {repo!r}: pass --repo owner/name')
        gh = Gh(repo, max_requests=getattr(args, 'max_requests', None))
        if args.command == 'plan':
            return cmd_plan(args, gh, None)
        default_branch = os.environ.get('DEFAULT_BRANCH') or gh.one(f'repos/{repo}', '.default_branch')
        return {'build': cmd_build, 'publish': cmd_publish, 'status': cmd_status,
                'cost': cmd_cost, 'failures': cmd_failures}[args.command](args, gh, default_branch)
    except Blocked as why:
        print(f'BLOCKED: {why}', file=sys.stderr)
        return 2


if __name__ == '__main__':
    sys.exit(main())

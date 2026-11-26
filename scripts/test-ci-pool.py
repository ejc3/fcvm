#!/usr/bin/env python3
"""Tests of scripts/ci-pool/ci_pool.py: who may be a pool member, how a pool is built
and published, and the cost account.

No network: `gh` is a fake that answers from a fixture and applies --jq with the real
jq, and `git` runs against repositories made in a temporary directory. Run with
make test-ci-pool.
"""
import datetime as dt
import importlib.util
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import textwrap
import unittest

ROOT = Path(__file__).resolve().parent.parent
TOOL = ROOT / 'scripts/ci-pool/ci_pool.py'
CI = ROOT / '.github/workflows/ci.yml'

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location('ci_pool', TOOL)
ci_pool = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ci_pool)

FAKE_GH = textwrap.dedent('''\
    #!/usr/bin/env python3
    # Fake gh: answers `gh api` from $FAKE_GH_FIXTURE, applies --jq with jq, logs calls.
    # A fixture key ending in * answers every path that starts with it.
    import json, os, subprocess, sys
    args = sys.argv[1:]
    assert args[0] == 'api', args
    args = args[1:]
    jq = None; method = 'GET'; path = None; fields = {}
    i = 0
    while i < len(args):
        a = args[i]
        if a == '--paginate': i += 1; continue
        if a == '--jq': jq = args[i + 1]; i += 2; continue
        if a == '-X': method = args[i + 1]; i += 2; continue
        if a == '-f': k, v = args[i + 1].split('=', 1); fields[k] = v; i += 2; continue
        path = a; i += 1
    with open(os.environ['FAKE_GH_LOG'], 'a') as log:
        log.write(json.dumps({'method': method, 'path': path, 'fields': fields}) + '\\n')
    fixture = json.load(open(os.environ['FAKE_GH_FIXTURE']))
    if method != 'GET':
        answer = fixture.get(method + ' ' + path, {})
    else:
        answer = fixture.get(path)
        if answer is None:
            for key, value in fixture.items():
                if key.endswith('*') and path.startswith(key[:-1]):
                    answer = value
        if answer is None or answer == 'FAIL':
            sys.stderr.write('HTTP 502 for ' + str(path) + '\\n'); sys.exit(1)
    sys.exit(subprocess.run(['jq', '-r', jq or '.'], input=json.dumps(answer), text=True).returncode)
''')

REPO = 'example-owner/example-repo'
GIT_ENV = {'GIT_CONFIG_GLOBAL': '/dev/null', 'GIT_CONFIG_SYSTEM': '/dev/null',
           'GIT_AUTHOR_NAME': 'Example', 'GIT_AUTHOR_EMAIL': 'example@example.invalid',
           'GIT_COMMITTER_NAME': 'Example', 'GIT_COMMITTER_EMAIL': 'example@example.invalid'}


def pr(number, head, base, sha='0' * 40, repo=REPO, draft=False, association='OWNER'):
    return {'number': number, 'draft': draft, 'author_association': association,
            'head': {'ref': head, 'sha': sha, 'repo': {'full_name': repo}}, 'base': {'ref': base}}


def flat(p):
    """A fixture pull request as ci_pool reads it."""
    return {'number': p['number'], 'draft': p.get('draft'), 'base': p['base']['ref'], 'head': p['head']['ref'],
            'sha': p['head']['sha'], 'repo': p['head']['repo']['full_name'], 'association': p['author_association']}


class Membership(unittest.TestCase):
    def chain(self, prs, member):
        flats = [flat(p) for p in prs]
        return [p['number'] for p in ci_pool.chain_of(flats, REPO, 'main', next(p for p in flats if p['number'] == member))]

    def test_the_bases_are_the_order_whichever_member_is_asked_about(self):
        prs = [pr(3, 'c', 'b'), pr(1, 'a', 'main'), pr(2, 'b', 'a')]
        for member in (1, 2, 3):
            self.assertEqual(self.chain(prs, member), [1, 2, 3])

    def test_a_fork_pull_request_is_never_a_member(self):
        # A fork's branch can share a name with a member's branch or be based on one.
        prs = [pr(1, 'a', 'main'), pr(2, 'b', 'a', repo='someone/fork'), pr(3, 'a', 'main', repo='someone/fork')]
        self.assertEqual(self.chain(prs, 1), [1])
        self.assertIn('someone/fork', ci_pool.eligible(flat(prs[1]), REPO))

    def test_a_pull_request_by_someone_who_is_not_a_collaborator_is_not_eligible(self):
        for association in ('CONTRIBUTOR', 'FIRST_TIME_CONTRIBUTOR', 'NONE', ''):
            self.assertIn('collaborator', ci_pool.eligible(flat(pr(1, 'a', 'main', association=association)), REPO))
        for association in ('OWNER', 'MEMBER', 'COLLABORATOR'):
            self.assertIsNone(ci_pool.eligible(flat(pr(1, 'a', 'main', association=association)), REPO))

    def test_a_draft_or_an_unknown_draft_state_is_not_eligible(self):
        self.assertIn('draft', ci_pool.eligible(flat(pr(1, 'a', 'main', draft=True)), REPO))
        unknown = pr(1, 'a', 'main')
        del unknown['draft']
        self.assertIn('draft', ci_pool.eligible(flat(unknown), REPO))

    def test_a_draft_under_a_member_blocks_the_chain(self):
        # A draft runs nothing and defers nothing, so the member under it would run the
        # lanes on its own: that is not one pool.
        prs = [pr(1, 'a', 'main'), pr(2, 'b', 'a', draft=True), pr(3, 'c', 'b')]
        with self.assertRaisesRegex(ci_pool.Blocked, '#2 .*draft'):
            self.chain(prs, 3)

    def test_a_draft_on_top_is_outside_the_pool(self):
        prs = [pr(1, 'a', 'main'), pr(2, 'b', 'a'), pr(3, 'c', 'b', draft=True)]
        self.assertEqual(self.chain(prs, 1), [1, 2])

    def test_two_ready_children_block(self):
        prs = [pr(1, 'a', 'main'), pr(2, 'b', 'a'), pr(3, 'c', 'a')]
        with self.assertRaisesRegex(ci_pool.Blocked, 'more than one ready'):
            self.chain(prs, 1)

    def test_a_base_that_is_no_members_branch_blocks(self):
        with self.assertRaisesRegex(ci_pool.Blocked, 'based on gone'):
            self.chain([pr(2, 'b', 'gone')], 2)

    def test_two_pull_requests_on_one_branch_block(self):
        with self.assertRaisesRegex(ci_pool.Blocked, 'share the branch'):
            self.chain([pr(1, 'a', 'main'), pr(2, 'a', 'main')], 1)

    def test_bases_that_loop_block(self):
        with self.assertRaisesRegex(ci_pool.Blocked, 'do not reach main'):
            self.chain([pr(1, 'a', 'b'), pr(2, 'b', 'a')], 1)

    def test_every_top_is_a_pool_and_a_broken_one_says_why(self):
        prs = [flat(p) for p in (pr(1, 'a', 'main'), pr(2, 'b', 'a'), pr(5, 'e', 'main'), pr(7, 'g', 'gone'))]
        found = ci_pool.pools(prs, REPO, 'main')
        self.assertEqual([[p['number'] for p in chain] for chain, _ in found], [[1, 2], [5], [7]])
        self.assertEqual([why is None for _, why in found], [True, True, False])


class Sandbox(unittest.TestCase):
    """A bare `origin` with main and one branch per member, a clone, and a fake gh."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix='ci-pool-test-')
        self.dir = Path(self.tmp.name)
        self.addCleanup(self.tmp.cleanup)
        self.env = dict(os.environ, **GIT_ENV)
        self.origin = self.dir / 'origin.git'
        self.git('init', '-q', '--bare', '-b', 'main', str(self.origin), cwd=self.dir)
        self.clone = self.dir / 'clone'
        self.git('clone', '-q', str(self.origin), str(self.clone), cwd=self.dir)
        self.commit('main', {'README': 'base\n'}, 'base', start=None)
        gh = self.dir / 'gh'
        gh.write_text(FAKE_GH)
        gh.chmod(0o755)
        self.log = self.dir / 'log'
        self.log.write_text('')

    def git(self, *args, cwd=None, check=True):
        proc = subprocess.run(['git', *args], cwd=cwd or self.clone, env=self.env, capture_output=True, text=True)
        if check:
            self.assertEqual(proc.returncode, 0, proc.stderr)
        return proc.stdout.strip() if check else proc

    def commit(self, branch, files, message, start='origin/main', push=True):
        """Commit `files` on `branch` (made from `start` if it is new) and push it."""
        if self.git('rev-parse', '-q', '--verify', f'refs/heads/{branch}', check=False).returncode == 0:
            self.git('checkout', '-q', branch)
        elif start is None:
            self.git('checkout', '-q', '-B', branch)
        else:
            self.git('checkout', '-q', '-b', branch, start)
        for name, text in files.items():
            (self.clone / name).write_text(text)
            self.git('add', name)
        self.git('commit', '-q', '-m', message)
        if push:
            self.git('push', '-q', 'origin', f'{branch}:{branch}')
        return self.git('rev-parse', 'HEAD')

    def remote(self, branch):
        return self.git('rev-parse', f'refs/heads/{branch}', cwd=self.origin)

    def contains(self, branch, commit):
        return self.git('merge-base', '--is-ancestor', commit, f'refs/heads/{branch}',
                        cwd=self.origin, check=False).returncode == 0

    def prs(self, *members, **overrides):
        """Fixture pull requests for (number, branch, base), heads read from origin."""
        return [pr(number, branch, base, self.remote(branch), **overrides.get(branch, {}))
                for number, branch, base in members]

    def tool(self, *args, prs=(), fixture=None, cwd=None, env=None):
        answers = {f'repos/{REPO}/pulls?state=open&per_page=100': list(prs),
                   f'repos/{REPO}/commits/*': {'statuses': []},
                   f'repos/{REPO}/actions/runs?head_sha=*': {'workflow_runs': []}}
        answers.update(fixture or {})
        (self.dir / 'fixture.json').write_text(json.dumps(answers))
        full = dict(self.env, GH=str(self.dir / 'gh'), FAKE_GH_FIXTURE=str(self.dir / 'fixture.json'),
                    FAKE_GH_LOG=str(self.log), REPO=REPO, DEFAULT_BRANCH='main')
        full.update(env or {})
        proc = subprocess.run([sys.executable, str(TOOL), *args], cwd=cwd or self.clone, env=full,
                              capture_output=True, text=True)
        calls = [json.loads(line) for line in self.log.read_text().splitlines()]
        self.log.write_text('')
        return proc.returncode, proc.stdout + proc.stderr, calls

    def writes(self, calls):
        return [(c['method'], c['path'].split('/', 3)[-1], c['fields']) for c in calls if c['method'] != 'GET']


class Build(Sandbox):
    def three(self):
        a = self.commit('a', {'a.txt': 'a\n'}, 'a')
        b = self.commit('b', {'b.txt': 'b\n'}, 'b')
        c = self.commit('c', {'c.txt': 'c\n'}, 'c')
        self.git('checkout', '-q', '--detach', 'origin/main')
        return a, b, c

    def test_each_member_ends_up_containing_the_one_below_and_based_on_it(self):
        a, b, c = self.three()
        code, out, calls = self.tool('build', '1', '2', '3', prs=self.prs((1, 'a', 'main'), (2, 'b', 'main'), (3, 'c', 'main')))
        self.assertEqual(code, 0, out)
        self.assertEqual(self.remote('a'), a, 'the bottom has nothing to merge and must not move')
        self.assertTrue(self.contains('b', a) and self.contains('b', b))
        self.assertTrue(self.contains('c', self.remote('b')) and self.contains('c', c))
        bases = [(path, fields['base']) for method, path, fields in self.writes(calls) if method == 'PATCH']
        self.assertEqual(bases, [('pulls/2', 'a'), ('pulls/3', 'b')])

    def test_a_build_posts_no_status(self):
        # Whether a head was tested is something only a run can state.
        self.three()
        _, _, calls = self.tool('build', '1', '2', '3', prs=self.prs((1, 'a', 'main'), (2, 'b', 'main'), (3, 'c', 'main')))
        self.assertEqual([path for method, path, _ in self.writes(calls) if method == 'POST'], [])

    def test_a_member_that_conflicts_is_left_out_whole(self):
        a = self.commit('a', {'shared.txt': 'from a\n'}, 'a')
        b = self.commit('b', {'shared.txt': 'from b\n'}, 'b')
        self.commit('c', {'c.txt': 'c\n'}, 'c')
        self.git('checkout', '-q', '--detach', 'origin/main')
        code, out, calls = self.tool('build', '1', '2', '3', prs=self.prs((1, 'a', 'main'), (2, 'b', 'main'), (3, 'c', 'main')))
        self.assertEqual(code, 3, out)
        self.assertIn('CONFLICT merging a into b (#2)', out)
        self.assertIn('shared.txt', out)
        self.assertEqual(self.remote('b'), b, 'the conflicting member was pushed')
        self.assertTrue(self.contains('c', a), 'the member above the conflict was not built on the one below it')
        self.assertFalse(self.contains('c', b))
        writes = self.writes(calls)
        self.assertEqual([(p, f['base']) for m, p, f in writes if m == 'PATCH'], [('pulls/3', 'a')])

    def test_a_conflicting_member_that_others_already_contain_stops_the_build(self):
        # b is already inside c. Leaving b out would keep its commits in the pool under c's name.
        self.commit('a', {'a.txt': 'a\n'}, 'a')
        b = self.commit('b', {'shared.txt': 'from b\n'}, 'b', start='a')
        c = self.commit('c', {'c.txt': 'c\n'}, 'c', start='b')
        a = self.commit('a', {'shared.txt': 'from a\n'}, 'a again')
        self.git('checkout', '-q', '--detach', 'origin/main')
        code, out, calls = self.tool('build', '--of', '3', prs=self.prs((1, 'a', 'main'), (2, 'b', 'a'), (3, 'c', 'b')))
        self.assertEqual(code, 3, out)
        self.assertIn('Nothing was pushed', out)
        self.assertEqual((self.remote('a'), self.remote('b'), self.remote('c')), (a, b, c))
        self.assertEqual(self.writes(calls), [])

    def hook(self, name, body):
        path = self.origin / 'hooks' / name
        path.write_text('#!/bin/sh\n' + body)
        path.chmod(0o755)

    def test_a_refused_branch_leaves_every_branch_where_it_was(self):
        # One push for all branches, atomic: a lower member must never be pushed while the
        # member above it, which its run defers to, is not.
        a, b, c = self.three()
        self.hook('update', '[ "$1" = refs/heads/c ] && exit 1\nexit 0\n')
        code, out, _ = self.tool('build', '1', '2', '3', prs=self.prs((1, 'a', 'main'), (2, 'b', 'main'), (3, 'c', 'main')))
        self.assertEqual(code, 4, out)
        self.assertIn('PUSH REFUSED', out)
        self.assertEqual((self.remote('a'), self.remote('b'), self.remote('c')), (a, b, c))

    def test_a_branch_is_never_forced(self):
        a, b, c = self.three()
        moved = self.commit('b', {'late.txt': 'late\n'}, 'pushed after the lookup')
        self.git('checkout', '-q', '--detach', 'origin/main')
        stale = pr(2, 'b', 'main', b)
        code, out, calls = self.tool('build', '1', '2', '3', prs=self.prs((1, 'a', 'main')) + [stale] + self.prs((3, 'c', 'main')))
        self.assertEqual(code, 2, out)
        self.assertIn('moved during the lookup', out)
        self.assertEqual((self.remote('b'), self.remote('c')), (moved, c))
        self.assertEqual(self.writes(calls), [])
        source = TOOL.read_text()
        pushes = re.findall(r"git\('push'.*?\)", source, re.S)
        self.assertTrue(pushes)
        # The one lease is on the removal of a queue entry (never a branch): it is removed
        # only while it still names the commit that went in.
        leased = [push for push in pushes if 'force' in push]
        self.assertEqual(len(leased), 1, leased)
        self.assertIn("f'--force-with-lease={PENDING}{branch}:{sha}'", leased[0])
        pushes = [push for push in pushes if push not in leased]
        for push in pushes:
            self.assertNotRegex(push, r"force|'-f'|\+")
        self.assertNotRegex(source, r"git\('(rebase|reset)'")

    def test_a_dry_run_changes_nothing(self):
        a, b, c = self.three()
        code, out, calls = self.tool('build', '--dry-run', '1', '2', '3',
                                     prs=self.prs((1, 'a', 'main'), (2, 'b', 'main'), (3, 'c', 'main')))
        self.assertEqual(code, 0, out)
        self.assertIn('dry run', out)
        # A dry run names the tree each changed head would have, and the commit can be read back.
        printed = re.search(r'#3 c \w{9} -> (\w{9}) tree (\w{12})', out)
        self.assertTrue(printed, out)
        self.assertTrue(self.git('rev-parse', printed[1] + '^{tree}').startswith(printed[2]))
        self.assertEqual((self.remote('a'), self.remote('b'), self.remote('c')), (a, b, c))
        self.assertEqual(self.writes(calls), [])

    def test_a_fork_a_draft_or_an_outsider_cannot_be_built_in(self):
        self.three()
        for override, word in (({'repo': 'someone/fork'}, 'matches|not example-owner'), ({'draft': True}, 'draft'),
                               ({'association': 'CONTRIBUTOR'}, 'collaborator')):
            prs = self.prs((1, 'a', 'main'), (2, 'b', 'main'), b=override)
            code, out, calls = self.tool('build', '1', '2', prs=prs)
            self.assertEqual(code, 2, out)
            self.assertRegex(out, word)
            self.assertEqual(self.writes(calls), [])

    def test_sync_main_brings_the_default_branch_into_the_bottom(self):
        self.three()
        main = self.commit('main', {'README': 'newer\n'}, 'main moved', start=None)
        self.git('checkout', '-q', '--detach', 'origin/main')
        prs = self.prs((1, 'a', 'main'), (2, 'b', 'a'), (3, 'c', 'b'))
        code, out, _ = self.tool('build', '1', '2', '3', prs=prs)
        self.assertEqual(code, 0, out)
        self.assertFalse(self.contains('c', main), 'main was merged without being asked for')
        prs = self.prs((1, 'a', 'main'), (2, 'b', 'a'), (3, 'c', 'b'))
        code, out, _ = self.tool('build', '--sync-main', '--of', '2', prs=prs)
        self.assertEqual(code, 0, out)
        for branch in 'abc':
            self.assertTrue(self.contains(branch, main), branch)

    def test_the_callers_checkout_is_not_touched(self):
        self.three()
        (self.clone / 'untracked.txt').write_text('mine\n')
        before = self.git('rev-parse', 'HEAD')
        self.tool('build', '1', '2', '3', prs=self.prs((1, 'a', 'main'), (2, 'b', 'main'), (3, 'c', 'main')))
        self.assertEqual(self.git('rev-parse', 'HEAD'), before)
        self.assertEqual((self.clone / 'untracked.txt').read_text(), 'mine\n')
        self.assertEqual(self.git('worktree', 'list').count('\n'), 0, 'a work tree was left behind')

    def test_a_failed_lookup_blocks(self):
        self.three()
        code, out, calls = self.tool('build', '1', '2', fixture={f'repos/{REPO}/pulls?state=open&per_page=100': 'FAIL'})
        self.assertEqual(code, 2, out)
        self.assertIn('BLOCKED', out)
        self.assertEqual(self.writes(calls), [])


class Publish(Sandbox):
    def pool(self):
        self.commit('a', {'a.txt': 'a\n'}, 'a')
        self.commit('b', {'b.txt': 'b\n'}, 'b', start='a')
        self.commit('c', {'c.txt': 'c\n'}, 'c', start='b')
        return (1, 'a', 'main'), (2, 'b', 'a'), (3, 'c', 'b')

    def test_local_commits_reach_the_member_and_every_member_above_in_one_push(self):
        members = self.pool()
        prs = self.prs(*members)
        new = self.commit('b', {'b.txt': 'b fixed\n'}, 'fix b', push=False)
        self.hook_log()
        code, out, _ = self.tool('publish', prs=prs)
        self.assertEqual(code, 0, out)
        self.assertEqual(self.remote('b'), new)
        self.assertTrue(self.contains('c', new))
        self.assertEqual((self.dir / 'pushes').read_text().count('push'), 1, 'more than one push')

    def test_several_members_commits_go_in_one_push(self):
        # Fixes to more than one member: one build, one push, so the top's run restarts once.
        members = self.pool()
        prs = self.prs(*members)
        fix_a = self.commit('a', {'a.txt': 'a fixed\n'}, 'fix a', push=False)
        fix_c = self.commit('c', {'c.txt': 'c fixed\n'}, 'fix c', push=False)
        self.git('checkout', '-q', '--detach', 'origin/main')
        self.hook_log()
        code, out, _ = self.tool('build', '--of', '3', '--pending', f'a={fix_a}', '--pending', f'c={fix_c}', prs=prs)
        self.assertEqual(code, 0, out)
        self.assertEqual(self.remote('a'), fix_a)
        for branch in 'bc':
            self.assertTrue(self.contains(branch, fix_a), branch)
        self.assertTrue(self.contains('c', fix_c))
        self.assertEqual((self.dir / 'pushes').read_text().count('push'), 1, 'more than one push')

    def test_a_pending_commit_for_a_branch_outside_the_pool_is_refused(self):
        members = self.pool()
        prs = self.prs(*members)
        code, out, calls = self.tool('build', '--of', '3', '--pending', f"zzz={self.remote('a')}", prs=prs)
        self.assertEqual(code, 2, out)
        self.assertIn('zzz is not a member of this pool', out)
        self.assertEqual(self.writes(calls), [])

    # ---- the queue

    def flying(self, sha, jobs=(), number=900):
        return {f'repos/{REPO}/actions/runs?head_sha={sha}&per_page=100': {'workflow_runs': [
                    {'id': number, 'path': '.github/workflows/ci.yml', 'event': 'pull_request', 'status': 'in_progress',
                     'created_at': '2026-01-01T00:00:00Z'}]},
                f'repos/{REPO}/actions/runs/{number}/jobs?filter=latest&per_page=100': {'jobs': list(jobs)}}

    def queued(self):
        out = self.git('ls-remote', str(self.origin), 'refs/ci-pool/pending/*')
        return {line.split('\t')[1].rsplit('/', 1)[-1]: line.split('\t')[0] for line in out.splitlines() if line}

    RUNNING = [{'name': 'Host-arm64', 'conclusion': None}, {'name': 'Container-arm64', 'conclusion': 'success'}]

    def test_a_publish_while_the_tops_run_is_in_flight_is_queued_and_moves_no_branch(self):
        members = self.pool()
        prs = self.prs(*members)
        before = {branch: self.remote(branch) for branch in 'abc'}
        new = self.commit('b', {'b.txt': 'b fixed\n'}, 'fix b', push=False)
        self.hook_log()
        code, out, calls = self.tool('publish', prs=prs, fixture=self.flying(before['c'], self.RUNNING))
        self.assertEqual(code, 0, out)
        self.assertIn('QUEUED: b at', out)
        self.assertEqual({branch: self.remote(branch) for branch in 'abc'}, before)
        self.assertEqual(self.queued(), {'b': new})
        self.assertEqual(self.writes(calls), [])
        # status shows it, from any clone
        _, out, _ = self.tool('status', prs=prs)
        self.assertRegex(out, rf'#2 .*\[queued: {new[:9]}, age unknown')
        made = (dt.datetime.now(dt.timezone.utc) - dt.timedelta(minutes=45)).strftime('%Y-%m-%dT%H:%M:%SZ')
        _, out, _ = self.tool('status', prs=prs, fixture={f'repos/{REPO}/git/commits/{new}': {'committer': {'date': made}}})
        self.assertRegex(out, rf'#2 .*\[queued: {new[:9]}, committed 4[45] min ago: `flush` puts it in\]')
        long_ago = {f'repos/{REPO}/git/commits/{new}': {'committer': {'date': '2026-01-01T00:00:00Z'}}}
        self.assertRegex(self.tool('status', prs=prs, fixture=long_ago)[1], r'committed \d+ h ago')

    def test_a_publish_goes_in_at_once_when_nothing_is_in_flight_or_a_lane_has_already_failed(self):
        members = self.pool()
        prs = self.prs(*members)
        top = self.remote('c')
        new = self.commit('b', {'b.txt': 'b fixed\n'}, 'fix b', push=False)
        failed = self.RUNNING + [{'name': 'Host-Root-arm64-SnapshotEnabled', 'conclusion': 'failure'}]
        code, out, _ = self.tool('publish', prs=prs, fixture=self.flying(top, failed))
        self.assertEqual(code, 0, out)
        self.assertEqual((self.remote('b'), self.queued()), (new, {}))
        prs = self.prs(*members)
        newer = self.commit('b', {'b.txt': 'b fixed again\n'}, 'fix b again', push=False)
        code, out, _ = self.tool('publish', '--now', prs=prs, fixture=self.flying(self.remote('c'), self.RUNNING))
        self.assertEqual((code, self.remote('b'), self.queued()), (0, newer, {}), out)

    def test_a_run_on_the_top_that_has_finished_holds_nothing(self):
        members = self.pool()
        prs = self.prs(*members)
        top = self.remote('c')
        new = self.commit('b', {'b.txt': 'b fixed\n'}, 'fix b', push=False)
        fixture = self.flying(top, self.RUNNING)
        fixture[f'repos/{REPO}/actions/runs?head_sha={top}&per_page=100']['workflow_runs'][0]['status'] = 'completed'
        code, out, _ = self.tool('publish', prs=prs, fixture=fixture)
        self.assertEqual((code, self.remote('b'), self.queued()), (0, new, {}), out)

    def test_a_run_on_the_top_that_is_not_its_own_run_of_the_lanes_holds_nothing(self):
        members = self.pool()
        prs = self.prs(*members)
        top = self.remote('c')
        new = self.commit('b', {'b.txt': 'b fixed\n'}, 'fix b', push=False)
        fixture = self.flying(top, [{'name': 'Changes', 'conclusion': 'success'}, {'name': 'Summary (deferred)', 'conclusion': None}])
        code, out, _ = self.tool('publish', prs=prs, fixture=fixture)
        self.assertEqual((code, self.remote('b')), (0, new), out)

    def test_a_later_publish_must_contain_what_is_already_queued(self):
        members = self.pool()
        prs = self.prs(*members)
        fixture = self.flying(self.remote('c'), self.RUNNING)
        self.commit('b', {'b.txt': 'first\n'}, 'first', push=False)
        self.assertEqual(self.tool('publish', prs=prs, fixture=fixture)[0], 0)
        second = self.commit('b', {'b.txt': 'second\n'}, 'second', push=False)
        code, out, _ = self.tool('publish', prs=prs, fixture=fixture)
        self.assertEqual((code, self.queued()), (0, {'b': second}), out)
        other = self.dir / 'other'
        self.git('clone', '-q', str(self.origin), str(other), cwd=self.dir)
        self.git('checkout', '-q', 'b', cwd=other)
        (other / 'x.txt').write_text('from another checkout\n')
        self.git('add', 'x.txt', cwd=other)
        self.git('commit', '-q', '-m', 'another', cwd=other)
        code, out, _ = self.tool('publish', prs=prs, fixture=fixture, cwd=other)
        self.assertEqual(code, 2, out)
        self.assertIn('already has', out)
        self.assertEqual(self.queued(), {'b': second}, 'a queued publish was dropped')

    def test_flush_waits_for_the_run_and_then_puts_everything_queued_in_with_one_push(self):
        members = self.pool()
        prs = self.prs(*members)
        top = self.remote('c')
        fixture = self.flying(top, self.RUNNING)
        fix_a = self.commit('a', {'a.txt': 'a fixed\n'}, 'fix a', push=False)
        self.assertEqual(self.tool('publish', prs=prs, fixture=fixture)[0], 0)
        fix_c = self.commit('c', {'c.txt': 'c fixed\n'}, 'fix c', push=False)
        self.assertEqual(self.tool('publish', prs=prs, fixture=fixture)[0], 0)
        self.git('checkout', '-q', '--detach', 'origin/main')
        code, out, _ = self.tool('flush', '--of', '2', prs=prs, fixture=fixture)
        self.assertEqual(code, 0, out)
        self.assertIn('WAITING: run 900', out)
        self.assertEqual((self.remote('c'), set(self.queued())), (top, {'a', 'c'}))
        self.hook_log()
        code, out, _ = self.tool('flush', '--of', '2', prs=prs)
        self.assertEqual(code, 0, out)
        self.assertEqual(self.remote('a'), fix_a)
        for branch in 'bc':
            self.assertTrue(self.contains(branch, fix_a), branch)
        self.assertTrue(self.contains('c', fix_c))
        self.assertEqual(self.queued(), {})
        # One push for the branches, one for the queue entries.
        self.assertEqual((self.dir / 'pushes').read_text().count('push'), 2)
        self.assertIn('nothing is queued', self.tool('flush', '--of', '2', prs=self.prs(*members))[1])

    def test_a_queue_entry_is_removed_only_while_it_names_the_commit_that_went_in(self):
        # Between a flush's build and its clean-up another publish of the same branch is
        # queued: it contains the first and more, and must stay queued.
        self.pool()
        first = self.commit('b', {'b.txt': 'one\n'}, 'fix one', push=False)
        self.git('push', '-q', str(self.origin), f'{first}:refs/ci-pool/pending/b')
        self.git('push', '-q', str(self.origin), f"{self.remote('a')}:refs/ci-pool/pending/a")
        second = self.commit('b', {'b.txt': 'two\n'}, 'fix two', push=False)
        self.git('push', '-q', str(self.origin), f'{second}:refs/ci-pool/pending/b')
        cwd = os.getcwd()
        os.chdir(self.clone)
        try:
            left = ci_pool.drop_queued(str(self.origin), {'a': self.remote('a'), 'b': first})
        finally:
            os.chdir(cwd)
        self.assertEqual((left, self.queued()), ([], {'b': second}))

    def test_a_publish_that_goes_straight_in_takes_what_is_queued_along(self):
        # c's fix was queued while a run was in flight. The run ended, and a publish of a
        # goes in at once: it rebuilds c, so c's queued commit must go in with it or it
        # would no longer contain its branch and no flush could put it in.
        members = self.pool()
        prs = self.prs(*members)
        fix_c = self.commit('c', {'c.txt': 'c fixed\n'}, 'fix c', push=False)
        self.assertEqual(self.tool('publish', prs=prs, fixture=self.flying(self.remote('c'), self.RUNNING))[0], 0)
        self.assertEqual(self.queued(), {'c': fix_c})
        fix_a = self.commit('a', {'a.txt': 'a fixed\n'}, 'fix a', push=False)
        before = self.remote('a')
        code, out, _ = self.tool('publish', '--dry-run', prs=prs)
        self.assertEqual((code, self.remote('a'), self.queued()), (0, before, {'c': fix_c}), out)
        code, out, _ = self.tool('publish', prs=prs)
        self.assertEqual(code, 0, out)
        self.assertEqual(self.remote('a'), fix_a)
        self.assertTrue(self.contains('c', fix_a) and self.contains('c', fix_c), out)
        self.assertEqual(self.queued(), {})
        self.assertIn('nothing is queued', self.tool('flush', '--of', '2', prs=self.prs(*members))[1])

    def test_a_publish_that_goes_straight_in_must_contain_what_its_branch_has_queued(self):
        members = self.pool()
        prs = self.prs(*members)
        queued = self.commit('b', {'b.txt': 'queued\n'}, 'queued fix', push=False)
        self.assertEqual(self.tool('publish', prs=prs, fixture=self.flying(self.remote('c'), self.RUNNING))[0], 0)
        self.git('reset', '-q', '--hard', 'origin/b')
        self.commit('b', {'other.txt': 'other\n'}, 'another fix', push=False)
        before = self.remote('b')
        code, out, calls = self.tool('publish', prs=prs)
        self.assertEqual(code, 2, out)
        self.assertIn(f'b already has {queued[:9]} queued, which these commits do not contain', out)
        self.assertEqual((self.remote('b'), self.queued()), (before, {'b': queued}))

    def test_a_flush_that_conflicts_keeps_the_queue(self):
        members = self.pool()
        prs = self.prs(*members)
        fixture = self.flying(self.remote('c'), self.RUNNING)
        self.commit('c', {'shared.txt': 'from c\n'}, 'c touches shared')
        prs = self.prs(*members)
        fixture = self.flying(self.remote('c'), self.RUNNING)
        fix_a = self.commit('a', {'shared.txt': 'from a\n'}, 'a touches shared', push=False)
        self.assertEqual(self.tool('publish', prs=prs, fixture=fixture)[0], 0)
        self.git('checkout', '-q', '--detach', 'origin/main')
        code, out, _ = self.tool('flush', '--of', '2', prs=prs)
        self.assertEqual(code, 3, out)
        self.assertIn('CONFLICT', out)
        self.assertEqual(self.queued(), {'a': fix_a})

    def hook_log(self):
        path = self.origin / 'hooks' / 'pre-receive'
        path.write_text(f'#!/bin/sh\necho push >> {self.dir}/pushes\ncat > /dev/null\n')
        path.chmod(0o755)

    def test_a_branch_outside_every_pool_or_a_dirty_tree_is_refused(self):
        members = self.pool()
        prs = self.prs(*members)
        self.commit('loose', {'x.txt': 'x\n'}, 'loose')
        code, out, _ = self.tool('publish', prs=prs)
        self.assertEqual(code, 2, out)
        self.assertIn('loose', out)
        self.git('checkout', '-q', 'b')
        (self.clone / 'b.txt').write_text('uncommitted\n')
        code, out, _ = self.tool('publish', prs=prs)
        self.assertEqual(code, 2, out)
        self.assertIn('uncommitted', out)

    def test_local_commits_that_lack_the_remote_branch_are_refused(self):
        members = self.pool()
        other = self.dir / 'other'
        self.git('clone', '-q', str(self.origin), str(other), cwd=self.dir)
        self.git('checkout', '-q', 'b', cwd=other)
        (other / 'b.txt').write_text('someone else\n')
        self.git('commit', '-q', '-am', 'someone else', cwd=other)
        self.git('push', '-q', 'origin', 'b', cwd=other)
        prs = self.prs(*members)
        mine = self.commit('b', {'mine.txt': 'mine\n'}, 'mine', push=False)
        before = self.remote('b')
        code, out, _ = self.tool('publish', prs=prs)
        self.assertEqual(code, 2, out)
        self.assertIn('do not contain origin/b', out)
        self.assertEqual(self.remote('b'), before)
        self.assertNotEqual(before, mine)


class Cost(unittest.TestCase):
    def job(self, name, conclusion, minutes, labels, attempt=1, runner=True):
        # A job that really ran again started later; only GitHub's copies keep the old start.
        start = dt.datetime(2026, 1, 1, tzinfo=dt.timezone.utc) + dt.timedelta(hours=2 * (attempt - 1))
        return {'name': name, 'conclusion': conclusion, 'run_attempt': attempt, 'labels': labels,
                'runner_name': runner, 'started_at': start.isoformat(),
                'completed_at': (start + dt.timedelta(minutes=minutes)).isoformat()}

    def a_run(self, number, event='pull_request', name='CI', path='.github/workflows/ci.yml'):
        return {'id': number, 'name': name, 'path': path, 'event': event, 'status': 'completed', 'run_attempt': 1}

    ARM = ['self-hosted', 'Linux', 'arm64']
    X64 = ['self-hosted', 'Linux', 'x64']

    def test_a_runner_is_classed_by_the_labels_the_job_asked_for(self):
        self.assertEqual(ci_pool.runner_class(self.ARM), ('self-hosted', 'arm64', 'spot'))
        self.assertEqual(ci_pool.runner_class(['self-hosted', 'Linux', 'ARM64']), ('self-hosted', 'arm64', 'spot'))
        self.assertEqual(ci_pool.runner_class(self.X64 + ['on-demand']), ('self-hosted', 'x64', 'on-demand'))
        self.assertEqual(ci_pool.runner_class(['ubuntu-latest']), ('github-hosted', '-', '-'))
        self.assertEqual(ci_pool.runner_class(['ubuntu-24.04-arm']), ('github-hosted', '-', '-'))

    def test_a_job_that_held_no_runner_costs_nothing(self):
        self.assertEqual(ci_pool.job_minutes(self.job('Lint', 'success', 12, ['ubuntu-latest'])), 12)
        self.assertEqual(ci_pool.job_minutes(self.job('Lint', 'skipped', 12, ['ubuntu-latest'])), 0)
        self.assertEqual(ci_pool.job_minutes(self.job('Host-arm64', 'cancelled', 12, self.ARM, runner=False)), 0)

    def test_the_wait_for_a_runner_is_from_a_jobs_creation_to_its_start(self):
        job = dict(self.job('Host-arm64', 'success', 30, self.ARM), created_at='2025-12-31T23:00:00+00:00')
        self.assertEqual(ci_pool.waited_minutes(job), 60)
        self.assertIsNone(ci_pool.waited_minutes(self.job('Host-arm64', 'success', 30, self.ARM)))
        self.assertIsNone(ci_pool.waited_minutes(dict(job, conclusion='skipped')))
        self.assertIsNone(ci_pool.waited_minutes(dict(job, runner_name=False)))
        self.assertIsNone(ci_pool.waited_minutes(dict(job, created_at='2026-01-01T00:05:00+00:00')),
                          'a job created after it started was counted as having waited zero minutes')
        account = ci_pool.summarise([self.a_run(1)], {1: [job, dict(job, name='Container-arm64', created_at='2025-12-31T23:30:00+00:00'),
                                                          dict(self.job('Lint', 'success', 3, ['ubuntu-latest']),
                                                               created_at='2025-12-31T23:59:58+00:00')]})
        self.assertEqual(account['waits'][('self-hosted', 'arm64', '2025-12-31')], [60, 30])
        self.assertEqual(len(account['waits'][('github-hosted', '-', '2025-12-31')]), 1)

    def test_a_job_listed_again_by_a_rerun_of_another_job_is_not_an_execution(self):
        # Run 45237585205, 2026-11-23: one lane was run again twice, and the API lists every
        # other job three times, each with a new id, the new attempt number and the old times.
        ran = self.job('Container-arm64', 'failure', 30, self.ARM)
        copies = [dict(ran, run_attempt=2), dict(ran, run_attempt=3)]
        again = self.job('Host-arm64', 'failure', 20, self.ARM)
        later = self.job('Host-arm64', 'success', 20, self.ARM, attempt=2)
        jobs = [copies[1], later, ran, copies[0], again, dict(later, run_attempt=3)]
        kept = ci_pool.executions(jobs)
        self.assertEqual(sorted((j['name'], j['run_attempt']) for j in kept),
                         [('Container-arm64', 1), ('Host-arm64', 1), ('Host-arm64', 2)])
        account = ci_pool.summarise([self.a_run(1)], {1: jobs})
        self.assertAlmostEqual(sum(g['minutes'] for g in account['groups'].values()), 30 + 20 + 20)
        self.assertEqual(sum(g['jobs'] for g in account['groups'].values()), 3)
        # A job that has not started is never taken for a copy of another.
        queued = [dict(ran, started_at=None, completed_at=None, conclusion=None, run_attempt=n) for n in (1, 2)]
        self.assertEqual(len(ci_pool.executions(queued)), 2)

    def test_every_class_of_runner_has_a_price(self):
        for labels in (self.ARM, self.X64, self.ARM + ['on-demand'], self.X64 + ['on-demand'], ['ubuntu-latest']):
            self.assertIn(ci_pool.runner_class(labels), ci_pool.PRICES_USD_PER_HOUR)

    def test_minutes_and_dollars_add_up_by_runner_class(self):
        jobs = {1: [self.job('Host-arm64', 'success', 60, self.ARM), self.job('Host-Root-x64-SnapshotEnabled', 'success', 30, self.X64),
                    self.job('Host-arm64', 'success', 60, self.ARM + ['on-demand'], attempt=2), self.job('Lint', 'success', 5, ['ubuntu-latest'])]}
        account = ci_pool.summarise([self.a_run(1)], jobs)
        usd = {key[2]: round(g['usd'], 4) for key, g in account['groups'].items()}
        self.assertEqual(usd, {('self-hosted', 'arm64', 'spot'): 0.6403, ('self-hosted', 'x64', 'spot'): 0.597,
                               ('self-hosted', 'arm64', 'on-demand'): 2.903, ('github-hosted', '-', '-'): 0.0})

    def all_lanes(self, **override):
        out = []
        for lane in ci_pool.PR_LANES:
            out.append(self.job(lane, override.get(lane, 'success'), 30, self.ARM if 'arm64' in lane else self.X64))
        return out

    def test_the_failure_rate_counts_first_attempts(self):
        # A run whose rerun passed still failed once: that is what a pool of it would have hit.
        rerun = self.all_lanes(**{'Host-arm64': 'failure'}) + [self.job('Host-arm64', 'success', 30, self.ARM, attempt=2)]
        cancelled = [self.job('Host-arm64', 'cancelled', 3, self.ARM)]
        deferred = [self.job('Summary (deferred)', 'success', 1, ['ubuntu-latest'])]
        jobs = {1: self.all_lanes(), 2: self.all_lanes(), 3: self.all_lanes(), 4: rerun, 5: cancelled, 6: deferred,
                7: self.all_lanes()}
        runs = [self.a_run(n) for n in range(1, 7)] + [self.a_run(7, event='push')]
        account = ci_pool.summarise(runs, jobs)
        self.assertEqual((account['pr_runs_passed'], account['pr_runs_failed']), (3, 1))
        self.assertEqual(account['failure_rate'], 0.25)

    def test_dorfman(self):
        self.assertAlmostEqual(ci_pool.dorfman_two_stage(0.01, 10), 0.1 + 1 - 0.99 ** 10)
        # Two-stage testing is cheapest near 1/sqrt(p).
        for p in (0.01, 0.04, 0.0625):
            best = min(range(2, 40), key=lambda k: ci_pool.dorfman_two_stage(p, k))
            self.assertLessEqual(abs(best - 1 / p ** 0.5), 1.5, p)

    def test_the_recommended_size_falls_as_the_failure_rate_rises(self):
        sizes = [ci_pool.recommend_size(p, 0.4)[0] for p in (0.02, 0.1, 0.3)]
        self.assertEqual(sizes, sorted(sizes, reverse=True))
        self.assertGreater(sizes[0], sizes[-1])
        self.assertAlmostEqual(ci_pool.pooled_runs_per_member(0.1, 4, 0.5), 1 / 4 + 0.1 * (0.5 * 3 + 1))

    def test_the_recommendation_is_the_smallest_size_near_the_fewest_runs(self):
        size, table = ci_pool.recommend_size(0.09, 0.49, 0.44)
        fewest = min(cost for _, cost in table)
        self.assertLess(size, min(k for k, cost in table if cost == fewest))
        self.assertLessEqual(dict(table)[size], fewest * 1.10)
        self.assertGreater(dict(table)[size - 1], fewest * 1.10)

    def test_no_size_is_recommended_when_pooling_cannot_win(self):
        # Two culprits cost two rounds: when most members fail, a pool costs more than testing alone.
        self.assertGreater(ci_pool.pooled_runs_per_member(0.9, 16, 1.0), ci_pool.alone_runs_per_member(0.9, 0))
        self.assertIsNone(ci_pool.recommend_size(0.9, 1.0)[0])
        self.assertIsNotNone(ci_pool.recommend_size(0.05, 1.0)[0])

    def test_a_failure_of_the_runs_own_is_met_once_per_pool(self):
        # The same failure rate, charged to the run instead of to the change, favours pooling:
        # alone, every pull request meets it; pooled, the pool does.
        charged = ci_pool.pooled_runs_per_member(0.5, 8, 0.5)
        split = ci_pool.pooled_runs_per_member(0.1, 8, 0.5, run_rate=0.4)
        self.assertLess(split, charged)
        self.assertAlmostEqual(split, (1 + 0.4 * 0.5 / 0.6) / 8 + 0.1 * (0.5 * 4 + 1))
        self.assertAlmostEqual(ci_pool.alone_runs_per_member(0.1, 0.4), 1.1 / 0.6)

    def test_a_split_needs_enough_reruns_to_rest_on(self):
        few = '\n'.join(ci_pool.size_report(0.5, 0.5, 3, 3))
        self.assertIn('only 3 failed runs were run again', few)
        self.assertIn('no size beats', few)
        enough = '\n'.join(ci_pool.size_report(0.5, 0.5, 20, 16))
        self.assertIn('0.80 of failures are taken as the run\'s own', enough)
        self.assertIn('<- recommended', enough)
        self.assertIn('if every failure were the change\'s, no size would beat', enough)

    def test_a_rerun_is_counted_when_every_failed_lane_ran_again(self):
        both = self.all_lanes(**{'Host-arm64': 'failure', 'Container-arm64': 'failure'})
        passed = both + [self.job('Host-arm64', 'success', 30, self.ARM, attempt=2),
                         self.job('Container-arm64', 'success', 30, self.ARM, attempt=2)]
        half = both + [self.job('Host-arm64', 'success', 30, self.ARM, attempt=2)]
        again = both + [self.job('Host-arm64', 'success', 30, self.ARM, attempt=2),
                        self.job('Container-arm64', 'failure', 30, self.ARM, attempt=2)]
        account = ci_pool.summarise([self.a_run(n) for n in (1, 2, 3)], {1: passed, 2: half, 3: again})
        self.assertEqual((account['pr_runs_failed'], account['reruns'], account['reruns_passed']), (3, 2, 1))


class CostCommand(Sandbox):
    def window(self, day):
        return f'repos/{REPO}/actions/runs?created=2026-01-0{day}T00:00:00Z..2026-01-0{day}T23:59:59Z&per_page=100'

    def listing(self, total, ids):
        return {'total_count': total, 'workflow_runs': [
            {'id': n, 'name': 'CI', 'path': '.github/workflows/ci.yml', 'event': 'pull_request', 'status': 'completed',
             'conclusion': 'success', 'run_attempt': 1, 'created_at': '2026-01-01T00:00:00Z', 'pull_requests': []} for n in ids]}

    def jobs(self, run):
        return f'repos/{REPO}/actions/runs/{run}/jobs?filter=all&per_page=100'

    LANE = {'name': 'Host-arm64', 'conclusion': 'success', 'run_attempt': 1, 'labels': ['self-hosted', 'Linux', 'arm64'],
            'runner_name': 'example-runner', 'started_at': '2026-01-01T00:00:00Z', 'completed_at': '2026-01-01T01:00:00Z'}

    def cost(self, fixture, *extra):
        return self.tool('cost', '--since', '2026-01-01', '--until', '2026-01-02', '--cache', str(self.dir / 'cache'),
                         *extra, fixture=fixture)

    def test_the_account_adds_up_and_keeps_no_runner_name(self):
        code, out, _ = self.cost({self.window(1): self.listing(2, [1, 2]), self.jobs(1): {'jobs': [self.LANE]},
                                  self.jobs(2): {'jobs': [self.LANE]}}, '--json')
        self.assertEqual(code, 0, out)
        self.assertAlmostEqual(json.loads(out.splitlines()[0])['usd'], 2 * 0.6403)
        cached = ''.join(path.read_text() for path in (self.dir / 'cache').iterdir())
        self.assertNotIn('example-runner', cached)

    def test_a_run_whose_jobs_cannot_be_read_makes_the_account_incomplete(self):
        code, out, _ = self.cost({self.window(1): self.listing(2, [1, 2]), self.jobs(1): {'jobs': [self.LANE]},
                                  self.jobs(2): 'FAIL'})
        self.assertEqual(code, 3, out)
        self.assertIn('INCOMPLETE: 1 finished runs are not in this account', out)

    def test_a_listing_shorter_than_its_count_blocks(self):
        code, out, _ = self.cost({self.window(1): self.listing(3, [1, 2])})
        self.assertEqual(code, 2, out)
        self.assertIn('reports 3 runs and listed 2', out)

    def test_a_window_at_the_search_cap_is_split(self):
        half = lambda a, b: f'repos/{REPO}/actions/runs?created=2026-01-01T{a}Z..2026-01-01T{b}Z&per_page=100'
        code, out, calls = self.cost({self.window(1): self.listing(1000, [1]),
                                      half('00:00:00', '11:59:59'): self.listing(1, [1]),
                                      half('12:00:00', '23:59:59'): self.listing(1, [2]),
                                      self.jobs(1): {'jobs': [self.LANE]}, self.jobs(2): {'jobs': [self.LANE]}}, '--json')
        self.assertEqual(code, 0, out)
        self.assertEqual(json.loads(out.splitlines()[0])['counts']['itemised'], 2)

    def test_a_finished_runs_jobs_are_asked_for_once(self):
        fixture = {self.window(1): self.listing(1, [1]), self.jobs(1): {'jobs': [self.LANE]}}
        self.cost(fixture)
        _, _, calls = self.cost(fixture)
        self.assertEqual([c['path'] for c in calls if '/jobs' in c['path']], [])

    def test_short_runs_are_left_out_only_when_asked_and_are_counted(self):
        listing = self.listing(3, [1, 2, 3])
        for run, (start, end, attempt) in zip(listing['workflow_runs'], (('00:00:00', '00:00:40', 1), ('00:00:00', '01:00:00', 1),
                                                                         ('05:00:00', '05:00:40', 2))):
            run.update(run_started_at=f'2026-01-01T{start}Z', updated_at=f'2026-01-01T{end}Z', run_attempt=attempt)
        fixture = {self.window(1): listing, self.jobs(1): {'jobs': [self.LANE]}, self.jobs(2): {'jobs': [self.LANE]},
                   self.jobs(3): {'jobs': [self.LANE]}}
        code, out, calls = self.cost(fixture, '--json')
        self.assertEqual((code, json.loads(out.splitlines()[0])['counts']['itemised']), (0, 3), out)
        (self.dir / 'cache' / '1-1-v2.json').unlink()
        code, out, calls = self.cost(fixture, '--json', '--skip-short', '3')
        counts = json.loads(out.splitlines()[0])['counts']
        # Run 3 is short too, but it is a rerun: its first attempt's time is not in its wall time.
        self.assertEqual((code, counts['itemised'], counts['short'], counts['short_minutes']), (0, 2, 1, 1), out)
        self.assertNotIn(self.jobs(1), [c['path'] for c in calls])
        _, out, _ = self.cost(fixture, '--skip-short', '3')
        self.assertIn('1 runs left out as asked (--skip-short)', out)

    def test_a_job_list_cached_in_an_older_form_is_read_again(self):
        fixture = {self.window(1): self.listing(1, [1]), self.jobs(1): {'jobs': [self.LANE]}}
        (self.dir / 'cache').mkdir()
        (self.dir / 'cache' / '1-1.json').write_text(json.dumps([{'name': 'Host-arm64'}]))
        _, _, calls = self.cost(fixture)
        self.assertEqual([c['path'] for c in calls if '/jobs' in c['path']], [self.jobs(1)])

    def test_a_request_budget_stops_the_account_and_says_so(self):
        code, out, _ = self.cost({self.window(1): self.listing(2, [1, 2]), self.jobs(1): {'jobs': [self.LANE]},
                                  self.jobs(2): {'jobs': [self.LANE]}}, '--max-requests', '2')
        self.assertEqual(code, 3, out)
        self.assertIn('INCOMPLETE', out)


LOG = """\
2026-01-01T00:00:01.0000000Z ##[group]Run make test-root
2026-01-01T00:00:02.0000000Z   TRY 1 FAIL [   0.006s] (─────────) fcvm::test_a retried_then_passed
2026-01-01T00:00:03.0000000Z   TRY 1 FAIL [   0.006s] (─────────) fcvm::test_b fails_every_time
2026-01-01T00:00:04.0000000Z \x1b[31;1m     Summary\x1b[0m [  71.446s] 3150 tests run: 3148 passed (1 flaky), 2 failed, 0 skipped
2026-01-01T00:00:05.0000000Z   FLAKY 2/3 [   0.010s] (  10/3150) fcvm::test_a retried_then_passed
2026-01-01T00:00:05.0000000Z   TRY 3 FAIL [   0.006s] (3104/3150) fcvm::test_b fails_every_time
2026-01-01T00:00:05.0000000Z \x1b[31;1m     TIMEOUT\x1b[0m [ 300.001s] ( 900/3150) fcvm::test_c hangs
2026-01-01T00:00:06.0000000Z ##[error]Process completed with exit code 100.
2026-01-01T00:00:07.0000000Z ##[group]Run ./scripts/ci-stray-vm-guard.sh post
"""


class Failures(unittest.TestCase):
    def test_the_tests_a_run_ended_with_are_the_cause(self):
        # A test that failed a try and then passed is in the log above the summary, not below it.
        self.assertEqual(ci_pool.classify_log(LOG), ['fcvm::test_b fails_every_time', 'fcvm::test_c hangs'])

    def test_a_log_cut_before_its_summary_still_names_what_failed(self):
        cut = LOG.split('2026-01-01T00:00:04')[0]
        self.assertEqual(ci_pool.classify_log(cut), ['fcvm::test_a retried_then_passed', 'fcvm::test_b fails_every_time'])

    def test_a_failure_with_no_test_is_named_by_its_error_and_step(self):
        step = '2026-01-01T00:00:01.0Z ##[group]Run make setup-fcvm\n2026-01-01T00:00:02.0Z building\n'
        self.assertEqual(ci_pool.classify_log(step + '2026-01-01T00:00:03.0Z ##[error]Process completed with exit code 2.\n'),
                         ['step failed without a failed test: make setup-fcvm'])
        self.assertEqual(ci_pool.classify_log(step + '##[error]The runner has received a shutdown signal. This can happen.\n'),
                         ['runner lost: shutdown signal'])
        self.assertEqual(ci_pool.classify_log(step + '##[error]The self-hosted runner lost communication with the server.\n'),
                         ['runner lost: no contact'])
        self.assertEqual(ci_pool.classify_log(step + '##[error]The job has exceeded the maximum execution time of 60m0s\n'),
                         ['job time limit, in step: make setup-fcvm'])
        self.assertEqual(ci_pool.classify_log(step + '##[error]The operation was canceled.\n'),
                         ['cancelled in step: make setup-fcvm'])
        self.assertEqual(ci_pool.classify_log(step + '##[error]No space left on device 12345\n'), ['No space left on device N'])

    def test_a_retried_timeout_and_an_unknown_status_are_failures(self):
        log = ('     Summary [ 245.578s] 3618 tests run: 3615 passed (1 slow), 1 timed out, 0 skipped\n'
               '   TRY 2 TMT [ 120.005s] (3618/3618) fcvm setup::tests::times_out\n'
               '        NEWWORD [   1.000s] (  12/3618) fcvm::test_x some_new_status\n'
               '        SLOW [  61.000s] (  13/3618) fcvm::test_x slow_but_running\n'
               '        PASS [   1.000s] (  14/3618) fcvm::test_x passes\n')
        self.assertEqual(ci_pool.classify_log(log), ['fcvm setup::tests::times_out', 'fcvm::test_x some_new_status'])

    def test_a_test_binary_that_does_not_compile_is_named(self):
        log = ('##[group]Run make test-root\nerror[E0063]: missing field `x` in initializer of `Y`\n'
               'error: could not compile `fcvm` (test "test_fleet_coord") due to 2 previous errors\n'
               'make: *** [Makefile:702: _test-root] Error 1\n##[error]Process completed with exit code 2.\n')
        self.assertEqual(ci_pool.classify_log(log), ['does not compile: fcvm (test "test_fleet_coord")'])

    def test_tests_that_failed_while_the_host_shut_down_are_not_the_cause(self):
        log = LOG + '2026-01-01T00:00:08.0Z ##[error]The runner has received a shutdown signal. This can happen.\n'
        self.assertEqual(ci_pool.classify_log(log), ['runner lost: shutdown signal'])

    def test_a_log_that_shows_no_failure_says_so(self):
        self.assertEqual(ci_pool.classify_log('2026-01-01T00:00:01.0Z all good\n'), ['no failure in the last 3 MB of the log'])

    def failed(self, number, lane, conclusion='failure', attempt=1, minutes=60):
        return {'id': number, 'name': lane, 'conclusion': conclusion, 'run_attempt': attempt, 'runner_name': True,
                'labels': ['self-hosted', 'Linux', 'arm64'], 'started_at': f'2026-01-0{attempt}T00:00:00Z',
                'completed_at': f'2026-01-0{attempt}T0{minutes // 60}:{minutes % 60:02d}:00Z'}

    def test_a_failed_attempt_knows_what_the_next_attempt_said(self):
        run = {'id': 1, 'event': 'pull_request'}
        jobs = {1: [self.failed(10, 'Host-arm64'), self.failed(11, 'Host-arm64', 'success', attempt=2),
                    self.failed(12, 'Container-arm64'), self.failed(13, 'Container-arm64', attempt=2),
                    self.failed(14, 'Host-Root-arm64-SnapshotEnabled'), self.failed(15, 'Lint'),
                    self.failed(16, 'Host-Root-arm64-SnapshotDisabled', 'cancelled')]}
        got = {job['id']: job['again'] for job in ci_pool.failed_lane_jobs([run], jobs)}
        self.assertEqual(got, {10: True, 12: False, 13: None, 14: None})

    def test_a_copied_failure_is_one_failed_attempt_and_was_not_run_again(self):
        run = {'id': 1, 'event': 'pull_request'}
        failed = self.failed(10, 'Container-arm64')
        other = self.failed(12, 'Host-arm64')
        rerun = dict(self.failed(13, 'Host-arm64', 'success', attempt=2))
        jobs = {1: [failed, dict(failed, id=11, run_attempt=2), other, rerun]}
        got = {job['id']: job['again'] for job in ci_pool.failed_lane_jobs([run], jobs)}
        self.assertEqual(got, {10: None, 12: True})

    def test_rows_count_each_cause_and_split_a_jobs_cost_between_its_causes(self):
        failed = [{'id': 1, 'lane': 'Host-arm64', 'at': '2026-01-02T00:00:00Z', 'again': True, 'usd': 1.0},
                  {'id': 2, 'lane': 'Host-x64', 'at': '2026-01-01T00:00:00Z', 'again': False, 'usd': 2.0},
                  {'id': 3, 'lane': 'Host-x64', 'at': '2026-01-03T00:00:00Z', 'again': None, 'usd': 4.0},
                  {'id': 4, 'lane': 'Host-x64', 'at': '2026-01-03T00:00:00Z', 'again': None, 'usd': 8.0}]
        rows = ci_pool.failure_table(failed, {1: ['t a'], 2: ['t a', 't b'], 3: ['runner lost']})
        by = {row['cause']: row for row in rows}
        self.assertEqual([row['cause'] for row in rows], ['log not available', 'runner lost', 't a', 't b'])
        self.assertEqual((by['t a']['failed'], by['t a']['rerun'], by['t a']['rerun_passed'], by['t a']['usd']), (2, 2, 1, 2.0))
        self.assertEqual((by['t a']['first'][:10], by['t a']['last'][:10]), ('2026-01-01', '2026-01-02'))
        self.assertEqual(by['t a']['lanes'], {'Host-arm64': 1, 'Host-x64': 1})
        self.assertEqual(sum(row['usd'] for row in rows), 15.0)
        self.assertEqual(by['log not available']['failed'], 1)


class FailuresCommand(Sandbox):
    def test_the_table_comes_from_the_runs_jobs_and_logs(self):
        window = f'repos/{REPO}/actions/runs?created=2026-01-01T00:00:00Z..2026-01-01T23:59:59Z&per_page=100'
        run = {'id': 1, 'name': 'CI', 'path': '.github/workflows/ci.yml', 'event': 'pull_request', 'status': 'completed',
               'conclusion': 'failure', 'run_attempt': 2, 'created_at': '2026-01-01T00:00:00Z', 'pull_requests': []}
        job = lambda number, conclusion, attempt: {
            'id': number, 'name': 'Host-arm64', 'conclusion': conclusion, 'run_attempt': attempt,
            'labels': ['self-hosted', 'Linux', 'arm64'], 'runner_name': 'example-runner',
            'started_at': f'2026-01-01T0{attempt}:00:00Z', 'completed_at': f'2026-01-01T0{attempt + 1}:00:00Z'}
        fixture = {window: {'total_count': 1, 'workflow_runs': [run]},
                   f'repos/{REPO}/actions/runs/1/jobs?filter=all&per_page=100': {'jobs': [job(10, 'failure', 1), job(11, 'failure', 2)]}}
        logs = self.dir / 'logs'
        logs.mkdir()
        (logs / '10.log').write_text(LOG)
        args = ('failures', '--since', '2026-01-01', '--until', '2026-01-02', '--cache', str(self.dir / 'cache'), '--json')
        code, out, _ = self.tool(*args, fixture=fixture, env={'CI_POOL_LOGS': str(logs)})
        self.assertEqual(code, 3, out)
        self.assertIn('INCOMPLETE: 0 runs without a job list, 1 failed jobs without a log', out)
        rows = {row['cause']: row for row in json.loads(out.splitlines()[0])}
        self.assertEqual(set(rows), {'fcvm::test_b fails_every_time', 'fcvm::test_c hangs', 'log not available'})
        self.assertEqual((rows['fcvm::test_c hangs']['failed'], rows['fcvm::test_c hangs']['rerun_passed']), (1, 0))
        self.assertAlmostEqual(sum(row['usd'] for row in rows.values()), 2 * 0.6403)
        # A runner that vanished leaves no log: the job's annotation names the cause.
        fixture[f'repos/{REPO}/check-runs/11/annotations?per_page=100'] = [
            {'message': 'The self-hosted runner lost communication with the server. Verify the machine is running.'}]
        code, out, _ = self.tool(*args, fixture=fixture, env={'CI_POOL_LOGS': str(logs)})
        self.assertEqual(code, 0, out)
        rows = {row['cause']: row['failed'] for row in json.loads(out.splitlines()[0])}
        self.assertEqual(rows, {'fcvm::test_b fails_every_time': 1, 'fcvm::test_c hangs': 1, 'runner lost: no contact': 1})

    def test_a_log_that_stops_with_no_error_is_explained_by_the_annotation_or_stays_unexplained(self):
        window = f'repos/{REPO}/actions/runs?created=2026-01-01T00:00:00Z..2026-01-01T23:59:59Z&per_page=100'
        run = {'id': 1, 'name': 'CI', 'path': '.github/workflows/ci.yml', 'event': 'push', 'status': 'completed',
               'conclusion': 'failure', 'run_attempt': 1, 'created_at': '2026-01-01T00:00:00Z', 'pull_requests': []}
        job = lambda number: {'id': number, 'name': {20: 'Host-x64', 21: 'Container-x64'}[number], 'conclusion': 'failure', 'run_attempt': 1,
                              'labels': ['self-hosted', 'Linux', 'x64'], 'runner_name': 'example-runner',
                              'started_at': '2026-01-01T00:00:00Z', 'completed_at': '2026-01-01T01:00:00Z'}
        fixture = {window: {'total_count': 1, 'workflow_runs': [run]},
                   f'repos/{REPO}/actions/runs/1/jobs?filter=all&per_page=100': {'jobs': [job(20), job(21)]},
                   f'repos/{REPO}/check-runs/20/annotations?per_page=100': [
                       {'message': 'The self-hosted runner lost communication with the server.'}],
                   f'repos/{REPO}/check-runs/21/annotations?per_page=100': []}
        logs = self.dir / 'logs'
        logs.mkdir()
        for number in (20, 21):
            (logs / f'{number}.log').write_text('2026-01-01T00:00:01.0Z a test was running\n')
        code, out, _ = self.tool('failures', '--since', '2026-01-01', '--until', '2026-01-02', '--cache',
                                 str(self.dir / 'cache'), '--json', fixture=fixture, env={'CI_POOL_LOGS': str(logs)})
        self.assertEqual(code, 0, out)
        self.assertEqual({row['cause'] for row in json.loads(out.splitlines()[0])},
                         {'runner lost: no contact', 'no failure in the last 3 MB of the log'})

    def test_an_annotation_names_only_what_it_says(self):
        self.assertEqual(ci_pool.classify_annotations(['Process completed with exit code 1.']), None)
        self.assertEqual(ci_pool.classify_annotations([]), None)
        self.assertEqual(ci_pool.classify_annotations(['x', 'The job has exceeded the maximum execution time of 60m0s']),
                         ['job time limit, no log'])


class Tool(Sandbox):
    def test_a_missing_dependency_blocks(self):
        code, out, _ = self.tool('status', env={'GH': str(self.dir / 'no-such-gh')})
        self.assertEqual(code, 2, out)
        self.assertIn("BLOCKED: '", out)

    def test_status_lists_each_pool_bottom_to_top_with_its_verdict(self):
        a = self.commit('a', {'a.txt': 'a\n'}, 'a')
        self.commit('b', {'b.txt': 'b\n'}, 'b', start='a')
        verdict = {'statuses': [{'context': 'other', 'state': 'failure'},
                                {'context': 'ci/pool', 'state': 'success', 'description': 'Tested by #2'}]}
        code, out, _ = self.tool('status', prs=self.prs((1, 'a', 'main'), (2, 'b', 'a')) + [pr(9, 'x', 'gone')],
                                 fixture={f'repos/{REPO}/commits/{a}/status': verdict})
        self.assertEqual(code, 0, out)
        self.assertRegex(out, r'pool of 2, top #2 b')
        self.assertRegex(out, r'1\. #1 .* success .*Tested by #2')
        self.assertRegex(out, r'2\. #2 .* none ')
        self.assertRegex(out, r'#9 x: NOT A POOL')


T1, T2 = '1' * 40, '2' * 40
HEAD, BEFORE, LANDED = 'a' * 40, 'b' * 40, 'c' * 40
CLEAN = {'status': 'ahead', 'files': ['src/lib.rs', 'docs/x.md']}


def lane(name, conclusion, attempt=1):
    return {'name': name, 'conclusion': conclusion, 'run_attempt': attempt}


def a_run(number, jobs, event='pull_request', tested=T1):
    return {'id': number, 'event': event, 'jobs': jobs, 'tested_tree': tested}


def passed(names):
    return [lane(name, 'success') for name in names]


SCOPE_SKIPPED = [lane('Changes', 'success'), lane('Skip Check', 'success'), lane('Lint', 'skipped'),
                 lane('Host-${{ matrix.arch }}', 'skipped'), lane('Host-Root-${{ matrix.arch }}-${{ matrix.mode }}', 'skipped'),
                 lane('Container-${{ matrix.arch }}', 'skipped'), lane('Summary', 'success')]
DEFERRED = [lane('Changes', 'success'), lane('Skip Check', 'success'), lane('Lint', 'skipped'),
            lane('Host-${{ matrix.arch }}', 'skipped'), lane('Host-Root-${{ matrix.arch }}-${{ matrix.mode }}', 'skipped'),
            lane('Container-${{ matrix.arch }}', 'skipped'), lane('Summary (deferred)', 'success')]


class Evidence(unittest.TestCase):
    def proof(self, jobs, tree=T1, compare=CLEAN, event='pull_request', **kw):
        return ci_pool.proof_from({'sha': HEAD, 'tree': tree}, [a_run(7, jobs, event)], T1, compare, **kw)

    def test_a_lane_that_succeeded_on_a_commit_with_the_landings_tree_is_proven(self):
        self.assertEqual(self.proof(passed(['Host-arm64'])), ({'Host-arm64': 7}, None, None))

    def test_a_commit_with_another_tree_proves_nothing(self):
        lanes, no_lanes, why = self.proof(passed(ci_pool.LANES), tree=T2)
        self.assertEqual((lanes, no_lanes), ({}, None))
        self.assertIn('another tree', why)

    LISTED = ['.config/nextest.toml', '.github/actions/checkout-deps/*', 'Containerfile*', 'rust-toolchain.toml', '.cargo/*',
              'scripts/ci-pr-scope.sh', 'scripts/root-test-runner.sh', 'scripts/confine-*', 'scripts/lib/*']

    def test_a_landing_that_changes_what_defines_the_lanes_proves_nothing(self):
        # A pull request runs its own workflow, Makefile and scripts: a job named like a
        # lane in such a run may have tested nothing.
        for path in ('.github/workflows/ci.yml', '.github/actions/checkout-deps/action.yml', 'Makefile',
                     '.config/nextest.toml', 'scripts/ci-pr-scope.sh', 'scripts/ci-pool/ci_pool.py',
                     'scripts/ci-pool/lane-defining.txt', 'scripts/root-test-runner.sh', 'scripts/confine-gate.sh',
                     'scripts/lib/vm-evidence.sh', 'Containerfile', 'Containerfile.nested', 'rust-toolchain.toml',
                     '.cargo/config.toml'):
            for listed in (self.LISTED, None):
                compare = {'status': 'ahead', 'files': ['src/lib.rs', path]}
                lanes, no_lanes, why = self.proof(passed(ci_pool.LANES), compare=compare, listed=listed)
                self.assertEqual((lanes, no_lanes), ({}, None), path)
                self.assertIn(path, why)
                skipped = self.proof(SCOPE_SKIPPED, compare={'status': 'ahead', 'files': [path]}, listed=listed)
                self.assertEqual(skipped[:2], ({}, None), path)
        for path in ('src/lib.rs', 'tests/test_sanity.rs', 'docs/scripts/x.md', 'fleet/web/Makefile', 'bench/scripts/a.sh'):
            for listed in (self.LISTED, None):
                self.assertEqual(self.proof(passed(['Host-arm64']), compare={'status': 'ahead', 'files': [path]}, listed=listed)[0],
                                 {'Host-arm64': 7}, path)

    def test_the_list_narrows_what_counts_and_its_absence_widens_it(self):
        # A script no lane runs, and a workflow that is not ci.yml, define no lane. Without a
        # readable list nothing is known about them, so they count.
        for path in ('scripts/claude-assistant/index.ts', 'scripts/docs-helper.sh', '.github/workflows/fleet-web-deploy.yml'):
            compare = {'status': 'ahead', 'files': [path]}
            self.assertEqual(self.proof(passed(['Host-arm64']), compare=compare, listed=self.LISTED)[0], {'Host-arm64': 7}, path)
            self.assertEqual(self.proof(passed(['Host-arm64']), compare=compare, listed=None)[0], {}, path)
            self.assertEqual(self.proof(passed(['Host-arm64']), compare=compare, listed=[])[0], {}, path)

    def test_the_list_cannot_take_itself_the_tool_the_workflow_or_the_makefile_off(self):
        for path in ('.github/workflows/ci.yml', 'Makefile', 'scripts/ci-pool/ci_pool.py', 'scripts/ci-pool/lane-defining.txt'):
            self.assertTrue(ci_pool.lane_defining(path, ['src/unrelated']), path)

    def test_a_renamed_file_is_judged_by_both_of_its_names(self):
        # landing_evidence lists a renamed file under both names: moving a script out of
        # scripts/ is a change to what defines the lanes.
        compare = {'status': 'ahead', 'files': ['tools/x.sh', 'scripts/root-test-runner.sh']}
        self.assertEqual(self.proof(passed(ci_pool.LANES), compare=compare, listed=self.LISTED)[0], {})

    def test_a_comparison_that_may_be_cut_short_or_was_not_read_proves_nothing(self):
        many = {'status': 'ahead', 'files': [f'src/{n}.rs' for n in range(300)]}
        self.assertEqual(self.proof(passed(ci_pool.LANES), compare=many)[0], {})
        self.assertEqual(self.proof(passed(ci_pool.LANES), compare=None)[0], {})
        nearly = {'status': 'ahead', 'files': [f'src/{n}.rs' for n in range(299)]}
        self.assertEqual(len(self.proof(passed(ci_pool.LANES), compare=nearly)[0]), len(ci_pool.LANES))
        # A commit ahead of the default branch changed something: a comparison that lists
        # nothing did not list it.
        self.assertEqual(self.proof(passed(ci_pool.LANES), compare={'status': 'ahead', 'files': []})[0], {})

    def test_a_commit_that_does_not_contain_the_default_branch_proves_nothing(self):
        # A pull request's run tests the merge of its head into its base, which is its
        # head's tree only when the head already contains the base.
        for status in ('diverged', 'behind', '', None):
            lanes, _, why = self.proof(passed(ci_pool.LANES), compare={'status': status, 'files': []})
            self.assertEqual(lanes, {}, status)
        self.assertEqual(len(self.proof(passed(ci_pool.LANES), compare={'status': 'identical', 'files': []})[0]), 8)

    def test_only_a_success_proves_a_lane(self):
        for conclusion in ('failure', 'cancelled', 'skipped', 'timed_out', 'neutral', 'action_required', None):
            self.assertEqual(self.proof([lane('Host-arm64', conclusion)])[0], {}, conclusion)
            self.assertEqual(self.proof([lane('Host-arm64', conclusion)], accept_rerun=True)[0], {}, conclusion)
            if conclusion != 'skipped':
                self.assertEqual(self.proof([lane('Host-arm64', 'success', 1), lane('Host-arm64', conclusion, 2)],
                                            accept_rerun=True)[0], {}, conclusion)
        # A later attempt that skipped the lane says nothing: the earlier success on this tree stands.
        self.assertEqual(self.proof([lane('Host-arm64', 'success', 1), lane('Host-arm64', 'skipped', 2)])[0], {'Host-arm64': 7})

    def test_a_job_that_is_not_exactly_a_lane_proves_no_lane(self):
        jobs = passed(['Summary', 'Lint', 'Host-${{ matrix.arch }}', 'Summary (deferred)', 'host-arm64', 'Host-arm64 ',
                       'Host-arm64 (hosted)', 'Unit-arm64'])
        self.assertEqual(self.proof(jobs), ({}, None, None))

    def test_the_latest_attempt_decides_whatever_the_earlier_ones_were(self):
        states = lambda *jobs: ci_pool.lane_states(list(jobs))
        self.assertEqual(states(lane('Host-arm64', 'success', 1), lane('Host-arm64', 'failure', 2)), {'Host-arm64': 'fail'})
        self.assertEqual(states(lane('Host-arm64', 'failure', 2), lane('Host-arm64', 'success', 1)), {'Host-arm64': 'fail'})
        self.assertEqual(states(lane('Host-arm64', 'failure', 1), lane('Host-arm64', 'success', 2)), {'Host-arm64': 'pass-on-rerun'})
        self.assertEqual(states(lane('Host-arm64', 'cancelled', 1), lane('Host-arm64', 'success', 2)), {'Host-arm64': 'pass-on-rerun'})
        self.assertEqual(states(lane('Host-arm64', 'success', 1), lane('Host-arm64', 'success', 2)), {'Host-arm64': 'pass'})
        self.assertEqual(states(lane('Host-arm64', 'failure', 1), lane('Host-arm64', None, 2)), {'Host-arm64': 'open'})
        for ended in ('cancelled', 'neutral', 'action_required', 'stale'):
            self.assertEqual(states(lane('Host-arm64', ended)), {'Host-arm64': 'cancelled'}, ended)
        self.assertEqual(states(lane('Host-arm64', 'skipped', 1)), {})

    def test_a_lane_that_passed_only_on_a_rerun_is_not_proven_unless_asked(self):
        jobs = [lane('Host-arm64', 'failure', 1), lane('Host-arm64', 'success', 2), lane('Container-arm64', 'success', 1)]
        self.assertEqual(self.proof(jobs)[0], {'Container-arm64': 7})
        self.assertEqual(self.proof(jobs, accept_rerun=True)[0], {'Host-arm64': 7, 'Container-arm64': 7})

    def test_a_lane_counts_whatever_the_rest_of_its_run_did(self):
        # The live case of 2026-11-25: six lanes passed in a run whose Summary failed for a
        # GitHub-hosted job. The lanes are evidence; the run's conclusion is not consulted.
        jobs = passed(l for l in ci_pool.LANES if l not in ('Host-Root-x64-SnapshotDisabled', 'Container-x64'))
        jobs += [lane('Fleet Web', 'failure'), lane('Summary', 'failure'), lane('Stack coverage', 'skipped')]
        lanes, no_lanes, _ = self.proof(jobs)
        self.assertEqual(set(ci_pool.LANES) - set(lanes), {'Host-Root-x64-SnapshotDisabled', 'Container-x64'})
        self.assertIsNone(no_lanes)

    def test_a_run_that_skipped_the_matrix_by_its_own_scope_says_the_tree_needs_no_lane(self):
        self.assertEqual(self.proof(SCOPE_SKIPPED), ({}, 7, None))

    def test_a_deferred_or_failed_or_filtered_run_never_says_the_tree_needs_no_lane(self):
        # A draft's or a lower member's run skips every job under "Summary (deferred)".
        self.assertEqual(self.proof(DEFERRED), ({}, None, None))
        failed_summary = [dict(job, conclusion='failure') if job['name'] == 'Summary' else job for job in SCOPE_SKIPPED]
        self.assertEqual(self.proof(failed_summary)[1], None)
        failed_scope = [dict(job, conclusion='failure') if job['name'] == 'Changes' else job for job in SCOPE_SKIPPED]
        self.assertEqual(self.proof(failed_scope)[1], None)
        rerun = SCOPE_SKIPPED + [lane('Summary', 'failure', 2)]
        self.assertEqual(self.proof(rerun)[1], None)
        # A dispatch that named some lanes skips the others: that is its filter, not a scope.
        for event in ('workflow_dispatch', 'push'):
            self.assertEqual(self.proof(SCOPE_SKIPPED, event=event)[1], None, event)
        ran_one = SCOPE_SKIPPED + passed(['Host-arm64'])
        self.assertEqual(self.proof(ran_one)[1], None)

    def test_a_run_of_an_event_that_is_not_listed_proves_nothing(self):
        for event in ('schedule', 'workflow_run', 'pull_request_target', 'merge_group', ''):
            self.assertEqual(self.proof(passed(ci_pool.LANES), event=event), ({}, None, None), event)

    def test_only_exact_lane_names_can_be_asked_for(self):
        self.assertEqual(ci_pool.validate_lanes(''), ci_pool.LANES)
        self.assertEqual(ci_pool.validate_lanes(' \n'), ci_pool.LANES)
        self.assertEqual(ci_pool.validate_lanes('Host-x64, Host-arm64'), ('Host-arm64', 'Host-x64'))
        for bad in ('host-arm64', 'Host', 'Host-arm64;Host-x64', 'all', 'Host-*', 'Lint', 'Host-arm64 nope'):
            with self.assertRaises(ci_pool.Blocked, msg=bad):
                ci_pool.validate_lanes(bad)


class Plan(unittest.TestCase):
    ALL = set(ci_pool.LANES)

    def evidence(self, jobs, tree=T1, compare=CLEAN, event='pull_request'):
        return [({'sha': HEAD, 'tree': tree}, [a_run(7, jobs, event)], compare)]

    def test_a_pull_request_takes_every_arm64_lane_and_one_x64_lane(self):
        lanes, _ = ci_pool.plan_lanes('pull_request')
        self.assertEqual(set(lanes), {l for l in ci_pool.LANES if 'arm64' in l} | {'Host-Root-x64-SnapshotEnabled'})
        confined, _ = ci_pool.plan_lanes('pull_request', confine=True)
        self.assertEqual(set(confined), set(lanes) | {'Host-x64'})

    def test_main_runs_what_its_pull_request_left_out(self):
        pr, _ = ci_pool.plan_lanes('pull_request')
        main, _ = ci_pool.plan_lanes('push', T1, self.evidence(passed(pr) + passed(['Lint', 'Summary'])))
        self.assertEqual(set(main), self.ALL - set(pr))
        self.assertEqual(set(main) | set(pr), self.ALL, 'the two runs together are not the full matrix')

    def test_a_push_takes_every_lane_without_proof_for_exactly_its_tree(self):
        self.assertEqual(set(ci_pool.plan_lanes('push', T1)[0]), self.ALL)
        self.assertEqual(set(ci_pool.plan_lanes('push', T1, self.evidence(passed(ci_pool.LANES), tree=T2))[0]), self.ALL)
        self.assertEqual(ci_pool.plan_lanes('push', T1, self.evidence(passed(ci_pool.LANES)))[0], ())
        self.assertEqual(ci_pool.plan_lanes('push', T1, self.evidence(SCOPE_SKIPPED))[0], ())
        self.assertEqual(set(ci_pool.plan_lanes('push', T1, self.evidence(DEFERRED))[0]), self.ALL)
        edited = {'status': 'ahead', 'files': ['.github/workflows/ci.yml']}
        self.assertEqual(set(ci_pool.plan_lanes('push', T1, self.evidence(passed(ci_pool.LANES), compare=edited))[0]), self.ALL)

    def test_a_push_runs_again_a_lane_that_passed_only_on_a_rerun(self):
        jobs = passed(l for l in ci_pool.LANES if l != 'Host-arm64') + [lane('Host-arm64', 'failure', 1), lane('Host-arm64', 'success', 2)]
        self.assertEqual(ci_pool.plan_lanes('push', T1, self.evidence(jobs))[0], ('Host-arm64',))

    def test_lanes_add_up_across_runs_and_commits_of_one_tree(self):
        evidence = [({'sha': HEAD, 'tree': T1}, [a_run(7, passed(['Host-arm64'])),
                                                  a_run(8, passed(['Host-x64']) + [lane('Container-arm64', 'failure')], 'workflow_dispatch')], CLEAN),
                    ({'sha': 'd' * 40, 'tree': T2}, [a_run(9, passed(['Container-x64']))], CLEAN)]
        self.assertEqual(self.ALL - set(ci_pool.plan_lanes('push', T1, evidence)[0]), {'Host-arm64', 'Host-x64'})

    def test_a_lane_that_failed_in_one_run_on_the_commit_and_passed_in_another_is_not_proven(self):
        # A dispatch, or closing and reopening the pull request, gives the same commit a
        # second run whose first attempt passes. The lane still failed on this commit once.
        for first, second in (('pull_request', 'workflow_dispatch'), ('workflow_dispatch', 'pull_request'),
                              ('pull_request', 'pull_request')):
            evidence = [({'sha': HEAD, 'tree': T1}, [a_run(7, [lane('Host-arm64', 'failure')] + passed(['Host-x64']), first),
                                                      a_run(8, passed(['Host-arm64']), second)], CLEAN)]
            self.assertEqual(self.ALL - set(ci_pool.plan_lanes('push', T1, evidence)[0]), {'Host-x64'}, (first, second))
        both = ci_pool.proof_from({'sha': HEAD, 'tree': T1}, evidence[0][1], T1, CLEAN, accept_rerun=True)[0]
        self.assertEqual(set(both), {'Host-arm64', 'Host-x64'})

    def test_a_run_that_did_not_record_the_landed_tree_as_tested_counts_for_nothing(self):
        # The same head run against a base it does not contain tested another tree: the
        # merge into that base, with that base's Makefile.
        for tested in (T2, None, ''):
            other = a_run(7, passed(ci_pool.LANES), tested=tested)
            skipped = a_run(8, SCOPE_SKIPPED, tested=tested)
            failed = a_run(6, [lane('Host-x64', 'failure')], tested=tested)
            evidence = [({'sha': HEAD, 'tree': T1}, [failed, other, skipped, a_run(9, passed(['Host-x64']), 'workflow_dispatch')], CLEAN)]
            self.assertEqual(self.ALL - set(ci_pool.plan_lanes('push', T1, evidence)[0]), {'Host-x64'}, tested)

    def test_evidence_is_never_applied_to_another_event(self):
        for event in ('workflow_dispatch', 'workflow_run', 'schedule', 'merge_group', ''):
            self.assertEqual(set(ci_pool.plan_lanes(event, T1, self.evidence(passed(ci_pool.LANES)))[0]), self.ALL, event)
        self.assertEqual(len(ci_pool.plan_lanes('pull_request', T1, self.evidence(passed(ci_pool.LANES)))[0]), len(ci_pool.PR_LANES))

    def test_a_dispatch_takes_the_lanes_it_names_and_refuses_any_other_name(self):
        self.assertEqual(ci_pool.plan_lanes('workflow_dispatch', lanes_input='Container-x64 Host-x64')[0],
                         ('Host-x64', 'Container-x64'))
        with self.assertRaises(ci_pool.Blocked):
            ci_pool.plan_lanes('workflow_dispatch', lanes_input='Container-x86')
        # The input belongs to a dispatch: on any other event it selects nothing.
        self.assertEqual(len(ci_pool.plan_lanes('pull_request', lanes_input='Host-x64')[0]), len(ci_pool.PR_LANES))
        self.assertEqual(set(ci_pool.plan_lanes('push', T1, lanes_input='Host-x64')[0]), self.ALL)
        self.assertEqual(set(ci_pool.plan_lanes('workflow_run', lanes_input='Host-x64')[0]), self.ALL)

    def test_the_matrix_has_a_flag_for_every_list(self):
        out = ci_pool.matrix_outputs(ci_pool.LANES)
        self.assertEqual(json.loads(out['host']), {'arch': ['arm64', 'x64']})
        self.assertEqual(json.loads(out['container']), {'arch': ['arm64', 'x64']})
        self.assertEqual(json.loads(out['host_root'])['include'],
                         [{'arch': 'arm64', 'mode': 'SnapshotDisabled', 'fcvm_no_snapshot': '1', 'test_runs': '1'},
                          {'arch': 'arm64', 'mode': 'SnapshotEnabled', 'fcvm_no_snapshot': '', 'test_runs': '2'},
                          {'arch': 'x64', 'mode': 'SnapshotDisabled', 'fcvm_no_snapshot': '1', 'test_runs': '1'},
                          {'arch': 'x64', 'mode': 'SnapshotEnabled', 'fcvm_no_snapshot': '', 'test_runs': '2'}])
        self.assertEqual((out['host_run'], out['host_root_run'], out['container_run'], out['skip']),
                         ('true', 'true', 'true', 'false'))
        none = ci_pool.matrix_outputs(())
        self.assertEqual((none['host'], none['host_root'], none['container']),
                         ('{"arch":[]}', '{"include":[]}', '{"arch":[]}'))
        self.assertEqual((none['host_run'], none['host_root_run'], none['container_run'], none['skip']),
                         ('false', 'false', 'false', 'true'))
        one = ci_pool.matrix_outputs(('Host-Root-x64-SnapshotEnabled',))
        self.assertEqual((one['host_run'], one['host_root_run'], one['container_run'], one['skip']),
                         ('false', 'true', 'false', 'false'))


class PlanCommand(Sandbox):
    """`plan` as ci.yml's skip-check job runs it, for a commit pushed to the default branch."""

    def answers(self, runs, parents=(BEFORE, HEAD), pulls=(), head_tree=T1, compare=CLEAN, path='.github/workflows/ci.yml',
                created=None, head=HEAD, before=BEFORE, tested=T1):
        created = created or dt.datetime.now(dt.timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ')
        fixture = {f'repos/{REPO}/git/commits/{LANDED}': {'tree': {'sha': T1}, 'parents': [{'sha': p} for p in parents]},
                   f'repos/{REPO}/commits/{LANDED}/pulls?per_page=100': [
                       {'head': {'sha': sha, 'repo': {'full_name': repo}}} for sha, repo in pulls],
                   f'repos/{REPO}/git/commits/{head}': {'tree': {'sha': head_tree}},
                   f'repos/{REPO}/compare/{before}...{head}': {
                       'status': compare['status'], 'files': [{'filename': name} for name in compare['files']]},
                   f'repos/{REPO}/actions/runs?head_sha={head}&per_page=100': {'workflow_runs': [
                       {'id': 100 + n, 'path': path, 'event': event, 'created_at': created} for n, (event, _) in enumerate(runs)]}}
        for n, (_, jobs) in enumerate(runs):
            # Each run's planning job, and the notice in which it recorded the tree it tested.
            planner = dict(lane('Skip Check', 'success'), id=9000 + n)
            jobs = [dict(job, id=job.get('id', 1000 * (n + 1) + k)) for k, job in enumerate(jobs) if job['name'] != 'Skip Check'] + [planner]
            fixture[f'repos/{REPO}/actions/runs/{100 + n}/jobs?filter=all&per_page=100'] = {'jobs': jobs}
            fixture[f'repos/{REPO}/check-runs/{9000 + n}/annotations?per_page=100'] = [] if tested is None else [
                {'annotation_level': 'notice', 'title': 'ci-pool tested tree', 'message': tested}]
        return fixture

    def plan(self, fixture, event='push', **env):
        out = self.dir / 'github_output'
        out.write_text('')
        env.setdefault('BEFORE', BEFORE)
        code, text, calls = self.tool('plan', fixture=fixture, env=dict(EVENT=event, SHA=LANDED, GITHUB_OUTPUT=str(out), **env))
        outputs = dict(line.split('=', 1) for line in out.read_text().splitlines())
        planned = set()
        if outputs:
            planned |= {f'Host-{a}' for a in json.loads(outputs['host'])['arch']}
            planned |= {f'Container-{a}' for a in json.loads(outputs['container'])['arch']}
            planned |= {f"Host-Root-{e['arch']}-{e['mode']}" for e in json.loads(outputs['host_root'])['include']}
            self.assertEqual(outputs['skip'], str(not planned).lower())
        return code, text, planned, calls

    ALL = set(ci_pool.LANES)

    def test_main_runs_only_what_the_merged_head_did_not_prove(self):
        code, text, planned, _ = self.plan(self.answers([('pull_request', passed(ci_pool.PR_LANES))]))
        self.assertEqual(code, 0, text)
        self.assertEqual(planned, self.ALL - set(ci_pool.PR_LANES))

    def test_lanes_proven_by_a_dispatch_on_the_merged_head_count_too(self):
        rest = [l for l in ci_pool.LANES if l not in ci_pool.PR_LANES]
        fixture = self.answers([('pull_request', passed(ci_pool.PR_LANES)), ('workflow_dispatch', passed(rest))])
        self.assertEqual(self.plan(fixture)[2], set())

    def test_a_squashed_pull_request_is_found_through_the_commit(self):
        fixture = self.answers([('pull_request', passed(ci_pool.LANES))], parents=(BEFORE,), pulls=[(HEAD, REPO)])
        self.assertEqual(self.plan(fixture)[2], set())

    def test_a_forks_pull_request_is_not_evidence(self):
        fixture = self.answers([('pull_request', passed(ci_pool.LANES))], parents=(BEFORE,), pulls=[(HEAD, 'someone/fork')])
        code, text, planned, calls = self.plan(fixture)
        self.assertEqual(planned, self.ALL)
        self.assertFalse([c for c in calls if 'head_sha=' in c['path']], 'the fork head was looked up')

    def test_a_head_with_another_tree_proves_nothing_and_is_not_looked_at_further(self):
        # The pull request was behind main when it merged: its runs tested another tree.
        code, text, planned, calls = self.plan(self.answers([('pull_request', passed(ci_pool.LANES))], head_tree=T2))
        self.assertEqual(planned, self.ALL)
        self.assertFalse([c for c in calls if 'head_sha=' in c['path'] or '/compare/' in c['path']])

    def test_a_run_from_a_tree_with_an_edited_workflow_proves_nothing(self):
        edited = {'status': 'ahead', 'files': ['src/lib.rs', '.github/workflows/ci.yml']}
        code, text, planned, _ = self.plan(self.answers([('pull_request', passed(ci_pool.LANES))], compare=edited))
        self.assertEqual(planned, self.ALL)
        self.assertIn('the landing changes what defines the lanes: .github/workflows/ci.yml', text)

    def test_the_planner_reads_the_committed_list_and_falls_back_to_everything_without_it(self):
        other = {'status': 'ahead', 'files': ['src/lib.rs', '.github/workflows/fleet-web-deploy.yml', 'scripts/docs-helper.sh']}
        fixture = self.answers([('pull_request', passed(ci_pool.LANES))], compare=other)
        code, text, planned, _ = self.plan(fixture)
        self.assertEqual(planned, self.ALL, 'with no list every script and workflow must count')
        self.assertIn('cannot be read', text)
        (self.clone / 'scripts/ci-pool').mkdir(parents=True)
        (self.clone / 'scripts/ci-pool/lane-defining.txt').write_text('# derived\nscripts/root-test-runner.sh\n')
        self.assertEqual(self.plan(fixture)[2], set())
        listed = {'status': 'ahead', 'files': ['src/lib.rs', 'scripts/root-test-runner.sh']}
        self.assertEqual(self.plan(self.answers([('pull_request', passed(ci_pool.LANES))], compare=listed))[2], self.ALL)
        (self.clone / 'scripts/ci-pool/lane-defining.txt').write_text('# nothing\n')
        self.assertEqual(self.plan(fixture)[2], self.ALL, 'an empty list was taken as nothing being lane-defining')

    def test_a_job_named_like_a_lane_in_a_run_of_another_workflow_proves_nothing(self):
        for path in ('.github/workflows/other.yml', '.github/workflows/ci.yml.bak', 'ci.yml', '.github/workflows/fork/ci.yml',
                     'dynamic/.github/workflows/ci.yml'):
            fixture = self.answers([('pull_request', passed(ci_pool.LANES))], path=path)
            self.assertEqual(self.plan(fixture)[2], self.ALL, path)

    def test_a_renamed_lane_defining_file_is_seen_under_its_old_name(self):
        fixture = self.answers([('pull_request', passed(ci_pool.LANES))])
        fixture[f'repos/{REPO}/compare/{BEFORE}...{HEAD}']['files'] = [{'filename': 'tools/x.sh', 'previous_filename': 'scripts/x.sh'}]
        self.assertEqual(self.plan(fixture)[2], self.ALL)

    def test_a_deferred_run_on_the_merged_head_proves_nothing(self):
        self.assertEqual(self.plan(self.answers([('pull_request', DEFERRED)]))[2], self.ALL)
        self.assertEqual(self.plan(self.answers([('pull_request', SCOPE_SKIPPED)]))[2], set())

    def test_every_failed_lookup_runs_the_lanes(self):
        good = self.answers([('pull_request', passed(ci_pool.LANES))])
        self.assertEqual(self.plan(good)[2], set())
        for path in good:
            if path.endswith('/pulls?per_page=100'):
                continue  # the merged head is also a parent here: this lookup adds nothing
            broken = dict(good)
            broken[path] = 'FAIL'
            code, text, planned, _ = self.plan(broken)
            self.assertEqual((code, planned), (0, self.ALL), f'{path} failed and lanes were skipped: {text}')
        squashed = self.answers([('pull_request', passed(ci_pool.LANES))], parents=(BEFORE,), pulls=[(HEAD, REPO)])
        self.assertEqual(self.plan(squashed)[2], set())
        squashed[f'repos/{REPO}/commits/{LANDED}/pulls?per_page=100'] = 'FAIL'
        self.assertEqual(self.plan(squashed)[2], self.ALL)

    def test_changes_are_judged_against_main_before_the_push_not_the_tips_first_parent(self):
        # A fast-forward of two commits: the first changes the Makefile, the second is the
        # pull request's head. The tip's first parent already has the Makefile change.
        older = 'e' * 40
        fixture = self.answers([('pull_request', passed(ci_pool.LANES))], parents=(BEFORE,), pulls=[(HEAD, REPO)], before=older,
                               compare={'status': 'ahead', 'files': ['Makefile', 'src/lib.rs']})
        fixture[f'repos/{REPO}/compare/{BEFORE}...{HEAD}'] = {'status': 'ahead', 'files': [{'filename': 'src/lib.rs'}]}
        code, text, planned, calls = self.plan(fixture, BEFORE=older)
        self.assertEqual(planned, self.ALL, text)
        self.assertIn('the landing changes what defines the lanes: Makefile', text)
        self.assertNotIn(f'repos/{REPO}/compare/{BEFORE}...{HEAD}', [c['path'] for c in calls])

    def test_a_push_that_does_not_say_what_main_was_before_proves_nothing(self):
        for before in ('', '0' * 40, 'main', 'abc'):
            # Even if the comparison with such a name were answered.
            good = self.answers([('pull_request', passed(ci_pool.LANES))], before=before)
            code, text, planned, _ = self.plan(good, BEFORE=before)
            self.assertEqual(planned, self.ALL, before)
            self.assertIn('does not say what the default branch was before it', text)

    def test_a_run_counts_only_when_its_planning_job_recorded_the_landed_tree(self):
        # The head was also run against a base it does not contain (a second pull request
        # of the same commit, or a stack member whose lower branch had moved): that run
        # tested the merge into that base and recorded that merge's tree.
        jobs = [('pull_request', passed(ci_pool.LANES))]
        self.assertEqual(self.plan(self.answers(jobs))[2], set())
        for tested in (T2, None, 'not a tree'):
            code, text, planned, _ = self.plan(self.answers(jobs, tested=tested))
            self.assertEqual(planned, self.ALL, tested)
            self.assertIn('counts for nothing', text)
        # Only a notice with this title on the planning job is the record.
        for change in ({'title': 'another title'}, {'annotation_level': 'warning'}):
            fixture = self.answers(jobs)
            fixture[f'repos/{REPO}/check-runs/9000/annotations?per_page=100'][0].update(change)
            self.assertEqual(self.plan(fixture)[2], self.ALL, change)
        printed_by_a_lane = self.answers(jobs, tested=None)
        printed_by_a_lane[f'repos/{REPO}/check-runs/1000/annotations?per_page=100'] = [
            {'annotation_level': 'notice', 'title': 'ci-pool tested tree', 'message': T1}]
        self.assertEqual(self.plan(printed_by_a_lane)[2], self.ALL)
        # Two records that disagree are no record, whichever is read first.
        class Answers:
            repo = REPO

            def __init__(self, messages):
                self.messages = messages

            def call(self, *_args, **_kwargs):
                return self.messages
        planner = [dict(lane('Skip Check', 'success'), id=1)]
        self.assertEqual(ci_pool.tested_tree_of(Answers([T1, T1]), planner), T1)
        for messages in ([T1, T2], [T2, T1], []):
            self.assertIsNone(ci_pool.tested_tree_of(Answers(messages), planner), messages)
        unreadable = self.answers(jobs)
        unreadable[f'repos/{REPO}/check-runs/9000/annotations?per_page=100'] = 'FAIL'
        self.assertEqual(self.plan(unreadable)[2], self.ALL)

    def test_every_run_of_the_planner_records_the_tree_it_is_run_on(self):
        tree = subprocess.run(['git', 'rev-parse', 'HEAD^{tree}'], cwd=self.clone, capture_output=True, text=True).stdout.strip()
        for event in ('pull_request', 'workflow_dispatch', 'push'):
            code, text, _, _ = self.plan({}, event=event)
            self.assertIn(f'::notice title=ci-pool tested tree::{tree}\n', text, event)

    def test_a_lane_that_failed_in_another_run_on_the_merged_head_runs_on_main(self):
        fixture = self.answers([('pull_request', passed(l for l in ci_pool.LANES if l != 'Host-arm64') + [lane('Host-arm64', 'failure')]),
                                ('workflow_dispatch', passed(['Host-arm64']))])
        self.assertEqual(self.plan(fixture)[2], {'Host-arm64'})

    def test_a_push_without_its_commit_runs_the_lanes(self):
        out = self.dir / 'github_output'
        out.write_text('')
        code, text, _ = self.tool('plan', fixture={}, env=dict(EVENT='push', GITHUB_OUTPUT=str(out)))
        self.assertEqual(code, 0, text)
        self.assertIn('skip=false', out.read_text())

    def test_old_evidence_is_not_used(self):
        fixture = self.answers([('pull_request', passed(ci_pool.LANES))], created='2020-01-01T00:00:00Z')
        self.assertEqual(self.plan(fixture)[2], self.ALL)

    def test_no_other_event_asks_for_evidence(self):
        for event in ('workflow_dispatch', 'workflow_run'):
            code, text, planned, calls = self.plan({}, event=event)
            self.assertEqual((code, planned), (0, self.ALL), text)
            self.assertEqual(calls, [])
        code, text, planned, calls = self.plan({}, event='pull_request')
        self.assertEqual(planned, set(ci_pool.PR_LANES))
        self.assertEqual(calls, [])
        _, _, planned, _ = self.plan({}, event='pull_request', CONFINE='true')
        self.assertEqual(planned, set(ci_pool.PR_LANES) | {'Host-x64'})

    def test_a_dispatch_with_a_bad_lane_fails_the_job_and_plans_nothing(self):
        code, text, planned, _ = self.plan({}, event='workflow_dispatch', LANES='Host-arm65')
        self.assertEqual(code, 2, text)
        self.assertIn('not a lane: Host-arm65', text)
        self.assertEqual(planned, set())
        code, _, planned, _ = self.plan({}, event='workflow_dispatch', LANES='Host-x64,Container-x64')
        self.assertEqual((code, planned), (0, {'Host-x64', 'Container-x64'}))


class Wiring(unittest.TestCase):
    """ci.yml's side of the planner."""

    def test_the_planner_step_runs_the_tool_with_what_it_reads(self):
        text = CI.read_text()
        block = text.split('\n  skip-check:\n', 1)[1].split('\n  # Runner 0', 1)[0]
        for line in ('run: python3 scripts/ci-pool/ci_pool.py plan', 'EVENT: ${{ github.event_name }}', 'SHA: ${{ github.sha }}',
                     'REPO: ${{ github.repository }}', 'CONFINE: ${{ needs.changes.outputs.confine }}',
                     'BEFORE: ${{ github.event.before }}',
                     'LANES: ${{ github.event.inputs.lanes }}', 'GH_TOKEN: ${{ github.token }}', 'id: check'):
            self.assertIn(line, block)
        # An input is data: it reaches the planner through the environment, never a script.
        # Its only other use is in the name of the report job.
        uses = [line.strip().split(':', 1)[0] for line in text.splitlines() if 'inputs.lanes' in line]
        self.assertEqual(uses, ['LANES', 'name'])

    def test_the_committed_list_is_what_ci_yml_and_the_makefile_give(self):
        # A script that ci.yml or the Makefile starts to call has to reach the list:
        # `python3 scripts/ci-pool/ci_pool.py lane-files --write`.
        self.assertEqual(ci_pool.read_lane_files(ROOT), ci_pool.lane_files(ROOT))

    def test_the_committed_list_holds_what_the_lanes_are_known_to_run(self):
        listed = ci_pool.read_lane_files(ROOT)
        for path in ('.github/workflows/ci.yml', 'Makefile', '.config/nextest.toml', 'rust-toolchain.toml',
                     '.github/actions/checkout-deps/action.yml', 'scripts/ci-pr-scope.sh', 'scripts/ci-pool/ci_pool.py',
                     'scripts/root-test-runner.sh', 'scripts/with-test-config.sh', 'scripts/cargo-target-run.sh',
                     'scripts/cargo-target-lib.sh', 'scripts/ci-apt-get.sh', 'scripts/check-runner-kernel.py',
                     'scripts/no-sudo.sh', 'Containerfile'):
            self.assertTrue(ci_pool.lane_defining(path, listed), f'{path} is run by a lane and is not on the list')
        for path in ('src/main.rs', 'tests/test_sanity.rs', '.github/workflows/fleet-web-deploy.yml',
                     'scripts/check-review-threads.sh', 'scripts/analyze_ci_vms.py'):
            self.assertFalse(ci_pool.lane_defining(path, listed), path)
        for entry in listed:
            self.assertTrue('*' in entry or (ROOT / entry).exists(), f'{entry} is listed and does not exist')


MAKEFILE = """\
RUNNER := ./scripts/runner.sh
WRAP = $(RUNNER) --wrap
STAMP := $(shell scripts/stamp.sh)
OTHER := scripts/unused-var.sh
export EVERYWHERE

build: private SHELL := scripts/lease-shell.sh
build: prepare
\tcargo build

prepare:
\t./scripts/prepare.sh

test-unit: build _test-unit
_test-unit:
\t$(WRAP) cargo test \\
\t\t--release

test-root: build
\t$(MAKE) --no-print-directory _test-root
_test-root:
\tscripts/root-runner.sh

docs:
\tscripts/docs-only.sh
"""


class LaneFiles(Sandbox):
    """The derivation, on a small tree."""

    def test_the_makefile_is_followed_from_the_targets_the_lanes_make(self):
        text = ci_pool.makefile_text_for(MAKEFILE, ['test-unit'])
        for part in ('scripts/runner.sh', 'scripts/prepare.sh', 'scripts/lease-shell.sh', 'scripts/stamp.sh', '--release'):
            self.assertIn(part, text)
        for part in ('scripts/root-runner.sh', 'scripts/docs-only.sh', 'scripts/unused-var.sh'):
            self.assertNotIn(part, text)
        self.assertIn('scripts/root-runner.sh', ci_pool.makefile_text_for(MAKEFILE, ['test-root']))

    def test_a_target_that_is_not_a_rule_is_not_followed(self):
        self.assertIsNone(ci_pool.makefile_text_for(MAKEFILE, ['test-unit', 'no-such-target']))

    def tree(self, files):
        for name, text in files.items():
            path = self.clone / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text)
        self.git('add', '-A')
        self.git('commit', '-q', '-m', 'tree')
        return ci_pool.lane_files(self.clone)

    CI_YML = """\
name: CI
jobs:
  changes:
    steps:
      - run: bash scripts/scope.sh
  skip-check:
    steps:
      - run: python3 scripts/ci-pool/ci_pool.py plan
  host:
    steps:
      - uses: ./fcvm/.github/actions/checkout-deps
      - run: ./fcvm/scripts/apt.sh update
      - run: make test-unit
      - run: for t in scripts/suite-*.sh; do "$t"; done
  host-root:
    steps:
      - run: make test-root
  container:
    steps:
      - run: make test-unit
  fleet:
    steps:
      - run: bash scripts/fleet-only.sh
      - uses: ./.github/actions/fleet-setup
"""

    FILES = {'.github/workflows/ci.yml': CI_YML, 'Makefile': MAKEFILE, '.github/actions/checkout-deps/action.yml': 'x',
             '.github/actions/fleet-setup/action.yml': 'x', '.github/workflows/other.yml': 'x',
             'scripts/scope.sh': 'x', 'scripts/ci-pool/ci_pool.py': 'x', 'scripts/apt.sh': '. "$HERE/apt-lib.sh"\nscripts/tools/\n',
             'scripts/apt-lib.sh': 'x', 'scripts/tools/a.sh': 'x', 'scripts/tools/b.sh': 'x', 'scripts/suite-1.sh': 'x',
             'scripts/suite-2.sh': 'x', 'scripts/fleet-only.sh': 'x', 'scripts/runner.sh': 'x', 'scripts/prepare.sh': 'x',
             'scripts/lease-shell.sh': 'x', 'scripts/stamp.sh': 'x', 'scripts/root-runner.sh': 'x', 'scripts/docs-only.sh': 'x',
             'scripts/unused-var.sh': 'x', 'scripts/other/apt-lib.sh': 'x', 'Containerfile.nested': 'x', 'src/lib.rs': 'x'}

    def test_what_the_lanes_run_is_listed_and_what_they_do_not_run_is_not(self):
        listed = self.tree(self.FILES)
        on = lambda path: ci_pool.lane_defining(path, [e for e in listed])
        for path in ('scripts/scope.sh', 'scripts/apt.sh', 'scripts/runner.sh', 'scripts/prepare.sh', 'scripts/lease-shell.sh',
                     'scripts/stamp.sh', 'scripts/root-runner.sh', '.github/actions/checkout-deps/action.yml',
                     'Containerfile.nested', '.github/workflows/ci.yml', 'Makefile'):
            self.assertTrue(on(path), path)
        # Named by another listed script: by file name beside it, and by directory.
        for path in ('scripts/apt-lib.sh', 'scripts/tools/a.sh', 'scripts/tools/b.sh', 'scripts/tools/later.sh'):
            self.assertTrue(on(path), path)
        # Named with a glob: whatever starts that way, files added later included.
        for path in ('scripts/suite-1.sh', 'scripts/suite-2.sh', 'scripts/suite-new.sh'):
            self.assertTrue(on(path), path)
        # Run only by a job that is not a lane, by a Makefile target no lane makes, or by nothing.
        entries = set(listed)
        for path in ('scripts/fleet-only.sh', 'scripts/docs-only.sh', 'scripts/unused-var.sh', 'scripts/other/apt-lib.sh'):
            self.assertNotIn(path, entries, path)
        self.assertFalse(any(entry.startswith('.github/actions/fleet-setup') for entry in listed))
        self.assertFalse(any('other.yml' in entry or entry.startswith('src/') for entry in listed))

    def test_a_makefile_that_cannot_be_followed_is_taken_whole(self):
        files = dict(self.FILES, **{'.github/workflows/ci.yml': self.CI_YML.replace('make test-root', 'make no-such-target')})
        self.assertIn('scripts/docs-only.sh', self.tree(files))

    def test_the_command_prints_the_list_and_writes_it_only_when_asked(self):
        self.tree(self.FILES)
        code, out, _ = self.tool('lane-files')
        self.assertEqual(code, 0, out)
        self.assertIn('scripts/root-runner.sh\n', out)
        self.assertFalse((self.clone / 'scripts/ci-pool/lane-defining.txt').exists())
        code, out, _ = self.tool('lane-files', '--write')
        self.assertEqual(ci_pool.read_lane_files(self.clone), ci_pool.lane_files(self.clone))


BOUNDARY = 'fcvm commands::common::tests::a_boundarys_token_is_on_the_state_from_its_prepare_until_its_gate_is_open'
SIX = ('Host-arm64', 'Host-x64', 'Host-Root-arm64-SnapshotDisabled', 'Host-Root-arm64-SnapshotEnabled',
       'Host-Root-x64-SnapshotEnabled', 'Container-arm64')


class RedPool(unittest.TestCase):
    """What `diagnose` says for a pool, from the states of the runs on its members' heads."""

    def pool(self, size):
        return [{'number': 100 + n, 'head': f'm{n}', 'sha': f'{n:040x}'} for n in range(size)]

    def diagnose(self, members, states=None, causes=None, tries=None, spent=0, **kw):
        at = lambda table: {members[i]['sha']: value for i, value in (table or {}).items()}
        return ci_pool.diagnose(members, at(states), at(causes), at(tries), spent, **kw)

    def all(self, state, lanes=ci_pool.PR_LANES):
        return {lane: state for lane in lanes}

    # ---- the live case of 2026-11-25: a pool of nine, a failure that needs two members

    def live(self, more=None):
        members = [{'number': n, 'head': h, 'sha': f'{i:040x}'} for i, (n, h) in enumerate((
            (2062, 'confine-2-seccomp'), (2133, 'wake-model'), (2167, 'restore-no-view-listing'),
            (2161, 'ami-boots-checkout-kernel'), (2175, 'runner-arm64-nic-order'), (2170, 'renders-host-rate'),
            (2171, 'renders-host-image'), (2166, 'boundary-token'), (2077, 'state-store-buckets')))]
        states = {6: self.all('pass', SIX), 8: self.all('fail', SIX)}
        states.update(more or {})
        causes = {8: {lane: [BOUNDARY] for lane in SIX}}
        causes.update({index: {lane: [BOUNDARY] for lane, state in lanes.items() if state == 'fail'}
                       for index, lanes in (more or {}).items()})
        # boundary-token (index 7) wrote the failing test; state-store-buckets (8) moved the state.
        files = {7: ['src/commands/common.rs', 'src/boundary.rs'], 8: ['src/state/manager.rs', 'src/paths.rs']}
        first = ci_pool.suspects([BOUNDARY], files)
        return members, self.diagnose(members, states, causes, required=SIX, first=first), first

    def test_live_a_test_that_failed_on_six_hosts_is_not_run_again(self):
        _, step, _ = self.live()
        self.assertNotEqual(step['outcome'], 'rerun')

    def test_live_the_one_unknown_prefix_is_probed_on_the_cheapest_lane_that_shows_it(self):
        members, step, first = self.live()
        self.assertEqual(first, [7], 'the member that wrote the failing test is where to look first')
        self.assertEqual((step['outcome'], members[step['member']]['number']), ('probe', 2166))
        self.assertEqual(step['lanes'], ('Host-Root-arm64-SnapshotDisabled',))
        self.assertEqual(step['between'], (6, 8))

    def test_live_the_verdict_names_the_prefix_and_not_the_member_alone(self):
        members, step, first = self.live({7: {'Host-Root-arm64-SnapshotDisabled': 'pass'}})
        self.assertEqual((step['outcome'], members[step['member']]['number']), ('culprit', 2077))
        self.assertIn('the failure appears when #2077 is added to the pool', step['text'])
        self.assertIn('The prefix ending at #2166 passes', step['text'])
        self.assertNotRegex(step['text'], r'alone|by itself|on its own')
        # The suspect was #2166, which is not the first failing prefix: a suspect orders
        # the probes and is never the verdict.
        self.assertNotEqual(first, [step['member']])

    def test_live_had_the_probe_failed_the_member_that_wrote_the_test_is_the_first_failing_prefix(self):
        members, step, _ = self.live({7: {'Host-Root-arm64-SnapshotDisabled': 'fail'}})
        self.assertEqual((step['outcome'], members[step['member']]['number']), ('culprit', 2166))

    # ---- green and waiting

    def test_nothing_is_concluded_before_the_tops_run_has_finished(self):
        members = self.pool(3)
        self.assertEqual(self.diagnose(members)['outcome'], 'waiting')
        self.assertEqual(self.diagnose(members, {2: dict(self.all('pass'), **{'Host-arm64': 'open'})})['outcome'], 'waiting')
        self.assertEqual(self.diagnose(members, {2: dict(self.all('fail'), **{'Host-arm64': 'open'})})['outcome'], 'waiting')

    def test_green_needs_every_lane_the_tops_run_takes(self):
        members = self.pool(3)
        self.assertEqual(self.diagnose(members, {2: self.all('pass')})['outcome'], 'green')
        partial = {lane: 'pass' for lane in ci_pool.PR_LANES[:-1]}
        step = self.diagnose(members, {2: partial})
        self.assertEqual(step['outcome'], 'waiting')
        self.assertIn(ci_pool.PR_LANES[-1], step['why'])
        # A lower member's green run says nothing about the top.
        self.assertEqual(self.diagnose(members, {1: self.all('pass')})['outcome'], 'waiting')
        # No lane is required only when the top's own run found that the change needs none.
        self.assertEqual(self.diagnose(members, {2: {}}, required=())['outcome'], 'green')
        self.assertEqual(self.diagnose(members, {2: {}})['outcome'], 'waiting')

    def test_a_lane_that_failed_and_then_passed_is_not_simply_green(self):
        members = self.pool(3)
        states = dict(self.all('pass'), **{'Host-arm64': 'pass-on-rerun'})
        step = self.diagnose(members, {2: states}, {2: {'Host-arm64': ['fcvm::test_x sometimes']}})
        self.assertEqual((step['outcome'], step['lanes'], step['causes']), ('passed-on-rerun', ['Host-arm64'], ['fcvm::test_x sometimes']))

    # ---- the rerun

    def test_a_failure_seen_once_is_run_again_once(self):
        members = self.pool(4)
        states = dict(self.all('pass'), **{'Host-arm64': 'fail'})
        causes = {3: {'Host-arm64': ['fcvm::test_x fails']}}
        step = self.diagnose(members, {3: states}, causes, {3: {'Host-arm64': 1}})
        self.assertEqual((step['outcome'], step['lanes']), ('rerun', ['Host-arm64']))
        again = self.diagnose(members, {3: states}, causes, {3: {'Host-arm64': 2}}, spent=1)
        self.assertEqual(again['outcome'], 'probe')

    def test_a_cause_that_is_not_a_test_is_run_again_even_when_it_took_several_lanes(self):
        # A mirror outage or a reclaimed spot pool takes several lanes at once.
        members = self.pool(4)
        for cause in ('runner lost: shutdown signal', 'runner lost: no contact', 'log not available',
                      'step failed without a failed test: ./fcvm/scripts/ci-apt-get.sh update',
                      'job time limit, in step: make test-root', 'no failure in the last 3 MB of the log'):
            states = dict(self.all('pass'), **{'Host-arm64': 'fail', 'Container-arm64': 'fail'})
            causes = {3: {'Host-arm64': [cause], 'Container-arm64': [cause]}}
            step = self.diagnose(members, {3: states}, causes, {3: {'Host-arm64': 1, 'Container-arm64': 1}})
            self.assertEqual(step['outcome'], 'rerun', cause)
        unread = self.diagnose(members, {3: dict(self.all('pass'), **{'Host-arm64': 'fail', 'Container-arm64': 'fail'})})
        self.assertEqual(unread['outcome'], 'rerun', 'failed lanes whose cause was not read were not run again')

    def test_only_the_lanes_not_yet_reproduced_are_run_again(self):
        members = self.pool(4)
        states = dict(self.all('pass'), **{'Host-arm64': 'fail', 'Container-arm64': 'fail', 'Host-Root-arm64-SnapshotEnabled': 'fail'})
        causes = {3: {'Host-arm64': ['fcvm::test_x fails'], 'Container-arm64': ['fcvm::test_x fails'],
                      'Host-Root-arm64-SnapshotEnabled': ['fcvm::test_y fails_here_only']}}
        step = self.diagnose(members, {3: states}, causes)
        self.assertEqual((step['outcome'], step['lanes']), ('rerun', ['Host-Root-arm64-SnapshotEnabled']))

    # ---- the bisect

    def red(self, size, culprit, probes=(), cause='fcvm::test_x fails', lane='Host-arm64'):
        """A pool whose top fails `lane` twice, and what the probes so far showed."""
        members = self.pool(size)
        states = {size - 1: dict(self.all('pass'), **{lane: 'fail'})}
        causes = {size - 1: {lane: [cause]}}
        for index in probes:
            states[index] = {lane: 'fail' if index >= culprit else 'pass'}
            if index >= culprit:
                causes[index] = {lane: [cause]}
        return members, states, causes, {size - 1: {lane: 2}}

    def bisect(self, size, culprit, **kw):
        probes, steps = [], []
        for _ in range(size + 2):
            members, states, causes, tries = self.red(size, culprit, probes)
            step = self.diagnose(members, states, causes, tries, spent=1 + len(probes), **kw)
            steps.append(step)
            if step['outcome'] != 'probe':
                return step, probes
            self.assertNotIn(step['member'], probes, 'a prefix was probed twice')
            probes.append(step['member'])
        self.fail(f'no verdict after {steps}')

    def test_every_culprit_position_is_found_in_about_log2_runs(self):
        for size in (2, 3, 5, 8, 12, 16):
            bound = (size - 1).bit_length()
            for culprit in range(size):
                step, probes = self.bisect(size, culprit, cap=99, base={'Host-arm64': 'pass'})
                self.assertEqual((step['outcome'], step['member']), ('culprit', culprit), (size, culprit, probes))
                self.assertLessEqual(len(probes), bound, (size, culprit, probes))

    def test_a_suspect_is_settled_first_and_a_wrong_one_costs_at_most_two_runs(self):
        step, probes = self.bisect(16, 11, first=[11], cap=99)
        self.assertEqual((step['member'], probes), (11, [10, 11]))
        step, probes = self.bisect(16, 3, first=[11], cap=99)
        self.assertEqual(step['member'], 3)
        self.assertLessEqual(len(probes), 2 + 4)

    def test_the_bottom_is_the_verdict_only_when_the_commit_the_pool_is_based_on_passes(self):
        members, states, causes, tries = self.red(3, 0, probes=(0, 1))
        asked = self.diagnose(members, states, causes, tries, spent=3)
        self.assertEqual(asked['outcome'], 'need-base')
        self.assertNotIn('member', asked)
        passing = self.diagnose(members, states, causes, tries, spent=3, base={'Host-arm64': 'pass'})
        self.assertEqual((passing['outcome'], passing['member']), ('culprit', 0))
        self.assertIn('The commit the pool is based on passes Host-arm64', passing['text'])
        # With no passing prefix there is no verdict: the failure may be older than the pool.
        for base in ({}, {'Host-x64': 'pass'}, {'Host-arm64': 'open'}, {'Host-arm64': 'pass-on-rerun'}):
            unknown = self.diagnose(members, states, causes, tries, spent=3, base=base)
            self.assertEqual(unknown['outcome'], 'not-isolated', base)
            self.assertIn('may have been there before the pool', unknown['why'])
        failing = self.diagnose(members, states, causes, tries, spent=3, base={'Host-arm64': 'fail'})
        self.assertEqual(failing['outcome'], 'not-isolated')
        self.assertIn('the commit the pool is based on fails these lanes too', failing['why'])

    def test_a_failure_whose_cause_was_not_read_is_not_the_same_failure(self):
        # The probe on the prefix failed and its log could not be fetched. It may be a lost
        # runner: calling it the same failure would move the verdict down the pool.
        for nothing in ([], [ci_pool.NO_FAILURE_IN_LOG]):
            members, states, causes, tries = self.red(3, 1, probes=(0, 1))
            causes[1] = {'Host-arm64': nothing}
            step = self.diagnose(members, states, causes, tries, spent=3)
            self.assertEqual(step['outcome'], 'not-isolated', nothing)
            self.assertIn(f"why the prefix ending at #{members[1]['number']} fails could not be read", step['why'])
            # The same when it is the top's own cause that was not read.
            members, states, causes, tries = self.red(3, 1, probes=(0,))
            causes[2] = {'Host-arm64': nothing}
            step = self.diagnose(members, states, causes, tries, spent=2)
            self.assertEqual(step['outcome'], 'not-isolated', nothing)
            self.assertIn('failed on the top could not be read', step['why'])
        members, states, causes, tries = self.red(3, 1, probes=(0, 1))
        self.assertEqual(self.diagnose(members, states, causes, tries, spent=3)['outcome'], 'culprit')

    def test_a_probe_in_flight_is_waited_for_and_not_dispatched_again(self):
        members, states, causes, tries = self.red(8, 5)
        states[3] = {'Host-arm64': 'open'}
        self.assertEqual(self.diagnose(members, states, causes, tries, spent=2)['outcome'], 'waiting')

    def test_two_causes_are_probed_with_the_cheapest_lanes_that_cover_both(self):
        self.assertEqual(ci_pool.probe_lanes({'Host-Root-x64-SnapshotEnabled': ['t a'], 'Host-arm64': ['t a'],
                                              'Container-arm64': ['t b'], 'Container-x64': ['t b']}),
                         ('Host-arm64', 'Container-arm64'))
        self.assertEqual(ci_pool.probe_lanes({'Host-arm64': ['t a', 't b'], 'Host-Root-arm64-SnapshotDisabled': ['t a']}),
                         ('Host-arm64',))
        self.assertEqual(ci_pool.probe_lanes({'Host-x64': [], 'Host-arm64': ['t a']}), ('Host-arm64', 'Host-x64'))

    # ---- what cannot be isolated

    def test_the_cap_ends_it_with_every_member_untested(self):
        members, states, causes, tries = self.red(16, 9)
        step = self.diagnose(members, states, causes, tries, spent=ci_pool.MAX_EXTRA_RUNS)
        self.assertEqual(step['outcome'], 'not-isolated')
        self.assertIn('the cap is 8', step['why'])
        once = self.diagnose(members, states, causes, {15: {'Host-arm64': 1}}, spent=ci_pool.MAX_EXTRA_RUNS)
        self.assertEqual(once['outcome'], 'not-isolated', 'a rerun was asked for past the cap')
        # A verdict the runs already show is still given past the cap: it costs nothing.
        members, states, causes, tries = self.red(4, 2, probes=(1, 2))
        self.assertEqual(self.diagnose(members, states, causes, tries, spent=50)['outcome'], 'culprit')

    def test_the_worst_case_fits_under_the_cap(self):
        # One rerun and a bisect of the largest pool the size table covers.
        self.assertLessEqual(1 + (16 - 1).bit_length(), ci_pool.MAX_EXTRA_RUNS)

    def test_a_longer_prefix_that_passes_above_one_that_fails_is_not_resolved_by_guessing(self):
        members, states, causes, tries = self.red(8, 5)
        states[2], causes[2] = {'Host-arm64': 'fail'}, {'Host-arm64': ['fcvm::test_x fails']}
        states[4] = {'Host-arm64': 'pass'}
        step = self.diagnose(members, states, causes, tries, spent=3)
        self.assertEqual(step['outcome'], 'not-isolated')
        self.assertIn('#102 fails and the longer one ending at #104 passes', step['why'])

    def test_a_prefix_that_both_failed_and_passed_ends_it(self):
        members, states, causes, tries = self.red(8, 5)
        states[3] = {'Host-arm64': 'pass-on-rerun'}
        step = self.diagnose(members, states, causes, tries, spent=3)
        self.assertEqual(step['outcome'], 'not-isolated')
        self.assertIn('does not reproduce the same way', step['why'])

    def test_a_prefix_that_fails_for_another_cause_is_not_counted_as_the_failure(self):
        members, states, causes, tries = self.red(8, 5)
        states[3], causes[3] = {'Host-arm64': 'fail'}, {'Host-arm64': ['runner lost: shutdown signal']}
        step = self.diagnose(members, states, causes, tries, spent=3)
        self.assertEqual(step['outcome'], 'not-isolated')
        self.assertIn('#103 fails these lanes for another cause', step['why'])

    def test_no_outcome_marks_a_member_green_but_green(self):
        # Every path out of a red top leaves the other members untested: only GREEN and
        # PASSED_ON_RERUN speak for the pool, and both need the top's own lanes.
        self.assertEqual({ci_pool.GREEN, ci_pool.PASSED_ON_RERUN, ci_pool.WAITING, ci_pool.RERUN, ci_pool.PROBE,
                          ci_pool.CULPRIT, ci_pool.NOT_ISOLATED},
                         {'green', 'passed-on-rerun', 'waiting', 'rerun', 'probe', 'culprit', 'not-isolated'})

    # ---- helpers

    def test_states_of_several_runs_on_one_commit_merge(self):
        merge = ci_pool.merge_lane_states
        self.assertEqual(merge([{'Host-arm64': 'fail'}, {'Host-arm64': 'pass'}]), {'Host-arm64': 'pass-on-rerun'})
        self.assertEqual(merge([{'Host-arm64': 'pass'}, {'Host-arm64': 'fail'}]), {'Host-arm64': 'pass-on-rerun'})
        self.assertEqual(merge([{'Host-arm64': 'fail'}, {'Host-arm64': 'open'}]), {'Host-arm64': 'fail'})
        self.assertEqual(merge([{'Host-arm64': 'pass'}, {'Host-arm64': 'open'}, {}]), {'Host-arm64': 'pass'})
        self.assertEqual(merge([{'Host-arm64': 'open'}]), {'Host-arm64': 'open'})
        self.assertEqual(merge([{'Host-arm64': 'cancelled'}]), {'Host-arm64': 'cancelled'})
        self.assertEqual(merge([{'Host-arm64': 'cancelled'}, {'Host-arm64': 'open'}]), {'Host-arm64': 'open'})
        self.assertEqual(merge([{'Host-arm64': 'cancelled'}, {'Host-arm64': 'pass'}]), {'Host-arm64': 'pass'})
        self.assertEqual(merge([{'Host-arm64': 'cancelled'}, {'Host-arm64': 'fail'}]), {'Host-arm64': 'fail'})
        self.assertEqual(merge([{'Host-arm64': 'pass'}, {'Host-x64': 'fail'}]), {'Host-arm64': 'pass', 'Host-x64': 'fail'})
        self.assertEqual(merge([]), {})

    def test_a_failing_tests_name_points_at_its_file(self):
        self.assertEqual(ci_pool.test_file_of(BOUNDARY), ('src/commands/common.rs', 'src/commands/common/mod.rs'))
        self.assertEqual(ci_pool.test_file_of('fcvm::test_netpool clones_run_in_recycled_namespaces'), ('tests/test_netpool.rs',))
        for cause in ('runner lost: no contact', 'does not compile: fcvm (test "test_fleet_coord")', 'log not available'):
            self.assertEqual(ci_pool.test_file_of(cause), (), cause)

    def test_a_suspect_is_the_only_member_that_changed_the_tests_file(self):
        files = {2: ['src/commands/common.rs'], 5: ['src/state/manager.rs']}
        self.assertEqual(ci_pool.suspects([BOUNDARY], files), [2])
        self.assertEqual(ci_pool.suspects([BOUNDARY], {2: files[2], 5: ['src/commands/common.rs']}), [])
        self.assertEqual(ci_pool.suspects(['runner lost: no contact', 'fcvm::test_x y'], files), [])


class PoolOfThree(Sandbox):
    """A pool of three on a fake GitHub, and runs on its members' heads."""

    def setUp(self):
        super().setUp()
        self.SHAS = ['1' * 40, '2' * 40, '3' * 40]
        self.logs = self.dir / 'logs'
        self.logs.mkdir()
        self.next_run = 500
        self.fixture = {f'repos/{REPO}/actions/runs?head_sha=*': {'workflow_runs': []},
                        f'repos/{REPO}/check-runs/*': [], f'repos/{REPO}/compare/*': {'files': []},
                        f'repos/{REPO}/issues?state=open&per_page=100': []}
        self.members = [pr(11, 'a', 'main', self.SHAS[0]), pr(12, 'b', 'a', self.SHAS[1]), pr(13, 'c', 'b', self.SHAS[2])]

    def add_run(self, index, lanes, event='pull_request', attempt=1, test='fcvm::test_x fails', concluded='success'):
        """A run on member `index` with {lane: conclusion or [conclusion per attempt]}."""
        self.next_run += 1
        number, jobs = self.next_run, []
        for lane_name, conclusions in lanes.items():
            for n, conclusion in enumerate(conclusions if isinstance(conclusions, list) else [conclusions], 1):
                job_id = number * 100 + len(jobs)
                jobs.append({'id': job_id, 'name': lane_name, 'conclusion': conclusion, 'run_attempt': n})
                if conclusion == 'failure':
                    (self.logs / f'{job_id}.log').write_text(
                        f'     Summary [  1.0s] 9 tests run: 8 passed, 1 failed\n        FAIL [   0.1s] ( 3/9) {test}\n')
        key = f'repos/{REPO}/actions/runs?head_sha={self.SHAS[index]}&per_page=100'
        listed = self.fixture.setdefault(key, {'workflow_runs': []})['workflow_runs']
        listed.append({'id': number, 'path': '.github/workflows/ci.yml', 'event': event, 'status': 'completed',
                       'conclusion': concluded, 'run_attempt': attempt, 'html_url': f'https://example.invalid/runs/{number}',
                       'created_at': '2026-01-01T00:00:00Z'})
        self.fixture[f'repos/{REPO}/actions/runs/{number}/jobs?filter=all&per_page=100'] = {'jobs': jobs}
        return number

    def green(self):
        return {lane_name: 'success' for lane_name in ci_pool.PR_LANES}


class BisectCommand(PoolOfThree):
    """`bisect`: what it reads, says and does."""

    def bisect(self, *args):
        code, out, calls = self.tool('bisect', '--of', '13', *args, prs=self.members, fixture=self.fixture,
                                     env={'CI_POOL_LOGS': str(self.logs)})
        return code, out, self.writes(calls)

    def test_a_green_top_needs_nothing(self):
        self.add_run(2, self.green())
        code, out, writes = self.bisect('--go')
        self.assertEqual((code, writes), (0, []), out)
        self.assertIn('GREEN', out)

    def test_without_go_nothing_is_done(self):
        self.add_run(2, dict(self.green(), **{'Host-arm64': 'failure'}))
        code, out, writes = self.bisect()
        self.assertEqual((code, writes), (0, []), out)
        self.assertIn('NEXT: run Host-arm64 again on the top', out)

    def test_a_failure_seen_once_reruns_that_lanes_job_and_no_other(self):
        # Not "the run's failed jobs": after the unit watcher stopped a run, that would run
        # every cancelled lane again.
        lanes = dict(self.green(), **{'Host-arm64': 'failure', 'Container-arm64': 'cancelled'})
        run = self.add_run(2, lanes)
        failed = run * 100 + list(lanes).index('Host-arm64')
        code, out, writes = self.bisect('--go')
        self.assertEqual(writes, [('POST', f'actions/jobs/{failed}/rerun', {})], out)

    def test_a_run_stopped_at_a_failed_unit_step_is_diagnosed_from_the_lane_that_failed(self):
        lanes = {lane_name: 'cancelled' for lane_name in ci_pool.PR_LANES}
        lanes['Host-arm64'] = ['failure', 'failure']
        self.add_run(2, lanes, attempt=2)
        code, out, writes = self.bisect('--go')
        self.assertNotIn('WAITING', out)
        self.assertEqual(writes, [('POST', 'actions/workflows/ci.yml/dispatches', {'ref': 'a', 'inputs[lanes]': 'Host-arm64'})], out)

    def test_a_top_whose_lanes_were_only_cancelled_is_not_green(self):
        self.add_run(2, dict(self.green(), **{'Container-arm64': 'cancelled'}))
        code, out, writes = self.bisect('--go')
        self.assertIn("WAITING: the top's run has not run Container-arm64", out)

    def test_a_failure_seen_twice_dispatches_the_failing_lane_on_a_lower_member(self):
        self.add_run(2, dict(self.green(), **{'Host-arm64': ['failure', 'failure']}), attempt=2)
        code, out, writes = self.bisect('--go')
        self.assertEqual(writes, [('POST', 'actions/workflows/ci.yml/dispatches', {'ref': 'a', 'inputs[lanes]': 'Host-arm64'})], out)
        self.assertIn('1 of 8 extra runs spent', out)

    def test_a_probe_that_passes_names_the_first_failing_prefix_and_posts_it_on_that_member(self):
        top = self.add_run(2, dict(self.green(), **{'Host-arm64': ['failure', 'failure']}), attempt=2)
        self.add_run(0, {'Host-arm64': 'success'}, event='workflow_dispatch')
        self.add_run(1, {'Host-arm64': 'success'}, event='workflow_dispatch')
        code, out, writes = self.bisect('--go')
        self.assertIn('VERDICT: first failing prefix: the failure appears when #13 is added to the pool', out)
        self.assertIn('3 of 8 extra runs spent', out)
        self.assertEqual(len(writes), 1, writes)
        method, path, fields = writes[0]
        self.assertEqual((method, path), ('POST', f'statuses/{self.SHAS[2]}'))
        self.assertEqual((fields['state'], fields['context'], fields['target_url']),
                         ('failure', 'ci/pool', f'https://example.invalid/runs/{top}'))
        self.assertIn('fails when #13 is added', fields['description'])
        self.assertLessEqual(len(fields['description']), 140)

    def test_a_lane_listed_again_under_a_later_attempt_was_not_run_again(self):
        # Run 45237585205: rerunning one job listed every other job again under the new
        # attempt, with the old conclusion and start. A lane that ran once is run again once.
        top = self.add_run(2, dict(self.green(), **{'Host-arm64': 'failure'}), attempt=3)
        jobs = self.fixture[f'repos/{REPO}/actions/runs/{top}/jobs?filter=all&per_page=100']['jobs']
        for job in jobs:
            job['started_at'] = '2026-01-01T00:00:00Z'
        jobs += [dict(job, id=job['id'] + 50 * attempt, run_attempt=attempt) for job in list(jobs) for attempt in (2, 3)]
        code, out, writes = self.bisect('--go')
        self.assertIn('NEXT: run Host-arm64 again', out)
        newest = max(job['id'] for job in jobs if job['name'] == 'Host-arm64')
        self.assertEqual([path for _, path, _ in writes], [f'actions/jobs/{newest}/rerun'])

    def base(self, lanes=None, tested_head=None, head_tree=None):
        """The commit of main the pool is based on, and the runs on it or on the head that
        landed as it."""
        base, tree = '8' * 40, '7' * 40
        self.SHAS.append(base)
        self.fixture[f'repos/{REPO}/compare/main...{self.SHAS[0]}'] = {'merge_base_commit': {'sha': base}}
        parents = ['6' * 40] + ([tested_head] if tested_head else [])
        self.fixture[f'repos/{REPO}/git/commits/{base}'] = {'tree': {'sha': tree}, 'parents': [{'sha': p} for p in parents]}
        if tested_head:
            self.fixture[f'repos/{REPO}/git/commits/{tested_head}'] = {'tree': {'sha': head_tree or tree}}
            self.SHAS.append(tested_head)
        if lanes:
            self.add_run(len(self.SHAS) - 1, lanes, event='pull_request' if tested_head else 'push')

    def all_fail(self):
        top = self.add_run(2, dict(self.green(), **{'Host-arm64': ['failure', 'failure']}), attempt=2)
        self.add_run(0, {'Host-arm64': 'failure'}, event='workflow_dispatch')
        self.add_run(1, {'Host-arm64': 'failure'}, event='workflow_dispatch')
        return top

    def test_the_bottom_is_marked_only_when_the_commit_the_pool_is_based_on_passes(self):
        self.all_fail()
        self.base({'Host-arm64': 'success'})
        code, out, writes = self.bisect('--go')
        self.assertIn('VERDICT: first failing prefix: the failure appears with #11, the bottom of the pool', out)
        self.assertEqual([(m, p) for m, p, _ in writes], [('POST', f'statuses/{self.SHAS[0]}')])

    def test_the_run_of_the_head_that_landed_as_the_base_counts_for_the_base(self):
        # Main skips the lanes a landing already proved, so its own commit often has no run.
        self.all_fail()
        self.base({'Host-arm64': 'success'}, tested_head='5' * 40)
        code, out, writes = self.bisect('--go')
        self.assertIn('the bottom of the pool', out)
        self.assertEqual(len(writes), 1, writes)
        # A parent with another tree is a commit that was merged in, not what landed.
        self.setUp()
        self.all_fail()
        self.base({'Host-arm64': 'success'}, tested_head='5' * 40, head_tree='4' * 40)
        code, out, writes = self.bisect('--go')
        self.assertEqual((code, writes), (3, []), out)

    def test_with_no_passing_prefix_and_nothing_passing_on_the_base_nobody_is_marked(self):
        for lanes in (None, {'Host-x64': 'success'}, {'Host-arm64': 'failure'}):
            self.setUp()
            self.all_fail()
            self.base(lanes)
            code, out, writes = self.bisect('--go')
            self.assertEqual((code, writes), (3, []), out)
            self.assertIn('NOT-ISOLATED', out)
        # Main's tip passing says nothing: the pool is based on an older commit of main.
        self.setUp()
        self.all_fail()
        self.SHAS.append('4' * 40)
        self.fixture[f'repos/{REPO}/commits/main'] = {'sha': '4' * 40}
        self.fixture[f'repos/{REPO}/git/commits/{"4" * 40}'] = {'tree': {'sha': '7' * 40}, 'parents': [{'sha': '6' * 40}]}
        self.add_run(3, {'Host-arm64': 'success'}, event='push')
        code, out, writes = self.bisect('--go')
        self.assertEqual((code, writes), (3, []), out)

    def test_the_only_status_bisect_posts_is_a_failure(self):
        # A success is a claim that lanes passed: only the run that passed states it (`mark`).
        source = TOOL.read_text()
        body = source.split('def cmd_bisect(', 1)[1].split('\n# ---', 1)[0]
        self.assertEqual(re.findall(r"status_fields\('(\w+)'", body), ['failure'])
        self.assertEqual(source.count('/statuses/'), 3, 'a status is posted somewhere other than mark, bisect and unit-watch')
        watcher = source.split('def cmd_unit_watch(', 1)[1].split('\n# ---', 1)[0]
        self.assertEqual(re.findall(r"status_fields\('(\w+)'", watcher), ['failure'])
        self.assertEqual(source.split('def cmd_mark(', 1)[1].split('\n# ---', 1)[0].count('/statuses/'), 1)

    def test_a_failure_that_then_passed_is_filed_under_the_issue_that_owns_its_cause(self):
        run = self.add_run(2, dict(self.green(), **{'Host-arm64': ['failure', 'success']}), attempt=2,
                           test='fcvm::test_fuzz_gym gym_pairs_04')
        self.fixture[f'repos/{REPO}/issues?state=open&per_page=100'] = [
            {'number': 70, 'body': 'no marker here'},
            # A marker owns a cause it equals, or one that continues it after a space: not any longer name.
            {'number': 71, 'body': '<!-- ci-pool-cause: fcvm::test_fuzz_gym_other -->'},
            {'number': 72, 'body': '<!-- ci-pool-cause: fcvm::test_fuzz -->'},
            # A pull request whose description quotes a marker owns nothing.
            {'number': 80, 'body': '<!-- ci-pool-cause: fcvm::test_fuzz_gym -->', 'pull_request': {}},
            {'number': 85, 'body': '<!-- ci-pool-cause: fcvm::test_fuzz_gym -->\n<!-- ci-pool-cause: runner lost: no contact -->'}]
        self.fixture[f'repos/{REPO}/issues/85/comments?per_page=100'] = [{'body': 'an earlier run'}]
        code, out, writes = self.bisect('--go')
        self.assertEqual(code, 0, out)
        self.assertIn('PASSED ON RERUN', out)
        self.assertIn('issue #85', out)
        self.assertEqual([(m, p) for m, p, _ in writes], [('POST', 'issues/85/comments')])
        self.assertIn(f'https://example.invalid/runs/{run}', writes[0][2]['body'])
        # The same run is not reported twice.
        self.fixture[f'repos/{REPO}/issues/85/comments?per_page=100'].append({'body': writes[0][2]['body']})
        self.assertEqual(self.bisect('--go')[2], [])

    def test_a_cause_no_issue_owns_gets_a_new_issue_that_owns_it(self):
        self.add_run(2, dict(self.green(), **{'Host-arm64': ['failure', 'success']}), attempt=2)
        self.fixture[f'POST repos/{REPO}/issues'] = {'number': 99}
        code, out, writes = self.bisect('--go')
        self.assertIn('issue #99', out)
        self.assertEqual([(m, p) for m, p, _ in writes], [('POST', 'issues')])
        self.assertEqual(writes[0][2]['title'], 'CI failure: fcvm::test_x fails')
        self.assertTrue(writes[0][2]['body'].startswith('<!-- ci-pool-cause: fcvm::test_x fails -->'))

    def test_reruns_of_the_top_and_dispatches_on_members_are_the_extra_runs_spent(self):
        self.add_run(2, dict(self.green(), **{'Host-arm64': ['failure', 'failure', 'failure']}), attempt=3)
        self.add_run(0, {'Host-arm64': 'success'}, event='workflow_dispatch')
        self.add_run(0, self.green())      # a pull request's own run is not an extra run
        _, out, _ = self.bisect()
        self.assertIn('3 of 8 extra runs spent', out)

    def test_the_cap_stops_it_and_says_every_member_is_untested(self):
        self.add_run(2, dict(self.green(), **{'Host-arm64': ['failure', 'failure']}), attempt=9)
        code, out, writes = self.bisect('--go')
        self.assertEqual((code, writes), (3, []), out)
        self.assertIn('NOT-ISOLATED: 8 extra runs spent, the cap is 8', out)
        self.assertIn('every member stays untested', out)

    def test_only_a_run_of_its_own_lanes_makes_the_top_green(self):
        # A dispatch that named one lane passed on the top, and its pull request's run was
        # cancelled before a lane started: the lanes a pull request takes have not run.
        self.add_run(2, {'Host-arm64': 'success'}, event='workflow_dispatch')
        code, out, writes = self.bisect('--go')
        self.assertIn('WAITING', out)
        self.assertNotIn('GREEN', out)

    def test_a_top_whose_scope_needed_no_lane_is_green(self):
        self.next_run += 1
        key = f'repos/{REPO}/actions/runs?head_sha={self.SHAS[2]}&per_page=100'
        self.fixture[key] = {'workflow_runs': [{'id': self.next_run, 'path': '.github/workflows/ci.yml', 'event': 'pull_request',
                                                'status': 'completed', 'run_attempt': 1, 'html_url': 'https://example.invalid/r',
                                                'created_at': '2026-01-01T00:00:00Z'}]}
        self.fixture[f'repos/{REPO}/actions/runs/{self.next_run}/jobs?filter=all&per_page=100'] = {
            'jobs': [dict(job, id=n) for n, job in enumerate(SCOPE_SKIPPED)]}
        code, out, writes = self.bisect('--go')
        self.assertIn('GREEN', out)
        self.fixture[f'repos/{REPO}/actions/runs/{self.next_run}/jobs?filter=all&per_page=100'] = {
            'jobs': [dict(job, id=n) for n, job in enumerate(DEFERRED)]}
        self.assertIn('WAITING', self.bisect('--go')[1])

    def unfinished(self, index, attempt=1):
        key = f'repos/{REPO}/actions/runs?head_sha={self.SHAS[index]}&per_page=100'
        run = self.fixture[key]['workflow_runs'][-1]
        run.update(status='in_progress', run_attempt=attempt)
        return run

    def test_a_probe_that_was_just_dispatched_is_not_dispatched_again(self):
        # For about a minute after a dispatch the run exists and lists no lane job.
        self.add_run(2, dict(self.green(), **{'Host-arm64': ['failure', 'failure']}), attempt=2)
        self.add_run(0, {}, event='workflow_dispatch')
        self.unfinished(0)
        code, out, writes = self.bisect('--go')
        self.assertEqual((code, writes), (0, []), out)
        self.assertIn('WAITING', out)
        self.assertIn('1 of 8', out.replace('2 of 8', '1 of 8'))

    def test_a_rerun_that_was_just_asked_for_is_not_asked_for_again(self):
        # The run is on its second attempt and the lane's new job is not listed yet.
        every = {lane_name: 'success' for lane_name in ci_pool.LANES}
        self.add_run(2, dict(every, **{'Host-arm64': 'failure'}))
        self.unfinished(2, attempt=2)
        code, out, writes = self.bisect('--go')
        self.assertEqual((code, writes), (0, []), out)
        self.assertIn('WAITING: the run on the top has not finished', out)

    def test_a_probe_still_running_is_waited_for(self):
        self.add_run(2, dict(self.green(), **{'Host-arm64': ['failure', 'failure']}), attempt=2)
        self.add_run(0, {'Host-arm64': None}, event='workflow_dispatch')
        code, out, writes = self.bisect('--go')
        self.assertEqual((code, writes), (0, []), out)
        self.assertIn('WAITING', out)

    def test_a_run_list_that_cannot_be_read_blocks(self):
        self.add_run(2, self.green())
        self.fixture[f'repos/{REPO}/actions/runs?head_sha={self.SHAS[1]}&per_page=100'] = 'FAIL'
        code, out, writes = self.bisect('--go')
        self.assertEqual((code, writes), (2, []), out)

    def test_the_member_that_changed_the_failing_tests_file_is_probed_first(self):
        self.SHAS += ['4' * 40, '5' * 40, '6' * 40]
        self.members += [pr(14, 'd', 'c', self.SHAS[3]), pr(15, 'e', 'd', self.SHAS[4]), pr(16, 'f', 'e', self.SHAS[5])]
        self.add_run(5, dict(self.green(), **{'Host-arm64': ['failure', 'failure']}), attempt=2)
        self.fixture[f'repos/{REPO}/compare/{self.SHAS[3]}...{self.SHAS[4]}'] = {'files': [{'filename': 'tests/test_x.rs'}]}
        code, out, writes = self.tool('bisect', '--of', '16', '--go', prs=self.members, fixture=self.fixture,
                                      env={'CI_POOL_LOGS': str(self.logs)})
        self.assertEqual([c['fields'].get('ref') for c in writes if c['method'] == 'POST'], ['d'], out)


class Landing(unittest.TestCase):
    MEMBERS = [{'number': 11, 'head': 'a', 'sha': 'a' * 40}, {'number': 12, 'head': 'b', 'sha': 'b' * 40}]

    def blockers(self, facts=None, contains_main=True, contained=None, failed=(), despite=None):
        return ci_pool.landing_blockers(self.MEMBERS, facts or {'outcome': 'green'}, contains_main,
                                        {11: True, 12: True} if contained is None else contained, list(failed), despite or {})

    def test_a_green_head_that_contains_main_and_every_member_may_land(self):
        self.assertEqual(self.blockers(), [])
        self.assertEqual(self.blockers({'outcome': 'passed-on-rerun', 'lanes': ['Host-arm64']}), [])

    def test_anything_but_green_blocks(self):
        for outcome in ('waiting', 'rerun', 'probe', 'culprit', 'not-isolated'):
            reasons = self.blockers({'outcome': outcome, 'why': 'x', 'lanes': ['Host-arm64']})
            self.assertTrue(any('the lanes are not green' in reason for reason in reasons), outcome)

    def test_a_head_that_lacks_main_or_a_member_blocks(self):
        self.assertIn('--sync-main', ' '.join(self.blockers(contains_main=False)))
        self.assertIn('does not contain #11', ' '.join(self.blockers(contained={11: False, 12: True})))
        self.assertIn('does not contain #11', ' '.join(self.blockers(contained={12: True})))

    def test_a_failed_job_blocks_unless_it_is_accepted_by_name_and_is_not_a_lane(self):
        self.assertIn('Fleet Web failed', ' '.join(self.blockers(failed=['Fleet Web'])))
        self.assertEqual(self.blockers(failed=['Fleet Web'], despite={'Fleet Web': '#2177'}), [])
        self.assertIn('Lint failed', ' '.join(self.blockers(failed=['Fleet Web', 'Lint'], despite={'Fleet Web': '#2177'})))
        for name in ('Host-arm64', 'Host-${{ matrix.arch }}'):
            self.assertTrue(self.blockers(failed=[name], despite={name: '#1'}), name)
        self.assertIn('is a lane', ' '.join(self.blockers(despite={'Container-x64': '#1'})))

    def test_the_message_names_the_members_the_head_and_the_run_of_each_lane(self):
        text = ci_pool.landing_message(self.MEMBERS, self.MEMBERS[-1], ('Host-arm64', 'Container-arm64'),
                                       {'Host-arm64': 7, 'Container-arm64': 8}, {'Host-arm64': 85}, {'Fleet Web': '#2177'})
        for part in ('Land a pool of 2: #11, #12', 'Tested head: ' + 'b' * 40, '- Host-arm64: run 7', '- Container-arm64: run 8',
                     '- Host-arm64 failed and then passed on this commit: #85', 'Landed with Fleet Web failing: #2177',
                     '- #11 a at aaaaaaaaaaaa'):
            self.assertIn(part, text)
        self.assertIn('No lane:', ci_pool.landing_message(self.MEMBERS, self.MEMBERS[-1], (), {}, {}, {}))


class LandCommand(PoolOfThree):
    """`land` on the pool of three."""

    MAIN = '9' * 40

    def land(self, *args, compares=None):
        self.fixture.setdefault(f'repos/{REPO}/commits/main', {'sha': self.MAIN})
        self.fixture[f'repos/{REPO}/compare/{self.MAIN}...{self.SHAS[2]}'] = {'status': 'ahead'}
        self.fixture[f'repos/{REPO}/compare/{self.MAIN}...{self.SHAS[1]}'] = {'status': 'ahead'}
        for sha in self.SHAS[:2]:
            self.fixture.setdefault(f'repos/{REPO}/compare/{sha}...{self.SHAS[2]}', {'status': 'ahead'})
        self.fixture.setdefault(f'repos/{REPO}/compare/{self.SHAS[0]}...{self.SHAS[1]}', {'status': 'ahead'})
        self.fixture.update(compares or {})
        self.fixture[f'PUT repos/{REPO}/pulls/13/merge'] = {'sha': 'f' * 40, 'merged': True}
        self.fixture[f'PUT repos/{REPO}/pulls/12/merge'] = {'sha': 'f' * 40, 'merged': True}
        self.fixture.setdefault(f'repos/{REPO}/git/commits/{"f" * 40}', {'tree': {'sha': T1}})
        for sha in self.SHAS:
            self.fixture.setdefault(f'repos/{REPO}/git/commits/{sha}', {'tree': {'sha': T1}})
        code, out, calls = self.tool('land', '--of', '13', *args, prs=self.members, fixture=self.fixture,
                                     env={'CI_POOL_LOGS': str(self.logs)})
        return code, out, self.writes(calls)

    def full(self, extra=None):
        return dict(dict(self.green(), **{'Summary': 'success', 'Lint': 'success'}), **(extra or {}))

    def test_a_green_pool_lands_with_bases_first_and_one_merge_of_the_tested_head(self):
        run = self.add_run(2, self.full())
        code, out, writes = self.land('--go')
        self.assertEqual(code, 0, out)
        self.assertEqual([(m, p) for m, p, _ in writes], [('PATCH', 'pulls/12'), ('PATCH', 'pulls/13'), ('PUT', 'pulls/13/merge')])
        self.assertEqual([f['base'] for m, _, f in writes if m == 'PATCH'], ['main', 'main'])
        merge = writes[-1][2]
        self.assertEqual((merge['merge_method'], merge['sha']), ('merge', self.SHAS[2]))
        for part in ('Land a pool of 3: #11, #12, #13', f'- Host-arm64: run {run}'):
            self.assertIn(part, merge['commit_message'])

    def test_without_go_nothing_is_changed(self):
        self.add_run(2, self.full())
        code, out, writes = self.land()
        self.assertEqual((code, writes), (0, []), out)
        self.assertIn('would retarget 3 pull requests', out)

    def test_nothing_lands_on_anything_but_green(self):
        cases = {'no run': None,
                 'a failed lane': self.full({'Host-arm64': 'failure'}),
                 'a lane still running': self.full({'Host-arm64': None}),
                 'a lane missing': {k: v for k, v in self.full().items() if k != 'Container-arm64'},
                 'a hosted job failed': self.full({'Fleet Web': 'failure', 'Summary': 'failure'}),
                 'a deferred run': {job['name']: job['conclusion'] for job in DEFERRED}}
        for name, jobs in cases.items():
            self.setUp()
            if jobs is not None:
                self.add_run(2, jobs)
            code, out, writes = self.land('--go')
            self.assertEqual((code, writes), (2, []), f'{name}: {out}')
            self.assertIn('BLOCKED', out, name)

    def test_a_filtered_dispatch_on_the_head_does_not_make_it_landable(self):
        self.add_run(2, {'Host-arm64': 'success', 'Summary (some lanes)': 'success'}, event='workflow_dispatch')
        code, out, writes = self.land('--go')
        self.assertEqual((code, writes), (2, []), out)

    def test_a_head_that_lacks_main_or_a_member_does_not_land(self):
        self.add_run(2, self.full())
        for key, word in ((f'repos/{REPO}/compare/{self.MAIN}...{self.SHAS[2]}', '--sync-main'),
                          (f'repos/{REPO}/compare/{self.SHAS[0]}...{self.SHAS[2]}', 'does not contain #11')):
            for status in ('diverged', 'behind'):
                code, out, writes = self.land('--go', compares={key: {'status': status}})
                self.assertEqual((code, writes), (2, []), out)
                self.assertIn(word, out)
            code, out, writes = self.land('--go', compares={key: 'FAIL'})
            self.assertEqual((code, writes), (2, []), out)
            del self.fixture[key]

    def test_a_failed_hosted_job_lands_only_when_accepted_by_name_and_the_message_says_so(self):
        self.add_run(2, self.full({'Fleet Web': 'failure', 'Summary': 'failure'}))
        code, out, writes = self.land('--go', '--despite', 'Fleet Web=#2177')
        self.assertEqual(code, 0, out)
        self.assertIn('Landed with Fleet Web failing: #2177', writes[-1][2]['commit_message'])
        self.setUp()
        self.add_run(2, self.full({'Fleet Web': 'failure', 'Summary': 'failure'}))
        self.assertEqual(self.land('--go', '--despite', 'Fleet Web')[0], 2)
        self.setUp()
        self.add_run(2, self.full({'Host-arm64': 'failure', 'Summary': 'failure'}))
        code, out, writes = self.land('--go', '--despite', 'Host-arm64=#1')
        self.assertEqual((code, writes), (2, []), out)

    def test_a_pool_that_passed_on_a_rerun_lands_with_its_issue_in_the_message(self):
        self.add_run(2, self.full({'Host-arm64': ['failure', 'success']}), attempt=2)
        self.fixture[f'POST repos/{REPO}/issues'] = {'number': 99}
        code, out, writes = self.land('--go')
        self.assertEqual(code, 0, out)
        self.assertEqual([(m, p) for m, p, _ in writes][0], ('POST', 'issues'))
        self.assertIn('- Host-arm64 failed and then passed on this commit: #99', writes[-1][2]['commit_message'])

    def test_a_lower_part_lands_on_a_run_of_its_own_head_and_the_member_above_moves_to_main(self):
        self.add_run(2, self.full())
        code, out, writes = self.land('--go', '--through', '12')
        self.assertEqual((code, writes), (2, []), 'the top\'s run was taken as a run of the lower head: ' + out)
        self.add_run(1, self.full(), event='workflow_dispatch')
        code, out, writes = self.land('--go', '--through', '12')
        self.assertEqual(code, 0, out)
        self.assertEqual([(m, p) for m, p, _ in writes], [('PATCH', 'pulls/12'), ('PUT', 'pulls/12/merge'), ('PATCH', 'pulls/13')])
        self.assertEqual(writes[1][2]['sha'], self.SHAS[1])

    def test_a_lower_part_does_not_land_on_some_of_its_lanes(self):
        self.add_run(2, self.full())
        self.add_run(1, {'Host-arm64': 'success', 'Summary (some lanes)': 'success'}, event='workflow_dispatch')
        code, out, writes = self.land('--go', '--through', '12')
        self.assertEqual((code, writes), (2, []), out)
        self.assertIn('has not run', out)

    def test_lanes_that_passed_in_a_run_that_has_not_finished_do_not_land(self):
        # Its other jobs are still running: what failed outside the lanes is not known yet.
        number = self.add_run(2, self.green())
        key = f'repos/{REPO}/actions/runs?head_sha={self.SHAS[2]}&per_page=100'
        self.fixture[key]['workflow_runs'][0]['status'] = 'in_progress'
        code, out, writes = self.land('--go')
        self.assertEqual((code, writes), (2, []), out)
        self.assertIn('the run on the top has not finished', out)
        self.assertTrue(number)

    def test_every_lane_green_in_a_run_whose_other_jobs_are_still_running_does_not_land(self):
        number = self.add_run(2, {lane_name: 'success' for lane_name in ci_pool.LANES}, event='workflow_dispatch')
        key = f'repos/{REPO}/actions/runs?head_sha={self.SHAS[2]}&per_page=100'
        self.fixture[key]['workflow_runs'][0]['status'] = 'in_progress'
        code, out, writes = self.land('--go')
        self.assertEqual((code, writes), (2, []), out)
        self.assertIn(f'run {number} on the head has not finished', out)

    def test_an_older_finished_run_does_not_stand_in_for_the_one_still_running(self):
        # The head was a lower member (a deferred run, finished), then its lanes were
        # dispatched. Every lane is green and Lint has failed, in a run that is not over.
        self.add_run(2, {job['name']: job['conclusion'] for job in DEFERRED})
        every = {lane_name: 'success' for lane_name in ci_pool.LANES}
        number = self.add_run(2, dict(every, Lint='failure', Summary=None), event='workflow_dispatch')
        key = f'repos/{REPO}/actions/runs?head_sha={self.SHAS[2]}&per_page=100'
        self.fixture[key]['workflow_runs'][-1]['status'] = 'in_progress'
        code, out, writes = self.land('--go')
        self.assertEqual((code, writes), (2, []), out)
        self.assertIn(f'run {number} on the head has not finished', out)
        # Finished, the run that supplied the lanes is the one whose other jobs are read.
        self.fixture[key]['workflow_runs'][-1]['status'] = 'completed'
        jobs = self.fixture[f'repos/{REPO}/actions/runs/{number}/jobs?filter=all&per_page=100']['jobs']
        next(job for job in jobs if job['name'] == 'Summary')['conclusion'] = 'failure'
        code, out, writes = self.land('--go')
        self.assertEqual((code, writes), (2, []), out)
        self.assertIn('Lint failed', out)

    def test_a_run_still_going_blocks_a_head_that_an_older_run_passed(self):
        self.add_run(2, self.full())
        every = {lane_name: 'success' for lane_name in ci_pool.LANES}
        number = self.add_run(2, dict(every, Lint='failure', Summary=None), event='workflow_dispatch')
        key = f'repos/{REPO}/actions/runs?head_sha={self.SHAS[2]}&per_page=100'
        self.fixture[key]['workflow_runs'][-1]['status'] = 'in_progress'
        code, out, writes = self.land('--go')
        self.assertEqual((code, writes), (2, []), out)
        self.assertIn(f'run {number} on the head has not finished', out)

    def test_a_run_with_no_summary_is_not_a_verdict(self):
        # A cancelled run reports no Summary. Lanes that finished before it was cancelled
        # read green, and nothing says what the jobs that never ran would have shown.
        cases = {'cancelled, no Summary': (self.green(), 'cancelled'),
                 'cancelled, with a Summary': (self.full(), 'cancelled'),
                 'finished, no Summary': (self.green(), 'success'),
                 'finished, a deferred Summary': (dict(self.green(), **{'Summary (deferred)': 'success'}), 'success')}
        for name, (jobs, concluded) in cases.items():
            self.setUp()
            self.add_run(2, jobs, concluded=concluded)
            code, out, writes = self.land('--go')
            self.assertEqual((code, writes), (2, []), f'{name}: {out}')
            self.assertIn('no finished run on the head', out, name)

    def test_a_superseded_run_beside_the_one_that_passed_does_not_block(self):
        # GitHub sometimes starts two runs for one push; the second cancels the first.
        self.add_run(2, {'Changes': 'cancelled', 'Lint': 'cancelled', 'Summary': 'failure',
                         'Host-${{ matrix.arch }}': 'cancelled'}, concluded='cancelled')
        self.add_run(2, self.full())
        code, out, writes = self.land('--go')
        self.assertEqual(code, 0, out)

    def test_a_refused_merge_puts_every_base_back(self):
        self.add_run(2, self.full())
        self.land()
        self.fixture[f'PUT repos/{REPO}/pulls/13/merge'] = 'FAIL'
        code, out, calls = self.tool('land', '--of', '13', '--go', prs=self.members, fixture=self.fixture,
                                     env={'CI_POOL_LOGS': str(self.logs)})
        writes = self.writes(calls)
        self.assertEqual(code, 2, out)
        self.assertIn('nothing landed and every base is as it was', out)
        self.assertEqual([(m, p, f.get('base')) for m, p, f in writes],
                         [('PATCH', 'pulls/12', 'main'), ('PATCH', 'pulls/13', 'main'), ('PUT', 'pulls/13/merge', None),
                          ('PATCH', 'pulls/13', 'b'), ('PATCH', 'pulls/12', 'a')])

    def test_a_landing_whose_tree_is_not_the_tested_one_says_so(self):
        self.add_run(2, self.full())
        self.fixture[f'repos/{REPO}/git/commits/{"f" * 40}'] = {'tree': {'sha': T2}}
        code, out, _ = self.land('--go')
        self.assertIn('a tree no run tested', out)

    def test_a_member_outside_the_pool_cannot_be_the_end_of_a_landing(self):
        self.add_run(2, self.full())
        self.members.append(pr(20, 'z', 'main', 'e' * 40))
        code, out, writes = self.land('--go', '--through', '20')
        self.assertEqual((code, writes), (2, []), out)


class Marks(unittest.TestCase):
    """The `ci/pool` statuses a run states, from what it showed."""

    HEAD = 'c' * 40
    LOWER = [(12, 'b' * 40), (11, 'a' * 40)]

    def marks(self, ran=True, summary='success', lanes=None, failed=(), lower=LOWER, reason='stacked under #14'):
        lanes = {lane_name: 'pass' for lane_name in ci_pool.PR_LANES} if lanes is None else lanes
        return ci_pool.marks(ran, reason, summary, lanes, list(failed), '13', self.HEAD, list(lower), 77)

    def test_a_run_that_tested_nothing_is_pending_on_its_own_head_and_nowhere_else(self):
        for summary in ('success', 'failure', 'skipped'):
            got = self.marks(ran=False, summary=summary)
            self.assertEqual([(sha, state) for sha, state, _ in got], [(self.HEAD, 'pending')])
            self.assertIn('not tested by this run: stacked under #14', got[0][2])

    def test_a_passing_run_marks_its_head_and_every_lower_head_it_contains(self):
        got = self.marks()
        self.assertEqual([(sha, state) for sha, state, _ in got],
                         [(self.HEAD, 'success'), ('b' * 40, 'success'), ('a' * 40, 'success')])
        self.assertIn('5 lanes passed: run 77 on ccccccccc', got[0][2])
        self.assertIn('#13 run 77', got[1][2])

    def test_a_lane_that_passed_only_on_a_rerun_is_named(self):
        lanes = dict({lane_name: 'pass' for lane_name in ci_pool.PR_LANES}, **{'Host-arm64': 'pass-on-rerun'})
        self.assertIn('1 only on a rerun (Host-arm64)', self.marks(lanes=lanes)[0][2])

    def test_a_run_whose_scope_needed_no_lane_says_so(self):
        self.assertIn('no lane needed', self.marks(lanes={})[0][2])

    def test_a_failed_run_marks_only_its_own_head(self):
        lanes = dict({lane_name: 'pass' for lane_name in ci_pool.PR_LANES}, **{'Host-arm64': 'fail'})
        got = self.marks(summary='failure', lanes=lanes, failed=['Host-arm64', 'Summary'])
        self.assertEqual([(sha, state) for sha, state, _ in got], [(self.HEAD, 'failure')])
        self.assertIn('failed: Host-arm64; 4 of 5 lanes passed', got[0][2])

    def test_a_run_that_failed_outside_the_lanes_says_which_job_and_that_the_lanes_passed(self):
        # 2026-11-25: every lane passed and Fleet Web failed. The head is not green, and the
        # status says what is and is not.
        got = self.marks(summary='failure', failed=['Fleet Web', 'Summary'])
        self.assertEqual([(sha, state) for sha, state, _ in got], [(self.HEAD, 'failure')])
        self.assertIn('failed: Fleet Web; 5 of 5 lanes passed', got[0][2])

    def test_a_cancelled_or_skipped_summary_states_nothing(self):
        for summary in ('cancelled', 'skipped', ''):
            self.assertEqual(self.marks(summary=summary), [])

    def test_no_state_but_these_three_is_ever_stated(self):
        states = set()
        for ran in (True, False):
            for summary in ('success', 'failure', 'cancelled', 'skipped'):
                states |= {state for _, state, _ in self.marks(ran=ran, summary=summary)}
        self.assertEqual(states, {'success', 'failure', 'pending'})


class MarkCommand(Sandbox):
    """`mark` as ci.yml's pool-status job runs it."""

    HEAD = 'c' * 40

    def mark(self, prs, jobs, compares=None, ran='true', summary='success', **more):
        fixture = {f'repos/{REPO}/actions/runs/77/jobs?filter=all&per_page=100': {'jobs': jobs}}
        for sha, status in (compares or {}).items():
            fixture[f'repos/{REPO}/compare/{sha}...{self.HEAD}'] = status if status == 'FAIL' else {'status': status}
        fixture.update(more)
        env = dict(RAN=ran, REASON='stacked under #14: CI runs on the top of the stack', SUMMARY=summary, PR='13',
                   HEAD_SHA=self.HEAD, RUN_ID='77', RUN_ATTEMPT='1', RUN_URL='https://example.invalid/runs/77')
        code, out, calls = self.tool('mark', prs=prs, fixture=fixture, env=env)
        posts = [(c['path'].rsplit('/', 1)[-1], c['fields']['state'], c['fields']['context']) for c in calls if c['method'] == 'POST']
        return code, out, posts, calls

    STACK = [pr(11, 'feature-a', 'main', 'a' * 40), pr(12, 'feature-b', 'feature-a', 'b' * 40),
             pr(13, 'feature-c', 'feature-b', 'c' * 40)]

    def test_each_lower_member_whose_head_this_run_tested_is_marked(self):
        code, out, posts, calls = self.mark(self.STACK, passed(ci_pool.PR_LANES), {'b' * 40: 'ahead', 'a' * 40: 'ahead'})
        self.assertEqual(code, 0, out)
        self.assertEqual(posts, [('c' * 40, 'success', 'ci/pool'), ('b' * 40, 'success', 'ci/pool'), ('a' * 40, 'success', 'ci/pool')])
        self.assertEqual({c['fields']['target_url'] for c in calls if c['method'] == 'POST'}, {'https://example.invalid/runs/77'})

    def test_a_lower_member_that_moved_after_this_head_is_not_marked_nor_anything_below(self):
        for moved in ('diverged', 'behind', 'FAIL'):
            _, out, posts, _ = self.mark(self.STACK, passed(ci_pool.PR_LANES), {'b' * 40: moved, 'a' * 40: 'ahead'})
            self.assertEqual(posts, [('c' * 40, 'success', 'ci/pool')], moved)

    def test_a_fork_pull_request_on_the_base_branch_is_not_marked(self):
        stack = [pr(12, 'feature-b', 'main', 'b' * 40, repo='someone/fork'), pr(13, 'feature-c', 'feature-b', 'c' * 40)]
        _, out, posts, _ = self.mark(stack, passed(ci_pool.PR_LANES), {'b' * 40: 'ahead'})
        self.assertEqual(posts, [('c' * 40, 'success', 'ci/pool')], out)

    def test_a_pool_that_cannot_be_read_marks_only_this_head(self):
        _, out, posts, _ = self.mark(self.STACK, passed(ci_pool.PR_LANES), {'b' * 40: 'ahead', 'a' * 40: 'ahead'},
                                     **{f'repos/{REPO}/pulls?state=open&per_page=100': 'FAIL'})
        self.assertEqual(posts, [('c' * 40, 'success', 'ci/pool')], out)

    def test_a_deferred_run_asks_nothing_and_marks_its_head_pending(self):
        code, out, posts, calls = self.mark(self.STACK, [], ran='false')
        self.assertEqual((code, posts), (0, [('c' * 40, 'pending', 'ci/pool')]), out)
        self.assertEqual([c for c in calls if c['method'] == 'GET'], [])

    def test_a_failed_run_marks_only_its_head_and_looks_at_no_other_member(self):
        jobs = passed(ci_pool.PR_LANES) + [lane('Fleet Web', 'failure'), lane('Summary', 'failure')]
        code, out, posts, calls = self.mark(self.STACK, jobs, summary='failure')
        self.assertEqual((code, posts), (0, [('c' * 40, 'failure', 'ci/pool')]), out)
        self.assertFalse([c for c in calls if '/compare/' in c['path'] or '/pulls' in c['path']])

    def test_a_passing_summary_over_a_lane_that_did_not_pass_states_nothing(self):
        jobs = passed(ci_pool.PR_LANES[1:]) + [lane(ci_pool.PR_LANES[0], 'failure')]
        code, out, posts, _ = self.mark(self.STACK, jobs)
        self.assertEqual((code, posts), (2, []), out)

    def test_a_job_list_that_cannot_be_read_states_nothing(self):
        code, out, posts, _ = self.mark(self.STACK, [], **{f'repos/{REPO}/actions/runs/77/jobs?filter=all&per_page=100': 'FAIL'})
        self.assertEqual((code, posts), (2, []), out)


class Inventory(unittest.TestCase):
    """Only ci.yml may run a self-hosted job once per pull request or per pushed branch:
    its lanes are pooled. Any other workflow that did would pay per pull request again."""

    HOSTED = "name: X\non:\n  pull_request:\n    paths:\n      - 'a/**'\n  push:\n    branches: [main]\njobs:\n  build:\n    runs-on: ubuntu-24.04\n"
    METAL = '    runs-on: ["self-hosted", "Linux", "ARM64"]\n'

    def reasons(self, on, runs_on=METAL):
        return ci_pool.per_branch_self_hosted(f'name: X\non:\n{on}jobs:\n  one:\n    name: One\n{runs_on}    steps: []\n')

    def test_no_workflow_but_ci_yml_runs_a_self_hosted_job_per_pull_request_or_branch(self):
        found = {}
        for path in sorted((ROOT / '.github/workflows').glob('*.y*ml')):
            reasons = ci_pool.per_branch_self_hosted(path.read_text())
            if reasons:
                found[path.name] = reasons
        self.assertEqual(found, {'ci.yml': ['pull_request']},
                         'a workflow other than ci.yml runs a self-hosted job per pull request or per branch: '
                         'pool it (docs/ci-pool.md) or move it to a GitHub-hosted runner')

    def test_the_workflows_known_to_use_self_hosted_runners_are_seen_as_such(self):
        # The reader must see a self-hosted job where there is one, or the test above cannot fail.
        seen = {path.name for path in (ROOT / '.github/workflows').glob('*.yml')
                if ci_pool.workflow_shape(path.read_text())[1]}
        for name in ('ci.yml', 'weekly.yml', 'kernels.yml', 'teardown-evidence.yml'):
            self.assertIn(name, seen)
        self.assertEqual(sorted(ci_pool.workflow_shape((ROOT / '.github/workflows/ci.yml').read_text())[1]),
                         ['container', 'host', 'host-root'])

    def test_a_self_hosted_job_on_a_pull_request_or_any_branch_is_found(self):
        self.assertEqual(self.reasons('  pull_request:\n    paths:\n      - x\n'), ['pull_request'])
        self.assertEqual(self.reasons('  pull_request_target:\n'), ['pull_request_target'])
        self.assertEqual(self.reasons('  push:\n'), ['push to branches other than main'])
        self.assertEqual(self.reasons("  push:\n    branches: ['**']\n"), ['push to branches other than main'])
        self.assertEqual(self.reasons('  push:\n    branches:\n      - main\n      - release\n'), ['push to branches other than main'])
        self.assertEqual(self.reasons('  push:\n    paths:\n      - x\n'), ['push to branches other than main'])
        matrix = '    runs-on: ${{ fromJSON(format(\'["self-hosted","Linux","{0}"]\', matrix.arch)) }}\n'
        self.assertEqual(self.reasons('  pull_request:\n', matrix), ['pull_request'])
        folded = '    runs-on:\n      - self-hosted\n      - Linux\n'
        self.assertEqual(self.reasons('  pull_request:\n', folded), ['pull_request'])

    def test_a_self_hosted_job_on_main_a_schedule_or_a_dispatch_is_not(self):
        for on in ('  push:\n    branches: [main]\n', '  push:\n    branches:\n      - main\n', '  schedule:\n    - cron: "0 4 * * *"\n',
                   '  workflow_dispatch:\n', '  workflow_run:\n    workflows: ["CI"]\n', '  workflow_call:\n'):
            self.assertEqual(self.reasons(on), [], on)

    def test_a_hosted_workflow_on_pull_requests_is_not(self):
        self.assertEqual(ci_pool.per_branch_self_hosted(self.HOSTED), [])
        self.assertEqual(self.reasons('  pull_request:\n', '    runs-on: ubuntu-24.04-arm\n'), [])


def with_steps(name, conclusion, **steps):
    return {'name': name, 'conclusion': conclusion, 'steps': [{'name': k.replace('_', '-'), 'conclusion': v} for k, v in steps.items()]}


class UnitWatch(Sandbox):
    """The watcher that stops a run at the first failed unit step."""

    def test_a_failed_unit_step_is_the_only_thing_that_counts_as_failed(self):
        verdict = ci_pool.unit_verdict
        self.assertEqual(verdict([with_steps('Host-arm64', None, test_unit='failure')]), ('failed', 'Host-arm64'))
        self.assertEqual(verdict([with_steps('Container-x64', 'failure', container_test_unit='failure')]), ('failed', 'Container-x64'))
        # Another step failed, the runner was lost, or the lane was cancelled: not the unit suite.
        for job in (with_steps('Host-arm64', 'failure', test_unit='success', test_fast='failure'),
                    with_steps('Host-arm64', 'failure', Install_dependencies='failure'),
                    with_steps('Host-arm64', 'failure'), with_steps('Host-arm64', 'cancelled', test_unit='cancelled'),
                    with_steps('Host-Root-arm64-SnapshotEnabled', 'failure', test_root='failure'),
                    with_steps('Lint', 'failure', test_unit='failure')):
            self.assertEqual(verdict([job]), ('done', None), job)

    def test_it_keeps_watching_until_every_unit_step_is_over(self):
        verdict = ci_pool.unit_verdict
        running = with_steps('Host-arm64', None, Install_dependencies='success', test_unit=None)
        self.assertEqual(verdict([running]), ('running', None))
        self.assertEqual(verdict([with_steps('Host-arm64', None)]), ('running', None))
        past = with_steps('Host-arm64', None, test_unit='success', test_fast=None)
        self.assertEqual(verdict([past, with_steps('Container-arm64', 'success', container_test_unit='success')]), ('done', None))
        self.assertEqual(verdict([past, running]), ('running', None))
        self.assertEqual(verdict([running, with_steps('Host-x64', None, test_unit='failure')]), ('failed', 'Host-x64'))
        self.assertEqual(verdict([]), ('done', None))

    def watch(self, jobs, *args, run=None):
        fixture = {f'repos/{REPO}/actions/runs/77/jobs?per_page=100': jobs if jobs == 'FAIL' else {'jobs': jobs},
                   f'repos/{REPO}/actions/runs/77': {'path': '.github/workflows/ci.yml', 'head_sha': 'c' * 40} if run is None else run}
        # A deadline on every call, so that no change to the watcher can make a test wait an hour.
        if '--minutes' not in args:
            args = ('--minutes', '0.05', *args)
        code, out, calls = self.tool('unit-watch', '--interval', '0.05', *args, fixture=fixture,
                                     env=dict(RUN_ID='77', HEAD_SHA='c' * 40, RUN_URL='https://example.invalid/runs/77'))
        return code, out, [(c['method'], c['path'].split('/', 3)[-1], c['fields']) for c in calls if c['method'] != 'GET'], calls

    def test_a_failed_unit_step_marks_the_head_and_cancels_the_run(self):
        code, out, writes, _ = self.watch([with_steps('Host-arm64', None, test_unit='failure'),
                                           with_steps('Container-arm64', None, container_test_unit=None)])
        self.assertEqual(code, 0, out)
        self.assertEqual([(m, p) for m, p, _ in writes], [('POST', 'statuses/' + 'c' * 40), ('POST', 'actions/runs/77/cancel')])
        fields = writes[0][2]
        self.assertEqual((fields['state'], fields['context']), ('failure', 'ci/pool'))
        self.assertIn('test-unit on Host-arm64', fields['description'])

    def test_nothing_is_cancelled_when_the_unit_steps_pass_or_something_else_fails(self):
        for jobs in ([with_steps('Host-arm64', None, test_unit='success'), with_steps('Container-arm64', 'success', container_test_unit='success')],
                     [with_steps('Host-arm64', 'failure', Install_dependencies='failure')],
                     [with_steps('Host-Root-arm64-SnapshotEnabled', 'failure', test_root='failure')]):
            code, out, writes, _ = self.watch(jobs)
            self.assertEqual((code, writes), (0, []), out)

    def test_it_stops_watching_at_its_deadline_and_never_fails_the_run(self):
        code, out, writes, calls = self.watch([with_steps('Host-arm64', None, test_unit=None)], '--minutes', '0.005')
        self.assertEqual((code, writes), (0, []), out)
        self.assertIn('no longer watching', out)
        self.assertGreaterEqual(len(calls), 2, 'it did not look again before its deadline')
        code, out, writes, _ = self.watch('FAIL', '--minutes', '0.005')
        self.assertEqual((code, writes), (0, []), out)
        self.assertIn('could not read the jobs', out)

    FAILED = [with_steps('Host-arm64', None, test_unit='failure')]

    def test_it_acts_only_on_a_run_of_this_workflow_on_the_head_it_was_given(self):
        for run in ({'path': '.github/workflows/other.yml', 'head_sha': 'c' * 40},
                    {'path': '.github/workflows/ci.yml', 'head_sha': 'd' * 40},
                    {'path': 'dynamic/ci.yml', 'head_sha': 'c' * 40}, {}, 'FAIL'):
            code, out, writes, _ = self.watch(self.FAILED, run=run)
            self.assertEqual((code, writes), (0, []), f'{run}: {out}')
            self.assertIn('nothing is watched', out)
        self.assertEqual(len(self.watch(self.FAILED)[2]), 2)

    def test_only_names_and_conclusions_from_the_api_decide_never_what_a_test_printed(self):
        # A job that is not a lane, or a step that is not the unit step, cannot stop a run
        # whatever it is called; and the watcher asks for no log and no artifact at all.
        for jobs in ([with_steps('Host-arm64 (hosted)', None, test_unit='failure')],
                     [with_steps('host-arm64', None, test_unit='failure')],
                     [with_steps('Host-arm64', None, **{'test_unit_': 'failure', 'TEST_UNIT': 'failure'})],
                     [with_steps('Host-arm64', None, **{'test-unit failed on Host-arm64': 'failure'})]):
            code, out, writes, calls = self.watch(jobs, '--minutes', '0.005')
            self.assertEqual(writes, [], jobs)
        _, _, _, calls = self.watch(self.FAILED)
        asked = [c['path'] for c in calls if c['method'] == 'GET']
        self.assertEqual(asked, [f'repos/{REPO}/actions/runs/77', f'repos/{REPO}/actions/runs/77/jobs?per_page=100'])
        source = TOOL.read_text().split('def unit_verdict(', 1)[1].split('\n# ---', 1)[0]
        for reader in ('log_tail', 'classify_log', 'annotations', '/logs', 'artifacts'):
            self.assertNotIn(reader, source)

    def test_the_watcher_job_runs_no_code_of_the_change_and_takes_every_value_through_env(self):
        text = CI.read_text()
        body = text.split('\njobs:\n', 1)[1]
        block = dict(re.findall(r'^  ([\w-]+):\n(.*?)(?=^  [\w-]+:\n|\Z)', body, re.M | re.S))['unit-watch']
        # The default branch's copy of the tool, and nothing else of the repository.
        checkout = re.search(r'      - uses: actions/checkout@.*\n        with:\n((?:          .*\n)+)', block)[1]
        self.assertIn('ref: ${{ github.event.repository.default_branch }}', checkout)
        self.assertIn('sparse-checkout: scripts/ci-pool', checkout)
        self.assertIn('persist-credentials: false', checkout)
        self.assertEqual(len(re.findall(r'^\s+(?:- )?uses:', block, re.M)), 1, 'the watcher job uses an action besides checkout')
        # No expression inside the script text: every value arrives through env.
        script = block.split('        run: |\n', 1)[1]
        self.assertNotIn('${{', script)
        self.assertNotRegex(script, r'\bmake\b|cargo|\./scripts|bash scripts')
        self.assertIn('runs-on: ubuntu-latest', block)

    def test_the_watcher_job_does_not_run_for_a_fork_or_another_repositorys_workflow_run(self):
        import importlib.util as util
        scope = util.spec_from_file_location('scope', ROOT / 'scripts/test-ci-stack-scope.py')
        helpers = util.module_from_spec(scope)
        scope.loader.exec_module(helpers)
        gate = helpers.job_if(helpers.jobs(CI.read_text())['unit-watch'])

        def runs(event, **event_fields):
            return bool(helpers.evaluate(gate, {
                'needs': {'changes': {'outputs': {'code': 'true'}}, 'skip_check': {'outputs': {'skip': 'false'}}},
                'github': {'event_name': event, 'repository': REPO, 'event': event_fields}}))

        ours, theirs = {'repo': {'full_name': REPO}}, {'repo': {'full_name': 'someone/fork'}}
        self.assertTrue(runs('pull_request', pull_request={'head': ours}))
        self.assertFalse(runs('pull_request', pull_request={'head': theirs}))
        self.assertTrue(runs('push'))
        self.assertTrue(runs('workflow_dispatch'))
        self.assertTrue(runs('workflow_run', workflow_run={'head_repository': {'full_name': REPO}}))
        self.assertFalse(runs('workflow_run', workflow_run={'head_repository': {'full_name': 'someone/fork'}}))
        text = CI.read_text()
        self.assertNotIn('pull_request_target', text.split('\njobs:\n', 1)[0])

    def test_the_watcher_names_the_steps_the_lanes_have(self):
        text = CI.read_text()
        body = text.split('\njobs:\n', 1)[1]
        blocks = dict(re.findall(r'^  ([\w-]+):\n(.*?)(?=^  [\w-]+:\n|\Z)', body, re.M | re.S))
        self.assertIn('      - name: test-unit\n', blocks['host'])
        self.assertIn('      - name: container-test-unit\n', blocks['container'])
        self.assertEqual(set(ci_pool.UNIT_STEPS), {l for l in ci_pool.LANES if l.startswith(('Host-a', 'Host-x', 'Container-'))})
        self.assertIn('python3 scripts/ci-pool/ci_pool.py unit-watch --minutes 60', blocks['unit-watch'])
        self.assertNotIn('unit-watch', re.search(r'^    needs: \[(.*)\]$', blocks['summary'], re.M)[1])


def takes_fcvm_with_default_features(root):
    """The workspace members whose manifest takes the `fcvm` package with its default
    features, as 'member: table' strings. fcvm's default features are its integration
    test tiers, and Cargo unifies features across a workspace build: one such dependency
    compiles the VM tests into `make test-unit`, which runs with --no-default-features on
    a box with no VM setup."""
    import tomllib
    root = Path(root)
    workspace = tomllib.loads((root / 'Cargo.toml').read_text())
    found = []
    for member in workspace['workspace']['members']:
        manifest = tomllib.loads((root / member / 'Cargo.toml').read_text())
        tables = {name: manifest.get(name, {}) for name in ('dependencies', 'dev-dependencies', 'build-dependencies')}
        for target, body in manifest.get('target', {}).items():
            for name in ('dependencies', 'dev-dependencies', 'build-dependencies'):
                tables[f'target.{target}.{name}'] = body.get(name, {})
        for table, dependencies in tables.items():
            for name, spec in dependencies.items():
                package = spec.get('package', name) if isinstance(spec, dict) else name
                if package != 'fcvm':
                    continue
                if not isinstance(spec, dict) or spec.get('default-features', True) is not False:
                    found.append(f'{member}: {table}.{name}')
    return found


class UnitStep(unittest.TestCase):
    """What `make test-unit` may contain."""

    def test_no_workspace_member_takes_fcvm_with_its_default_features(self):
        # 2026-11-22: a new workspace member did, and the unit step of four jobs ran and
        # failed 127 to 208 VM tests (#2190). The fix pinned that one manifest; this
        # reads them all.
        self.assertEqual(takes_fcvm_with_default_features(ROOT), [])

    def test_the_check_reads_every_member_and_every_dependency_table(self):
        with tempfile.TemporaryDirectory(prefix='ci-pool-manifests-') as tmp:
            root = Path(tmp)
            members = {
                'ok': '[dependencies]\nfcvm = { path = "..", default-features = false }\nserde = "1"\n',
                'plain': '[dependencies]\nfcvm = { path = ".." }\n',
                'version': '[dependencies]\nfcvm = "0.1"\n',
                'explicit': '[dependencies]\nfcvm = { path = "..", default-features = true }\n',
                'dev': '[dev-dependencies]\nfcvm = { path = ".." }\n',
                'build': '[build-dependencies]\nfcvm = { path = "..", default-features = false }\n',
                'renamed': '[dependencies]\nvm = { path = "..", package = "fcvm" }\n',
                'target': '[target.\'cfg(unix)\'.dependencies]\nfcvm = { path = ".." }\n',
                'other': '[dependencies]\nfcvm-extras = { path = "x" }\n',
            }
            (root / 'Cargo.toml').write_text('[workspace]\nmembers = [".", ' + ', '.join(f'"{m}"' for m in members) + ']\n'
                                             '[package]\nname = "fcvm"\n[features]\ndefault = ["integration-fast"]\n')
            for member, text in members.items():
                (root / member).mkdir()
                (root / member / 'Cargo.toml').write_text(f'[package]\nname = "{member}"\n' + text)
            self.assertEqual(takes_fcvm_with_default_features(root),
                             ['plain: dependencies.fcvm', 'version: dependencies.fcvm', 'explicit: dependencies.fcvm',
                              'dev: dev-dependencies.fcvm', 'renamed: dependencies.vm', "target: target.cfg(unix).dependencies.fcvm"])

    def test_the_unit_step_runs_without_default_features(self):
        makefile = (ROOT / 'Makefile').read_text()
        recipe = re.search(r'^_test-unit:.*\n((?:\t.*\n)+)', makefile, re.M)[1]
        self.assertIn('--no-default-features', recipe)
        self.assertNotIn('--features', recipe.replace('--no-default-features', ''))


class HostRecord(unittest.TestCase):
    """What a lane keeps about its host, so that a failure can be attributed afterwards."""

    def blocks(self):
        body = CI.read_text().split('\njobs:\n', 1)[1]
        return dict(re.findall(r'^  ([\w-]+):\n(.*?)(?=^  [\w-]+:\n|\Z)', body, re.M | re.S))

    def test_the_lanes_that_run_on_the_host_record_its_memory_swap_load_and_oom_kills(self):
        # The Host lane runs the unit suite and Host-Root the privileged one: both on the
        # runner itself. Until #2199 no artifact recorded host memory at all.
        for job in ('host', 'host-root'):
            block = self.blocks()[job]
            step = re.search(r'      - name: Capture host load record\n        if: always\(\)\n        run: \|\n((?:          .*\n|\n)+)', block)
            self.assertTrue(step, f'{job} has no "Capture host load record" step that always runs')
            for command in ('LC_ALL=C sar -q', 'LC_ALL=C sar -u', 'LC_ALL=C sar -r', 'LC_ALL=C sar -S',
                            "grep -E '^(MemTotal|MemAvailable|SwapTotal|SwapFree|Committed_AS):' /proc/meminfo",
                            "grep -E '^oom_kill ' /proc/vmstat", 'cat /proc/loadavg'):
                self.assertIn(command, step[1], f'{job}: {command}')
            self.assertIn('> /tmp/fcvm-test-logs/host-load.log', step[1])
            # It is written before the directory is uploaded.
            self.assertLess(block.index('name: Capture host load record'), block.index('name: Upload test logs'), job)
            self.assertIn('path: /tmp/fcvm-test-logs/', block.split('name: Upload test logs', 1)[1][:600])


class Lanes(unittest.TestCase):
    def test_the_planned_matrix_expands_to_the_lanes_names(self):
        # The names the Actions API reports are the job name templates of ci.yml filled in
        # from the planner's matrix. Evidence is matched by those names, so a renamed job or
        # a new matrix value must reach LANES.
        text = CI.read_text()
        templates = {key: re.search(rf'^  {key}:\n    name: (.*)$', text, re.M)[1] for key in ('host', 'host-root', 'container')}
        self.assertEqual(templates, {'host': 'Host-${{ matrix.arch }}', 'container': 'Container-${{ matrix.arch }}',
                                     'host-root': 'Host-Root-${{ matrix.arch }}-${{ matrix.mode }}'})
        out = ci_pool.matrix_outputs(ci_pool.LANES)
        names = [f'Host-{arch}' for arch in json.loads(out['host'])['arch']]
        names += [f"Host-Root-{e['arch']}-{e['mode']}" for e in json.loads(out['host_root'])['include']]
        names += [f'Container-{arch}' for arch in json.loads(out['container'])['arch']]
        self.assertEqual(sorted(names), sorted(ci_pool.LANES))
        for key, output in (('host', 'host'), ('host-root', 'host_root'), ('container', 'container')):
            block = text.split(f'\n  {key}:\n', 1)[1].split('\n    steps:', 1)[0]
            self.assertIn(f'matrix: ${{{{ fromJSON(needs.skip-check.outputs.{output}) }}}}', block)
            self.assertIn(f"needs.skip-check.outputs.{output}_run == 'true'", block)

    def test_a_pull_requests_lanes_are_every_arm64_lane_and_one_x64_lane(self):
        self.assertEqual(set(ci_pool.PR_LANES), {lane for lane in ci_pool.LANES if 'arm64' in lane} | {'Host-Root-x64-SnapshotEnabled'})


def defined_tests():
    return sum(1 for cls in list(globals().values()) if isinstance(cls, type) and issubclass(cls, unittest.TestCase)
               for name in dir(cls) if name.startswith('test_'))


if __name__ == '__main__':
    program = unittest.main(verbosity=1, exit=False)
    # A helper named like one of TestCase's own methods (`run`) silently turns a whole
    # class into tests that never execute. Every test defined here must have run.
    if len(sys.argv) == 1 and program.result.testsRun != defined_tests():
        print(f'{program.result.testsRun} tests ran but {defined_tests()} are defined', file=sys.stderr)
        sys.exit(1)
    sys.exit(0 if program.result.wasSuccessful() else 1)

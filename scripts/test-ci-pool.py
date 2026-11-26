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
        answer = {}
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
                   f'repos/{REPO}/commits/*': {'statuses': []}}
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

    def test_the_tool_posts_no_status(self):
        # Whether a head was tested is something only a run can state.
        self.three()
        _, _, calls = self.tool('build', '1', '2', '3', prs=self.prs((1, 'a', 'main'), (2, 'b', 'main'), (3, 'c', 'main')))
        self.assertEqual([path for method, path, _ in self.writes(calls) if method == 'POST'], [])
        self.assertNotIn('/statuses/', TOOL.read_text())

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
        start = dt.datetime(2026, 1, 1, tzinfo=dt.timezone.utc)
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
        (self.dir / 'cache' / '1-1.json').unlink()
        code, out, calls = self.cost(fixture, '--json', '--skip-short', '3')
        counts = json.loads(out.splitlines()[0])['counts']
        # Run 3 is short too, but it is a rerun: its first attempt's time is not in its wall time.
        self.assertEqual((code, counts['itemised'], counts['short'], counts['short_minutes']), (0, 2, 1, 1), out)
        self.assertNotIn(self.jobs(1), [c['path'] for c in calls])
        _, out, _ = self.cost(fixture, '--skip-short', '3')
        self.assertIn('1 runs left out as asked (--skip-short)', out)

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
            'started_at': '2026-01-01T00:00:00Z', 'completed_at': '2026-01-01T01:00:00Z'}
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
        job = lambda number: {'id': number, 'name': 'Host-x64', 'conclusion': 'failure', 'run_attempt': 1,
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
                                {'context': 'Stack CI', 'state': 'success', 'description': 'Tested by #2'}]}
        code, out, _ = self.tool('status', prs=self.prs((1, 'a', 'main'), (2, 'b', 'a')) + [pr(9, 'x', 'gone')],
                                 fixture={f'repos/{REPO}/commits/{a}/status': verdict})
        self.assertEqual(code, 0, out)
        self.assertRegex(out, r'pool of 2, top #2 b')
        self.assertRegex(out, r'1\. #1 .* success .*Tested by #2')
        self.assertRegex(out, r'2\. #2 .* top .*its own run')
        self.assertRegex(out, r'#9 x: NOT A POOL')


T1, T2 = '1' * 40, '2' * 40
HEAD, BEFORE, LANDED = 'a' * 40, 'b' * 40, 'c' * 40
CLEAN = {'status': 'ahead', 'files': ['src/lib.rs', 'docs/x.md']}


def lane(name, conclusion, attempt=1):
    return {'name': name, 'conclusion': conclusion, 'run_attempt': attempt}


def a_run(number, jobs, event='pull_request'):
    return {'id': number, 'event': event, 'jobs': jobs}


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

    def test_a_landing_that_changes_what_defines_the_lanes_proves_nothing(self):
        # A pull request runs its own workflow, Makefile and scripts: a job named like a
        # lane in such a run may have tested nothing.
        for path in ('.github/workflows/ci.yml', '.github/actions/checkout-deps/action.yml', 'Makefile',
                     '.config/nextest.toml', 'scripts/ci-pr-scope.sh', 'scripts/ci-pool/ci_pool.py',
                     'scripts/root-test-runner.sh', 'Containerfile', 'Containerfile.nested', 'rust-toolchain.toml',
                     '.cargo/config.toml'):
            lanes, no_lanes, why = self.proof(passed(ci_pool.LANES), compare={'status': 'ahead', 'files': ['src/lib.rs', path]})
            self.assertEqual((lanes, no_lanes), ({}, None), path)
            self.assertIn(path, why)
            skipped = self.proof(SCOPE_SKIPPED, compare={'status': 'ahead', 'files': [path]})
            self.assertEqual(skipped[:2], ({}, None), path)
        for path in ('src/lib.rs', 'tests/test_sanity.rs', 'docs/scripts/x.md', 'fleet/web/Makefile', 'bench/scripts/a.sh'):
            self.assertEqual(self.proof(passed(['Host-arm64']), compare={'status': 'ahead', 'files': [path]})[0],
                             {'Host-arm64': 7}, path)

    def test_a_renamed_file_is_judged_by_both_of_its_names(self):
        # landing_evidence lists a renamed file under both names: moving a script out of
        # scripts/ is a change to what defines the lanes.
        compare = {'status': 'ahead', 'files': ['tools/x.sh', 'scripts/x.sh']}
        self.assertEqual(self.proof(passed(ci_pool.LANES), compare=compare)[0], {})

    def test_a_comparison_that_may_be_cut_short_or_was_not_read_proves_nothing(self):
        many = {'status': 'ahead', 'files': [f'src/{n}.rs' for n in range(300)]}
        self.assertEqual(self.proof(passed(ci_pool.LANES), compare=many)[0], {})
        self.assertEqual(self.proof(passed(ci_pool.LANES), compare=None)[0], {})
        nearly = {'status': 'ahead', 'files': [f'src/{n}.rs' for n in range(299)]}
        self.assertEqual(len(self.proof(passed(ci_pool.LANES), compare=nearly)[0]), len(ci_pool.LANES))

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
                                                  a_run(8, passed(['Host-x64']) + [lane('Host-arm64', 'failure')], 'workflow_dispatch')], CLEAN),
                    ({'sha': 'd' * 40, 'tree': T2}, [a_run(9, passed(['Container-x64']))], CLEAN)]
        self.assertEqual(self.ALL - set(ci_pool.plan_lanes('push', T1, evidence)[0]), {'Host-arm64', 'Host-x64'})

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
                created=None, head=HEAD):
        created = created or dt.datetime.now(dt.timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ')
        fixture = {f'repos/{REPO}/git/commits/{LANDED}': {'tree': {'sha': T1}, 'parents': [{'sha': p} for p in parents]},
                   f'repos/{REPO}/commits/{LANDED}/pulls?per_page=100': [
                       {'head': {'sha': sha, 'repo': {'full_name': repo}}} for sha, repo in pulls],
                   f'repos/{REPO}/git/commits/{head}': {'tree': {'sha': head_tree}},
                   f'repos/{REPO}/compare/{BEFORE}...{head}': {
                       'status': compare['status'], 'files': [{'filename': name} for name in compare['files']]},
                   f'repos/{REPO}/actions/runs?head_sha={head}&per_page=100': {'workflow_runs': [
                       {'id': 100 + n, 'path': path, 'event': event, 'created_at': created} for n, (event, _) in enumerate(runs)]}}
        for n, (_, jobs) in enumerate(runs):
            fixture[f'repos/{REPO}/actions/runs/{100 + n}/jobs?filter=all&per_page=100'] = {'jobs': jobs}
        return fixture

    def plan(self, fixture, event='push', **env):
        out = self.dir / 'github_output'
        out.write_text('')
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
                     'LANES: ${{ github.event.inputs.lanes }}', 'GH_TOKEN: ${{ github.token }}', 'id: check'):
            self.assertIn(line, block)
        # An input is data: it reaches the planner through the environment, never a script.
        # Its only other use is in the name of the report job.
        uses = [line.strip().split(':', 1)[0] for line in text.splitlines() if 'inputs.lanes' in line]
        self.assertEqual(uses, ['LANES', 'name'])

    def test_every_script_and_local_action_a_lane_runs_is_lane_defining(self):
        text = CI.read_text()
        body = text.split('\njobs:\n', 1)[1]
        blocks = dict(re.findall(r'^  ([\w-]+):\n(.*?)(?=^  [\w-]+:\n|\Z)', body, re.M | re.S))
        used = set()
        for name in ('changes', 'skip-check', 'host', 'host-root', 'container'):
            used |= set(re.findall(r'(?:\./)?(?:fcvm/)?(scripts/[\w./-]+)', blocks[name]))
            used |= {'.github/' + path for path in re.findall(r'uses: \./(?:fcvm/)?\.github/([\w./-]+)', blocks[name])}
            used |= set(re.findall(r'(Containerfile[\w.-]*)', blocks[name]))
        self.assertTrue({'scripts/ci-pr-scope.sh', 'scripts/ci-pool/ci_pool.py', '.github/actions/checkout-deps'} <= used, used)
        for path in sorted(used) + ['.github/workflows/ci.yml', 'Makefile', '.config/nextest.toml', 'rust-toolchain.toml']:
            self.assertTrue(ci_pool.lane_defining(path.rstrip('.')), f'{path} is run by a lane and is not lane-defining')
        self.assertTrue((ROOT / '.config/nextest.toml').exists() and (ROOT / 'rust-toolchain.toml').exists())

    def test_what_the_makefiles_test_recipes_run_is_lane_defining(self):
        makefile = (ROOT / 'Makefile').read_text()
        for path in sorted(set(re.findall(r'(?:\./|\$\(MAKEFILE_DIR\))(scripts/[\w./-]+)', makefile))):
            self.assertTrue(ci_pool.lane_defining(path), path)
        self.assertFalse(ci_pool.lane_defining('src/main.rs'))
        self.assertFalse(ci_pool.lane_defining('tests/test_sanity.rs'))


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

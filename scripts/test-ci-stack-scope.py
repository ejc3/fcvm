#!/usr/bin/env python3
"""Tests of CI's pull request scope: drafts and lower stack PRs run nothing, the top
of a stack classifies the whole stack's diff, and a passing top marks the lower PRs.

Runs scripts/ci-pr-scope.sh and scripts/ci-stack-coverage.sh against a fake `gh`
that answers from a fixture and applies --jq with the real jq. Run with
make test-ci-scope.
"""
import ast
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import textwrap
import unittest

ROOT = Path(__file__).resolve().parent.parent
CI = ROOT / '.github/workflows/ci.yml'

FAKE_GH = textwrap.dedent('''\
    #!/usr/bin/env python3
    # Fake gh: answers `gh api` from $FAKE_GH_FIXTURE, applies --jq with jq, logs calls.
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
    if method == 'POST':
        print('{}'); sys.exit(0)
    if path not in fixture or fixture[path] == 'FAIL':
        sys.stderr.write('HTTP 502 for ' + str(path) + '\\n'); sys.exit(1)
    body = json.dumps(fixture[path])
    if jq is None:
        print(body); sys.exit(0)
    sys.exit(subprocess.run(['jq', '-r', jq], input=body, text=True).returncode)
''')

REPO = 'example-owner/example-repo'


def pr(number, head_ref, base_ref, sha='0' * 40, repo=REPO):
    return {'number': number, 'head': {'ref': head_ref, 'sha': sha, 'repo': {'full_name': repo}},
            'base': {'ref': base_ref}}


def files(*names, renamed_from=None):
    out = [{'filename': n} for n in names]
    if renamed_from:
        out[0]['previous_filename'] = renamed_from
    return out


class Harness(unittest.TestCase):
    def run_script(self, script, fixture, **env):
        with tempfile.TemporaryDirectory(prefix='ci-scope-') as tmp:
            gh = Path(tmp) / 'gh'
            gh.write_text(FAKE_GH)
            gh.chmod(0o755)
            (Path(tmp) / 'fixture.json').write_text(json.dumps(fixture))
            out = Path(tmp) / 'output'
            out.write_text('')
            log = Path(tmp) / 'log'
            log.write_text('')
            full = dict(os.environ, GH=str(gh), FAKE_GH_FIXTURE=str(Path(tmp) / 'fixture.json'),
                        FAKE_GH_LOG=str(log), GITHUB_OUTPUT=str(out), REPO=REPO,
                        DEFAULT_BRANCH='main', **env)
            proc = subprocess.run(['bash', str(ROOT / 'scripts' / script)], env=full,
                                  capture_output=True, text=True)
            self.assertEqual(proc.returncode, 0, proc.stderr + proc.stdout)
            outputs = dict(line.split('=', 1) for line in out.read_text().splitlines() if line)
            calls = [json.loads(line) for line in log.read_text().splitlines()]
            return outputs, calls, proc.stdout


class PrScope(Harness):
    def scope(self, fixture, **env):
        base = dict(EVENT='pull_request', PR='12', DRAFT='false', HEAD_REPO=REPO, HEAD_REF='feature-b',
                    HEAD_SHA='b' * 40, BASE_REF='main')
        base.update(env)
        return self.run_script('ci-pr-scope.sh', fixture, **base)

    def flags(self, outputs):
        return {k: outputs[k] for k in ('run', 'code', 'bench', 'fleet', 'web')}

    def test_a_push_runs_everything(self):
        outputs, calls, _ = self.scope({}, EVENT='push')
        self.assertEqual(self.flags(outputs), dict(run='true', code='true', bench='true', fleet='true', web='true'))
        self.assertEqual(calls, [])

    def test_a_closed_pr_runs_nothing_and_asks_nothing(self):
        outputs, calls, _ = self.scope({}, ACTION='closed')
        self.assertEqual(self.flags(outputs), dict(run='false', code='false', bench='false', fleet='false', web='false'))
        self.assertIn('closed', outputs['reason'])
        self.assertEqual(calls, [])

    def test_a_draft_runs_nothing_and_asks_nothing(self):
        outputs, calls, _ = self.scope({}, DRAFT='true')
        self.assertEqual(self.flags(outputs), dict(run='false', code='false', bench='false', fleet='false', web='false'))
        self.assertIn('draft', outputs['reason'])
        self.assertEqual(calls, [])

    def test_a_pr_with_another_stacked_on_it_runs_nothing(self):
        fixture = {f'repos/{REPO}/pulls?state=open&base=feature-b&per_page=100': [pr(13, 'feature-c', 'feature-b', 'c' * 40)],
                   f'repos/{REPO}/compare/{"b" * 40}...{"c" * 40}': {'status': 'ahead'}}
        outputs, _, _ = self.scope(fixture)
        self.assertEqual(outputs['run'], 'false')
        self.assertEqual(outputs['code'], 'false')
        self.assertIn('#13', outputs['reason'])

    def test_a_child_that_does_not_contain_this_head_defers_nothing(self):
        # A fix pushed here before the child was rebased: the child's runs do not
        # test this commit, so this PR runs.
        fixture = {f'repos/{REPO}/pulls?state=open&base=feature-b&per_page=100': [pr(13, 'feature-c', 'feature-b', 'c' * 40)],
                   f'repos/{REPO}/compare/{"b" * 40}...{"c" * 40}': {'status': 'diverged'},
                   f'repos/{REPO}/pulls/12/files': files('src/lib.rs')}
        outputs, _, _ = self.scope(fixture)
        self.assertEqual(outputs['run'], 'true')
        self.assertEqual(outputs['code'], 'true')

    def test_a_failed_ancestry_lookup_defers_nothing(self):
        fixture = {f'repos/{REPO}/pulls?state=open&base=feature-b&per_page=100': [pr(13, 'feature-c', 'feature-b', 'c' * 40)],
                   f'repos/{REPO}/compare/{"b" * 40}...{"c" * 40}': 'FAIL',
                   f'repos/{REPO}/pulls/12/files': files('src/lib.rs')}
        outputs, _, _ = self.scope(fixture)
        self.assertEqual(outputs['run'], 'true')

    def test_a_fork_pr_on_this_branch_does_not_defer_it(self):
        fixture = {f'repos/{REPO}/pulls?state=open&base=feature-b&per_page=100':
                   [pr(13, 'feature-c', 'feature-b', repo='someone/fork')],
                   f'repos/{REPO}/pulls/12/files': files('src/lib.rs')}
        outputs, _, _ = self.scope(fixture)
        self.assertEqual(outputs['run'], 'true')
        self.assertEqual(outputs['code'], 'true')

    def test_a_fork_pr_whose_branch_shares_a_stack_branch_name_runs(self):
        # Upstream has a PR stacked on feature-b; this PR's feature-b is a fork's branch.
        fixture = {f'repos/{REPO}/pulls?state=open&base=feature-b&per_page=100': [pr(13, 'feature-c', 'feature-b')],
                   f'repos/{REPO}/pulls/12/files': files('src/lib.rs')}
        outputs, _, _ = self.scope(fixture, HEAD_REPO='someone/fork')
        self.assertEqual(outputs['run'], 'true')
        self.assertEqual(outputs['code'], 'true')

    def test_a_failed_stack_lookup_runs(self):
        fixture = {f'repos/{REPO}/pulls?state=open&base=feature-b&per_page=100': 'FAIL',
                   f'repos/{REPO}/pulls/12/files': files('docs/x.md')}
        outputs, _, _ = self.scope(fixture)
        self.assertEqual(outputs['run'], 'true')

    def test_a_pr_on_main_classifies_its_own_files(self):
        fixture = {f'repos/{REPO}/pulls?state=open&base=feature-b&per_page=100': [],
                   f'repos/{REPO}/pulls/12/files': files('docs/x.md')}
        outputs, calls, _ = self.scope(fixture)
        self.assertEqual(self.flags(outputs), dict(run='true', code='false', bench='false', fleet='false', web='false'))
        self.assertNotIn(f'repos/{REPO}/compare/main...' + 'b' * 40, [c['path'] for c in calls])

    def test_the_top_of_a_stack_classifies_the_whole_stack(self):
        # The top PR changes only docs; the PR below it changed code. Classifying the
        # top's own files would skip the matrix for that code.
        fixture = {f'repos/{REPO}/pulls?state=open&base=feature-b&per_page=100': [],
                   f'repos/{REPO}/pulls/12/files': files('docs/x.md'),
                   f'repos/{REPO}/compare/main...' + 'b' * 40: {'files': files('docs/x.md', 'src/lib.rs')}}
        outputs, _, _ = self.scope(fixture, BASE_REF='feature-a')
        self.assertEqual(outputs['run'], 'true')
        self.assertEqual(outputs['code'], 'true')

    def test_a_stack_diff_the_compare_api_may_truncate_runs_everything(self):
        many = files(*[f'docs/{i}.md' for i in range(300)])
        fixture = {f'repos/{REPO}/pulls?state=open&base=feature-b&per_page=100': [],
                   f'repos/{REPO}/compare/main...' + 'b' * 40: {'files': many}}
        outputs, _, _ = self.scope(fixture, BASE_REF='feature-a')
        self.assertEqual(self.flags(outputs), dict(run='true', code='true', bench='true', fleet='true', web='true'))

    def test_a_failed_stack_diff_runs_everything(self):
        fixture = {f'repos/{REPO}/pulls?state=open&base=feature-b&per_page=100': [],
                   f'repos/{REPO}/compare/main...' + 'b' * 40: 'FAIL'}
        outputs, _, _ = self.scope(fixture, BASE_REF='feature-a')
        self.assertEqual(self.flags(outputs), dict(run='true', code='true', bench='true', fleet='true', web='true'))

    def test_a_rename_is_judged_by_where_it_came_from(self):
        fixture = {f'repos/{REPO}/pulls?state=open&base=feature-b&per_page=100': [],
                   f'repos/{REPO}/pulls/12/files': files('bench/chromium/foo.rs', renamed_from='src/foo.rs')}
        outputs, _, _ = self.scope(fixture)
        self.assertEqual(outputs['code'], 'true')
        self.assertEqual(outputs['bench'], 'true')


class StackCoverage(Harness):
    def cover(self, fixture, base_ref='feature-b'):
        return self.run_script('ci-stack-coverage.sh', fixture, PR='13', HEAD_SHA='c' * 40,
                               BASE_REF=base_ref, RUN_URL='https://example.invalid/run/1')

    def posts(self, calls):
        return [(c['path'], c['fields']['context'], c['fields']['state']) for c in calls if c['method'] == 'POST']

    def test_each_lower_pr_whose_head_this_run_tested_is_marked(self):
        fixture = {
            f'repos/{REPO}/pulls?state=open&head=example-owner:feature-b&per_page=100': [pr(12, 'feature-b', 'feature-a', 'b' * 40)],
            f'repos/{REPO}/compare/{"b" * 40}...{"c" * 40}': {'status': 'ahead'},
            f'repos/{REPO}/pulls?state=open&head=example-owner:feature-a&per_page=100': [pr(11, 'feature-a', 'main', 'a' * 40)],
            f'repos/{REPO}/compare/{"a" * 40}...{"c" * 40}': {'status': 'ahead'},
        }
        _, calls, _ = self.cover(fixture)
        self.assertEqual(self.posts(calls), [(f'repos/{REPO}/statuses/' + 'b' * 40, 'Stack CI', 'success'),
                                             (f'repos/{REPO}/statuses/' + 'a' * 40, 'Stack CI', 'success')])

    def test_a_lower_pr_that_moved_after_this_head_is_not_marked_nor_anything_below(self):
        fixture = {
            f'repos/{REPO}/pulls?state=open&head=example-owner:feature-b&per_page=100': [pr(12, 'feature-b', 'feature-a', 'b' * 40)],
            f'repos/{REPO}/compare/{"b" * 40}...{"c" * 40}': {'status': 'diverged'},
            f'repos/{REPO}/pulls?state=open&head=example-owner:feature-a&per_page=100': [pr(11, 'feature-a', 'main', 'a' * 40)],
            f'repos/{REPO}/compare/{"a" * 40}...{"c" * 40}': {'status': 'ahead'},
        }
        _, calls, _ = self.cover(fixture)
        self.assertEqual(self.posts(calls), [])

    def test_a_base_with_no_open_pr_ends_the_walk(self):
        fixture = {f'repos/{REPO}/pulls?state=open&head=example-owner:feature-b&per_page=100': []}
        _, calls, _ = self.cover(fixture)
        self.assertEqual(self.posts(calls), [])

    def test_a_fork_pr_with_the_same_branch_name_is_not_marked(self):
        fixture = {f'repos/{REPO}/pulls?state=open&head=example-owner:feature-b&per_page=100':
                   [pr(12, 'feature-b', 'main', 'b' * 40, repo='someone/fork')]}
        _, calls, _ = self.cover(fixture)
        self.assertEqual(self.posts(calls), [])


def jobs(text):
    body = text.split('\njobs:\n', 1)[1]
    found = {}
    for m in re.finditer(r'^  ([\w-]+):\n(.*?)(?=^  [\w-]+:\n|\Z)', body, re.M | re.S):
        found[m[1]] = m[2]
    return found


def job_if(block):
    m = re.search(r'^    if:\s*(.*)$', block, re.M)
    if not m:
        return None
    value = m[1].strip()
    if value in {'>-', '>', '|'}:
        tail = block[m.end():]
        value = ' '.join(re.match(r'\n((?:      .*(?:\n|$))*)', tail)[1].split())
    return value.removeprefix('${{').removesuffix('}}').strip()


def evaluate(expression, context):
    expression = expression.replace('skip-check', 'skip_check').replace('&&', ' and ').replace('||', ' or ')
    tree = ast.parse(expression, mode='eval')

    def visit(node):
        if isinstance(node, ast.Expression):
            return visit(node.body)
        if isinstance(node, ast.Constant):
            return node.value
        if isinstance(node, ast.Name):
            return context.get(node.id, {})
        if isinstance(node, ast.Attribute):
            parent = visit(node.value)
            return parent.get(node.attr, '') if isinstance(parent, dict) else ''
        if isinstance(node, ast.BoolOp):
            for value in node.values:
                result = visit(value)
                if isinstance(node.op, ast.And) and not result:
                    return result
                if isinstance(node.op, ast.Or) and result:
                    return result
            return result
        if isinstance(node, ast.Compare) and len(node.ops) == 1:
            left, right = visit(node.left), visit(node.comparators[0])
            return left == right if isinstance(node.ops[0], ast.Eq) else left != right
        if isinstance(node, ast.Call) and isinstance(node.func, ast.Name) and node.func.id == 'always':
            return True
        raise AssertionError(f'Unsupported expression: {ast.dump(node)}')

    return visit(tree)


class Workflow(unittest.TestCase):
    def test_a_deferred_run_skips_every_job_that_tests(self):
        # Every job other than the scope, the matrix planner and the report must be
        # skipped when `changes` says run=false, whatever the planner says.
        deferred = {k: 'false' for k in ('run', 'code', 'bench', 'fleet', 'web')}
        context = {
            'needs': {'changes': {'outputs': deferred, 'result': 'success'},
                      'skip_check': {'outputs': {k: 'true' for k in ('host_run', 'host_root_run', 'container_run', 'skip')}},
                      'summary': {'result': 'success'}},
            'github': {'event_name': 'pull_request', 'event': {'inputs': {'job': ''},
                       'pull_request': {'base': {'ref': 'feature-a'}, 'head': {'repo': {'full_name': REPO}}},
                       'repository': {'default_branch': 'main'}}, 'repository': REPO},
        }
        for name, block in jobs(CI.read_text()).items():
            if name in ('changes', 'skip-check', 'summary'):
                continue
            with self.subTest(job=name):
                gate = job_if(block)
                self.assertIsNotNone(gate, f'{name} has no if: and would run on a deferred PR')
                self.assertFalse(evaluate(gate, context), f'{name} runs on a deferred PR: {gate}')

    def test_a_deferred_summary_is_not_named_summary(self):
        block = jobs(CI.read_text())['summary']
        name = re.search(r'^    name:\s*(.*)$', block, re.M)[1]
        self.assertEqual(name, "${{ needs.changes.outputs.run == 'false' && 'Summary (deferred)' || 'Summary' }}")

    def test_the_scope_sees_draft_state_and_the_stack(self):
        block = jobs(CI.read_text())['changes']
        for env in ('DRAFT: ${{ github.event.pull_request.draft }}', 'HEAD_REF: ${{ github.event.pull_request.head.ref }}',
                    'HEAD_REPO: ${{ github.event.pull_request.head.repo.full_name }}',
                    'BASE_REF: ${{ github.event.pull_request.base.ref }}', 'run: bash scripts/ci-pr-scope.sh'):
            self.assertIn(env, block)

    def test_only_the_coverage_job_may_write_statuses(self):
        text = CI.read_text()
        self.assertEqual(text.count('statuses: write'), 1)
        self.assertIn('statuses: write', jobs(text)['stack-coverage'])
        self.assertIn("needs.summary.result == 'success'", jobs(text)['stack-coverage'])


if __name__ == '__main__':
    unittest.main(verbosity=1)

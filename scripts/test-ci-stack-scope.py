#!/usr/bin/env python3
"""Tests of CI's pull request scope: drafts and lower stack PRs run nothing, the top
of a stack classifies the whole stack's diff, and a passing top marks the lower PRs.

Runs scripts/ci-pr-scope.sh against a fake `gh` that answers from a fixture and applies
--jq with the real jq. What a passing top marks is tested in scripts/test-ci-pool.py. Run with
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


def pr(number, head_ref, base_ref, sha='0' * 40, repo=REPO, draft=False):
    return {'number': number, 'draft': draft,
            'head': {'ref': head_ref, 'sha': sha, 'repo': {'full_name': repo}},
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

    def test_a_draft_child_defers_nothing(self):
        # A draft runs no CI, so a draft stacked here tests nothing: deferring to it
        # left this PR's head with every job skipped and a green Summary (deferred).
        fixture = {f'repos/{REPO}/pulls?state=open&base=feature-b&per_page=100':
                   [pr(13, 'feature-c', 'feature-b', 'c' * 40, draft=True)],
                   f'repos/{REPO}/compare/{"b" * 40}...{"c" * 40}': {'status': 'ahead'},
                   f'repos/{REPO}/pulls/12/files': files('src/lib.rs')}
        outputs, _, stdout = self.scope(fixture)
        self.assertEqual(outputs['run'], 'true')
        self.assertEqual(outputs['code'], 'true')
        self.assertIn('#13 is stacked here but is a draft', stdout)

    def test_a_ready_child_beside_a_draft_one_still_defers(self):
        fixture = {f'repos/{REPO}/pulls?state=open&base=feature-b&per_page=100':
                   [pr(13, 'feature-c', 'feature-b', 'c' * 40, draft=True),
                    pr(14, 'feature-d', 'feature-b', 'd' * 40)],
                   f'repos/{REPO}/compare/{"b" * 40}...{"c" * 40}': {'status': 'ahead'},
                   f'repos/{REPO}/compare/{"b" * 40}...{"d" * 40}': {'status': 'ahead'}}
        outputs, _, _ = self.scope(fixture)
        self.assertEqual(outputs['run'], 'false')
        self.assertIn('#14', outputs['reason'])
        self.assertNotIn('#13', outputs['reason'])

    def test_a_child_whose_draft_state_is_unknown_defers_nothing(self):
        child = pr(13, 'feature-c', 'feature-b', 'c' * 40)
        del child['draft']
        fixture = {f'repos/{REPO}/pulls?state=open&base=feature-b&per_page=100': [child],
                   f'repos/{REPO}/compare/{"b" * 40}...{"c" * 40}': {'status': 'ahead'},
                   f'repos/{REPO}/pulls/12/files': files('src/lib.rs')}
        outputs, _, _ = self.scope(fixture)
        self.assertEqual(outputs['run'], 'true')

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
                   [pr(13, 'feature-c', 'feature-b', 'c' * 40, repo='someone/fork')],
                   # The fork's head would cover this one if the fork counted as a child.
                   f'repos/{REPO}/compare/{"b" * 40}...{"c" * 40}': {'status': 'ahead'},
                   f'repos/{REPO}/pulls/12/files': files('src/lib.rs')}
        outputs, calls, _ = self.scope(fixture)
        self.assertEqual(outputs['run'], 'true')
        self.assertEqual(outputs['code'], 'true')
        self.assertFalse([c for c in calls if '/compare/' in c['path']], 'the fork child was looked up')

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

    def test_a_scripts_only_pr_still_runs_ci(self):
        # The tree-wide guards sit in the actionlint job, which runs whenever `run` is true.
        fixture = {f'repos/{REPO}/pulls?state=open&base=feature-b&per_page=100': [],
                   f'repos/{REPO}/pulls/12/files': files('scripts/ci-pr-scope.sh')}
        outputs, _, _ = self.scope(fixture)
        self.assertEqual(outputs['run'], 'true')
        self.assertEqual(outputs['bench'], 'false')

    def test_what_fleet_web_reads_outside_fleet_web_runs_fleet_web(self):
        # mock/test/potato.test.ts compares the mock with fleet/core/tests/vectors and fleet/PROTOCOL.md.
        for changed in ('fleet/core/tests/vectors/33_potato.json', 'fleet/core/src/potato.rs', 'fleet/PROTOCOL.md'):
            fixture = {f'repos/{REPO}/pulls?state=open&base=feature-b&per_page=100': [],
                       f'repos/{REPO}/pulls/12/files': files(changed)}
            outputs, _, _ = self.scope(fixture)
            self.assertEqual(outputs['web'], 'true', changed)

    def test_the_top_of_a_stack_classifies_the_whole_stack(self):
        # The top PR changes only docs; the PR below it changed code. Classifying the
        # top's own files would skip the matrix for that code.
        fixture = {f'repos/{REPO}/pulls?state=open&base=feature-b&per_page=100': [],
                   f'repos/{REPO}/pulls/12/files': files('docs/x.md'),
                   f'repos/{REPO}/compare/main...' + 'b' * 40: {'files': files('docs/x.md', 'src/lib.rs')}}
        outputs, _, _ = self.scope(fixture, BASE_REF='feature-a')
        self.assertEqual(outputs['run'], 'true')
        self.assertEqual(outputs['code'], 'true')

    def test_a_change_to_the_vmm_confinement_asks_for_the_x64_host_lane(self):
        # Host-x64's test-confine step is the only place the x86_64 seccomp policy runs
        # under real confinement, and a pull request leaves that job out unless asked.
        for path, confine in (('resources/seccomp/confined-changes.json', 'true'),
                              ('src/confine.rs', 'true'),
                              ('src/firecracker/vm.rs', 'true'),
                              ('src/hypervisor/firecracker.rs', 'true'),
                              # The pre_exec and keep_fds, the preflight and the restore's
                              # thread check, setup's compile and digest, the assertion
                              # every gate test makes, and the gate's filter and count.
                              ('src/utils.rs', 'true'),
                              ('src/commands/common.rs', 'true'),
                              ('src/setup/kernel.rs', 'true'),
                              ('tests/common/mod.rs', 'true'),
                              ('Makefile', 'true'),
                              # The Firecracker pin and [confine], their parser, and the
                              # two commands that call the preflight.
                              ('rootfs-config.toml', 'true'),
                              ('src/setup/rootfs.rs', 'true'),
                              ('src/commands/snapshot.rs', 'true'),
                              ('src/commands/podman/mod.rs', 'true'),
                              ('src/setup/pasta.rs', 'false'),
                              ('scripts/confine-gate.sh', 'true'),
                              ('tests/test_confine.rs', 'true'),
                              ('src/main.rs', 'false'),
                              ('docs/x.md', 'false')):
            fixture = {f'repos/{REPO}/pulls?state=open&base=feature-b&per_page=100': [],
                       f'repos/{REPO}/pulls/12/files': files(path)}
            outputs, _, _ = self.scope(fixture)
            self.assertEqual(outputs['confine'], confine, path)

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
    expression = re.sub(r'!(?!=)', ' not ', expression).strip()
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
        if isinstance(node, ast.UnaryOp) and isinstance(node.op, ast.Not):
            return not visit(node.operand)
        if isinstance(node, ast.Call) and isinstance(node.func, ast.Name) and node.func.id == 'always':
            return True
        if isinstance(node, ast.Call) and isinstance(node.func, ast.Name) and node.func.id == 'cancelled':
            return False
        if isinstance(node, ast.Call) and isinstance(node.func, ast.Name) and node.func.id == 'format':
            args = [visit(a) for a in node.args]
            return args[0].format(*args[1:])
        raise AssertionError(f'Unsupported expression: {ast.dump(node)}')

    return visit(tree)


class Workflow(unittest.TestCase):
    def test_a_deferred_run_skips_every_job_that_tests(self):
        # Every job other than the scope, the matrix planner, the report and the status job
        # (which tests nothing and says so) must be skipped when `changes` says run=false,
        # whatever the planner says.
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
            if name in ('changes', 'skip-check', 'summary', 'pool-status'):
                continue
            with self.subTest(job=name):
                gate = job_if(block)
                self.assertIsNotNone(gate, f'{name} has no if: and would run on a deferred PR')
                self.assertFalse(evaluate(gate, context), f'{name} runs on a deferred PR: {gate}')

    def test_a_merged_pr_closure_never_joins_mains_concurrency_group(self):
        # For a merged PR's `closed` event github.ref is the target branch; grouping by it
        # would cancel main's run (cancel-in-progress is on for pull_request) and leave the
        # PR's own runs alone.
        text = CI.read_text()
        group = re.search(r'^concurrency:\n  group: \$\{\{ (.*) \}\}$', text, re.M)[1]
        def g(ctx):
            # The group is "<workflow>-<expr>": evaluate the expression part.
            part = group.split('}}-${{', 1)[1] if '}}-${{' in group else group.split('-', 1)[1]
            return 'CI-' + str(evaluate(part.strip(), ctx))
        merged = {'github': {'ref': 'refs/heads/main', 'event_name': 'pull_request',
                             'event': {'pull_request': {'number': 1234}}}}
        opened = {'github': {'ref': 'refs/pull/1234/merge', 'event_name': 'pull_request',
                             'event': {'pull_request': {'number': 1234}}}}
        push = {'github': {'ref': 'refs/heads/main', 'event_name': 'push', 'event': {}}}
        self.assertEqual(g(merged), g(opened), 'a merged PR\'s closed run is not in its PR\'s group')
        self.assertNotEqual(g(merged), g(push), 'a merged PR\'s closed run joins main\'s group')

    def test_a_deferred_summary_is_not_named_summary(self):
        block = jobs(CI.read_text())['summary']
        name = re.search(r'^    name:\s*(.*)$', block, re.M)[1]
        self.assertEqual(name, "${{ needs.changes.outputs.run == 'false' && 'Summary (deferred)' || "
                               "(github.event.inputs.lanes && 'Summary (some lanes)' || 'Summary') }}")
        # The name is `Summary` only for a run that deferred nothing and filtered no lane.
        expression = name.removeprefix('${{').removesuffix('}}').strip()
        def named(run, lanes):
            return evaluate(expression, {'needs': {'changes': {'outputs': {'run': run}}},
                                         'github': {'event': {'inputs': {'lanes': lanes}}}})
        self.assertEqual(named('true', ''), 'Summary')
        self.assertEqual(named('true', 'Host-arm64'), 'Summary (some lanes)')
        self.assertEqual(named('false', ''), 'Summary (deferred)')
        self.assertEqual(named('false', 'Host-arm64'), 'Summary (deferred)')

    def test_the_scope_sees_draft_state_and_the_stack(self):
        block = jobs(CI.read_text())['changes']
        for env in ('DRAFT: ${{ github.event.pull_request.draft }}', 'HEAD_REF: ${{ github.event.pull_request.head.ref }}',
                    'HEAD_REPO: ${{ github.event.pull_request.head.repo.full_name }}',
                    'BASE_REF: ${{ github.event.pull_request.base.ref }}', 'run: bash scripts/ci-pr-scope.sh'):
            self.assertIn(env, block)

    def test_only_the_status_job_may_write_statuses(self):
        text = CI.read_text()
        self.assertEqual(text.count('statuses: write'), 1)
        # No job may cancel or rerun a run: a watcher that stopped runs was measured and
        # taken out (docs/ci-pool.md, "A run that was stopped").
        self.assertNotIn('actions: write', text)
        block = jobs(text)['pool-status']
        self.assertIn('statuses: write', block)
        self.assertIn('run: python3 scripts/ci-pool/ci_pool.py mark', block)
        for env in ('RAN: ${{ needs.changes.outputs.run }}', 'SUMMARY: ${{ needs.summary.result }}',
                    'HEAD_SHA: ${{ github.event.pull_request.head.sha }}', 'RUN_ID: ${{ github.run_id }}'):
            self.assertIn(env, block)

    def status_gate(self, **override):
        context = {
            'needs': {'changes': {'outputs': {'run': 'true'}, 'result': 'success'}, 'summary': {'result': 'success'}},
            'github': {'event_name': 'pull_request', 'repository': REPO,
                       'event': {'action': 'synchronize',
                                 'pull_request': {'base': {'ref': 'feature-a'}, 'head': {'repo': {'full_name': REPO}}},
                                 'repository': {'default_branch': 'main'}}},
        }
        context['github']['event'].update(override.pop('event', {}))
        context['github'].update(override)
        return evaluate(job_if(jobs(CI.read_text())['pool-status']), context)

    def test_the_status_job_runs_whatever_summary_waited_for_and_however_it_ended(self):
        # GitHub adds success() to an `if:` that names no status function, and success() is false
        # when a job anywhere before this one in the needs graph was skipped. Summary waits for
        # jobs most PRs skip (the bench and fleet tests), so a status job gated on the event alone
        # would be skipped on every pool. It also has to run after a failed Summary (to say so)
        # and on a run that tested nothing (to say that).
        blocks = jobs(CI.read_text())
        waited_for = re.search(r'^    needs: \[(.*)\]$', blocks['summary'], re.M)[1].split(', ')
        self.assertTrue([name for name in waited_for if job_if(blocks[name]) is not None])
        gate = job_if(blocks['pool-status'])
        self.assertTrue(gate.startswith('!cancelled() && '), gate)
        self.assertNotIn('needs.summary.result', gate)
        self.assertNotIn('needs.changes.outputs.run', gate)
        self.assertTrue(self.status_gate())

    def test_summary_reports_on_every_run_that_was_not_itself_cancelled(self):
        # A cancelled run's failed Summary stayed on the head for good: on a lower member of
        # a stack no later check has its name. So Summary does not report on a cancelled run.
        blocks = jobs(CI.read_text())
        self.assertEqual(job_if(blocks['summary']), '!cancelled()')
        # cancelled() is about the run. A lane that ended cancelled while the run went on
        # (its own timeout, a lost runner) must still fail Summary: Summary waits for every
        # lane job, and the step that fails it looks for a cancelled result as for a failed
        # one. Run 45830927225 (a throwaway probe, 2026-11-26): a job that hit its
        # timeout-minutes ended cancelled, and a job needing it with this `if:` ran and
        # read `needs.<job>.result == 'cancelled'`.
        waited_for = re.search(r'^    needs: \[(.*)\]$', blocks['summary'], re.M)[1].split(', ')
        for lane_job in ('host', 'host-root', 'container'):
            self.assertIn(lane_job, waited_for)
        gate = re.search(r"      - name: Fail if any gating job did not succeed\n        if: (.*)\n", blocks['summary'])
        self.assertEqual(gate[1], "contains(needs.*.result, 'failure') || contains(needs.*.result, 'cancelled')")
        step = blocks['summary'][gate.end():].split('\n      - ', 1)[0]
        self.assertRegex(step, r'(?m)^\s+exit 1$')

    def test_the_status_job_never_runs_with_a_forks_code_or_outside_a_pull_request(self):
        fork = {'pull_request': {'base': {'ref': 'main'}, 'head': {'repo': {'full_name': 'someone/fork'}}}}
        self.assertFalse(self.status_gate(event=fork))
        self.assertFalse(self.status_gate(event={'action': 'closed'}))
        for event in ('push', 'workflow_dispatch', 'workflow_run', 'pull_request_target'):
            self.assertFalse(self.status_gate(event_name=event), event)


if __name__ == '__main__':
    unittest.main(verbosity=1)

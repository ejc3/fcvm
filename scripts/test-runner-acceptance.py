#!/usr/bin/env python3
"""Offline tests of the exact manual runner-acceptance workflow (no AWS calls)."""
import ast
from pathlib import Path
import re
import shlex
import subprocess
import textwrap
import unittest

WORKFLOW = Path(__file__).resolve().parent.parent / '.github/workflows/runner-acceptance.yml'


class RunnerAcceptanceTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.workflow = WORKFLOW.read_text()
        body = cls.workflow.split("          python3 - <<'PY'\n", 1)[1].split('\n          PY', 1)[0]
        tree = ast.parse(textwrap.dedent(body))
        functions = [node for node in tree.body if isinstance(node, ast.FunctionDef)]
        namespace = {'re': re, 'shlex': shlex}
        exec(compile(ast.Module(body=functions, type_ignores=[]), '<workflow>', 'exec'), namespace)
        cls.validate = staticmethod(namespace['validate'])

    def valid(self):
        instance = 'i-0123456789abcdef0'
        service = f'actions.runner.ejc3-fcvm.runner-{instance}.service'
        return {
            'identity': {'accountId': '928413605543', 'region': 'us-west-1',
                         'instanceId': instance, 'architecture': 'arm64'},
            'runner_name': f'runner-{instance}', 'machine': 'aarch64',
            'settings': {'agentName': f'runner-{instance}', 'ephemeral': True},
            'service': service, 'cgroup': f'0::/system.slice/{service}\n',
            'dropin': '[Service]\nRestart=no\nEnvironment=GITHUB_ACTIONS_SERVICE_EXIT_AFTER_N_FAILURES=1\nExecStopPost=+/usr/bin/systemctl --no-block poweroff\n',
            'dropin_uid': 0, 'dropin_mode': 0o644,
            'restart': 'no', 'active': 'active',
            'environment': 'GITHUB_ACTIONS_SERVICE_EXIT_AFTER_N_FAILURES=1',
            'stop_post': '{ path=/usr/bin/systemctl ; argv[]=/usr/bin/systemctl --no-block poweroff ; ignore_errors=no ; }',
        }

    def test_brokered_host_is_accepted(self):
        self.validate(self.valid())

    def test_legacy_or_missing_ephemeral_flag_is_rejected(self):
        for value in (False, None, 'true', 1):
            data = self.valid()
            data['settings']['ephemeral'] = value
            with self.assertRaises(ValueError):
                self.validate(data)

    def test_wrong_identity_is_rejected(self):
        for key, value in [('accountId', '111111111111'), ('region', 'us-east-1'),
                           ('instanceId', 'not-an-instance'), ('architecture', 'x86_64')]:
            data = self.valid()
            data['identity'][key] = value
            with self.assertRaises(ValueError):
                self.validate(data)

    def test_runner_name_and_settings_are_bound_to_instance(self):
        for key in ('runner_name', 'settings'):
            data = self.valid()
            data[key] = 'runner-other' if key == 'runner_name' else {'ephemeral': True, 'agentName': 'other'}
            with self.assertRaises(ValueError):
                self.validate(data)

    def test_inactive_restarting_or_foreign_service_is_rejected(self):
        for key, value in [('restart', 'always'), ('active', 'inactive'),
                           ('service', '../other.service'), ('cgroup', '0::/other.service'),
                           ('machine', 'x86_64')]:
            data = self.valid()
            data[key] = value
            with self.assertRaises(ValueError):
                self.validate(data)

    def test_missing_unprivileged_or_writable_dropin_is_rejected(self):
        for key, value in [('dropin', ''), ('dropin_uid', 1000), ('dropin_mode', 0o666),
                           ('dropin', self.valid()['dropin'].replace('=+', '='))]:
            data = self.valid()
            data[key] = value
            with self.assertRaises(ValueError):
                self.validate(data)

    def test_effective_poweroff_must_match_without_extra_commands(self):
        for value in ('', '{ path=/bin/true ; argv[]=/bin/true ; }',
                      self.valid()['stop_post'].replace('poweroff ;', 'poweroff extra ;'),
                      self.valid()['stop_post'] + ' { path=/bin/true ; }'):
            data = self.valid()
            data['stop_post'] = value
            with self.assertRaises(ValueError):
                self.validate(data)

    def test_failure_retry_environment_must_be_effective_and_exact(self):
        for value in ('', 'GITHUB_ACTIONS_SERVICE_EXIT_AFTER_N_FAILURES=2',
                      'PREFIX_GITHUB_ACTIONS_SERVICE_EXIT_AFTER_N_FAILURES=1',
                      'GITHUB_ACTIONS_SERVICE_EXIT_AFTER_N_FAILURES=1 GITHUB_ACTIONS_SERVICE_EXIT_AFTER_N_FAILURES=2'):
            data = self.valid()
            data['environment'] = value
            with self.assertRaises(ValueError):
                self.validate(data)
        data = self.valid()
        data['environment'] += ' "OTHER=unrelated value"'
        self.validate(data)

    def test_workflow_is_manual_main_only_single_short_arm_job(self):
        self.assertIn('on:\n  workflow_dispatch:\n', self.workflow)
        self.assertIn('  accept:\n    needs: authorize\n', self.workflow)
        self.assertIn('runs-on: [self-hosted, Linux, ARM64]', self.workflow)
        self.assertIn('timeout-minutes: 5', self.workflow)
        self.assertIn('permissions: {}', self.workflow)
        jobs = self.workflow.split('jobs:\n', 1)[1]
        self.assertEqual(re.findall(r'^  [\w-]+:$', jobs, re.M), ['  authorize:', '  accept:'])
        self.assertEqual(self.workflow.count('runs-on: [self-hosted,'), 1)
        for forbidden in ['pull_request:', 'push:', 'schedule:', 'workflow_run:', 'inputs:',
                          'uses:', 'secrets.', 'id-token:', 'configure-aws', 'aws ssm',
                          'iam/security-credentials', 'get-secret-value']:
            self.assertNotIn(forbidden, self.workflow)

    def test_runtime_uses_imdsv2_identity_only_and_read_only_systemctl(self):
        self.assertIn("method='PUT'", self.workflow)
        self.assertIn('X-aws-ec2-metadata-token-ttl-seconds', self.workflow)
        self.assertIn('latest/dynamic/instance-identity/document', self.workflow)
        self.assertIn('urllib.request.ProxyHandler({})', self.workflow)
        self.assertIn("['systemctl', 'show', service", self.workflow)
        self.assertIn('timeout=5', self.workflow)

    def test_observer_window_follows_validation_and_smoke(self):
        window = self.workflow.index('time.sleep(90)')
        self.assertLess(self.workflow.index("validate({'identity': identity"), window)
        self.assertLess(self.workflow.index('if sum(range(100)) != 4950:'), window)
        self.assertLess(self.workflow.index("'phase': 'ready-for-observer'"), window)
        self.assertGreater(self.workflow.index("'phase': 'smoke-complete'"), window)
        self.assertEqual(self.workflow.count('time.sleep('), 1)

    def test_offline_tests_run_on_the_hosted_linter(self):
        ci = WORKFLOW.with_name('ci.yml').read_text()
        lint = ci.split('  actionlint:\n', 1)[1].split('\n  skip-check:', 1)[0]
        self.assertIn('runs-on: ubuntu-latest', lint)
        self.assertIn('run: make test-runner-acceptance', lint)

    def test_non_main_dispatch_fails_instead_of_skipping_green(self):
        gate = re.search(r'^  authorize:\n(.*?)(?=^  accept:)', self.workflow, re.M | re.S)
        self.assertIsNotNone(gate, 'No executing authorization job: a skipped acceptance can look green')
        self.assertIn('runs-on: ubuntu-latest', gate[1])
        self.assertIsNone(re.search(r'^    if:', gate[1], re.M))
        script = textwrap.dedent(gate[1].split('        run: |\n', 1)[1])
        for event, ref, allowed in [('workflow_dispatch', 'refs/heads/main', True),
                                   ('workflow_dispatch', 'refs/heads/feature', False),
                                   ('workflow_dispatch', 'refs/tags/main', False),
                                   ('pull_request', 'refs/heads/main', False),
                                   ('workflow_dispatch', '', False)]:
            result = subprocess.run(['/bin/bash', '-c', script], text=True, capture_output=True,
                                    env={'GITHUB_EVENT_NAME': event, 'GITHUB_REF': ref}, timeout=5)
            self.assertEqual(result.returncode == 0, allowed, (event, ref, result.stderr))


if __name__ == '__main__':
    unittest.main(verbosity=2)

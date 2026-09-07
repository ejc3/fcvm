#!/usr/bin/env python3
"""Evaluate the checked-in job gates against trusted and drive-by events.

Run with make test-ci-security. The small expression evaluator accepts only
the GitHub boolean/comparison/context subset used by these gates, not Python
code. These tests do not replace live GitHub environment/approval inspection.
"""
import ast
import json
from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parent.parent


def job_field(workflow, job, field):
    content = (ROOT / '.github/workflows' / workflow).read_text()
    block = re.search(r'^  ' + re.escape(job) + r':\n(.*?)(?=^  [\w-]+:|\Z)',
                      content, re.M | re.S)
    if block is None:
        raise AssertionError(f'Missing job {job}')
    found = re.search(r'^    ' + re.escape(field) + r':\s*(.*)', block[1], re.M)
    if found is None:
        raise AssertionError(f'Missing {job}.{field}')
    value = found[1].strip()
    if value in {'>-', '>', '|', '|-'}:
        tail = block[1][found.end():]
        value = ' '.join(re.match(r'\n((?:      .*(?:\n|$))*)', tail)[1].split())
    return value.removeprefix('${{').removesuffix('}}').strip()


def evaluate(expression, context):
    tree = ast.parse(expression.replace('&&', ' and ').replace('||', ' or '), mode='eval')

    def visit(node):
        if isinstance(node, ast.Expression):
            return visit(node.body)
        if isinstance(node, ast.Constant):
            return node.value
        if isinstance(node, ast.Name) and node.id == 'github':
            return context
        if isinstance(node, ast.Attribute):
            parent = visit(node.value)
            return parent.get(node.attr, '') if isinstance(parent, dict) else ''
        if isinstance(node, ast.BoolOp):
            values = [bool(visit(value)) for value in node.values]
            return all(values) if isinstance(node.op, ast.And) else any(values)
        if isinstance(node, ast.Compare) and len(node.ops) == 1:
            left, right = visit(node.left), visit(node.comparators[0])
            if isinstance(node.ops[0], ast.Eq):
                return left == right
            if isinstance(node.ops[0], ast.NotEq):
                return left != right
        if isinstance(node, ast.Call) and isinstance(node.func, ast.Name) and not node.keywords:
            args = [visit(arg) for arg in node.args]
            if node.func.id == 'fromJSON' and len(args) == 1:
                return json.loads(args[0])
            if node.func.id == 'contains' and len(args) == 2:
                return args[1] in args[0]
        raise AssertionError(f'Unsupported job-gate expression: {ast.dump(node)}')

    return bool(visit(tree))


def github(event_name, event=None, **extra):
    return {'actor': 'ejc3', 'repository': 'ejc3/fcvm', 'ref': 'refs/heads/main',
            'event_name': event_name, 'event': event or {}, **extra}


class WorkflowSecurityTest(unittest.TestCase):
    def safety(self, context):
        return evaluate(job_field('claude.yml', 'safety-check', 'if'), context)

    def builder(self, context):
        return evaluate(job_field('build-runner-ami.yml', 'build-ami', 'if'), context)

    def test_trusted_pr_and_comment_events_keep_ci(self):
        for association in ['OWNER', 'MEMBER', 'COLLABORATOR']:
            for event_name in ['pull_request', 'issue_comment', 'pull_request_review_comment']:
                event = {'pull_request': {'author_association': association},
                         'issue': {'pull_request': {'url': 'https://api.github.com/repos/ejc3/fcvm/pulls/1'}},
                         'comment': {'author_association': association}}
                with self.subTest(association=association, event_name=event_name):
                    self.assertTrue(self.safety(github(event_name, event)))

    def test_drive_by_comments_cannot_start_gatekeeper_or_mint_token(self):
        for association in ['NONE', 'FIRST_TIMER', 'FIRST_TIME_CONTRIBUTOR', 'CONTRIBUTOR', '']:
            for event_name in ['pull_request', 'issue_comment', 'pull_request_review_comment']:
                event = {'pull_request': {'author_association': association},
                         'issue': {'pull_request': {'url': 'present'}},
                         'comment': {'author_association': association}}
                with self.subTest(association=association, event_name=event_name):
                    self.assertFalse(self.safety(github(event_name, event)))

    def test_plain_issue_comments_and_unexpected_events_do_not_run(self):
        self.assertFalse(self.safety(github('issue_comment', {'comment': {'author_association': 'OWNER'}})))
        self.assertFalse(self.safety(github('push')))
        self.assertFalse(self.safety(github('pull_request', {'pull_request': {'author_association': 'OWNER'}}, actor='dependabot[bot]')))

    def test_workflow_run_requires_same_repository_even_if_branch_is_main(self):
        for origin in ['ejc3/fcvm', 'outsider/fcvm', '']:
            for event in ['push', 'pull_request', 'workflow_dispatch', 'issue_comment', '']:
                run = {'head_repository': {'full_name': origin}, 'head_branch': 'main', 'event': event}
                with self.subTest(origin=origin, event=event):
                    self.assertEqual(self.safety(github('workflow_run', {'workflow_run': run})),
                                     origin == 'ejc3/fcvm' and event in {'push', 'pull_request', 'workflow_dispatch'})

    def test_builder_manual_runs_require_main(self):
        self.assertTrue(self.builder(github('workflow_dispatch')))
        for ref in ['refs/heads/feature', 'refs/tags/v1', 'refs/pull/1/merge', '']:
            self.assertFalse(self.builder(github('workflow_dispatch', ref=ref)))

    def test_builder_automatic_runs_require_successful_same_repo_main(self):
        for origin in ['ejc3/fcvm', 'outsider/fcvm', '']:
            for branch in ['main', 'feature', '']:
                for conclusion in ['success', 'failure', 'cancelled']:
                    run = {'head_repository': {'full_name': origin}, 'head_branch': branch, 'conclusion': conclusion}
                    self.assertEqual(self.builder(github('workflow_run', {'workflow_run': run})),
                                     origin == 'ejc3/fcvm' and branch == 'main' and conclusion == 'success')

    def test_builder_publish_environment_and_immutable_checkout(self):
        self.assertEqual(job_field('build-runner-ami.yml', 'build-ami', 'environment'), 'runner-ami-publish')
        content = (ROOT / '.github/workflows/build-runner-ami.yml').read_text()
        self.assertIn('role/github-actions-ami-builder', content)
        self.assertNotIn('role/github-actions-terraform', content)
        self.assertIn('ref: ${{ github.sha }}', content)
        self.assertIn('persist-credentials: false', content)
        self.assertIn('fetch-depth: 0', content)
        self.assertRegex(content, r'uses: actions/checkout@[0-9a-f]{40}')
        self.assertRegex(content, r'uses: aws-actions/configure-aws-credentials@[0-9a-f]{40}')


if __name__ == '__main__':
    unittest.main(verbosity=2)

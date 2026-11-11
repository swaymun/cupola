#!/usr/bin/env python3
"""Offline contracts of scripts/check-cli-codex: what fails each check, and the isolation."""
import copy
import importlib.machinery
from pathlib import Path
import types
import unittest

loader = importlib.machinery.SourceFileLoader('check', str(Path(__file__).with_name('check-cli-codex')))
check = types.ModuleType(loader.name)
check.__file__ = loader.path
loader.exec_module(check)

CLI = '/private/tmp/cli-codex-x/npm/node_modules/@openai/codex-darwin-arm64/vendor/aarch64-apple-darwin/bin/codex'
CHATGPT = '/Applications/ChatGPT.app/Contents/Resources/codex-cli/bin/codex'
# The shape a passing run recorded (2026-10-21, codex-cli 0.163.0).
PASSING = {
    'cliVersion': 'codex-cli 0.163.0',
    'local': {'installed': True, 'path': CLI, 'version': 'codex-cli 0.163.0'},
    'ready': {'ready': True, 'detail': 'Ready', 'reason': 'ready'},
    'providers': {'providers': [{'id': 'codex', 'state': 'signedIn', 'running': True, 'incompatible': False},
                                {'id': 'claude', 'state': 'notInstalled', 'running': False}]},
    'runtimeCommands': [CLI + ' app-server --stdio'],
    'models': {'models': [{'agentFamily': 'codex', 'id': 'gpt-6.1-sol', 'isDefault': True},
                          {'agentFamily': 'codex', 'id': 'gpt-6-luna', 'isDefault': False}]},
    'thread': {'conversationId': 'c1', 'family': 'codex'},
    'threads': {'threads': [{'conversationId': 'c1', 'family': 'codex'}], 'partial': []},
    'read': {'messages': [], 'thread': {'turns': []}},
    'sessionFiles': [],
}


def failing(obs):
    return [c['name'] for c in check.evaluate(obs) if not c['pass']]


class EvaluateTests(unittest.TestCase):
    def test_a_recorded_passing_run_passes_every_check(self):
        self.assertEqual(failing(PASSING), [])

    def test_each_regression_fails_exactly_its_check(self):
        cases = [
            ('ChatGPT visible to the locator', 'Codex found on PATH, not in ChatGPT',
             lambda o: (o['local'].update(path=CHATGPT), o.update(runtimeCommands=[CHATGPT + ' app-server --stdio']))),
            ('no Codex on PATH', 'Codex found on PATH, not in ChatGPT', lambda o: o['local'].update(installed=False)),
            ('locator found another Codex', 'The same Codex the PATH runs', lambda o: o.update(cliVersion='codex-cli 0.151.0')),
            ('host not ready', 'Host ready', lambda o: o.update(ready={'ready': False, 'reason': 'runtime_unavailable'})),
            ('signed out', 'Codex signed in and running', lambda o: o['providers']['providers'][0].update(state='signedOut')),
            ('incompatible', 'Codex signed in and running', lambda o: o['providers']['providers'][0].update(incompatible=True)),
            ('host ran ChatGPT\'s Codex', 'The host runs that Codex',
             lambda o: o['runtimeCommands'].append(CHATGPT + ' app-server --stdio')),
            ('no app server', 'The host runs that Codex', lambda o: o.update(runtimeCommands=[])),
            ('no default model', 'Codex models listed', lambda o: o['models']['models'][0].update(isDefault=False)),
            ('thread not created', 'Read-only thread started', lambda o: o['thread'].update(family='claude')),
            ('thread missing from the list', 'Thread listed', lambda o: o['threads'].update(threads=[])),
            ('Codex listing failed', 'Thread listed', lambda o: o['threads'].update(partial=[{'family': 'codex'}])),
            ('a message was sent', 'No model work', lambda o: o['read'].update(messages=[{'id': 'm'}])),
            ('Codex wrote a session', 'No model work', lambda o: o.update(sessionFiles=['2026/10/21/rollout.jsonl'])),
        ]
        for label, name, change in cases:
            obs = copy.deepcopy(PASSING)
            change(obs)
            with self.subTest(label):
                self.assertEqual(failing(obs), [name])

    def test_a_run_that_stopped_early_fails_what_it_never_observed_and_says_where(self):
        obs = {k: PASSING[k] for k in ('cliVersion', 'local', 'ready')}
        obs['error'] = 'providers.list: no answer'
        rows = check.evaluate(obs)
        self.assertEqual([c['name'] for c in rows if c['pass']],
                         ['Codex found on PATH, not in ChatGPT', 'The same Codex the PATH runs', 'Host ready'])
        self.assertTrue(all('stopped at: providers.list: no answer' in c['detail'] for c in rows if not c['pass']))


class IsolationTests(unittest.TestCase):
    def test_the_host_sees_a_fresh_account_and_none_of_the_callers_runtime_settings(self):
        root = Path('/private/tmp/cli-codex-x/mac')
        real = {'HOME': '/Users/owner', 'PATH': '/opt/homebrew/bin:/usr/bin', 'WONDER_CODEX_BIN': CHATGPT,
                'WONDER_DATA_DIR': '/Users/owner/.wonder', 'CODEX_HOME': '/Users/owner/.codex',
                'CUPOLA_DATA_DIR': '/Users/owner/.wonder', 'USER': 'owner', 'TMPDIR': '/var/tmp/'}
        env = check.isolated_env(root, real)
        self.assertEqual(env['HOME'], str(root / 'home'))
        self.assertEqual(env['WONDER_DATA_DIR'], str(root / 'data'))
        self.assertEqual(env['PATH'].split(':')[0], str(root / 'bin'))
        self.assertNotIn('/opt/homebrew/bin', env['PATH'])
        self.assertEqual(env['WONDER_CHATGPT_APP'], str(root / 'no-chatgpt'))
        for leaked in ('WONDER_CODEX_BIN', 'CODEX_HOME', 'CUPOLA_DATA_DIR'):
            self.assertNotIn(leaked, env)
        self.assertTrue(all(str(root) in value or key in ('USER', 'TMPDIR') or value.endswith('/sbin')
                            for key, value in env.items()), env)

    def test_only_a_chatgpt_bundle_counts_as_chatgpt(self):
        self.assertTrue(check.inside_chatgpt(CHATGPT))
        self.assertFalse(check.inside_chatgpt(CLI))
        self.assertFalse(check.inside_chatgpt('/opt/ChatGPT.app.backup/codex'))


if __name__ == '__main__':
    unittest.main()

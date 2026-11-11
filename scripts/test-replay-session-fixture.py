#!/usr/bin/env python3
"""Replay boundary contracts: schema, paging, timing, identity and privacy."""
import asyncio
from copy import deepcopy
import importlib.util
import json
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch
from jsonschema import Draft7Validator

spec = importlib.util.spec_from_file_location('replay', Path(__file__).with_name('replay-session-fixture.py'))
replay = importlib.util.module_from_spec(spec)
spec.loader.exec_module(replay)


class ReplayTests(unittest.IsolatedAsyncioTestCase):
    @classmethod
    def setUpClass(cls):
        cls.profile = json.loads(replay.fixtures.PROFILE.read_text())
        cls.bundles = {p: replay.fixtures.generate(cls.profile, p, count=4, chunk_chars=4096) for p in ('codex', 'claude')}
        cls.definitions = json.loads(replay.fixtures.SCHEMA.read_text())['definitions']

    def host(self, provider='codex', **options):
        host = replay.ReplayHost(deepcopy(self.bundles[provider]), Path('/fixture/todo'), **options)
        host.request('initialize', {})
        host.ready = True
        return host

    def schema(self, name, value):
        Draft7Validator({'$ref': '#/definitions/' + name, 'definitions': self.definitions}).validate(value)

    def test_prepare_produces_launchable_verified_config(self):
        from types import SimpleNamespace
        import contextlib
        import io
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / 'source.json'
            source.write_text(json.dumps(self.bundles['claude']))
            target = root / '.local' / 'prepared'
            with contextlib.redirect_stdout(io.StringIO()):
                replay.prepare(SimpleNamespace(bundle=source, output=target, history_turns=2, speed=10000))
            config = json.loads((target / 'config.json').read_text())
            bundle = replay.read_prepared(target, config)
            host = replay.ReplayHost(bundle, target / 'workspace', config['historyTurns'], config['speed'])
            self.assertEqual(host.parent['id'], config['threadId'])
            self.assertEqual(host.parent['sessionId'], config['nativeSessionId'])
            self.assertEqual(host.child['id'], config['childThreadId'])
            self.assertEqual(len(host.parent['turns']), 2)

    def test_prepared_bundle_rejects_changed_bytes_before_runtime_start(self):
        import hashlib
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            raw = json.dumps(self.bundles['codex']).encode()
            (root / 'bundle.json').write_bytes(raw)
            config = {'bundle': 'bundle.json', 'bundleSha256': hashlib.sha256(raw).hexdigest()}
            self.assertEqual(replay.read_prepared(root, config), self.bundles['codex'])
            (root / 'bundle.json').write_bytes(raw + b' ')
            with self.assertRaises(ValueError):
                replay.read_prepared(root, config)
            with self.assertRaises(ValueError):
                replay.read_prepared(root, {**config, 'bundle': '../bundle.json'})

    def test_project_discovery_preserves_provider_identities_and_schema(self):
        for provider in self.bundles:
            host = self.host(provider)
            model = host.request('model/list', {})
            if provider == 'codex':
                self.schema('GetAccountResponse', host.request('account/read', {}))
                self.schema('GetAccountRateLimitsResponse', host.request('account/rateLimits/read', {}))
                self.schema('ModelListResponse', model)
                self.assertEqual(host.parent['id'], replay.native_id('fixture-codex'))
            else:
                self.assertTrue(host.parent['id'].startswith('claude-'))
                self.assertTrue(model['data'][0]['id'].startswith('claude:'))
                self.assertEqual(host.request('project/sessions/list', {'dirs': ['/unrelated']})['data'], [])
                sessions = host.request('project/sessions/list', {'dirs': [host.parent['cwd']]})['data']
                session = sessions[0]['sessionId']
                self.assertEqual(host.request('project/session/attach', {'sessionId': session})['thread']['id'], host.parent['id'])
                self.assertEqual(host.request('project/sessions/list', {'dirs': [host.parent['cwd']], 'excludeSessionIds': [session]})['data'], [])
            self.assertNotEqual(host.child['id'], host.parent['id'])

    def test_history_uses_normal_schema_and_exclusive_paging(self):
        host = self.host()
        thread = host.parent['id']
        self.assertEqual([t['id'] for t in host.request('thread/list', {'sourceKinds': ['cli', 'appServer']})['data']], [thread])
        self.schema('ThreadReadResponse', host.request('thread/read', {'threadId': thread, 'includeTurns': True}))
        self.schema('ThreadResumeResponse', host.request('thread/resume', {'threadId': thread}))
        params = {'threadId': thread, 'limit': 2, 'itemsView': 'notLoaded'}
        first = host.request('thread/turns/list', params)
        self.schema('ThreadTurnsListResponse', first)
        second = host.request('thread/turns/list', {**params, 'cursor': first['nextCursor']})
        self.assertEqual([t['id'] for t in first['data'] + second['data']], [t['id'] for t in reversed(host.all_turns)])
        self.assertIsNone(second['nextCursor'])
        self.assertTrue(all(not t['items'] and t['itemCount'] > 0 for t in first['data']))
        reverse = host.request('thread/turns/list', {**params, 'sortDirection': 'asc', 'cursor': first['backwardsCursor']})
        self.assertEqual(reverse['data'][0]['id'], first['data'][0]['id'])
        with self.assertRaises(ValueError):
            host.request('thread/items/list', {'threadId': thread, 'cursor': first['nextCursor']})
        turn = host.all_turns[0]
        params = {'threadId': thread, 'turnId': turn['id'], 'limit': 2, 'sortDirection': 'asc'}
        items = host.request('thread/items/list', params)
        self.schema('ThreadItemsListResponse', items)
        after = host.request('thread/items/list', {**params, 'cursor': {'type': 'item', 'itemId': items['data'][-1]['item']['id']}})
        self.assertEqual(after['data'][0]['item'], turn['items'][2])
        reverse = host.request('thread/items/list', {**params, 'sortDirection': 'desc', 'cursor': after['backwardsCursor']})
        self.assertEqual(reverse['data'][0]['item'], turn['items'][1])

    def test_children_are_owned_by_the_project_parent_for_both_providers(self):
        for provider in self.bundles:
            host = self.host(provider)
            child = host.request('thread/read', {'threadId': replay.native_id('fixture-child'), 'includeTurns': True})['thread']
            self.assertEqual(child['source']['subAgent']['thread_spawn']['parent_thread_id'], host.parent['id'])
            self.assertEqual(child['cwd'], host.parent['cwd'])
            self.assertTrue(child['turns'][0]['items'])
            if provider == 'codex':
                self.schema('ThreadReadResponse', {'thread': child})
            else:
                self.assertEqual(host.request('thread/backgroundTasks/list', {'threadId': host.parent['id']})['data'][0]['id'], child['id'])
                self.assertTrue(host.request('thread/backgroundTask/read', {'threadId': host.parent['id'], 'taskId': child['id']})['items'])
            self.assertEqual(host.request('thread/list', {'parentThreadId': 'unrelated'})['data'], [])
            with self.assertRaises(ValueError):
                host.request('thread/read', {'threadId': 'unrelated'})

    def test_claude_pages_match_bridge_order_and_reject_changed_transcript(self):
        host = self.host('claude')
        p = {'threadId': host.parent['id'], 'limit': 2, 'sortDirection': 'desc'}
        first = host.request('thread/items/list', p)
        expected = [i for t in reversed(host.parent['turns']) for i in t['items']]
        self.assertEqual([r['item'] for r in first['data']], expected[:2])
        self.assertNotIn('backwardsCursor', first)
        second = host.request('thread/items/list', {**p, 'cursor': first['nextCursor']})
        self.assertEqual([r['item'] for r in second['data']], expected[2:4])
        host.parent['turns'][-1]['items'].append({'type': 'agentMessage', 'id': 'new', 'text': 'New synthetic item'})
        with self.assertRaises(ValueError):
            host.request('thread/items/list', {**p, 'cursor': first['nextCursor']})

    async def test_declining_approval_never_emits_the_recorded_success(self):
        frames = []
        host = self.host(history_turns=0, speed=10000, emit=frames.append)
        host.request('turn/start', {'threadId': host.parent['id'], 'input': [{'type': 'text', 'text': replay.TRIGGER}]})
        task = host.active
        async def decline():
            while not task.done():
                requests = [f for f in frames if f.get('method', '').endswith('/requestApproval')]
                if requests:
                    host.answer({'id': requests[0]['id'], 'result': {'decision': 'decline'}})
                    return
                await asyncio.sleep(0.001)
        await asyncio.wait_for(asyncio.gather(task, decline()), 10)
        self.assertEqual(frames[-1]['params']['turn']['status'], 'interrupted')
        self.assertEqual(host.metrics['turnsReplayed'], 0)

    async def test_replay_enters_transport_order_and_waits_for_real_approval_answers(self):
        for provider in self.bundles:
            frames, timing = [], []
            host = self.host(provider, history_turns=0, speed=10000, emit=frames.append, trace=timing.append)
            host.request('turn/start', {'threadId': host.parent['id'], 'input': [{'type': 'text', 'text': replay.TRIGGER}], 'clientUserMessageId': 'synthetic-client-message'})
            task = host.active
            async def answer():
                handled = set()
                while not task.done():
                    for frame in frames[:]:
                        if frame.get('method', '').endswith('/requestApproval') and frame['id'] not in handled:
                            handled.add(frame['id'])
                            # No recorded client answer may be emitted by the host.
                            self.assertFalse(any('result' in f for f in frames))
                            host.answer({'id': frame['id'], 'result': {'decision': 'accept'}})
                    await asyncio.sleep(0.001)
            await asyncio.wait_for(asyncio.gather(task, answer()), 10)
            expected = [row['message'] for row in host.rows[host.all_turns[0]['id']] if row['direction'] == 'server']
            self.assertEqual(frames, expected)
            self.assertEqual([point['event'] for point in timing], ['turn_start', 'first_delta'])
            self.assertEqual({point['turnId'] for point in timing}, {host.all_turns[0]['id']})
            self.assertLessEqual(timing[0]['monotonicNs'], timing[1]['monotonicNs'])
            self.assertEqual(set(timing[0]), {'event', 'turnId', 'monotonicNs'})
            self.assertEqual(host.parent['turns'], host.all_turns[:1])
            user = next(i for i in host.parent['turns'][0]['items'] if i['type'] == 'userMessage')
            self.assertEqual(user['clientId'], 'synthetic-client-message')
            self.assertGreater(host.metrics['approvalsAnswered'], 0)
            self.assertEqual(host.metrics['modelCalls'], 0)
            with self.assertRaises(ValueError):
                host.answer({'id': 'unrelated', 'result': {'decision': 'accept'}})

    async def test_original_pacing_and_interrupt_do_not_claim_recorded_success(self):
        # Replay uses absolute due times. A late first wake-up shortens the gap
        # between emitted frames; wall-clock bounds on that gap assert the wrong
        # contract under load. Control only replay's clock/sleep, not the event
        # loop running this test, and verify on-time, late and accelerated cases.
        for speed, first_lag in ((1, 0), (1, .02), (1, .2), (2, .02)):
            with self.subTest(speed=speed, first_lag=first_lag):
                host = self.host(history_turns=0, speed=speed)
                turn = host.all_turns[0]
                start, end = deepcopy(host.rows[turn['id']][0]), deepcopy(host.rows[turn['id']][-1])
                start['atMs'], end['atMs'] = 0, 120
                host.rows[turn['id']] = [start, end]
                clock, sleeps, frames = [10.0], [], []
                async def advance(delay):
                    sleeps.append(delay)
                    clock[0] += delay + (first_lag if len(sleeps) == 1 else 0)
                scheduler = SimpleNamespace(**vars(asyncio))
                scheduler.get_running_loop = lambda: SimpleNamespace(time=lambda: clock[0])
                scheduler.sleep = advance
                host.emit = lambda frame: frames.append((clock[0], frame))
                with patch.object(replay, 'asyncio', scheduler):
                    host.request('turn/start', {'threadId': host.parent['id'], 'input': [{'type': 'text', 'text': replay.TRIGGER}]})
                    await host.active
                self.assertEqual(len(sleeps), 2)
                self.assertEqual(sleeps[0], 0)
                self.assertAlmostEqual(sleeps[1], max(0, .12 / speed - first_lag))
                self.assertAlmostEqual(frames[-1][0], 10 + max(first_lag, .12 / speed))
                self.assertEqual([frame for _, frame in frames], [start['message'], end['message']])
                self.assertEqual(host.metrics['turnsReplayed'], 1)
        # Keep real task cancellation and wrong-turn rejection coverage.
        host = self.host(history_turns=0, emit=lambda f: None)
        with self.assertRaises(ValueError):
            host.request('turn/start', {'threadId': host.parent['id'], 'input': [{'type': 'text', 'text': 'Run a real task'}]})
        host.request('turn/start', {'threadId': host.parent['id'], 'input': [{'type': 'text', 'text': replay.TRIGGER}]})
        task = host.active
        await asyncio.sleep(0)
        with self.assertRaises(ValueError):
            host.request('turn/interrupt', {'threadId': host.parent['id'], 'turnId': 'other'})
        host.request('turn/interrupt', {'threadId': host.parent['id'], 'turnId': turn['id']})
        await task
        self.assertEqual(host.parent['turns'][-1]['status'], 'interrupted')
        self.assertEqual(host.metrics['turnsReplayed'], 0)

    def test_preparation_rejects_private_content_and_tampered_assets(self):
        for mutation in ('private', 'asset'):
            bundle = deepcopy(self.bundles['codex'])
            if mutation == 'private':
                bundle['project']['name'] = 'fictional-person@example.invalid'
            else:
                bundle['assets'][0]['base64'] = 'dGFtcGVyZWQ='
            with tempfile.TemporaryDirectory() as temp:
                path = Path(temp) / 'bundle.json'
                path.write_text(json.dumps(bundle))
                with self.assertRaises(ValueError):
                    replay.read_bundle(path)


if __name__ == '__main__':
    unittest.main()

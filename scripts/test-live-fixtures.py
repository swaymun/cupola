#!/usr/bin/env python3
"""Offline guards for the bounded live fixture runner; never starts a model."""
from concurrent.futures import ProcessPoolExecutor
from copy import deepcopy
import base64
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from types import SimpleNamespace
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('live', Path(__file__).with_name('run-live-fixtures.py'))
live = importlib.util.module_from_spec(spec)
spec.loader.exec_module(live)


def claim(path, i):
    try:
        live.claim_turn(Path(path), 'codex', str(i))
        return True
    except live.InfrastructureFailure:
        return False


class LocalEventServer:
    """Real loopback WebSocket; metadata is synthetic and no provider runs."""
    def __init__(self, handler):
        self.websocket = live.check_dependencies()
        self.handler = handler
        self.release = threading.Event()
        self.completed = threading.Event()
        self.error = None
        self.peer = None
        self.listener = socket.socket()
        self.listener.bind(('127.0.0.1', 0)); self.listener.listen()
        self.listener.settimeout(10)
        challenge = dict.fromkeys(('deviceId', 'challengeId', 'nonce', 'origin', 'hostInstallationId', 'issuedAtMs', 'expiresAtMs'), 'offline')
        self.probe = SimpleNamespace(address='http://127.0.0.1:' + str(self.listener.getsockname()[1]),
            origin='https://fixture.invalid', session={'deviceId': 'offline', 'sessionToken': 'offline', 'csrfToken': 'offline'},
            sign=lambda _: 'offline', call=lambda path: challenge if path.endswith('/challenge') else {'hostEpoch': 'epoch', 'lastSequence': 0})
        self.worker = threading.Thread(target=self.serve, name='fixture-test-server')

    def send(self, value, opcode=1):
        frame = self.websocket.ABNF.create_frame(value, opcode)
        frame.mask_value = 0
        self.peer.send_frame(frame)

    def serve(self):
        try:
            with self.listener.accept()[0] as connection:
                connection.settimeout(.2)
                request = b''
                while not request.endswith(b'\r\n\r\n'):
                    byte = connection.recv(1)
                    if not byte: raise AssertionError('Client closed during handshake')
                    request += byte
                    if len(request) > 8192: raise AssertionError('Oversized handshake')
                headers = dict(line.split(':', 1) for line in request.decode().split('\r\n')[1:] if ':' in line)
                key = headers['Sec-WebSocket-Key'].strip()
                accept = base64.b64encode(hashlib.sha1((key + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11').encode()).digest()).decode()
                connection.sendall(('HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ' + accept + '\r\n\r\n').encode())
                self.peer = self.websocket.WebSocket()
                self.peer.sock = connection; self.peer.connected = True
                subscription = json.loads(self.peer.recv_frame().data)
                if subscription['deviceId'] != 'offline': raise AssertionError('Wrong subscription')
                self.handler(self)
                self.completed.set()
                self.release.wait(2)
        except BaseException as error:
            if not self.release.is_set(): self.error = error
            self.completed.set()

    def __enter__(self):
        self.worker.start()
        return self

    def __exit__(self, *_):
        self.release.set()
        if self.peer: self.peer.abort()
        self.listener.close()
        self.worker.join(timeout=10)
        if self.worker.is_alive(): raise AssertionError('Local event server did not stop')


class EventSocketTests(unittest.TestCase):
    def test_setup_gap_answers_multiple_pings_then_delivers_ordered_events(self):
        # Same Events constructor/read path as exercise's model-catalog wait,
        # with a 200 ms peer deadline instead of a provider call or a 30 s wait.
        first = {'sequence': 1, 'event': {'type': 'host_status'}}
        batch = [{'sequence': i, 'event': {'type': 'host_status'}} for i in (2, 3)]
        def setup(server):
            for index in range(4):
                payload = bytes([index])
                server.send(payload, server.websocket.ABNF.OPCODE_PING)
                reply = server.peer.recv_frame()
                self.assertEqual((reply.opcode, reply.data), (server.websocket.ABNF.OPCODE_PONG, payload))
                server.release.wait(.075)
            server.send(json.dumps(first))
            server.send(json.dumps(batch))
            # A final control round trip proves the reader consumed both data
            # messages before setup completes, without a scheduling-based sleep.
            server.send(b'', server.websocket.ABNF.OPCODE_PING)
            reply = server.peer.recv_frame()
            self.assertEqual((reply.opcode, reply.data), (server.websocket.ABNF.OPCODE_PONG, b''))
        with LocalEventServer(setup) as server:
            events = live.Events(server.probe)
            try:
                metadata = server.probe.call
                def catalog(path):
                    if path == '/api/v1/agent-options':
                        return {'models': [{'id': 'offline-model', 'agentFamily': 'codex'}] if server.completed.is_set() else []}
                    return metadata(path)
                # Exercise the actual pre-observation catalog wait with a local
                # readiness gate, never a provider/catalog endpoint.
                with patch.object(server.probe, 'call', side_effect=catalog):
                    setup_seconds = live.wait_for_model(server.probe, 'codex', 'offline-model', seconds=10)
                self.assertTrue(server.completed.wait(10), 'Setup gap exceeded its bounded deadline')
                if server.error: raise server.error
                read_started = time.monotonic_ns()
                received = [events.read(), events.read()]
                self.assertEqual([entry[1] for entry in received], [[first], batch])
                self.assertLessEqual(received[0][0], received[1][0])
                self.assertLess(received[1][0], read_started, 'Timestamp must reflect receipt during setup')
                self.assertEqual(events.frames, [first, *batch])
                self.assertIsNone(events.read(), 'No duplicate queued frames')
                self.assertGreaterEqual(setup_seconds, .3)
                print(f'setup gap: four timed Pongs plus a receipt fence across {setup_seconds * 1000:.2f} ms without Events.read; three later events in order')
            finally:
                events.close()

    def wait_for_reader_exit(self, events):
        events._reader.join(timeout=10)
        self.assertFalse(events._reader.is_alive(), 'Receiver did not release after terminal state')
        self.assertIsNone(events.socket.sock, 'Receiver must release the transport too')

    def test_frame_and_byte_overflow_fail_without_silently_evicting_events(self):
        frame = {'sequence': 1, 'event': {'type': 'host_status'}}
        raw = json.dumps(frame)
        for limits, message in (({'MAX_FRAMES': 2}, 'bounded fixture window'),
                                ({'MAX_BYTES': len(raw.encode()) * 2}, 'bounded fixture byte window')):
            with self.subTest(limits=limits):
                def overflow(server):
                    for _ in range(3): server.send(raw)
                with patch.multiple(live.Events, **limits), LocalEventServer(overflow) as server:
                    events = live.Events(server.probe)
                    try:
                        self.wait_for_reader_exit(events)
                        self.assertEqual(len(events._pending), 2, 'No unbounded backlog or silent eviction')
                        self.assertEqual([events.read()[1], events.read()[1]], [[frame], [frame]])
                        with self.assertRaisesRegex(live.InfrastructureFailure, message): events.read()
                        self.assertEqual(events.frames, [frame, frame])
                        self.assertEqual(len(events._pending), 0)
                    finally:
                        events.close()

    def test_queued_events_are_delivered_once_before_peer_close_error(self):
        frames = [{'sequence': i, 'event': {'type': 'host_status'}} for i in (1, 2, 3)]
        def deliver_then_close(server):
            server.send(json.dumps(frames[0]))
            server.send(json.dumps(frames[1:]))
            server.send(b'\x03\xe8', server.websocket.ABNF.OPCODE_CLOSE)
        with LocalEventServer(deliver_then_close) as server:
            events = live.Events(server.probe)
            try:
                self.wait_for_reader_exit(events)
                received = [events.read(), events.read()]
                self.assertEqual([entry[1] for entry in received], [frames[:1], frames[1:]])
                self.assertLessEqual(received[0][0], received[1][0])
                with self.assertRaisesRegex(live.InfrastructureFailure, 'Event stream closed'): events.read()
                self.assertEqual(events.frames, frames)
            finally:
                events.close()

    def test_peer_close_protocol_error_and_resync_surface_to_the_consumer(self):
        cases = [
            ('close', lambda server: server.send(b'\x03\xe8', server.websocket.ABNF.OPCODE_CLOSE), 'Event stream closed', None),
            ('invalid JSON', lambda server: server.send('{'), 'Event stream reader failed', json.JSONDecodeError),
            ('invalid control frame', lambda server: server.send(b'x' * 126, server.websocket.ABNF.OPCODE_PING), 'Event stream reader failed', None),
            ('resync', lambda server: server.send(json.dumps({'event': {'type': 'resync_required'}})), 'requires resynchronization', None),
            ('transport EOF', lambda server: server.peer.abort(), 'Event stream reader failed', None),
        ]
        for name, handler, message, cause in cases:
            with self.subTest(case=name), LocalEventServer(handler) as server:
                events = live.Events(server.probe)
                try:
                    self.wait_for_reader_exit(events)
                    with self.assertRaisesRegex(live.InfrastructureFailure, message) as raised: events.read()
                    if cause: self.assertIsInstance(raised.exception.__cause__, cause)
                    if name == 'resync':
                        self.assertEqual(events.frames, [{'event': {'type': 'resync_required'}}])
                finally:
                    events.close()

    def test_close_cancels_waiting_read_and_joins_the_receiver_repeatedly(self):
        for _ in range(3):
            with LocalEventServer(lambda _: None) as server:
                events = live.Events(server.probe)
                errors = []
                entered = threading.Event()
                def read():
                    entered.set()
                    try: events.read()
                    except Exception as error: errors.append(error)
                with patch.object(live.Events, 'READ_TIMEOUT', 5):
                    consumer = threading.Thread(target=read)
                    consumer.start()
                    self.assertTrue(entered.wait(10))
                    started = time.monotonic()
                    try:
                        events.close()
                        consumer.join(timeout=1)
                        self.assertFalse(consumer.is_alive(), 'close must wake a queued read')
                        self.wait_for_reader_exit(events)
                        self.assertLess(time.monotonic() - started, 1)
                        self.assertEqual(len(errors), 1)
                        self.assertIsInstance(errors[0], live.InfrastructureFailure)
                        self.assertEqual(str(errors[0]), 'Event stream closed')
                        events.close()  # Idempotent after reader/transport release.
                    finally:
                        events.close()
                        consumer.join(timeout=1)

    def test_normal_read_timeout_empty_batch_and_cleanup_snapshot(self):
        send = threading.Event()
        frame = {'sequence': 1, 'event': {'type': 'host_status'}}
        def deliver(server):
            if not send.wait(10): raise AssertionError('Test did not request data')
            server.send('[]')
            server.send(json.dumps([frame]))
            server.send(b'', server.websocket.ABNF.OPCODE_PING)
            self.assertEqual(server.peer.recv_frame().opcode, server.websocket.ABNF.OPCODE_PONG)
        with LocalEventServer(deliver) as server:
            events = live.Events(server.probe)
            try:
                self.assertIsNone(events.read(), 'An idle poll must retain the existing None result')
                self.assertEqual(events.frames, [])
                send.set()
                self.assertTrue(server.completed.wait(10))
                if server.error: raise server.error
                snapshot = events.frames
                snapshot.clear()
                self.assertEqual(events.frames, [frame], 'A snapshot cannot mutate stored evidence')
                # Cleanup must keep received data even if observe never read it.
                events.close()
                self.assertEqual(events.frames, [frame])
                self.assertEqual(events.read()[1], [frame])
                with self.assertRaisesRegex(live.InfrastructureFailure, 'Event stream closed'): events.read()
                self.assertEqual(events.frames, [frame], 'Reading must not append the same frames twice')
            finally:
                events.close()


class LiveFixtureTests(unittest.TestCase):
    def test_preflight_reports_missing_and_exited_codex_without_aborting_claude(self):
        node = shutil.which('node')
        self.assertIsNotNone(node, 'Node is required for offline preflight checks')
        with tempfile.TemporaryDirectory() as root:
            root = Path(root)
            sdk = root / 'node_modules/@anthropic-ai/claude-agent-sdk'; sdk.mkdir(parents=True)
            (sdk / 'package.json').write_text(json.dumps({'name': '@anthropic-ai/claude-agent-sdk', 'version': 'offline', 'type': 'module', 'main': 'index.mjs'}))
            (sdk / 'index.mjs').write_text('''
export const tool = () => {}; export const createSdkMcpServer = () => {};
export function query() { return {
  initializationResult: async () => new Promise(resolve => setTimeout(resolve, 100)),
  accountInfo: async () => ({ apiProvider: 'firstParty', subscriptionType: 'Pro' }),
  supportedModels: async () => [{ value: 'haiku', resolvedModel: 'claude-haiku-5-5' }],
  mcpServerStatus: async () => [], interrupt: () => {}, close: () => {}
}; }
''')
            platform = subprocess.check_output([node, '-p', 'process.platform+"-"+process.arch'], text=True).strip()
            binary = root / ('node_modules/@anthropic-ai/claude-agent-sdk-' + platform); binary.mkdir()
            (binary / 'claude').write_text('offline')
            exited = root / 'exited'; exited.write_text('#!/bin/sh\nexit 23\n'); exited.chmod(0o700)
            for codex in (root / 'missing', exited):
                with self.subTest(codex=codex.name):
                    result = subprocess.run([node, str(Path(__file__).with_name('preflight-live-fixtures.mjs')), str(codex), str(root), str(root)], capture_output=True, text=True, timeout=30)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    report = json.loads(result.stdout)
                    self.assertEqual(report['modelTurns'], 0)
                    self.assertIn('error', report['providers']['codex'])
                    self.assertTrue(report['providers']['claude']['connected'])

    def write_stub_sdk(self, root, version, resolved):
        node = shutil.which('node')
        sdk = root / 'node_modules/@anthropic-ai/claude-agent-sdk'; sdk.mkdir(parents=True)
        (sdk / 'package.json').write_text(json.dumps({'name': '@anthropic-ai/claude-agent-sdk', 'version': version, 'type': 'module', 'main': 'index.mjs'}))
        (sdk / 'index.mjs').write_text('''
export const tool = () => {}; export const createSdkMcpServer = () => {};
export function query() { return {
  initializationResult: async () => ({}),
  accountInfo: async () => ({ apiProvider: 'firstParty', subscriptionType: 'Pro' }),
  supportedModels: async () => [{ value: 'haiku', resolvedModel: 'RESOLVED' }],
  mcpServerStatus: async () => [], interrupt: () => {}, close: () => {}
}; }
'''.replace('RESOLVED', resolved))
        platform = subprocess.check_output([node, '-p', 'process.platform+"-"+process.arch'], text=True).strip()
        binary = root / ('node_modules/@anthropic-ai/claude-agent-sdk-' + platform); binary.mkdir()
        (binary / 'claude').write_text('offline')

    def test_preflight_reads_the_installed_apps_activated_sdk_not_the_bundled_one(self):
        # 2026-10-15 nightly: the bundled 0.3.283 resolved haiku to 4.5 while the installed
        # app (and the isolated daemon, mid-run) ran 0.3.300, which resolves it to 5.5.
        node = shutil.which('node')
        self.assertIsNotNone(node, 'Node is required for offline preflight checks')
        with tempfile.TemporaryDirectory() as root:
            root = Path(root)
            bundled, store = root / 'bundled', root / 'store'
            self.write_stub_sdk(bundled, '0.3.283', 'claude-haiku-4-5-20251001')
            self.write_stub_sdk(store / 'versions/0.3.300', '0.3.300', 'claude-haiku-5-5')
            (store / 'versions/0.3.300/package.json').write_text('{}')
            preflight = str(Path(__file__).with_name('preflight-live-fixtures.mjs'))
            def claude(*extra):
                result = subprocess.run([node, preflight, str(root / 'missing'), str(bundled), str(root), *extra],
                                        capture_output=True, text=True, timeout=10)
                self.assertEqual(result.returncode, 0, result.stderr)
                return json.loads(result.stdout)['providers']['claude']
            self.assertEqual(claude()['runtimeVersion'], '0.3.283')
            self.assertEqual(claude(str(store))['runtimeVersion'], '0.3.283', 'no state.json: bundled')
            (store / 'state.json').write_text(json.dumps({'active': '0.3.300', 'lastGood': '0.3.283'}))
            catalog = claude(str(store))
            self.assertEqual(catalog['runtimeVersion'], '0.3.300')
            self.assertEqual(live.select_model('claude', {**catalog, 'windows': [{'usedPercent': 5}]}),
                             {'model': 'claude:haiku', 'effort': None})

    def test_isolated_daemon_is_seeded_with_the_preflight_sdk(self):
        with tempfile.TemporaryDirectory() as root:
            root = Path(root)
            store = root / 'store'; version = store / 'versions/0.3.300'
            (version / 'node_modules').mkdir(parents=True); (version / 'package.json').write_text('{"version":"0.3.300"}')
            live.seed_claude_sdk(store, root / 'daemon', '0.3.300', '0.3.283')
            target = root / 'daemon/claude-runtime/sdk'
            self.assertEqual((target / 'versions/0.3.300/package.json').read_text(), '{"version":"0.3.300"}')
            state = json.loads((target / 'state.json').read_text())
            self.assertEqual((state['active'], state['lastGood'], state['staged']), ('0.3.300', '0.3.283', None))
            self.assertLess(abs(state['checkedAt'] / 1000 - time.time()), 60, 'suppresses the daily update check')
            live.seed_claude_sdk(store, root / 'bundled-only', '0.3.283', '0.3.283')
            self.assertFalse((root / 'bundled-only').exists(), 'the bundled SDK needs no seed')
            with self.assertRaises(live.InfrastructureFailure):
                live.seed_claude_sdk(store, root / 'missing', '0.3.301', '0.3.283')

    def catalog(self, provider='codex'):
        return {'connected': True, 'models': [{'id': live.MODELS[provider], 'supportedReasoningEfforts': [{'reasoningEffort': 'low'}]}], 'usage': {'ordinaryUsageAllowed': True, 'rateLimits': {'primary': {'usedPercent': 5}}}, 'windows': [{'usedPercent': 5}]}

    def test_exact_model_and_effort_no_haiku_fallback(self):
        self.assertEqual(live.select_model('codex', self.catalog())['effort'], 'low')
        claude = self.catalog('claude')
        claude['models'] = [{'id': 'haiku', 'resolvedModel': 'claude-haiku-4-5-20251001', 'efforts': []}]
        with self.assertRaises(live.InfrastructureFailure): live.select_model('claude', claude)
        for accepted in ('claude-haiku-5-5', 'claude-haiku-5-5-20261001'):
            claude['models'][0]['resolvedModel'] = accepted
            self.assertEqual(live.select_model('claude', claude), {'model': 'claude:haiku', 'effort': None})
        for rejected in ('claude-haiku-5-5-1', 'claude-sonnet-5-5'):
            claude['models'][0]['resolvedModel'] = rejected
            with self.assertRaises(live.InfrastructureFailure): live.select_model('claude', claude)
        codex = self.catalog(); codex['models'][0]['supportedReasoningEfforts'] = [{'reasoningEffort': 'high'}]
        with self.assertRaises(live.InfrastructureFailure): live.select_model('codex', codex)

    def test_missing_subscription_or_exhausted_usage_is_infrastructure_failure(self):
        for patch in ({'connected': False}, {'usage': None}, {'usage': {'ordinaryUsageAllowed': False}}, {'usage': {'ordinaryUsageAllowed': True, 'rateLimits': None}}, {'usage': {'rateLimits': {'primary': {'usedPercent': 100}}}}, {'usage': {'rateLimits': {'primary': {'usedPercent': None}}}}):
            with self.subTest(patch=patch), self.assertRaises(live.InfrastructureFailure):
                live.select_model('codex', {**self.catalog(), **patch})

    def test_budget_is_durable_across_processes_and_exhausted_reruns(self):
        with tempfile.TemporaryDirectory() as root:
            path = Path(root) / 'budget.json'
            with ProcessPoolExecutor(max_workers=4) as pool:
                results = list(pool.map(claim, [str(path)] * 8, range(8)))
            self.assertEqual(sum(results), 3)
            self.assertEqual(len(json.loads(path.read_text())['attempts']), 3)
            with self.assertRaises(live.InfrastructureFailure): live.claim_turn(path, 'claude', 'rerun')

    def test_budget_corruption_never_refills(self):
        with tempfile.TemporaryDirectory() as root:
            path = Path(root) / 'budget.json'; path.write_text('{incomplete')
            with self.assertRaises(json.JSONDecodeError): live.claim_turn(path, 'codex', 'retry')
            self.assertEqual(path.read_text(), '{incomplete')
            path.write_text('')
            with self.assertRaises(json.JSONDecodeError): live.claim_turn(path, 'codex', 'retry')

    def test_approval_requires_exact_native_toy_file_changes(self):
        todo = Path('/fixture/todo')
        approval = {'method': 'item/fileChange/requestApproval', 'params': {'threadId': 'thread', 'turnId': 'turn', 'itemId': 'patch'}}
        native = {'threadId': 'thread', 'turnId': 'turn', 'item': {'id': 'patch', 'type': 'fileChange', 'status': 'inProgress',
            'changes': [{'path': 'todos.py', 'kind': {'type': 'update', 'move_path': None}, 'diff': '@@ -1 +1 @@\n-old\n+new'}]}}
        self.assertTrue(live.allowed_approval(approval, todo, native))
        native['item']['changes'][0]['path'] = str(todo / 'todos.py')
        self.assertTrue(live.allowed_approval(approval, todo, native))
        for path in ('../outside', '/etc/passwd', 'README.md', '', None):
            for field in ('path', 'move_path'):
                if path is None and field == 'move_path': continue
                bad = deepcopy(native)
                target = bad['item']['changes'][0]
                (target if field == 'path' else target['kind'])[field] = path
                with self.subTest(field=field, path=path):
                    self.assertFalse(live.allowed_approval(approval, todo, bad))
        for field in ('threadId', 'turnId'):
            self.assertFalse(live.allowed_approval(approval, todo, {**native, field: 'other'}))
        for field, value in (('id', 'other'), ('status', 'completed'), ('type', 'commandExecution'), ('changes', [])):
            self.assertFalse(live.allowed_approval(approval, todo, {**native, 'item': {**native['item'], field: value}}))
        for key in ('grantRoot', 'networkApprovalContext'):
            self.assertFalse(live.allowed_approval({**approval, 'params': {**approval['params'], key: 'requested'}}, todo, native))
        with tempfile.TemporaryDirectory() as root:
            root = Path(root); toy = root / 'todo'; toy.mkdir()
            (toy / 'todos.py').write_text('toy')
            alias = root / 'outside'; alias.symlink_to(toy / 'todos.py')
            bad = deepcopy(native); bad['item']['changes'][0]['path'] = str(alias)
            self.assertFalse(live.allowed_approval(approval, toy, bad), 'An outside symlink is not an allowed edit target')
            (toy / 'todos.py').unlink(); alias.unlink(); alias.write_text('outside')
            (toy / 'todos.py').symlink_to(alias)
            bad['item']['changes'][0]['path'] = str(toy / 'todos.py')
            self.assertFalse(live.allowed_approval(approval, toy, bad), 'An allowed name must not escape through a symlink')
        # A display-only patch or old inline request is insufficient evidence.
        self.assertFalse(live.allowed_approval(approval, todo, {'item': {'paths': ['todos.py']}}))
        self.assertFalse(live.allowed_approval(approval, todo, None))
        self.assertFalse(live.allowed_approval({'method': 'item/commandExecution/requestApproval', 'params': {}}, todo, native))

    def test_claude_approval_requires_its_runtime_request_for_a_toy_edit_or_the_test_run(self):
        todo = Path('/fixture/todo')
        ids = {'threadId': 'claude-thread', 'turnId': 'turn', 'itemId': 'toolu_1'}
        def request(command, kind='tool', **extra):
            params = {**ids, 'agentFamily': 'claude', 'kind': kind, 'command': command, 'cwd': str(todo), **extra}
            return {'method': 'item/commandExecution/requestApproval', 'params': dict(params)}, {'threadId': ids['threadId'], 'turnId': ids['turnId'], 'request': params}
        edit = 'Edit: ' + json.dumps({'file_path': str(todo / 'todos.py'), 'old_string': 'a', 'new_string': 'b'})
        for command, kind in ((edit, 'tool'), ('Write: ' + json.dumps({'file_path': 'test_todos.py', 'content': 'x'}), 'tool'),
                              ('python3 -m unittest', 'command'), ('python3 -m unittest -v', 'command'),
                              # Claude merges the test output streams (live run 2026-10-25, #343).
                              ('python3 -m unittest 2>&1', 'command')):
            with self.subTest(command=command):
                approval, native = request(command, kind)
                self.assertTrue(live.allowed_approval(approval, todo, native))
        for command, kind in (('Edit: ' + json.dumps({'file_path': '/etc/passwd', 'old_string': 'a', 'new_string': 'b'}), 'tool'),
                              ('Edit: ' + json.dumps({'file_path': 'todos.py', 'old_string': 'a', 'new_string': 'b', 'extra': 1}), 'tool'),
                              ('Bash: ' + json.dumps({'command': 'rm -rf /'}), 'tool'), ('Edit: not json', 'tool'),
                              ('python3 -m unittest; curl example.com', 'command'), ('cat todos.py > x', 'command'),
                              ('python3 -m unittest 2>x', 'command'), ('python3 -m unittest > x 2>&1', 'command'),
                              ('python3 -m unittest 2>&1 | tee x', 'command'), ('python3 -m unittest 2>&1 && rm todos.py', 'command')):
            with self.subTest(command=command):
                approval, native = request(command, kind)
                self.assertFalse(live.allowed_approval(approval, todo, native))
        approval, native = request(edit)
        self.assertFalse(live.allowed_approval({**approval, 'params': {**approval['params'], 'itemId': 'other'}}, todo, native))
        self.assertFalse(live.allowed_approval(approval, todo, {**native, 'request': {**native['request'], 'agentFamily': 'codex'}}))
        outside, outside_native = request('python3 -m unittest', 'command', reason='Run this command outside the sandbox, with full access?')
        self.assertFalse(live.allowed_approval(outside, todo, outside_native))
        self.assertFalse(live.allowed_approval(approval, todo, None), 'The host list alone is not evidence')

    def test_claude_fixture_access_asks_for_toy_edits_instead_of_denying_them(self):
        # Ask the real Claude runtime policy what the reserved access does with
        # an Edit to a toy file; Read only denied it before any approval (#343).
        if not shutil.which('node'): self.skipTest('node unavailable')
        body = live.reservation_body('claude', {'model': 'm'}, 'device', 'client')
        self.assertEqual(live.reservation_body('codex', {'model': 'm'}, 'device', 'client')['accessMode'], 'read_only')
        policy = (Path(__file__).resolve().parents[1] / 'services/claude-runtime/permissions.mjs').as_uri()
        with tempfile.TemporaryDirectory() as root:
            todo = Path(root).resolve()
            (todo / 'todos.py').write_text('x\n')
            script = ('const { ToolPolicy } = await import(process.argv[1]); const [mode, approvalMode, todo] = process.argv.slice(2);'
                      'const policy = new ToolPolicy({ cwd: todo, mode, approvalMode, readRoots: [todo], writeRoots: [todo], deniedRoots: [] });'
                      'console.log(await policy.decision("Edit", { file_path: todo + "/todos.py", old_string: "x", new_string: "y" }));')
            decision = subprocess.run(['node', '--input-type=module', '-e', script, policy, body['accessMode'], body['claudeApproval'], str(todo)],
                                      capture_output=True, text=True, check=True).stdout.strip()
        self.assertEqual(decision, 'ask')

    def test_native_observer_preserves_transport_and_complete_patch_scope(self):
        with tempfile.TemporaryDirectory() as root:
            root = Path(root)
            binary = root / 'native-fixture'
            binary.write_text('#!' + sys.executable + '\n' + r'''import json, sys
from pathlib import Path
root = Path(__file__).parent
(root / 'input').write_bytes(sys.stdin.buffer.read())
sys.stdout.buffer.write((root / 'output').read_bytes())
''')
            binary.chmod(0o700)
            wrapper, reader = live.native_change_observer(root, binary)
            approval = {'method': 'item/fileChange/requestApproval', 'params': {'threadId': 'thread', 'turnId': 'turn', 'itemId': 'patch'}}
            self.assertIsNone(reader(approval))
            native_item = {'id': 'patch', 'type': 'fileChange', 'status': 'inProgress',
                'changes': [{'path': '/fixture/todo/todos.py', 'kind': {'type': 'update', 'move_path': '/outside'}, 'diff': 'patch'}]}
            started = {'method': 'item/started', 'params': {'threadId': 'thread', 'turnId': 'turn', 'item': native_item}}
            frames = [{'id': 1, 'result': {}}, started, {'id': 4, **approval}]
            output = ''.join(json.dumps(frame) + '\n' for frame in frames).encode()
            (root / 'output').write_bytes(output)
            request = b'{"id":1,"method":"initialize"}\n'
            result = subprocess.run([str(wrapper), 'app-server', '--stdio'], input=request, capture_output=True, timeout=30)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout, output)
            self.assertEqual((root / 'input').read_bytes(), request)
            native = reader(approval)
            self.assertEqual(native['item'], native_item)
            self.assertFalse(live.allowed_approval(approval, Path('/fixture/todo'), native))
            changes = deepcopy(native_item['changes']); changes[0]['kind']['move_path'] = None
            update = {'method': 'item/fileChange/patchUpdated', 'params': {**approval['params'], 'changes': changes}}
            (root / 'output').write_text(json.dumps(update) + '\n')
            subprocess.run([str(wrapper), 'app-server', '--stdio'], input=b'', capture_output=True, timeout=30, check=True)
            self.assertTrue(live.allowed_approval(approval, Path('/fixture/todo'), reader(approval)))
            completed = deepcopy(started); completed['method'] = 'item/completed'; completed['params']['item']['status'] = 'completed'
            (root / 'output').write_text(json.dumps(completed) + '\n')
            subprocess.run([str(wrapper), 'app-server', '--stdio'], input=b'', capture_output=True, timeout=30, check=True)
            self.assertFalse(live.allowed_approval(approval, Path('/fixture/todo'), reader(approval)))
            self.assertIsNone(reader({**approval, 'params': {**approval['params'], 'turnId': 'wrong'}}))
            (root / 'output').write_bytes(b'x' * (1024 * 1024 + 1))
            result = subprocess.run([str(wrapper), 'app-server', '--stdio'], input=b'', capture_output=True, timeout=30)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn(b'exceeded 1 MiB', result.stderr)

    def test_claude_edit_request_survives_its_item_events(self):
        # Live order (2026-10-25): Claude starts the edit item, asks, then completes it.
        with tempfile.TemporaryDirectory() as root:
            root = Path(root)
            binary = root / 'native-fixture'
            binary.write_text('#!' + sys.executable + '\nimport sys\nfrom pathlib import Path\n'
                              'sys.stdout.buffer.write((Path(__file__).parent / "output").read_bytes())\n')
            binary.chmod(0o700)
            wrapper, reader = live.native_change_observer(root, binary, 'node')
            ids = {'threadId': 'claude-thread', 'turnId': 'turn'}
            request = {**ids, 'itemId': 'toolu_1', 'agentFamily': 'claude', 'kind': 'tool', 'cwd': '/fixture/todo',
                       'command': 'Edit: ' + json.dumps({'file_path': '/fixture/todo/todos.py', 'old_string': 'a', 'new_string': 'b'})}
            item = {'id': 'toolu_1', 'type': 'fileChange', 'status': 'inProgress',
                    'changes': [{'path': '/fixture/todo/todos.py', 'kind': {'type': 'update'}, 'diff': 'patch'}]}
            frames = [{'method': 'item/started', 'params': {**ids, 'item': item, 'agentFamily': 'claude'}},
                      {'id': 4, 'method': 'item/commandExecution/requestApproval', 'params': request},
                      {'method': 'item/completed', 'params': {**ids, 'item': {**item, 'status': 'completed'}, 'agentFamily': 'claude'}}]
            (root / 'output').write_text(''.join(json.dumps(frame) + '\n' for frame in frames))
            subprocess.run([str(wrapper)], input=b'', capture_output=True, timeout=30, check=True)
            approval = {'method': 'item/commandExecution/requestApproval', 'params': dict(request)}
            self.assertEqual(reader(approval), {**ids, 'request': request})
            self.assertTrue(live.allowed_approval(approval, Path('/fixture/todo'), reader(approval)))

    def test_reset_restores_owned_repo_without_touching_neighbor(self):
        with tempfile.TemporaryDirectory() as root, patch.dict(os.environ):
            todo = Path(root) / 'todo'; todo.mkdir()
            neighbor = Path(root) / 'keep'; neighbor.write_text('keep')
            config = Path(root) / 'global.gitconfig'
            config.write_text('[commit]\n gpgSign = true\n[gpg]\n program = /usr/bin/false\n'
                              '[core]\n fsmonitor = /usr/bin/false\n')
            os.environ['GIT_CONFIG_GLOBAL'] = str(config)
            live.run('git', 'init', '-q', str(todo))
            file = todo / 'todos.py'; file.write_text('baseline')
            live.run('git', '-C', str(todo), 'add', '.')
            live.run('git', '-C', str(todo), '-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid',
                     'commit', '-qm', 'baseline', env=dict(os.environ, GIT_AUTHOR_DATE='2026-01-01T00:00:00Z'))
            baseline = live.run('git', '-C', str(todo), 'rev-parse', 'HEAD')
            self.assertEqual(live.run('git', '-C', str(todo), 'log', '-1', '--format=%at'), '1767225600')
            file.write_text('changed'); (todo / 'generated').write_text('remove')
            live.reset_todo(todo, baseline)
            self.assertEqual(file.read_text(), 'baseline')
            self.assertFalse((todo / 'generated').exists())
            self.assertEqual(neighbor.read_text(), 'keep')

    def test_stop_waits_for_stream_from_exact_conversation_and_turn(self):
        receipt = {'conversationId': 'owned', 'turnId': 'turn', 'event': {'type': 'message_state', 'data': {'state': 'accepted_by_codex'}}}
        delta = {'conversationId': 'owned', 'turnId': 'turn', 'event': {'type': 'assistant_delta', 'data': {'text': 'Starting'}}}
        self.assertFalse(live.reply_started([receipt], 'owned', 'turn'))
        self.assertFalse(live.reply_started([delta], 'other', 'turn'))
        self.assertFalse(live.reply_started([delta], 'owned', 'previous'))
        self.assertTrue(live.reply_started([receipt, delta], 'owned', 'turn'))

    def test_explicit_fourth_slot_preserves_prior_attempts_and_never_allows_fifth(self):
        with tempfile.TemporaryDirectory() as root:
            path = Path(root) / 'budget.json'
            for i in range(3): live.claim_turn(path, 'codex', str(i))
            self.assertEqual(live.claim_turn(path, 'codex', 'fourth', cap=4), 4)
            self.assertEqual([a['requestId'] for a in json.loads(path.read_text())['attempts']], ['0', '1', '2', 'fourth'])
            with self.assertRaises(live.InfrastructureFailure): live.claim_turn(path, 'codex', 'fifth', cap=4)

    def test_queue_placeholder_is_not_a_runtime_stop_target(self):
        placeholder = {'id': 'local:pending-message', 'status': 'inProgress'}
        runtime = {'id': 'provider-turn', 'status': 'inProgress'}
        self.assertEqual(live.provider_turns({'thread': {'turns': [placeholder]}}), [])
        self.assertEqual(live.provider_turns({'thread': {'turns': [placeholder, runtime]}}), [runtime])

    def test_private_state_rejects_symlink_escape(self):
        with tempfile.TemporaryDirectory() as root:
            root = Path(root); local = root / '.local'; local.mkdir()
            link = local / 'link'; link.symlink_to(root, target_is_directory=True)
            with self.assertRaises(ValueError): live.private(link / 'state')
            self.assertEqual(live.private(local / 'state'), (local / 'state').resolve())

    def test_cleanup_deadline_interrupts_stalled_http_and_preserves_ownership(self):
        with socket.socket() as listener:
            listener.bind(('127.0.0.1', 0)); listener.listen()
            finished = threading.Event()
            def stall():
                with listener.accept()[0] as connection:
                    connection.recv(8192)
                    finished.wait(2)
            worker = threading.Thread(target=stall); worker.start()
            probe = live.http.Probe(listener.getsockname()[1])
            outcomes = []
            try:
                start = time.monotonic()
                errors = live.cleanup_threads(probe, ['owned-only'], outcomes, seconds=.1)
                self.assertLess(time.monotonic() - start, 1)
                self.assertEqual(outcomes, [])
                self.assertEqual([e['conversationId'] for e in errors], ['owned-only'])
                self.assertIn('deadline', errors[0]['error'])
            finally:
                finished.set(); worker.join(timeout=10)
            self.assertFalse(worker.is_alive())

    def test_later_failure_preserves_completed_scenario_and_attempt_count(self):
        class Probe:
            session = {'deviceId': 'offline'}
            calls = []
            def call(self, path, body=None):
                self.calls.append(path)
                if path == '/api/v1/agent-options':
                    if len(self.calls) == 1: return {'models': []}
                    return {'models': [{'id': 'fixture-model', 'agentFamily': 'claude' if len(self.calls) == 2 else 'codex'}]}
                if path == '/api/v1/projects': return {'id': 'owned-project'}
                return {'conversation': {'conversationId': body['clientMessageId']}}
        with tempfile.TemporaryDirectory() as root:
            output = Path(root)
            report = {'providers': {'codex': {}}, 'modelTurnAttempts': 0}
            with patch.object(live, 'reset_todo'), patch.object(live, 'run', return_value='1\t0\ttodos.py'), patch.object(live, 'cleanup_threads', return_value=[]), patch.object(live, 'observe', side_effect=[{'status': 'completed'}, live.InfrastructureFailure('offline disconnect')]):
                with self.assertRaisesRegex(live.InfrastructureFailure, 'offline disconnect'):
                    live.exercise(Probe(), None, output, output / 'budget.json', 'codex', {'model': 'fixture-model'}, output, 'baseline', [], report, 'all', None)
            self.assertEqual(Probe.calls[:4], ['/api/v1/agent-options'] * 3 + ['/api/v1/projects'])
            saved = json.loads((output / 'report.json').read_text())
            self.assertEqual(saved['modelTurnAttempts'], 2)
            self.assertEqual([check['result'] for check in saved['providers']['codex']['checks']], ['passed', 'incomplete'])
            self.assertEqual(len(json.loads((output / 'owned-threads.json').read_text())), 2)

    def test_interrupted_archive_cleanup_still_stops_daemon_and_removes_auth(self):
        with tempfile.TemporaryDirectory() as root:
            root = Path(root); output = root / '.local/run'; output.mkdir(parents=True)
            home = root / 'home'; (home / '.codex').mkdir(parents=True)
            (home / '.codex/auth.json').write_text('offline credential')
            (output / 'daemon').mkdir(); (output / 'daemon/wonder.sqlite3').touch()
            (output / 'owned-threads.json').write_text('["12345678-1234-1234-1234-123456789abc"]')
            daemon = root / 'offline-daemon'; daemon.write_text('#!/bin/sh\nexec sleep 30\n'); daemon.chmod(0o700)
            argv = ['run-live-fixtures.py', '--recover', '--output', str(output), '--budget', str(output / 'budget.json'), '--daemon', str(daemon), '--codex', str(root / 'unused'), '--public-checkout', str(root / 'unused')]
            processes = []; spawn = subprocess.Popen
            def launch(*args, **kwargs):
                process = spawn(*args, **kwargs); processes.append(process); return process
            with patch.object(sys, 'argv', argv), patch.dict(os.environ, HOME=str(home)), patch.object(live.subprocess, 'Popen', side_effect=launch), patch.object(live.http, 'eventually', side_effect=live.InfrastructureFailure('offline startup failure')), patch.object(live, 'cleanup_threads', side_effect=KeyboardInterrupt), patch('builtins.print'):
                self.assertEqual(live.main(), 1)
            self.assertEqual(len(processes), 1)
            self.assertIsNotNone(processes[0].poll())
            self.assertFalse((output / 'codex-home/auth.json').exists())
            self.assertEqual((home / '.codex/auth.json').read_text(), 'offline credential')
            report = json.loads((output / 'recovery-report.json').read_text())
            self.assertEqual(report['modelTurnAttempts'], 0)
            self.assertEqual(report['cleanupErrors'][0]['step'], 'archive owned threads')


if __name__ == '__main__': unittest.main()

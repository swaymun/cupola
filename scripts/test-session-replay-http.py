#!/usr/bin/env python3
"""Bounded, paired-device HTTP acceptance against an isolated replay daemon.

Requires Python jsonschema and cryptography. Takes validated synthetic bundles,
never a provider home or owner database. Run after building wonderd; no models.
"""
import argparse
from collections import defaultdict
import base64
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import platform
import secrets
import shlex
import signal
import socket
import subprocess
import sys
import threading
import time
from types import SimpleNamespace
import urllib.error
import urllib.parse
import urllib.request
import uuid
from cryptography.hazmat.primitives import hashes
from cryptography.hazmat.primitives.asymmetric import ec, utils

spec = importlib.util.spec_from_file_location('replay', Path(__file__).with_name('replay-session-fixture.py'))
replay = importlib.util.module_from_spec(spec)
spec.loader.exec_module(replay)


def compact(value):
    return json.dumps(value, separators=(',', ':'), ensure_ascii=False).encode()


def b64(value):
    return base64.urlsafe_b64encode(value).decode().rstrip('=')


class Probe:
    def __init__(self, port, origin='https://replay.invalid'):
        self.address = f'http://127.0.0.1:{port}'
        parsed = urllib.parse.urlsplit(origin)
        if parsed.scheme != 'https' or not parsed.hostname or parsed.username or parsed.password or parsed.path or parsed.query or parsed.fragment:
            raise ValueError('Use an HTTPS origin without a path or credentials')
        self.origin = origin
        self.capability = secrets.token_urlsafe(32)
        self.private = ec.generate_private_key(ec.SECP256R1())
        self.session = None
        self.requests = []

    def sign(self, fields):
        der = self.private.sign('\n'.join(map(str, fields)).encode(), ec.ECDSA(hashes.SHA256()))
        r, s = utils.decode_dss_signature(der)
        return b64(r.to_bytes(32, 'big') + s.to_bytes(32, 'big'))

    def call(self, path, body=None, method=None, *, local=False, public=False, binary=False, expected=None):
        headers = {'Origin': self.origin, 'Host': urllib.parse.urlsplit(self.origin).netloc, 'x-wonder-history-view': 'compact'}
        if local:
            headers['x-wonder-loopback-capability'] = self.capability
        elif not public and self.session:
            headers['Cookie'] = '__Host-wonder_session=' + self.session['sessionToken']
            headers['x-wonder-csrf'] = self.session['csrfToken']
        if body is not None:
            headers['Content-Type'] = 'application/json'
        request = urllib.request.Request(self.address + path, data=compact(body) if body is not None else None,
                                         headers=headers, method=method)
        try:
            response = urllib.request.urlopen(request, timeout=30)
        except urllib.error.HTTPError as error:
            response = error
        raw = response.read(128 * 1024 * 1024)
        self.requests.append({'method': request.get_method(), 'path': path.split('?')[0], 'status': response.status})
        if expected is not None:
            assert response.status == expected, (path, response.status, raw[:400])
        elif not 200 <= response.status < 300:
            raise AssertionError((path, response.status, raw[:400]))
        if binary:
            return raw, {key.lower(): value for key, value in response.headers.items()}
        return json.loads(raw) if raw and response.headers.get_content_type() == 'application/json' else raw.decode()

    def pair(self):
        public = self.private.public_key().public_numbers()
        jwk = {'kty': 'EC', 'crv': 'P-256', 'x': b64(public.x.to_bytes(32, 'big')), 'y': b64(public.y.to_bytes(32, 'big'))}
        offer = self.call('/api/v1/pairing/offers', {}, local=True)
        claimed = self.call('/api/v1/pairing/code', {'humanCode': offer['humanCode'], 'publicKey': jwk,
                                                   'label': 'Synthetic replay HTTP', 'sessionExpiration': '1h'}, public=True)
        challenge = claimed['challenge']
        fields = ['wonder-session-v1'] + [challenge[k] for k in ('deviceId', 'challengeId', 'nonce', 'origin',
                                                                  'hostInstallationId', 'issuedAtMs', 'expiresAtMs')]
        # This probe is the synthetic phone: it owns the claim and signing key.
        # Derive its text from that claim, never from host pending metadata.
        code = hashlib.sha256('\n'.join(map(str, fields)).encode()).hexdigest()[:12].upper()
        confirm = '/api/v1/pairing/pending/' + claimed['deviceId'] + '/confirm'
        self.call(confirm, {}, local=True, expected=400)
        self.call(confirm, {'code': 'not-the-phone-code'}, local=True, expected=403)
        self.call(confirm, {'code': code}, local=True)
        self.session = self.call('/api/v1/pairing/session', {'challengeId': challenge['challengeId'],
                                                     'signature': self.sign(fields)}, public=True)

    def resolve(self, approval):
        path = '/api/v1/approvals/' + approval['approvalId'] + '/resolve'
        body = {'decision': 'accept', 'actionNonce': approval['actionNonce'], 'expectedState': 'pending',
                'idempotencyKey': str(uuid.uuid4()), 'issuedAtMs': int(time.time() * 1000)}
        digest = hashlib.sha256(compact([body[k] for k in ('decision', 'actionNonce', 'expectedState', 'idempotencyKey')] + [''])).hexdigest()
        fields = ['wonder-action-v1', 'approval.resolve', path, digest, body['actionNonce'], self.session['csrfToken'],
                  self.session['deviceId'], self.session['hostInstallationId'], body['issuedAtMs'], 'pending']
        body['signature'] = self.sign(fields)
        self.call(path, body)


def eventually(action, description, seconds=40):
    # Check once more after the deadline: a process stalled past it (a loaded
    # Mac) must still see a condition that became true meanwhile.
    deadline = time.monotonic() + seconds
    while True:
        expired = time.monotonic() >= deadline
        value = action()
        if value:
            return value
        if expired:
            raise AssertionError('Timed out: ' + description)
        time.sleep(.05)


def attach_provider(probe, prepared, provider):
    config = json.loads((prepared / provider / 'config.json').read_text())
    workspace = prepared / provider / 'workspace'
    bundle = replay.read_prepared(prepared / provider, config)
    project = probe.call('/api/v1/projects', {'requestId': str(uuid.uuid4()), 'name': provider.title() + ' synthetic replay',
                                            'folders': [str(workspace)], 'primaryIndex': 0})
    project_id = project['id']
    def discovery():
        catalog = probe.call(f'/api/v1/projects/{project_id}/threads')
        return next((r for r in catalog['threads'] if r['reference'] == provider + ':' + config['nativeSessionId']), None)
    row = eventually(discovery, provider + ' catalog')
    attached = probe.call(f'/api/v1/projects/{project_id}/threads/attach', {'reference': row['reference']})
    ident = attached['conversationId']
    base = f'/api/v1/conversations/{ident}'
    detail = probe.call(f'/api/v1/project-conversations/{ident}')
    assert detail['family'] == provider and detail['folderInProject'] and detail['hasNativeSession']
    def refreshed():
        status = probe.call(base + '/history/refresh')
        assert status.get('state') != 'failed', status
        return status if status.get('state') == 'completed' else None
    eventually(refreshed, provider + ' history hydration', 60)
    return config, bundle, project_id, ident


def check_provider(probe, prepared, provider):
    config, bundle, project_id, ident = attach_provider(probe, prepared, provider)
    base = f'/api/v1/conversations/{ident}'
    # Snapshot paging can continue importing earlier native turns on demand.
    pages, turns, cursor = 0, {}, None
    for _ in range(200):
        query = {'limit': 100}
        if cursor:
            query['before'] = cursor
        snapshot = probe.call(base + '/history?' + urllib.parse.urlencode(query))
        page = snapshot['thread']
        for turn in page['turns']:
            saved = turns.setdefault(turn['id'], {})
            for item in turn['items']:
                assert item['id'] not in saved, 'duplicate Project history item'
                saved[item['id']] = item
        pages += 1
        cursor = page['nextCursor']
        if not cursor:
            break
    expected = bundle['threadRead']['thread']['turns'][:config['historyTurns']]
    assert len(turns) == len(expected), (provider, 'history count', len(turns), len(expected))
    expected_items = sum(len(t['items']) for t in expected)
    assert sum(len(t) for t in turns.values()) == expected_items, 'history items lost'
    for turn in expected:
        assert set(turns[turn['id']]) == {item['id'] for item in turn['items']}, 'history item identities changed'
        for item in turn['items']:
            if item['type'] == 'agentMessage':
                actual = turns[turn['id']][item['id']]['payload']['text']
                assert actual == item['text'], (provider, 'history reply text changed', item['id'],
                                                'expectedChars', len(item['text']), 'actualChars', len(actual))
    media = []
    for asset in bundle['assets']:
        uploaded = probe.call(base + '/files', {'clientUploadId': str(uuid.uuid4()), 'name': asset['name'],
                                               'mimeType': asset['mimeType'], 'contentBase64': asset['base64']})
        path = base + '/files/' + uploaded['id']
        content, headers = probe.call(path, binary=True)
        assert hashlib.sha256(content).hexdigest() == asset['sha256'], 'download integrity mismatch'
        assert headers.get('content-type', '').split(';')[0] == asset['mimeType'], 'download MIME mismatch'
        probe.call(path, public=True, expected=401)
        media.append({'name': asset['name'], 'mime': asset['mimeType'], 'bytes': len(content), 'sha256': asset['sha256']})
    approvals = 0
    statuses = []
    for stop in (False, True):
        message = {'deviceId': probe.session['deviceId'], 'clientMessageId': str(uuid.uuid4()), 'body': replay.TRIGGER}
        receipt = probe.call(base + '/messages', message)
        assert receipt['clientMessageId'] == message['clientMessageId'], 'receipt identity changed'
        turn_index = config['historyTurns'] + int(stop)
        turn_id = bundle['threadRead']['thread']['turns'][turn_index]['id']
        answered = set()
        stopped = False
        def completed():
            nonlocal approvals, stopped
            pending = [a for a in probe.call('/api/v1/approvals') if a['conversationId'] == ident]
            if stop and pending and answered and not stopped:
                probe.call(base + '/turns/' + turn_id + '/interrupt', {})
                stopped = True
            for approval in (pending[:1] if stop and not answered else pending if not stop else []):
                if approval['approvalId'] not in answered:
                    probe.resolve(approval)
                    answered.add(approval['approvalId'])
                    approvals += 1
            snapshot = probe.call(base + '/history?limit=5')
            turn = next((t for t in snapshot['thread']['turns'] if t['id'] == turn_id), None)
            if turn and turn['status'] in ('completed', 'interrupted', 'failed'):
                return turn
            for saved in snapshot['messages']:
                if saved.get('clientMessageId') == message['clientMessageId'] and saved.get('state') == 'failed':
                    raise AssertionError(('dispatch failed', saved))
            return None
        terminal = eventually(completed, provider + ' replay terminal', 50)
        wanted = 'interrupted' if stop else 'completed'
        assert terminal['status'] == wanted, (provider, terminal['status'], wanted)
        statuses.append(wanted)
    assert approvals > 0, 'no signed approval completed'
    children = probe.call(f'/api/v1/project-conversations/{ident}/subagents')
    assert children['available'] and len(children['subagents']) == 1, children
    child_id = children['subagents'][0]['threadId']
    child = probe.call(f'/api/v1/project-conversations/{ident}/subagents/{child_id}/transcript')
    child_items = sum(len(t['items']) for t in child['snapshot']['thread']['turns'])
    assert child_items > 0, 'empty child transcript'
    return {'provider': provider, 'projectHistoryTurns': len(turns), 'historyItems': expected_items, 'historyPages': pages,
            'signedApprovals': approvals, 'terminalStatuses': statuses, 'media': media, 'childTranscripts': 1, 'childTranscriptItems': child_items,
            'modelCalls': 0, 'nativeUi': 'not checked', 'speed': config['speed']}


def serve_native(probe, prepared, output, process, seconds, report):
    # Use the same Project admission as the HTTP checks; the phone pairs itself.
    projects = []
    for provider in ('codex', 'claude'):
        config, bundle, project, conversation = attach_provider(probe, prepared, provider)
        files = []
        for asset in bundle['assets']:
            uploaded = probe.call(f'/api/v1/conversations/{conversation}/files',
                                  {'clientUploadId': str(uuid.uuid4()), 'name': asset['name'],
                                   'mimeType': asset['mimeType'], 'contentBase64': asset['base64']})
            files.append(uploaded['id'])
        projects.append({'provider': provider, 'projectId': project, 'conversationId': conversation,
                         'historyTurns': config['historyTurns'], 'speed': config['speed'], 'files': files})
    control = {'origin': probe.origin, 'loopbackAddress': probe.address, 'pid': process.pid,
               'capability': probe.capability, 'hostId': probe.session['hostInstallationId'], 'projects': projects}
    (output / 'control.json').write_text(json.dumps(control, indent=2))
    (output / 'pairing-offer.json').write_text(json.dumps(probe.call('/api/v1/pairing/offers', {}, local=True), indent=2))
    ready = {'origin': probe.origin, 'loopbackAddress': probe.address, 'projects': projects, 'modelCalls': 0}
    (output / 'ready.json').write_text(json.dumps(ready, indent=2))
    report.update(nativeHost=ready, nativeUi='Not checked by the host runner')
    print(f'Native replay ready. Private pairing offer: {output / "pairing-offer.json"}', flush=True)
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise AssertionError('Native replay daemon exited before its serving deadline')
        time.sleep(min(.25, max(0, deadline - time.monotonic())))
    report['result'] = 'served'


def benchmark(probe, prepared, count):
    """Measure the paired loopback path, not phone, tailnet or model latency."""
    def load(name, filename):
        spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(filename))
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        return module
    live = load('live_benchmark_events', 'run-live-fixtures.py')
    analysis = load('diagnostic_distributions', 'analyze-ios-diagnostics.py')
    rows, streams, cold, sources = [], [], [], []
    def measured(provider, path, action):
        start = time.monotonic_ns()
        result = action()
        rows.append({'provider': provider, 'path': path, 'durationMs': (time.monotonic_ns() - start) / 1e6})
        return result
    for provider in ('codex', 'claude'):
        start = time.monotonic_ns()
        config, bundle, project, ident = attach_provider(probe, prepared, provider)
        sources.append({'provider': provider, 'bundleSha256': config['bundleSha256'],
                        'historyTurns': config['historyTurns'], 'replaySpeed': config['speed']})
        cold.append({'provider': provider, 'path': 'attachAndHydrateCold',
                     'durationMs': (time.monotonic_ns() - start) / 1e6})
        if len(bundle['threadRead']['thread']['turns']) - config['historyTurns'] < count:
            raise ValueError('Benchmark needs one reserved synthetic turn per sample')
        base = '/api/v1/conversations/' + ident
        first = probe.call(base + '/history?limit=100')
        cursor = first['thread']['nextCursor']
        assert cursor, 'History benchmark requires a real older page'
        paths = {
            'chatsCatalog': '/api/v1/projects',
            'projectThreads': f'/api/v1/projects/{project}/threads?limit=10',
            'threadDetail': '/api/v1/project-conversations/' + ident,
            'threadHistory': base + '/history?limit=100',
            'historyOlderPage': base + '/history?' + urllib.parse.urlencode({'limit': 100, 'before': cursor}),
        }
        # Hydration above is cold. All repeated reads below are explicitly warm.
        for _ in range(count):
            for name, path in paths.items():
                value = measured(provider, name, lambda path=path: probe.call(path))
                assert value, 'Empty benchmark response'
        for index in range(count):
            turn = bundle['threadRead']['thread']['turns'][config['historyTurns'] + index]['id']
            checkpoint = probe.call('/api/v1/sync/checkpoint')
            events = live.Events(probe, checkpoint)
            received, errors, stop = [], [], threading.Event()
            def receive():
                try:
                    while not stop.is_set():
                        if batch := events.read():
                            received.append(batch)
                except BaseException as error:
                    errors.append(error)
            reader = threading.Thread(target=receive, daemon=True)
            reader.start()
            start = time.monotonic_ns()
            try:
                message = {'deviceId': probe.session['deviceId'], 'clientMessageId': str(uuid.uuid4()), 'body': replay.TRIGGER}
                receipt = probe.call(base + '/messages', message)
                acknowledged = time.monotonic_ns()
                assert receipt['clientMessageId'] == message['clientMessageId']
                answered = set()
                def completed():
                    if errors:
                        raise errors[0]
                    for approval in probe.call('/api/v1/approvals'):
                        if approval['conversationId'] == ident and approval['approvalId'] not in answered:
                            probe.resolve(approval)
                            answered.add(approval['approvalId'])
                    snapshot = probe.call(base + '/history?limit=5')
                    for item in snapshot.get('messages', []):
                        if item.get('clientMessageId') == message['clientMessageId'] and item.get('state') == 'failed':
                            raise AssertionError('Synthetic dispatch failed')
                    return next((t for t in snapshot['thread']['turns'] if t['id'] == turn
                                 and t['status'] in ('completed', 'interrupted', 'failed')), None)
                terminal = eventually(completed, provider + ' benchmark turn', 60)
                expected = bundle['threadRead']['thread']['turns'][config['historyTurns'] + index]['status']
                assert terminal['status'] == expected, (terminal['status'], expected)
                def first_delta():
                    if errors:
                        raise errors[0]
                    return next((stamp for stamp, frames in received[:] for frame in frames
                                 if frame.get('conversationId') == ident and frame.get('turnId') == turn
                                 and frame['event']['type'] == 'assistant_delta'
                                 and frame['event']['data'].get('text')), None)
                first = eventually(first_delta, provider + ' first streamed fixture text', 5)
                streams.append({'provider': provider, 'turnId': turn, 'sendNs': start,
                                'receiptNs': acknowledged, 'firstDeltaNs': first})
                rows.extend([{'provider': provider, 'path': name, 'durationMs': (end - start) / 1e6}
                             for name, end in [('sendReceipt', acknowledged), ('sendToFirstFixtureDelta', first)]])
            finally:
                stop.set()
                reader.join(timeout=2)
                events.close()
                reader.join(timeout=2)
                if reader.is_alive():
                    raise RuntimeError('Benchmark event reader did not stop')
            # Reconnect with the actual pre-turn checkpoint; a successful socket
            # upgrade alone cannot prove replay reached the paired client.
            reconnect = time.monotonic_ns()
            events = live.Events(probe, checkpoint)
            try:
                def resumed():
                    if batch := events.read():
                        stamp, frames = batch
                        if any(f.get('conversationId') == ident and f.get('turnId') == turn for f in frames):
                            return stamp
                restored = eventually(resumed, provider + ' checkpoint replay', 5)
                rows.append({'provider': provider, 'path': 'reconnectToReplayedTurn',
                             'durationMs': (restored - reconnect) / 1e6})
            finally:
                events.close()
            print(f'{provider}: synthetic benchmark {index + 1}/{count}', flush=True)
    boundaries = {}
    for path in prepared.glob('*/runtime-timing/*.jsonl'):
        for line in path.read_text().splitlines():
            event = json.loads(line)
            boundaries.setdefault((event['provider'], event['turnId']), {})[event['event']] = event['monotonicNs']
    for stream in streams:
        stamps = boundaries[(stream['provider'], stream['turnId'])]
        start, runtime, emitted, received = stream['sendNs'], stamps['turn_start'], stamps['first_delta'], stream['firstDeltaNs']
        assert start <= runtime <= emitted <= received, 'Cross-process timing boundaries are inconsistent'
        for name, left, right in [('clientToRuntimeAdmission', start, runtime),
                                  ('syntheticRuntimeToFirstDelta', runtime, emitted),
                                  ('runtimeThroughDaemonToClient', emitted, received)]:
            rows.append({'provider': stream['provider'], 'path': name, 'durationMs': (right - left) / 1e6})
        stream.update(runtimeAdmittedNs=runtime, runtimeFirstDeltaNs=emitted)
    groups = defaultdict(list)
    for row in rows:
        groups[(row['provider'], row['path'])].append(row['durationMs'])
    return {'samplesPerPath': count, 'metrics': [dict(provider=p, path=n, **analysis.distribution(v))
                for (p, n), v in sorted(groups.items())], 'coldHydration': cold, 'samples': rows, 'streamBoundaries': streams,
            'fixtures': sources, 'environment': {'platform': platform.platform(), 'python': platform.python_version()},
            'modelCalls': 0, 'limitations': [
                'Python paired client on loopback, not iOS rendering, TLS, Tailscale or a relay.',
                'Synthetic runtime; first text timing includes accelerated replay and approval waits, not model inference.',
                'Forward and return phases combine daemon work with local transport; no unmeasured per-hop subtraction.',
                'Two timing records per replay turn add fixture-only file I/O. Production has no new telemetry.',
                'Cold hydration includes Project creation/discovery/attach and polling, one sample per provider.'
            ]}


def main():
    os.umask(0o077)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--daemon', type=Path, required=True)
    parser.add_argument('--host', type=Path, help='Exercise the experimental headless supervisor around the isolated daemon')
    parser.add_argument('--computer', action='store_true', help='Enable the Linux X11 helper during --host --serve-native (requires helper dependencies in this Python)')
    parser.add_argument('--bundles', type=Path)
    parser.add_argument('--prepared', type=Path, help='Reuse private prepared launchers; each check still uses a fresh daemon database')
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--serve-native', metavar='HTTPS_ORIGIN', help='Serve original-paced fixtures for normal native pairing; configure Tailscale Serve separately')
    parser.add_argument('--port', type=int, default=0, help='Loopback port for native serving; 0 selects a free port')
    parser.add_argument('--serve-seconds', type=int, default=1800, help='Native host lifetime after preparation, 1–7200 seconds')
    parser.add_argument('--benchmark-samples', type=int, default=0, help='1–50 bounded paired-loopback samples per provider; synthetic replay only')
    args = parser.parse_args()
    if args.computer and (not args.host or not args.serve_native or sys.platform != 'linux'):
        parser.error('--computer requires Linux --host --serve-native')
    if not 0 <= args.benchmark_samples <= 50 or (args.benchmark_samples and args.serve_native):
        parser.error('Benchmark uses 1–50 samples and cannot serve a native host')
    if args.benchmark_samples and not args.bundles:
        parser.error('Benchmark requires --bundles to prepare fresh synthetic launchers with timing enabled')
    if not 0 <= args.port <= 65535 or not 1 <= args.serve_seconds <= 7200:
        parser.error('Use a valid loopback port and a serving duration from 1 to 7200 seconds')
    if not args.serve_native and (args.port or args.serve_seconds != 1800):
        parser.error('--port and --serve-seconds require --serve-native')
    output = args.output.absolute()
    if '.local' not in output.parts or output.exists() or output.is_symlink():
        parser.error('Use a new directory under .local')
    output.mkdir(parents=True, mode=0o700)
    if bool(args.bundles) == bool(args.prepared):
        parser.error('Choose --bundles or --prepared')
    prepared = args.prepared.resolve() if args.prepared else output
    if '.local' not in prepared.parts:
        parser.error('Prepared fixtures must remain private under .local')
    if args.bundles:
        for provider in ('codex', 'claude'):
            replay.prepare(SimpleNamespace(bundle=args.bundles / (provider + '.json'), output=output / provider,
                                           history_turns=255, speed=1 if args.serve_native else 10000,
                                           timing=bool(args.benchmark_samples)))
    if args.serve_native:
        for provider in ('codex', 'claude'):
            config = json.loads((prepared / provider / 'config.json').read_text())
            if config['speed'] != 1:
                parser.error('Native serving requires original-paced prepared fixtures (speed 1)')
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', args.port))
        port = sock.getsockname()[1]
    probe = Probe(port, args.serve_native or 'https://replay.invalid')
    # The HTTPS origin and matching Host emulate Tailscale's local reverse proxy;
    # this isolated check does not claim TLS or tailnet transport verification.
    # Clear inherited Wonder overrides; every stateful path is this run's own.
    env = {k: v for k, v in os.environ.items() if not k.startswith('WONDER_')}
    env.update(GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM='1',
               WONDER_DATA_DIR=str(output / 'daemon'), WONDER_LOG_DIR=str(output / 'logs'),
               WONDER_CODEX_BIN=str(prepared / 'codex/replay-runtime'), CODEX_HOME=str(output / 'codex-home'),
               CLAUDE_CONFIG_DIR=str(output / 'claude-home'), WONDER_CLAUDE_BRIDGE=str(prepared / 'claude/replay-runtime'),
               WONDER_NODE_BIN=sys.executable, WONDER_LOOPBACK_CAPABILITY=probe.capability,
               WONDER_LISTEN_ADDR=f'127.0.0.1:{port}', WONDER_PUBLIC_ORIGIN=probe.origin)
    for folder in ('codex-home', 'claude-home'):
        (output / folder).mkdir()
    report = {'daemonSha256': hashlib.sha256(args.daemon.read_bytes()).hexdigest(), 'modelCalls': 0, 'checks': []}
    command = [str(args.daemon.resolve())]
    if args.host:
        host = str(args.host.resolve())
        env['WONDER_DAEMON_BIN'] = str(args.daemon.resolve())
        init = [host, 'init', '--name', 'Linux replay host', '--tunnel', 'none',
                '--origin', probe.origin, '--listen', f'127.0.0.1:{port}']
        if args.computer:
            script = Path(__file__).resolve().parents[1] / 'native/computer-use-linux/wonder_computer_linux.py'
            helper = output / 'computer-helper'
            helper.write_text('#!/bin/sh\nexec ' + shlex.quote(sys.executable) + ' ' + shlex.quote(str(script)) + '\n')
            helper.chmod(0o700)
            env['WONDER_COMPUTER_USE_BIN'] = str(helper)
            init.append('--computer')
        subprocess.run(init, env=env, check=True, timeout=10, stdout=subprocess.DEVNULL)
        probe.capability = (output / 'daemon/headless/loopback-capability').read_text().strip()
        command = [host, 'start', '--foreground']
        report['supervisorSha256'] = hashlib.sha256(args.host.read_bytes()).hexdigest()
    with (output / 'daemon.log').open('w') as log:
        process = subprocess.Popen(command, env=env, stdout=log, stderr=log, start_new_session=True)
        def requested_stop(_signum, _frame):
            raise KeyboardInterrupt
        signal.signal(signal.SIGTERM, requested_stop)
        try:
            def listening():
                if process.poll() is not None:
                    raise AssertionError('Isolated daemon exited during startup')
                try:
                    return probe.call('/healthz', local=True)
                except urllib.error.URLError:
                    return None
            eventually(listening, 'isolated daemon listening')
            probe.pair()
            def ready():
                try:
                    return probe.call('/readyz', local=True)
                except AssertionError as error:
                    if error.args[0][1] != 503:
                        raise
                    return None
            eventually(ready, 'runtime ready', 60)
            if args.serve_native:
                serve_native(probe, prepared, output, process, args.serve_seconds, report)
            elif args.benchmark_samples:
                report['benchmark'] = benchmark(probe, prepared, args.benchmark_samples)
                report['result'] = 'passed'
            else:
                for provider in ('codex', 'claude'):
                    report['checks'].append(check_provider(probe, prepared, provider))
                report['result'] = 'passed'
        except KeyboardInterrupt:
            report['result'] = 'stopped' if args.serve_native else 'failed'
            if not args.serve_native:
                raise
        except BaseException as error:
            report['result'] = 'failed'
            report['error'] = str(error)
            raise
        finally:
            # A supervising runner may stop us again while children drain.
            # Keep that signal from interrupting bounded cleanup and evidence.
            signal.signal(signal.SIGTERM, signal.SIG_IGN)
            cleanup_failure = None
            try:
                if process.poll() is None:
                    try:
                        os.killpg(process.pid, signal.SIGTERM)
                    except ProcessLookupError:
                        pass
                    try:
                        process.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        # This Popen still owns its unreaped process ID/group.
                        if process.poll() is None:
                            try:
                                os.killpg(process.pid, signal.SIGKILL)
                            except ProcessLookupError:
                                pass
                        process.wait(timeout=5)
            except (OSError, subprocess.SubprocessError) as error:
                report['cleanupError'] = str(error)
                if report.get('result') in ('passed', 'served', 'stopped'):
                    report['result'] = 'failed'
                    cleanup_failure = error
            finally:
                # Startup/cleanup failure must retain the original evidence.
                (output / 'report.json').write_text(json.dumps(report, indent=2))
                (output / 'http-requests.json').write_text(json.dumps(probe.requests, indent=2))
            if cleanup_failure is not None:
                raise cleanup_failure
    print(json.dumps(report, indent=2))


if __name__ == '__main__':
    main()

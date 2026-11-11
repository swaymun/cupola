#!/usr/bin/env python3
"""Bounded real-provider acceptance through Wonder's paired Project API.

All output/state is private under .local. Explicit --execute is required for
model work. Reuse the same --budget across reruns; attempts are spent before send.
"""
import argparse
from collections import Counter, deque
import contextlib
import fcntl
import hashlib
import importlib.util
import json
import math
import os
import re
from pathlib import Path
import shutil
import signal
import socket
import subprocess
import sys
import threading
import time
import urllib.parse
import urllib.error
import uuid

spec = importlib.util.spec_from_file_location('http_probe', Path(__file__).with_name('test-session-replay-http.py'))
http = importlib.util.module_from_spec(spec)
spec.loader.exec_module(http)
PINNED_PUBLIC_COMMIT = '98eae5337d82157081dedab322f5091f856a8564'
# Claude SDK runtime 0.3.300 resolves the `haiku` alias to Claude Haiku 5.5 (measured
# 2026-10-14); a runtime may report a dated snapshot of the same model instead.
MODELS = {'codex': 'gpt-6-luna', 'claude': 'claude-haiku-5-5'}
TERMINAL = {'completed', 'failed', 'interrupted'}


class InfrastructureFailure(Exception):
    pass


def check_dependencies():
    try:
        import websocket
        from websocket import create_connection, WebSocketTimeoutException
    except ImportError as error:
        raise InfrastructureFailure('Live fixture dependencies unavailable; run scripts/setup-nightly-sim '
                                    'or install scripts/live-fixtures-requirements.txt in a Python virtual environment') from error
    return websocket


def save(path, value):
    path.write_text(json.dumps(value, indent=2) + '\n')


def run(*args, cwd=None, env=None):
    if args and args[0] == 'git':
        # Fixture repositories must not inherit signing, hooks, filters or
        # fsmonitor from the owner's config. Preserve explicit dates/environment.
        env = dict(os.environ if env is None else env,
                   GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM='1')
        # Cloned/previously prepared repositories may also have local fsmonitor.
        args = ('git', '-c', 'core.fsmonitor=false', *args[1:])
    process = subprocess.Popen(args, cwd=cwd, env=env, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
    try:
        stdout, stderr = process.communicate(timeout=90)
    except BaseException:
        if process.poll() is None:
            os.killpg(process.pid, signal.SIGKILL)
        process.communicate()
        raise
    if process.returncode:
        raise subprocess.CalledProcessError(process.returncode, args, stdout, stderr)
    return stdout.strip()


def private(path):
    resolved = path.resolve()
    if '.local' not in resolved.parts or path.is_symlink():
        raise ValueError('Private state must be beneath .local, without symlinks')
    return resolved


def claim_turn(path, provider, request_id, cap=None):
    # Durable and locked across processes. A timeout/uncertain send still consumes
    # its slot; rerunning cannot silently refill the budget.
    with path.with_suffix(path.suffix + '.lock').open('a+') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        ledger = json.loads(path.read_text()) if path.exists() else {'limit': cap or 3, 'attempts': []}
        if ledger['limit'] not in (3, 4) or cap not in (None, 3, 4):
            raise InfrastructureFailure('Unsupported live turn budget')
        if cap is not None:
            if cap < ledger['limit']: raise InfrastructureFailure('Budget cap cannot decrease')
            ledger['limit'] = cap
        if len(ledger['attempts']) >= ledger['limit']:
            raise InfrastructureFailure('Live turn budget exhausted')
        ledger['attempts'].append({'provider': provider, 'requestId': request_id, 'at': time.time()})
        temporary = path.with_name(path.name + '.' + str(uuid.uuid4()))
        with temporary.open('x') as file:
            file.write(json.dumps(ledger, indent=2)); file.flush(); os.fsync(file.fileno())
        os.replace(temporary, path)
        directory = os.open(path.parent, os.O_RDONLY)
        try: os.fsync(directory)
        finally: os.close(directory)
        return len(ledger['attempts'])


def select_model(provider, catalog):
    if catalog.get('error') or not catalog.get('connected'):
        raise InfrastructureFailure(provider + ' subscription/catalog unavailable')
    wanted = MODELS[provider]
    for model in catalog.get('models', []):
        native = model.get('resolvedModel') if provider == 'claude' else model.get('model')
        # Exactly the wanted model, or its dated snapshot (wanted-YYYYMMDD).
        if not re.fullmatch(re.escape(wanted) + r'(-\d{8})?', native or model.get('id') or '') or model.get('hidden'):
            continue
        efforts = model.get('efforts', []) if provider == 'claude' else [e['reasoningEffort'] for e in model.get('supportedReasoningEfforts', [])]
        if efforts and 'low' not in efforts:
            raise InfrastructureFailure(provider + ' exact model does not offer low effort')
        windows = catalog.get('windows') if provider == 'claude' else catalog.get('usage')
        if not windows:
            raise InfrastructureFailure(provider + ' usage preflight unavailable')
        percentages = []
        def usage_percentages(value):
            if isinstance(value, dict):
                for key, nested in value.items():
                    if key in ('usedPercent', 'usedPercentage'):
                        if type(nested) not in (int, float) or not math.isfinite(nested) or nested < 0:
                            raise InfrastructureFailure(provider + ' usage preflight unavailable')
                        percentages.append(nested)
                    else: usage_percentages(nested)
            elif isinstance(value, list):
                for nested in value: usage_percentages(nested)
        usage_percentages(windows)
        if not percentages:
            raise InfrastructureFailure(provider + ' usage preflight unavailable')
        if catalog.get('usage', {}).get('ordinaryUsageAllowed') is False or any(p >= 100 for p in percentages):
            raise InfrastructureFailure(provider + ' usage limit reached')
        return {'model': ('claude:' if provider == 'claude' else '') + model['id'], 'effort': 'low' if efforts else None}
    raise InfrastructureFailure(provider + ' exact model unavailable: ' + wanted)


def seed_claude_sdk(store, data_dir, version, bundled):
    """Gives the isolated daemon the SDK the installed app runs. Without it the daemon
    starts on the bundled SDK and may download and activate an update mid-run, so the
    model the preflight selected and the model a turn resolves can differ. The copy is an
    APFS clone (the SDK is ~300 MB); `checkedAt` is now, so no update check runs today."""
    if not version or version == bundled:
        return
    source = store / 'versions' / version
    target = data_dir / 'claude-runtime' / 'sdk'
    if not (source / 'package.json').is_file():
        raise InfrastructureFailure('Claude SDK ' + version + ' missing from ' + str(store))
    (target / 'versions').mkdir(parents=True, exist_ok=True, mode=0o700)
    subprocess.run(['cp', '-cR', str(source), str(target / 'versions' / version)], check=True, capture_output=True)
    save(target / 'state.json', {'active': version, 'lastGood': bundled, 'staged': None,
                                 'lastFailure': None, 'checkedAt': int(time.time() * 1000)})


def bundled_claude_sdk(resources):
    package = resources / 'claude-runtime/node_modules/@anthropic-ai/claude-agent-sdk/package.json'
    return json.loads(package.read_text())['version']


def prepare_projects(output, public_checkout):
    todo = output / 'todo'
    todo.mkdir()
    (todo / 'todos.py').write_text('def toggle(todo):\n    return {**todo, "done": not todo["done"]}\n')
    (todo / 'test_todos.py').write_text('import unittest\nfrom todos import toggle\n\nclass Todos(unittest.TestCase):\n    def test_toggle(self):\n        self.assertTrue(toggle({"title": "Read", "done": False})["done"])\n')
    (todo / 'README.md').write_text('# Todo fixture\n\nA small todo-list model. Run `python3 -m unittest`.\n')
    (todo / 'AGENTS.md').write_text('Work only in this toy repository. No network, external files, delegation or subagents. Do not commit. Keep tasks small.\n')
    env = {**os.environ, 'GIT_AUTHOR_DATE': '2026-01-01T00:00:00Z', 'GIT_COMMITTER_DATE': '2026-01-01T00:00:00Z'}
    run('git', 'init', '-q', '--initial-branch=fixture', str(todo))
    run('git', '-C', str(todo), 'add', '.')
    run('git', '-C', str(todo), '-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid', '-c', 'commit.gpgsign=false', 'commit', '-qm', 'Fixed todo fixture', env=env)
    baseline = run('git', '-C', str(todo), 'rev-parse', 'HEAD')
    reset_todo(todo, baseline)
    origin = run('git', '-C', str(public_checkout), 'remote', 'get-url', 'origin')
    if origin.rstrip('/') not in ('https://github.com/swaymun/wonder.git', 'https://github.com/swaymun/wonder', 'git@github.com:swaymun/wonder.git'):
        raise ValueError('Browsing source must be the reviewed public Wonder repository')
    browse = output / 'wonder-public'
    run('git', 'clone', '--no-hardlinks', '--no-checkout', '--quiet', str(public_checkout), str(browse))
    run('git', '-C', str(browse), 'checkout', '--quiet', '--detach', PINNED_PUBLIC_COMMIT)
    run('git', '-C', str(browse), 'remote', 'set-url', 'origin', 'https://github.com/swaymun/wonder.git')
    return todo, browse, baseline


def reset_todo(todo, baseline):
    # Caller supplies only the repository this invocation just created.
    run('git', '-C', str(todo), 'reset', '--hard', baseline)
    run('git', '-C', str(todo), 'clean', '-fdx')


class Events:
    MAX_FRAMES = 20000
    MAX_BYTES = 32 * 1024 * 1024
    READ_TIMEOUT = .2

    def __init__(self, probe, checkpoint=None):
        self.websocket = websocket = check_dependencies()
        checkpoint = checkpoint or probe.call('/api/v1/sync/checkpoint')
        challenge = probe.call('/api/v1/events/challenge')
        fields = ['wonder-session-v1'] + [challenge[k] for k in ('deviceId', 'challengeId', 'nonce', 'origin', 'hostInstallationId', 'issuedAtMs', 'expiresAtMs')]
        self.socket = websocket.create_connection(probe.address.replace('http:', 'ws:') + '/api/v1/events',
            origin=probe.origin, host=urllib.parse.urlsplit(probe.origin).netloc,
            cookie='__Host-wonder_session=' + probe.session['sessionToken'],
            header=['x-wonder-history-view: compact'], timeout=.2)
        self._condition = threading.Condition()
        self._pending = deque()
        self._frames = []
        self._received_bytes = 0
        self._error = None
        self._closed = False
        self._stop = threading.Event()
        # One owner continuously reads, including during catalog/HTTP waits.
        # websocket-client answers Ping inside recv(); read() must never compete
        # for the socket or timestamp a frame later when observation resumes.
        self._reader = threading.Thread(target=self._receive, name='fixture-event-reader')
        try:
            self.socket.send(json.dumps({'deviceId': probe.session['deviceId'], 'hostEpoch': checkpoint['hostEpoch'],
                'lastSequence': checkpoint['lastSequence'], 'csrfToken': probe.session['csrfToken'],
                'challengeId': challenge['challengeId'], 'signature': probe.sign(fields), 'batch': True}))
            self._reader.start()
        except BaseException:
            self.socket.close(timeout=0)
            self.socket.shutdown()
            raise

    @property
    def frames(self):
        # Observation and cleanup get a stable snapshot, including frames received
        # while busy elsewhere. Queued read results reference the same records.
        with self._condition:
            return list(self._frames)

    def _receive(self):
        try:
            while not self._stop.is_set():
                try:
                    raw = self.socket.recv()
                except self.websocket.WebSocketTimeoutException:
                    continue
                if not raw:
                    raise InfrastructureFailure('Event stream closed')
                received_ns = time.monotonic_ns()
                size = len(raw.encode('utf-8')) if isinstance(raw, str) else len(raw)
                if self._received_bytes + size > self.MAX_BYTES:
                    raise InfrastructureFailure('Event stream exceeded the bounded fixture byte window')
                frames = json.loads(raw)
                if not isinstance(frames, list): frames = [frames]
                if len(self._frames) + len(frames) > self.MAX_FRAMES:
                    raise InfrastructureFailure('Event stream exceeded the bounded fixture window')
                with self._condition:
                    self._received_bytes += size
                    self._frames.extend(frames)
                    if any(frame['event']['type'] == 'resync_required' for frame in frames):
                        raise InfrastructureFailure('Event stream requires resynchronization')
                    # Empty arrays need no delivery entry; otherwise they could
                    # grow an unbounded queue without consuming the frame budget.
                    if frames:
                        self._pending.append((received_ns, frames))
                        self._condition.notify_all()
        except Exception as error:
            with self._condition:
                if not self._closed:
                    self._error = error
                self._condition.notify_all()
        finally:
            self.socket.close(timeout=0)
            self.socket.shutdown()

    def read(self):
        with self._condition:
            self._condition.wait_for(lambda: self._pending or self._error or self._closed, self.READ_TIMEOUT)
            # A terminal error follows the data already received on the wire.
            # Deliver that prefix once rather than losing it to a faster reader.
            if self._pending:
                return self._pending.popleft()
            if self._error:
                if isinstance(self._error, InfrastructureFailure): raise self._error
                raise InfrastructureFailure('Event stream reader failed') from self._error
            if self._closed:
                raise InfrastructureFailure('Event stream closed')

    def close(self):
        with self._condition:
            self._closed = True
            self._stop.set()
            self._condition.notify_all()
        # Wake recv without a second reader racing a WebSocket close handshake.
        self.socket.abort()
        self._reader.join(timeout=1)
        if self._reader.is_alive():
            raise InfrastructureFailure('Event stream reader did not stop')


def classify_error(text):
    if any(word in text.lower() for word in ('rate limit', 'usage limit', 'quota', 'model unavailable', 'model not found', 'not supported', 'authentication', 'sign in')):
        raise InfrastructureFailure(text)
    raise AssertionError(text)


def approval_item_path(directory, params):
    identities = [params.get(key) for key in ('threadId', 'turnId', 'itemId')]
    if not all(isinstance(value, str) and value for value in identities):
        raise InfrastructureFailure('Approval does not identify a native file-change item')
    return directory / (hashlib.sha256(json.dumps(identities).encode()).hexdigest() + '.json')


def record_native_changes(binary, directory, args):
    # Test-only transparent stdout observer: inspect the owning App Server's
    # events before its display projection. Never send or alter a request.
    child = subprocess.Popen([binary, *args], stdout=subprocess.PIPE)
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(143))
    try:
        while line := child.stdout.readline(1024 * 1024 + 1):
            if len(line) > 1024 * 1024:
                raise InfrastructureFailure('Native fixture frame exceeded 1 MiB')
            try:
                event = json.loads(line)
            except (ValueError, UnicodeDecodeError):
                event = {}
            params = event.get('params', {})
            item = params.get('item', {})
            # Claude's approvals are checked against its request alone; its items
            # share the request's ID and must not replace that saved evidence.
            if (event.get('method') in ('item/started', 'item/completed') and item.get('type') == 'fileChange'
                    and params.get('agentFamily') != 'claude'):
                path = approval_item_path(directory, {**params, 'itemId': item['id']})
                native = {'threadId': params['threadId'], 'turnId': params['turnId'], 'item': item}
            elif event.get('method') == 'item/commandExecution/requestApproval' and params.get('agentFamily') == 'claude':
                # Claude's runtime asks for an edit with the tool's own input.
                path = approval_item_path(directory, params)
                native = {'threadId': params['threadId'], 'turnId': params['turnId'], 'request': params}
            elif event.get('method') == 'item/fileChange/patchUpdated':
                path = approval_item_path(directory, params)
                native = json.loads(path.read_text()) if path.exists() else None
                if native is not None:
                    native['item']['changes'] = params['changes']
            else:
                path, native = None, None
            if native is not None:
                if not path.exists() and len(list(directory.glob('*.json'))) >= 64:
                    raise InfrastructureFailure('Native fixture exceeded 64 file-change items')
                temporary = path.with_suffix('.tmp')
                save(temporary, native)
                temporary.replace(path)
            sys.stdout.buffer.write(line)
            sys.stdout.buffer.flush()
        return child.wait()
    finally:
        if child.poll() is None:
            child.terminate()
            try: child.wait(timeout=3)
            except subprocess.TimeoutExpired:
                child.kill(); child.wait()


def native_change_observer(output, codex, name='codex'):
    directory = output / ('native-file-changes' if name == 'codex' else f'native-{name}-approvals'); directory.mkdir(exist_ok=True)
    wrapper = output / f'{name}-observer'
    wrapper.write_text('#!' + sys.executable + '\nimport os, sys\nos.execv(' + repr(sys.executable) + ', [' +
        repr(sys.executable) + ', ' + repr(str(Path(__file__).resolve())) + ', "--record-native-file-changes", ' +
        repr(str(codex.resolve())) + ', ' + repr(str(directory)) + '] + sys.argv[1:])\n')
    wrapper.chmod(0o700)
    # Keep the installer's native sibling-helper readiness check intact.
    helper = codex.resolve().with_name('codex-code-mode-host')
    link = wrapper.with_name('codex-code-mode-host')
    if helper.is_file() and not link.exists(): link.symlink_to(helper)
    def read_item(approval):
        if approval['method'] != ('item/fileChange/requestApproval' if name == 'codex' else 'item/commandExecution/requestApproval'):
            raise InfrastructureFailure('Only native file-change approvals are allowed')
        path = approval_item_path(directory, approval['params'])
        return json.loads(path.read_text()) if path.is_file() else None
    return wrapper, read_item


def toy_path(todo, value):
    allowed = {todo / 'todos.py', todo / 'test_todos.py'}
    if not isinstance(value, str) or not value or '\0' in value:
        return False
    candidate = todo / value
    return Path(os.path.abspath(candidate)) in allowed and candidate.resolve() in allowed


# Claude's edit tools and their complete inputs; anything else is out of scope.
CLAUDE_EDIT_INPUTS = {'Edit': {'file_path', 'old_string', 'new_string', 'replace_all'},
                      'Write': {'file_path', 'content'}, 'MultiEdit': {'file_path', 'edits'}}
# `2>&1` only merges the output streams; any other redirection or command is out of scope.
CLAUDE_TEST_COMMAND = re.compile(r'python3 -m unittest( -v)?( test_todos(\.py)?)?( 2>&1)?')


def allowed_claude_approval(approval, todo, native):
    # Claude's runtime sends the tool's own name and input; the host's list
    # may shorten them for display, so only the runtime's request counts.
    params, request = approval['params'], native.get('request') if isinstance(native, dict) else None
    if not isinstance(request, dict) or request.get('agentFamily') != 'claude':
        return False
    if any(source.get(key) is not None for source in (params, request) for key in ('grantRoot', 'networkApprovalContext')):
        return False
    if not all(isinstance(params.get(key), str) and params[key] and request.get(key) == params[key] and native.get(key, params[key]) == params[key]
               for key in ('threadId', 'turnId', 'itemId')):
        return False
    command = request.get('command')
    if not isinstance(command, str) or 'outside the sandbox' in str(request.get('reason', '')):
        return False
    if request.get('kind') == 'command':
        return CLAUDE_TEST_COMMAND.fullmatch(command) is not None
    name, separator, raw = command.partition(': ')
    if request.get('kind') != 'tool' or not separator or name not in CLAUDE_EDIT_INPUTS:
        return False
    try:
        tool_input = json.loads(raw)
    except ValueError:
        return False
    return (isinstance(tool_input, dict) and not tool_input.keys() - CLAUDE_EDIT_INPUTS[name]
            and toy_path(todo, tool_input.get('file_path')))


def allowed_approval(approval, todo, native):
    if approval['method'] == 'item/commandExecution/requestApproval' and isinstance(native, dict) and 'request' in native:
        return allowed_claude_approval(approval, todo, native)
    # Only the complete native patch establishes scope. The display projection
    # omits rename destinations and the request itself contains only item IDs.
    if approval['method'] != 'item/fileChange/requestApproval' or not isinstance(native, dict):
        return False
    params = approval['params']
    if params.get('grantRoot') is not None or params.get('networkApprovalContext') is not None:
        return False
    item = native.get('item')
    if not isinstance(item, dict):
        return False
    if (not all(isinstance(params.get(key), str) and params[key] for key in ('threadId', 'turnId', 'itemId'))
        or native.get('threadId') != params['threadId'] or native.get('turnId') != params['turnId']
        or item.get('id') != params['itemId'] or item.get('type') != 'fileChange'
        or item.get('status') != 'inProgress'):
        return False
    changes = item.get('changes')
    if not isinstance(changes, list) or not changes:
        return False
    def allowed_path(value):
        return toy_path(todo, value)
    for change in changes:
        if not isinstance(change, dict) or not allowed_path(change.get('path')) or not isinstance(change.get('diff'), str):
            return False
        kind = change.get('kind')
        if not isinstance(kind, dict) or kind.get('type') not in ('add', 'delete', 'update') or kind.keys() - {'type', 'move_path'}:
            return False
        if kind.get('move_path') is not None and (kind['type'] != 'update' or not allowed_path(kind['move_path'])):
            return False
    return True


def provider_turns(snapshot):
    # Project snapshots include local queue placeholders before runtime admission.
    return [turn for turn in snapshot['thread']['turns'] if not turn['id'].startswith('local:')]


def reply_started(frames, conversation, turn):
    # A snapshot can project accepted_by_codex before runtime work starts.
    return any(frame.get('conversationId') == conversation and frame.get('turnId') == turn
               and frame['event']['type'] == 'assistant_delta' and frame['event']['data'].get('text')
               for frame in frames)


def observe(probe, events, conversation, client_id, stop, todo, seconds=90, approval_reader=None):
    base = '/api/v1/conversations/' + conversation
    deadline = time.monotonic() + seconds
    approvals, interrupted = set(), False
    while time.monotonic() < deadline:
        events.read()
        snapshot = probe.call(base + '/history?limit=100')
        turns = provider_turns(snapshot)
        for saved in snapshot.get('messages', []):
            if saved.get('clientMessageId') == client_id and saved.get('state') == 'failed':
                classify_error(json.dumps(saved))
        active = next((t for t in turns if t['status'] not in TERMINAL), None)
        if stop and active and not interrupted and reply_started(events.frames, conversation, active['id']):
            probe.call(base + '/turns/' + active['id'] + '/interrupt', {})
            interrupted = True
        for approval in probe.call('/api/v1/approvals'):
            if approval['conversationId'] == conversation and approval['approvalId'] not in approvals and not stop:
                if approval_reader is None:
                    raise InfrastructureFailure('Native approval verification is unavailable for this provider')
                native = approval_reader(approval)
                if native is None:
                    continue  # The owning runtime event may not have been recorded yet.
                if not allowed_approval(approval, todo, native):
                    raise InfrastructureFailure('Native approval does not establish an edit limited to the two toy files')
                probe.resolve(approval); approvals.add(approval['approvalId'])
        terminal = next((t for t in turns if t['status'] in TERMINAL), None)
        if terminal:
            expected = 'interrupted' if stop else 'completed'
            if terminal['status'] != expected:
                classify_error(json.dumps(terminal))
            # Drain the coalesced terminal frame before computing stream evidence.
            for _ in range(3): events.read()
            own = [f for f in events.frames if f.get('conversationId') == conversation]
            kinds = Counter(f['event']['type'] for f in own)
            items = Counter(i['type'] for i in terminal['items'])
            if not stop:
                assert kinds['assistant_delta'] > 0, 'No reply streamed over the paired WebSocket'
                assert items['commandExecution'] + items['mcpToolCall'] > 0, 'No runtime tool call'
                assert items['fileChange'] > 0, 'No runtime diff'
                assert approvals, 'No signed approval resolved'
            else:
                assert interrupted, 'Stop never reached an active turn'
            return {'status': terminal['status'], 'eventKinds': dict(kinds), 'itemKinds': dict(items), 'signedApprovals': len(approvals)}
    raise InfrastructureFailure('Fixture turn exceeded its 90-second deadline')


def cleanup_threads(probe, owned, outcomes=None, seconds=15):
    outcomes = outcomes if outcomes is not None else []
    errors = []
    for conversation in owned:
        # A polling deadline cannot interrupt a stalled HTTP response. Bound the
        # complete cleanup attempt so daemon/auth cleanup still runs afterwards.
        def timed_out(*_):
            raise InfrastructureFailure('Own fixture thread cleanup deadline exceeded')
        previous = signal.signal(signal.SIGALRM, timed_out)
        signal.setitimer(signal.ITIMER_REAL, seconds)
        try:
            base = '/api/v1/conversations/' + conversation
            interrupted = set()
            def archived():
                snapshot = probe.call(base + '/history?limit=100')
                detail = probe.call('/api/v1/project-conversations/' + conversation)
                states = [message['state'] for message in snapshot.get('messages', [])]
                if not detail['hasNativeSession'] and all(state in ('failed', 'safe_to_retry', 'interrupted') for state in states):
                    outcomes.append({'conversationId': conversation, 'state': 'no_provider_session', 'messageStates': states})
                    return True
                for turn in provider_turns(snapshot):
                    if turn['status'] not in TERMINAL and turn['id'] not in interrupted:
                        probe.call(base + '/turns/' + turn['id'] + '/interrupt', {})
                        interrupted.add(turn['id'])
                try:
                    result = probe.call('/api/v1/project-conversations/' + conversation, {'isArchived': True}, method='PATCH').get('isArchived')
                    if result:
                        outcomes.append({'conversationId': conversation, 'state': 'archived',
                                         'archiveScope': 'codex' if detail['family'] == 'codex' else 'wonder'})
                    return result
                except AssertionError:
                    return False
            http.eventually(archived, 'archive own fixture thread', seconds)
        except Exception as error:
            errors.append({'conversationId': conversation, 'error': str(error)})
        finally:
            signal.setitimer(signal.ITIMER_REAL, 0)
            signal.signal(signal.SIGALRM, previous)
    return errors


def wait_for_model(probe, provider, model, seconds=60):
    # /readyz establishes ingestion, not completion of asynchronous provider discovery.
    started = time.monotonic()
    def available():
        return any(option['id'] == model and option['agentFamily'] == provider
                   for option in probe.call('/api/v1/agent-options')['models'])
    try:
        http.eventually(available, provider + ' model catalog', seconds)
    except AssertionError as error:
        raise InfrastructureFailure(provider + ' model not published by the host: ' + model) from error
    return time.monotonic() - started


def reservation_body(provider, selected, device_id, client_id):
    # Codex asks before each patch even in Read only. Claude denies every edit
    # in Read only before asking, so its fixture uses the app's default
    # Workspace access, where Ask still requires a signed approval per edit.
    access = {'accessMode': 'workspace', 'claudeApproval': 'ask'} if provider == 'claude' else {'accessMode': 'read_only'}
    return {'deviceId': device_id, 'clientMessageId': client_id, 'family': provider,
            **selected, **access, 'body': '', 'prepareOnly': True}


def exercise(probe, events, output, budget, provider, selected, todo, baseline, owned, report, scenario, cap, approval_reader=None):
    report['providers'][provider]['modelReadySeconds'] = wait_for_model(probe, provider, selected['model'])
    project = probe.call('/api/v1/projects', {'requestId': str(uuid.uuid4()), 'name': provider.title() + ' live todo', 'folders': [str(todo)], 'primaryIndex': 0})
    results = report['providers'][provider].setdefault('checks', [])
    for stop in ([False, True] if scenario == 'all' else [scenario == 'stop']):
        reset_todo(todo, baseline)
        client_id = str(uuid.uuid4())
        # Reserve metadata without starting a provider, so cleanup retains exact
        # ownership even if the subsequent model submission has an uncertain reply.
        body = reservation_body(provider, selected, probe.session['deviceId'], client_id)
        path = '/api/v1/projects/' + project['id'] + '/threads'
        reservation = probe.call(path, body)
        conversation = reservation['conversation']['conversationId']
        owned.append(conversation); save(output / 'owned-threads.json', owned)
        claim_turn(budget, provider, client_id, cap)
        report['modelTurnAttempts'] += 1; save(output / 'report.json', report)
        prompt = ('First briefly say you are starting. Then run python3 -c "import time; time.sleep(30)" in this toy folder. Do not change files.' if stop else
                  'Add an optional due_date field to this todo model and one unittest. Read todos.py and test_todos.py first. '
                  'Use ' + ('the Edit or Write tool' if provider == 'claude' else 'apply_patch') + ' for edits to only these two files, requesting permission if needed; do not write files through a shell command or Python. '
                  'Then run python3 -m unittest. '
                  'Do not use the network, external paths, delegation or subagents. Finish with a short explanation.')
        check = {'scenario': 'stop' if stop else 'edit', 'conversationId': conversation, 'result': 'running'}
        results.append(check)
        save(output / 'report.json', report)
        try:
            probe.call(path, {**body, 'body': prompt, 'prepareOnly': False})
            check.update(observe(probe, events, conversation, client_id, stop, todo, approval_reader=approval_reader))
            if not stop:
                assert run('git', '-C', str(todo), 'diff', '--numstat'), 'No actual git diff'
            cleanup_errors = cleanup_threads(probe, [conversation])
            if cleanup_errors: raise InfrastructureFailure('Own fixture thread could not be archived')
            check['result'] = 'passed'
        except BaseException:
            check['result'] = 'incomplete'
            raise
        finally:
            save(output / 'report.json', report)
    return results


def stop_process(process):
    if process and process.poll() is None:
        with contextlib.suppress(ProcessLookupError): os.killpg(process.pid, signal.SIGTERM)
        try: process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            if process.poll() is None:
                with contextlib.suppress(ProcessLookupError): os.killpg(process.pid, signal.SIGKILL)
            process.wait(timeout=5)


def main():
    os.umask(0o077)
    if sys.argv[1:2] == ['--record-native-file-changes']:
        return record_native_changes(sys.argv[2], Path(sys.argv[3]), sys.argv[4:])
    if sys.argv[1:] == ['--check-dependencies']:
        try:
            check_dependencies()
        except InfrastructureFailure as error:
            print(str(error), file=sys.stderr)
            return 1
        print('Live fixture dependencies ready')
        return 0
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--budget', type=Path, required=True)
    parser.add_argument('--turn-cap', type=int, choices=[3, 4], help='Explicitly initialize/extend this budget; requires authorization for that many attempts')
    parser.add_argument('--daemon', type=Path, required=True)
    parser.add_argument('--codex', type=Path, required=True)
    parser.add_argument('--resources', type=Path, default=Path('/Applications/Wonder.app/Contents/Resources'))
    parser.add_argument('--claude-sdk-store', type=Path, default=Path.home() / '.wonder/claude-runtime/sdk',
                        help="the installed app's Claude SDK update store, read only")
    parser.add_argument('--public-checkout', type=Path, required=True)
    parser.add_argument('--provider', choices=['all', *MODELS], default='all')
    parser.add_argument('--execute', action='store_true')
    parser.add_argument('--recover', action='store_true', help='Stop/archive the exact saved owned threads without submitting any new message')
    parser.add_argument('--scenario', choices=['all', 'edit', 'stop'], default='edit')
    args = parser.parse_args()
    output, budget = private(args.output), private(args.budget)
    if args.recover:
        if args.execute or not (output / 'owned-threads.json').is_file() or not (output / 'daemon/wonder.sqlite3').is_file():
            parser.error('--recover requires an existing owned run, without --execute')
    elif output.exists(): parser.error('--output must be a new directory')
    output.mkdir(parents=True, mode=0o700, exist_ok=args.recover); budget.parent.mkdir(parents=True, exist_ok=True)
    report = {'result': 'preflight_only', 'scenario': args.scenario, 'modelTurnAttempts': 0, 'providers': {}, 'nativeUi': 'not checked', 'transport': 'paired HTTP/WebSocket over loopback; no TLS/tailnet claim'}
    probe, process, events, owned = None, None, None, []
    try:
        if args.execute:
            check_dependencies()
        with args.daemon.open('rb') as executable:
            report['daemonSha256'] = hashlib.file_digest(executable, 'sha256').hexdigest()
        selected = {}
        if args.recover:
            owned = json.loads((output / 'owned-threads.json').read_text())
            if not owned or any(str(uuid.UUID(value)) != value for value in owned):
                raise ValueError('Invalid saved fixture ownership')
        else:
            todo, browse, baseline = prepare_projects(output, args.public_checkout.resolve())
            report['projects'] = {'todoBaseline': baseline, 'browsingCommit': PINNED_PUBLIC_COMMIT}
            catalog = json.loads(run(str(args.resources / 'node/bin/node'), str(Path(__file__).with_name('preflight-live-fixtures.mjs')),
                str(args.codex.resolve()), str(args.resources / 'claude-runtime'), str(todo), str(args.claude_sdk_store)))
            save(output / 'catalog.json', catalog)
            report['claudeRuntime'] = catalog['providers'].get('claude', {}).get('runtimeVersion')
            selected = {}
            for provider in MODELS if args.provider == 'all' else [args.provider]:
                try:
                    selected[provider] = select_model(provider, catalog['providers'][provider])
                    report['providers'][provider] = {'result': 'available', **selected[provider]}
                except InfrastructureFailure as error:
                    report['providers'][provider] = {'result': 'infrastructure_failed', 'error': str(error)}
        if args.recover or (args.execute and selected):
            # Private Codex state uses only the subscription credential, never
            # the owner's config/hooks/skills or native thread database.
            codex_home = output / 'codex-home'; codex_home.mkdir(exist_ok=args.recover)
            auth = Path.home() / '.codex/auth.json'
            if not auth.is_file(): raise InfrastructureFailure('Codex subscription auth file unavailable')
            shutil.copyfile(auth, codex_home / 'auth.json')
            with socket.socket() as listener:
                listener.bind(('127.0.0.1', 0)); port = listener.getsockname()[1]
            probe = http.Probe(port)
            env = {k: os.environ[k] for k in ('HOME', 'PATH', 'USER', 'LOGNAME', 'SHELL', 'TMPDIR', 'LANG', 'LC_ALL') if k in os.environ}
            observed_codex, native_approval = native_change_observer(output, args.codex)
            node = args.resources / 'node/bin/node'
            observed_node, claude_approval = native_change_observer(output, node, 'node') if 'claude' in selected else (node, None)
            env.update(GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM='1',
                WONDER_DATA_DIR=str(output / 'daemon'), WONDER_LOG_DIR=str(output / 'logs'),
                WONDER_CODEX_BIN=str(observed_codex), CODEX_HOME=str(codex_home),
                WONDER_CLAUDE_BRIDGE=str(args.resources / 'claude-runtime/main.mjs'),
                WONDER_NODE_BIN=str(observed_node), WONDER_LOOPBACK_CAPABILITY=probe.capability,
                WONDER_LISTEN_ADDR=f'127.0.0.1:{port}', WONDER_PUBLIC_ORIGIN=probe.origin)
            if not args.recover and 'claude' in selected:
                seed_claude_sdk(args.claude_sdk_store, output / 'daemon', report.get('claudeRuntime'),
                                bundled_claude_sdk(args.resources))
            log = (output / ('recovery-daemon.log' if args.recover else 'daemon.log')).open('w')
            process = subprocess.Popen([str(args.daemon.resolve())], env=env, stdout=log, stderr=log, start_new_session=True)
            log.close()
            def ready():
                if process.poll() is not None: raise InfrastructureFailure('Isolated daemon exited')
                try: return probe.call('/readyz', local=True)
                except (urllib.error.URLError, AssertionError): return None
            http.eventually(ready, 'isolated runtime ready', 60)
            probe.pair()
            if not args.recover:
                events = Events(probe)
                browsing = probe.call('/api/v1/projects', {'requestId': str(uuid.uuid4()), 'name': 'Wonder public browsing', 'folders': [str(browse)], 'primaryIndex': 0})
                report['projects']['browsingProjectId'] = browsing['id']
                for provider, model in selected.items():
                    try:
                        checks = exercise(probe, events, output, budget, provider, model, todo, baseline, owned, report, args.scenario, args.turn_cap, approval_reader={'codex': native_approval, 'claude': claude_approval}.get(provider))
                        report['providers'][provider].update(result='passed', checks=checks)
                    except InfrastructureFailure as error:
                        report['providers'][provider].update(result='infrastructure_failed', error=str(error))
                    except Exception as error:
                        report['providers'][provider].update(result='failed', error=str(error))
        states = [value['result'] for value in report['providers'].values()]
        report['result'] = 'recovered' if args.recover else 'infrastructure_failed' if 'infrastructure_failed' in states else 'failed' if 'failed' in states else 'passed' if args.execute else 'preflight_only'
    except (KeyboardInterrupt, SystemExit):
        report.update(result='infrastructure_failed', error='Run interrupted; owned cleanup attempted')
    except Exception as error:
        report.update(result='infrastructure_failed', error=str(error))
    finally:
        try:
            if probe and owned:
                report['cleanupOutcomes'] = []
                report['cleanupErrors'] = cleanup_threads(probe, owned, report['cleanupOutcomes'])
                if report['cleanupErrors']: report['result'] = 'infrastructure_failed'
        except BaseException as error:
            report.setdefault('cleanupErrors', []).append({'step': 'archive owned threads', 'error': str(error) or type(error).__name__})
            report['result'] = 'infrastructure_failed'
        # Each cleanup step runs even if another fails; especially credential
        # deletion and stopping the exact process this invocation launched.
        steps = []
        if events:
            steps.extend([('close events', events.close),
                          ('save events', lambda: save(output / 'events.json', events.frames))])
        steps.extend([('stop daemon', lambda: stop_process(process)),
                      ('remove temporary auth', lambda: (output / 'codex-home/auth.json').unlink(missing_ok=True))])
        if probe:
            steps.append(('save HTTP evidence', lambda: save(output / ('recovery-http-requests.json' if args.recover else 'http-requests.json'), probe.requests)))
        for label, action in steps:
            try: action()
            except BaseException as error:
                report.setdefault('cleanupErrors', []).append({'step': label, 'error': str(error)})
                report['result'] = 'infrastructure_failed'
        if budget.exists():
            try: report['budgetAttemptsTotal'] = len(json.loads(budget.read_text())['attempts'])
            except (ValueError, KeyError):
                report.update(result='infrastructure_failed', budgetError='Budget is unreadable; never refill it automatically')
        save(output / ('recovery-report.json' if args.recover else 'report.json'), report)
    print(json.dumps(report, indent=2))
    return 0 if report['result'] in ('passed', 'preflight_only', 'recovered') else 1


if __name__ == '__main__':
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(130))
    sys.exit(main())

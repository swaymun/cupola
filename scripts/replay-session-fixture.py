#!/usr/bin/env python3
"""Serve synthetic session bundles over the normal App Server JSONL transport.

No model, shell tool, network, provider home, or Wonder database is opened.
Prepare a fresh private directory, then point a test runtime at its launcher.
"""
import argparse
import asyncio
import base64
from copy import deepcopy
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import sys
import time
import uuid

spec = importlib.util.spec_from_file_location('session_fixtures', Path(__file__).with_name('generate-session-fixtures.py'))
fixtures = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixtures)
ROOT = fixtures.ROOT
VERSION = '0.162.0-alpha.17.2'
TRIGGER = 'Replay the next recorded turn.'
MAX_BUNDLE = 512 * 1024 * 1024
MAX_REQUEST = 1024 * 1024


def native_id(name):
    return str(uuid.uuid5(uuid.NAMESPACE_URL, 'wonder-synthetic-replay:' + name))


def native_identities(value):
    if isinstance(value, dict):
        return {native_identities(k): native_identities(v) for k, v in value.items()}
    if isinstance(value, list):
        return [native_identities(v) for v in value]
    if isinstance(value, str) and value in ('fixture-codex', 'fixture-claude', 'fixture-child', 'fixture-session'):
        return ('claude-' if value == 'fixture-claude' else '') + native_id(value)
    return value


def read_bundle(path):
    with path.open('rb') as handle:
        raw = handle.read(MAX_BUNDLE + 1)
    if len(raw) > MAX_BUNDLE:
        raise ValueError('Fixture exceeds the replay size bound')
    bundle = json.loads(raw)
    fixtures.validate_bundle(bundle)
    if bundle.get('version') != 1 or bundle.get('provider') not in ('codex', 'claude'):
        raise ValueError('Unsupported synthetic fixture')
    return bundle


def read_prepared(root, config):
    # Preparation validates the whole source. Recheck its exact bytes at launch
    # without repeating a 30s+ schema walk inside the provider handshake deadline.
    if config.get('bundle') != 'bundle.json':
        raise ValueError('Unexpected prepared bundle path')
    with (root / 'bundle.json').open('rb') as handle:
        raw = handle.read(MAX_BUNDLE + 1)
    if len(raw) > MAX_BUNDLE or hashlib.sha256(raw).hexdigest() != config.get('bundleSha256'):
        raise ValueError('Prepared bundle changed; prepare the fixture again')
    return json.loads(raw)


def bind_paths(value, workspace):
    # Only the exact producer-owned placeholder is rewritten, never arbitrary text.
    if isinstance(value, dict):
        return {k: bind_paths(v, workspace) for k, v in value.items()}
    if isinstance(value, list):
        return [bind_paths(v, workspace) for v in value]
    if isinstance(value, str) and (value == '/fixture/todo' or value.startswith('/fixture/todo/')):
        return str(workspace) + value[len('/fixture/todo'):]
    return value


def child_thread(parent):
    """Authored child coverage, clearly separate from the sampled parent shape."""
    child = {k: deepcopy(v) for k, v in parent.items() if k != 'turns'}
    child.update(id=native_id('fixture-child'), parentThreadId=parent['id'],
                 source={'subAgent': {'thread_spawn': {'parent_thread_id': parent['id'], 'depth': 1,
                                                       'agent_nickname': 'Todo reviewer', 'agent_role': 'Reviewer'}}},
                 preview='Checked the synthetic todo list.', turns=[{
                     'id': 'fixture-child-turn', 'status': 'completed', 'error': None,
                     'items': [{'id': 'fixture-child-user', 'type': 'userMessage',
                                'content': [{'type': 'text', 'text': 'Check due dates in the todo list.'}]},
                               {'id': 'fixture-child-reply', 'type': 'agentMessage',
                                'phase': 'final_answer', 'text': 'Checked the synthetic todo list.'}]}])
    return child


class ReplayHost:
    def __init__(self, bundle, workspace, history_turns=None, speed=1, emit=None, trace=None):
        self.provider = bundle['provider']
        self.bundle = bind_paths(native_identities(bundle), workspace)
        self.parent = self.bundle['threadRead']['thread']
        if self.provider == 'claude':
            self.parent['sessionId'] = native_id('fixture-session')
        self.all_turns = self.parent.pop('turns')
        self.position = len(self.all_turns) if history_turns is None else history_turns
        if not 0 <= self.position <= len(self.all_turns) or not 0.001 <= speed <= 10000:
            raise ValueError('Replay history or speed is outside bounds')
        self.parent['turns'] = deepcopy(self.all_turns[:self.position])
        self.child = child_thread(self.parent)
        self.speed = speed
        self.trace = trace
        self.traced_deltas = set()
        output = emit or self.print_frame
        def send(frame):
            stamp = time.monotonic_ns()
            output(frame)
            params = frame.get('params', {})
            turn = params.get('turnId')
            if trace and frame.get('method') == 'item/agentMessage/delta' and params.get('delta') and turn not in self.traced_deltas:
                self.traced_deltas.add(turn)
                trace({'event': 'first_delta', 'turnId': turn, 'monotonicNs': stamp})
        self.emit = send
        self.active = None
        self.interrupted = False
        self.replaying = False
        self.pending = {}
        self.archived = False
        self.initialized = False
        self.ready = False
        self.rows = {}
        self.item_times = {}
        self.cursors = {}
        self.metrics = {'speed': speed, 'turnsReplayed': 0, 'serverRecords': 0, 'approvalsAnswered': 0,
                        'maxScheduleLagMs': 0, 'modelCalls': 0}
        # Responses and resolved notifications omit turnId. Preserve original
        # order while taking their owner from the corresponding server request.
        owners = {r['message']['id']: r['message']['params']['turnId']
                  for r in self.bundle['replay'] if r['message'].get('method', '').endswith('/requestApproval')}
        for row in self.bundle['replay']:
            m = row['message']; p = m.get('params', {})
            turn = p.get('turnId') or p.get('turn', {}).get('id')
            turn = turn or owners.get(m.get('id') if row['direction'] == 'client' else p.get('requestId'))
            if turn:
                self.rows.setdefault(turn, []).append(row)
            if m.get('method') in ('item/started', 'item/completed'):
                times = self.item_times.setdefault((turn, p['item']['id']), {})
                for key in ('startedAtMs', 'completedAtMs'):
                    if key in p:
                        times[key] = p[key]

    @staticmethod
    def print_frame(frame):
        print(json.dumps(frame, separators=(',', ':')), flush=True)

    def thread(self, ident):
        if ident == self.parent['id']:
            return self.parent
        if ident == self.child['id'] and self.has_child():
            return self.child
        raise ValueError('Unknown fixture thread')

    def has_child(self):
        return any(i['type'] in ('collabAgentToolCall', 'subAgentActivity')
                   for t in self.parent['turns'] for i in t['items'])

    def page(self, rows, params, scope, default_direction='desc'):
        direction = params.get('sortDirection') or default_direction
        limit = params.get('limit') or 50
        if direction not in ('asc', 'desc') or type(limit) is not int or not 1 <= limit <= 200:
            raise ValueError('Invalid fixture page size or direction')
        identity = tuple(scope)
        cursor = params.get('cursor')
        start = len(rows) - 1 if direction == 'desc' else 0
        if cursor is not None:
            if isinstance(cursor, dict) and scope[0] == 'items' and params.get('turnId'):
                matches = [n for n, row in enumerate(rows) if row['item']['id'] == cursor.get('itemId')]
                if cursor.get('type') != 'item' or len(matches) != 1:
                    raise ValueError('Invalid item anchor')
                start = matches[0] + (1 if direction == 'asc' else -1)
            elif isinstance(cursor, str) and cursor in self.cursors:
                owned_scope, anchor, expected_direction, inclusive = self.cursors[cursor]
                if owned_scope != identity or direction != expected_direction:
                    raise ValueError('Cursor belongs to a different fixture page')
                start = anchor if inclusive else anchor + (1 if direction == 'asc' else -1)
            else:
                raise ValueError('Unknown fixture cursor')
        step = 1 if direction == 'asc' else -1
        indexes = list(range(start, len(rows) if step == 1 else -1, step))[:limit] if 0 <= start < len(rows) else []
        def token(anchor, requested_direction, inclusive):
            key = json.dumps([identity, anchor, requested_direction, inclusive])
            value = 'fixture-page-' + hashlib.sha256(key.encode()).hexdigest()[:24]
            if len(self.cursors) >= 4096:
                del self.cursors[next(iter(self.cursors))]
            self.cursors[value] = (identity, anchor, requested_direction, inclusive)
            return value
        more = indexes and 0 <= indexes[-1] + step < len(rows)
        return {'data': deepcopy([rows[n] for n in indexes]),
                'nextCursor': token(indexes[-1], direction, False) if more else None,
                'backwardsCursor': token(indexes[0], 'asc' if direction == 'desc' else 'desc', scope[0] == 'turns') if indexes else None}

    def claude_page(self, rows, p):
        # Mirror bridge.mjs page: Claude invalidates a cursor when the native
        # transcript changes and does not offer Codex reverse/item anchors.
        fingerprint = hashlib.sha256(json.dumps(rows, separators=(',', ':'), ensure_ascii=False).encode()).hexdigest()[:16]
        cursor = p.get('cursor') or fingerprint + ':0'
        if not isinstance(cursor, str):
            raise ValueError('Claude history requires an opaque cursor')
        parts = cursor.split(':')
        if len(parts) != 2 or parts[0] != fingerprint or not parts[1].isdigit():
            raise ValueError('Claude history changed; reload the fixture')
        offset = int(parts[1]); limit = p.get('limit', 50)
        if offset > len(rows):
            raise ValueError('Claude history cursor is out of bounds')
        limit = max(1, min(limit, 200)) if type(limit) is int else 50
        return {'data': deepcopy(rows[offset:offset + limit]),
                'nextCursor': f'{fingerprint}:{offset + limit}' if offset + limit < len(rows) else None}

    def request(self, method, p):
        if method == 'initialize':
            if self.initialized:
                raise ValueError('Fixture was already initialized')
            self.initialized = True
            return {'userAgent': 'Wonder synthetic replay', 'capabilities': {'experimentalApi': True},
                    **({'wonderBridge': {'protocolVersion': 1, 'family': 'claude'}} if self.provider == 'claude' else {})}
        if not self.ready:
            raise ValueError('Initialize and send initialized before using the fixture')
        if method == 'account/read':
            return {'account': {'type': 'claudeSubscription' if self.provider == 'claude' else 'chatgpt',
                                'planType': 'unknown', 'email': None}, 'connected': True, 'requiresOpenaiAuth': False}
        if method == 'account/rateLimits/read':
            return {'rateLimits': {'primary': None, 'secondary': None, 'credits': None, 'planType': None}, 'rateLimitsByLimitId': {}}
        if method == 'model/list':
            model = 'claude:fixture-model' if self.provider == 'claude' else 'fixture-model'
            return {'data': [{'id': model, 'model': model, 'displayName': 'Synthetic replay (no model)',
                              'description': 'Recorded synthetic data only.', 'hidden': False,
                              'isDefault': True, 'supportedReasoningEfforts': [{'reasoningEffort': 'low', 'description': 'Recorded replay'}], 'defaultReasoningEffort': 'low'}],
                    'nextCursor': None}
        if method == 'configRequirements/read':
            return {'requirements': None}
        if method in ('app/list', 'skills/list', 'mcpServerStatus/list', 'project/list'):
            return {'data': [], 'nextCursor': None}
        if method == 'project/sessions/list' and self.provider == 'claude':
            rows = [{'sessionId': native_id('fixture-session'), 'cwd': self.parent['cwd'],
                     'title': 'Synthetic session replay', 'updatedAt': self.parent['updatedAt']}]
            if self.parent['cwd'] not in p.get('dirs', []) or native_id('fixture-session') in p.get('excludeSessionIds', []):
                rows = []
            if p.get('sessionId') not in (None, native_id('fixture-session')):
                rows = []
            if p.get('cursor') is not None:
                raise ValueError('This synthetic catalog has only one page')
            return {'data': rows, 'nextCursor': None, 'partial': False}
        if method == 'thread/list':
            rows = [self.parent] + ([self.child] if self.has_child() else [])
            kinds = p.get('sourceKinds')
            if kinds:
                rows = [t for t in rows if ('subAgentThreadSpawn' if t is self.child else t['source']) in kinds]
            if p.get('parentThreadId'):
                rows = [t for t in rows if t.get('parentThreadId') == p['parentThreadId']]
            rows = [t for t in rows if (self.archived and t is self.parent) == p.get('archived', False)]
            cwd = p.get('cwd')
            if cwd and self.parent['cwd'] not in ([cwd] if isinstance(cwd, str) else cwd):
                rows = []
            return self.page([{**t, 'turns': []} for t in rows], p, ('threads', json.dumps({k: p.get(k) for k in ('cwd', 'sourceKinds', 'parentThreadId', 'archived')}, sort_keys=True)), 'asc')
        if method == 'thread/loaded/list':
            return {'data': [self.parent['id']] if self.active else [], 'nextCursor': None}
        if method == 'project/session/attach' and self.provider == 'claude':
            if p.get('sessionId') not in (native_id('fixture-session'), self.parent['id']):
                raise ValueError('Unknown fixture session')
            return {'thread': deepcopy(self.parent)}
        thread = self.thread(p.get('threadId'))
        if method == 'thread/resume':
            if self.active:
                raise ValueError('Wait for the recorded turn before resuming')
            return {'thread': {**deepcopy(thread), 'turns': [] if p.get('excludeTurns') else deepcopy(thread['turns'])},
                    'model': 'fixture-model', 'modelProvider': 'openai' if self.provider == 'codex' else 'anthropic',
                    'cwd': thread['cwd'], 'approvalPolicy': 'on-request', 'approvalsReviewer': 'user',
                    'sandbox': {'type': 'workspaceWrite', 'writableRoots': [thread['cwd']], 'networkAccess': False,
                                'excludeTmpdirEnvVar': True, 'excludeSlashTmp': True}}
        if self.provider == 'claude' and method in ('thread/backgroundTasks/list', 'thread/backgroundTask/read'):
            task = {'id': native_id('fixture-child'), 'title': 'Check due dates', 'kind': 'agent', 'role': 'Reviewer',
                    'status': 'completed', 'canStop': False, 'runningElsewhere': False}
            if method == 'thread/backgroundTasks/list':
                return {'data': [task] if self.has_child() else []}
            if not self.has_child() or p.get('taskId') != task['id'] or thread is not self.parent:
                raise ValueError('Unknown fixture task')
            return {'task': task, 'items': deepcopy(self.child['turns'][0]['items'][1:])}
        if method == 'thread/read':
            return {'thread': {**deepcopy(thread), 'turns': deepcopy(thread['turns']) if p.get('includeTurns') is True else []}}
        if method == 'thread/turns/list':
            view = p.get('itemsView') or 'summary'
            if view not in ('notLoaded', 'summary', 'full'):
                raise ValueError('Unknown items view')
            # Full is a valid display summary; never truncate a completed item.
            rows = [{**t, 'items': [] if view == 'notLoaded' else t['items'],
                     'itemsView': view, 'itemCount': len(t['items'])} for t in thread['turns']]
            if self.provider == 'claude':
                return self.claude_page(list(reversed(rows)), p)
            return self.page(rows, p, ('turns', thread['id']))
        if method == 'thread/items/list':
            selected = [t for t in thread['turns'] if not p.get('turnId') or t['id'] == p['turnId']]
            if p.get('turnId') and not selected:
                raise ValueError('Unknown fixture turn')
            if self.provider == 'claude' and not p.get('turnId') and p.get('sortDirection') != 'asc':
                selected.reverse()
            rows = [{'turnId': t['id'], 'item': i, **(self.item_times.get((t['id'], i['id']), {}) if self.provider == 'codex' else {})} for t in selected for i in t['items']]
            if self.provider == 'claude':
                return self.claude_page(rows, p)
            return self.page(rows, p, ('items', thread['id'], p.get('turnId')), 'asc')
        if method == 'thread/goal/get':
            return {'goal': None}
        if method == 'thread/backgroundTerminals/list':
            return {'data': [], 'nextCursor': None}
        if method == 'turn/start':
            if thread is not self.parent or self.active or self.position >= len(self.all_turns) or self.archived:
                raise ValueError('No idle recorded turn is available')
            if p.get('input') != [{'type': 'text', 'text': TRIGGER}]:
                raise ValueError('Only the explicit synthetic replay trigger is accepted')
            turn = self.all_turns[self.position]
            if self.trace:
                self.trace({'event': 'turn_start', 'turnId': turn['id'], 'monotonicNs': time.monotonic_ns()})
            client_id = p.get('clientUserMessageId')
            if client_id is not None:
                if not isinstance(client_id, str) or not 1 <= len(client_id) <= 128:
                    raise ValueError('Invalid client message identity')
                def bind_client(value):
                    if isinstance(value, dict):
                        if value.get('type') == 'userMessage':
                            value['clientId'] = client_id
                        for child in value.values():
                            bind_client(child)
                    elif isinstance(value, list):
                        for child in value:
                            bind_client(child)
                bind_client(turn)
                bind_client(self.rows[turn['id']])
            self.interrupted = False
            self.active = asyncio.create_task(self.replay(turn))
            return {'turn': {**deepcopy(turn), 'status': 'inProgress', 'items': []}}
        if method == 'turn/interrupt':
            if not self.active or p.get('turnId') != self.all_turns[self.position]['id']:
                raise ValueError('No matching active recorded turn')
            self.interrupted = True
            if self.replaying:
                self.active.cancel()
            return {}
        if method in ('thread/archive', 'thread/unarchive'):
            if thread is not self.parent or self.active:
                raise ValueError('Only the idle fixture parent can be archived')
            self.archived = method == 'thread/archive'
            return {}
        raise ValueError('Method is not implemented by the synthetic replay host')

    def answer(self, message):
        pending = self.pending.get(message.get('id'))
        if pending is None or pending.done():
            raise ValueError('Unknown or already answered fixture approval')
        pending.set_result(message.get('result'))

    def apply(self, message):
        method, p = message.get('method'), message.get('params', {})
        if method == 'turn/started':
            self.parent['status'] = {'type': 'active', 'activeFlags': []}
            self.parent['turns'].append(deepcopy(p['turn']))
        elif method == 'turn/completed':
            self.parent['turns'][-1] = deepcopy(p['turn'])
            self.parent['status'] = {'type': 'idle'}
        elif method in ('item/started', 'item/completed'):
            items = self.parent['turns'][-1]['items']
            index = next((n for n, i in enumerate(items) if i['id'] == p['item']['id']), len(items))
            if index == len(items):
                items.append(deepcopy(p['item']))
            else:
                items[index] = deepcopy(p['item'])
        elif method == 'item/agentMessage/delta':
            item = next(i for i in self.parent['turns'][-1]['items'] if i['id'] == p['itemId'])
            item['text'] = item.get('text', '') + p['delta']

    async def replay(self, turn):
        self.replaying = True
        rows = self.rows[turn['id']]
        loop = asyncio.get_running_loop()
        origin, first = loop.time(), rows[0]['atMs']
        try:
            if self.interrupted:
                raise asyncio.CancelledError
            for row in rows:
                due = origin + (row['atMs'] - first) / 1000 / self.speed
                await asyncio.sleep(max(0, due - loop.time()))
                self.metrics['maxScheduleLagMs'] = max(self.metrics['maxScheduleLagMs'], round(max(0, loop.time() - due) * 1000, 3))
                message = row['message']
                if row['direction'] == 'client':
                    started = loop.time()
                    answer = await asyncio.wait_for(asyncio.shield(self.pending[message['id']]), 30)
                    origin += loop.time() - started
                    del self.pending[message['id']]
                    if answer != message['result']:
                        raise ValueError('Recorded approval answer differs; replay stopped')
                    self.metrics['approvalsAnswered'] += 1
                    continue
                if message.get('method', '').endswith('/requestApproval'):
                    self.pending[message['id']] = loop.create_future()
                self.apply(message)
                self.emit(message)
                self.metrics['serverRecords'] += 1
            self.metrics['turnsReplayed'] += 1
        except (asyncio.CancelledError, TimeoutError, ValueError):
            # A refused, missing or interrupted answer must never replay success.
            current = self.parent['turns'][-1] if self.parent['turns'] and self.parent['turns'][-1]['id'] == turn['id'] else {**deepcopy(turn), 'items': []}
            if not self.parent['turns'] or self.parent['turns'][-1]['id'] != current['id']:
                self.parent['turns'].append(current)
            current.update(status='interrupted', error=None)
            self.parent['status'] = {'type': 'idle'}
            self.emit({'method': 'turn/completed', 'params': {'threadId': self.parent['id'], 'turn': current}})
        finally:
            for future in self.pending.values():
                future.cancel()
            self.pending.clear()
            self.position += 1
            self.active = None
            self.replaying = False

    async def serve(self):
        reader = asyncio.StreamReader(limit=MAX_REQUEST)
        await asyncio.get_running_loop().connect_read_pipe(lambda: asyncio.StreamReaderProtocol(reader), sys.stdin)
        try:
            while line := await reader.readline():
                if len(line) > MAX_REQUEST:
                    raise ValueError('Fixture request is too large')
                message = json.loads(line)
                if not isinstance(message, dict):
                    raise ValueError('Fixture request must be an object')
                try:
                    if message.get('method') == 'initialized' and 'id' not in message and self.initialized:
                        self.ready = True
                    elif 'method' not in message:
                        self.answer(message)
                    elif 'id' in message:
                        self.emit({'id': message['id'], 'result': self.request(message['method'], message.get('params', {}))})
                    else:
                        raise ValueError('Unsupported fixture notification')
                except (ValueError, KeyError, TypeError) as error:
                    if 'method' in message and 'id' in message:
                        self.emit({'id': message['id'], 'error': {'code': -32602, 'message': str(error)}})
                    else:
                        raise ValueError('Invalid fixture response or notification') from error
        finally:
            if self.active:
                self.active.cancel()
                await self.active


def prepare(args):
    bundle = read_bundle(args.bundle)
    ReplayHost(deepcopy(bundle), Path('/fixture/todo'), args.history_turns, args.speed)
    target = args.output.absolute()
    if '.local' not in target.parts or target.exists() or target.is_symlink():
        raise ValueError('Prepare requires a new directory beneath .local')
    target.mkdir(parents=True, mode=0o700)
    workspace = target / 'workspace'
    workspace.mkdir(mode=0o700)
    for asset in bundle['assets']:
        (workspace / asset['name']).write_bytes(base64.b64decode(asset['base64'], validate=True))
    # Copy only already-validated synthetic data. Source session paths never enter
    # a launcher; runtime startup verifies these exact bytes before serving them.
    raw = json.dumps(bundle, separators=(',', ':')).encode()
    (target / 'bundle.json').write_bytes(raw)
    config = {'bundle': 'bundle.json', 'bundleSha256': hashlib.sha256(raw).hexdigest(), 'workspace': 'workspace', 'historyTurns': args.history_turns, 'speed': args.speed,
              'threadId': native_identities('fixture-' + bundle['provider']), 'childThreadId': native_id('fixture-child'),
              'nativeSessionId': native_id('fixture-session' if bundle['provider'] == 'claude' else 'fixture-codex')}
    if getattr(args, 'timing', False):
        config['timing'] = True
        (target / 'runtime-timing').mkdir(mode=0o700)
    (target / 'config.json').write_text(json.dumps(config))
    launcher = target / 'replay-runtime'
    launcher.write_text('#!' + sys.executable + '\nimport runpy, sys\nsys.argv = [' + repr(str(Path(__file__).resolve())) + ', "--runtime-config", ' + repr(str(target / 'config.json')) + '] + sys.argv[1:]\nrunpy.run_path(sys.argv[0], run_name="__main__")\n')
    launcher.chmod(0o700)
    helper = target / 'codex-code-mode-host'
    shutil.copyfile(ROOT / 'tests/fixtures/code-mode-host.py', helper)
    helper.chmod(0o700)
    print(json.dumps({'launcher': str(launcher), 'workspace': str(workspace), 'provider': bundle['provider'],
                      'historyTurns': args.history_turns, 'speed': args.speed, 'modelCalls': 0}))


def main():
    os.umask(0o077)
    parser = argparse.ArgumentParser(description=__doc__, allow_abbrev=False)
    parser.add_argument('--bundle', type=Path)
    parser.add_argument('--output', type=Path)
    parser.add_argument('--history-turns', type=int)
    parser.add_argument('--speed', type=float, default=1, help='1 preserves recorded timing; acceleration is labeled, never a latency baseline')
    parser.add_argument('--runtime-config', type=Path)
    args, runtime_args = parser.parse_known_args()
    try:
        if not args.runtime_config:
            if runtime_args or not args.bundle or not args.output:
                parser.error('Prepare with --bundle and --output')
            prepare(args)
            return 0
        if '--version' in runtime_args:
            print('codex-cli ' + VERSION)
            return 0
        if runtime_args[:2] == ['app-server', 'generate-json-schema']:
            output = Path(runtime_args[runtime_args.index('--out') + 1])
            variant = 'experimental' if '--experimental' in runtime_args else 'stable'
            output.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(ROOT / 'research/codex-app-server' / VERSION / variant / 'codex_app_server_protocol.v2.schemas.json', output / 'codex_app_server_protocol.v2.schemas.json')
            return 0
        config = json.loads(args.runtime_config.read_text())
        root = args.runtime_config.parent
        # Optional benchmark evidence belongs only to this synthetic runtime.
        # It records two timing boundaries per turn, never message contents.
        trace = None
        if config.get('timing') is True:
            directory = (root / 'runtime-timing').resolve()
            if '.local' not in directory.parts or not directory.is_dir():
                raise ValueError('Timing output must be an existing private directory')
            trace = (directory / (str(os.getpid()) + '.jsonl')).open('x')
        try:
            def record(value):
                value['provider'] = host.provider
                trace.write(json.dumps(value, separators=(',', ':')) + '\n')
                trace.flush()
            host = ReplayHost(read_prepared(root, config), root / config['workspace'], config['historyTurns'], config['speed'],
                              trace=record if trace else None)
            asyncio.run(host.serve())
        finally:
            if trace:
                trace.close()
        (root / 'replay-report.json').write_text(json.dumps(host.metrics, indent=2))
        return 0
    except (ValueError, OSError, KeyError, TypeError) as error:
        # Input can be private/untrusted. Never print values or paths on rejection.
        print('Synthetic replay failed: ' + type(error).__name__, file=sys.stderr)
        return 1


if __name__ == '__main__':
    sys.exit(main())

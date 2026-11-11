#!/usr/bin/env python3
"""Create synthetic Project threads from content-free local session measurements."""
import argparse
import base64
from collections import Counter
from copy import deepcopy
from datetime import datetime
import hashlib
import json
import os
from pathlib import Path
import random
import re
import subprocess
import struct
import sys
import zlib

ROOT = Path(__file__).resolve().parents[1]
SCHEMA = ROOT / 'research/codex-app-server/0.162.0-alpha.17.2/experimental/codex_app_server_protocol.v2.schemas.json'
PROFILE = ROOT / 'scripts/fixtures/session-shapes.json'
KINDS = ('userMessage', 'agentMessage', 'reasoning', 'commandExecution', 'fileChange',
         'dynamicToolCall', 'mcpToolCall', 'collabAgentToolCall', 'plan', 'webSearch', 'imageView')
MAX_TEXT = 65536
EPOCH = 1800000000
# These bytes are generated assets, never values read from a source session.
def preview_png():
    def chunk(kind, data):
        return struct.pack('!I', len(data)) + kind + data + struct.pack('!I', zlib.crc32(kind + data))
    pixels = b''.join(b'\0' + bytes((245, 190 if y < 24 else 225, 85)) * 96 for y in range(64))
    return b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('!2I5B', 96, 64, 8, 2, 0, 0, 0)) + chunk(b'IDAT', zlib.compress(pixels)) + chunk(b'IEND', b'')

PNG = preview_png()
ASSETS = {'preview.png': PNG, 'notes.txt': b'Synthetic todo notes\n- Add due dates\n- Toggle completed items\n'}
SAFE_FILES = {'todos.js', 'todos.test.js', 'README.md', *ASSETS}


def privacy_scan(value):
    """Fail without echoing a matched value. Scan keys, values and filenames."""
    patterns = {
        'home path': r'(?i)(?:/(?:Users|home)/|[A-Z]:[\\/]Users[\\/]|file://)',
        'address': r'(?i)[\w.+-]+@[\w.-]+\.[a-z]{2,}',
        'credential': r'(?i)(?:\b(?:sk|ghp|gho|github_pat|xox[baprs])-?[A-Za-z0-9_-]{12,}|Bearer\s+\S+|-----BEGIN[\w ]*PRIVATE KEY|(?:api[_ -]?key|access[_ -]?token|password|secret)\s*[:=]\s*\S+)',
        'phone': r'(?<!\w)(?:\+?\d[ ().-]*){7,15}(?!\w)',
        'network address': r'(?i)(?:https?://|ssh://|\.ts\.net\b|\btail[a-z0-9-]*\b|\b(?:\d{1,3}\.){3}\d{1,3}\b|\b(?:[a-f0-9]{1,4}:){3,}[a-f0-9:]+\b)',
        'opaque token': r'(?<![\w])[A-Za-z0-9_+/=-]{48,}(?![\w])',
    }
    def visit(obj, key=None):
        if isinstance(obj, dict):
            for k, v in obj.items():
                visit(k)
                if k.lower().replace('_', '') in ('apikey', 'password', 'secret', 'token', 'accesstoken'):
                    raise ValueError('Privacy scan: credential field')
                # Only the exact generated asset encoding/hash is exempt.
                if k in ('base64', 'sha256'):
                    allowed = {base64.b64encode(b).decode() if k == 'base64' else hashlib.sha256(b).hexdigest() for b in ASSETS.values()}
                    if v not in allowed: raise ValueError('Privacy scan: unknown asset encoding')
                else: visit(v, k)
        elif isinstance(obj, list):
            for v in obj: visit(v, key)
        elif isinstance(obj, str):
            text = obj
            for label, pattern in patterns.items():
                if re.search(pattern, text): raise ValueError('Privacy scan: ' + label)
            for host in re.findall(r'(?i)\b(?:[a-z][a-z0-9-]*\.)+[a-z]{2,63}\b', text):
                if host not in SAFE_FILES: raise ValueError('Privacy scan: hostname')
            if key and key.lower() in ('host', 'hostname', 'domain'):
                raise ValueError('Privacy scan: hostname field')
    visit(value)


def privacy_filename(name):
    if '/' in name or '\\' in name or name in ('.', '..'):
        raise ValueError('Privacy scan: invalid filename')
    privacy_scan(name.rsplit('.', 1)[0] if name.endswith(('.json', '.png', '.txt')) else name)


def timestamp(value):
    try: return int(datetime.fromisoformat(value.replace('Z', '+00:00')).timestamp() * 1000)
    except (ValueError, TypeError, AttributeError, OverflowError): return None


def tool_kind(name, provider):
    # Unknown names enter a reviewed generic category; names/arguments never escape.
    name = str(name).lower()
    if name in ('bash', 'exec_command', 'functions.exec_command', 'shell', 'shell_command'): return 'commandExecution'
    if name in ('apply_patch', 'functions.apply_patch', 'write', 'edit', 'multiedit'): return 'fileChange'
    if name in ('update_plan', 'functions.update_plan', 'exitplanmode'): return 'plan'
    if name in ('agent', 'task') or name.rsplit('.', 1)[-1] in ('spawn_agent', 'send_message', 'followup_task', 'wait_agent', 'list_agents', 'interrupt_agent'): return 'collabAgentToolCall'
    if provider == 'claude': return 'mcpToolCall'
    if name.startswith('mcp'): return 'mcpToolCall'
    if name in ('web.run', 'web__run', 'web_search'): return 'webSearch'
    if name in ('view_image', 'functions.view_image'): return 'imageView'
    return 'dynamicToolCall'


def text_length(content):
    if isinstance(content, str): return len(content)
    if not isinstance(content, list): return 0
    return sum(len(str(b.get('text', ''))) for b in content if isinstance(b, dict) and b.get('type') in ('text', 'input_text', 'output_text'))


def sample(root, provider, limit=24, byte_limit=4 * 1024 * 1024):
    """Bound reads and retain only numeric sizes, fixed enums, and item order."""
    files = sorted(p for p in root.rglob('*.jsonl') if p.is_file() and not p.is_symlink())
    if not files: raise ValueError('No session files for ' + provider)
    part = max(1, limit // 3)
    selected = list(dict.fromkeys(sorted(files, key=lambda p: p.stat().st_mtime, reverse=True)[:part]
                    + sorted(files, key=lambda p: p.stat().st_size, reverse=True)[:part]
                    + [files[int(i * (len(files) - 1) / max(1, part - 1))] for i in range(part)]))[:limit]
    counts = Counter(); templates = []; total = truncated = invalid = 0; streams = 0
    for path in selected:
        with path.open('rb') as handle: raw = handle.read(byte_limit)
        total += len(raw); partial = path.stat().st_size > len(raw); truncated += partial
        if partial: raw = raw.rsplit(b'\n', 1)[0]
        current = []; calls = {}; previous = None
        def append(kind, length, at, phase='final_answer'):
            nonlocal previous
            gap = min(3600000, max(0, at - previous)) if at is not None and previous is not None else 0
            if at is not None: previous = at
            # [kind, text/argument length, result length, gap ms, duration ms, failed, phase]
            row = [kind, min(MAX_TEXT, length), 0, gap, 0, False, phase]
            current.append(row); counts[kind] += 1
            return row
        def user(length, at):
            nonlocal current, previous
            if current: templates.append(current)
            current = []; previous = at
            append('userMessage', length, at)
        for line in raw.splitlines():
            try: record = json.loads(line)
            except (ValueError, UnicodeDecodeError): invalid += 1; continue
            if not isinstance(record, dict): invalid += 1; continue
            at = timestamp(record.get('timestamp')); kind = record.get('type')
            streams += kind == 'stream_event'
            if provider == 'codex':
                payload = record.get('payload', {})
                if kind != 'response_item' or not isinstance(payload, dict): continue
                kind = payload.get('type')
                if kind == 'message':
                    role = payload.get('role'); content = payload.get('content', [])
                    if role == 'user':
                        first = next((str(b.get('text', '')) for b in content if isinstance(b, dict)), '')
                        if first.startswith(('# AGENTS.md', '<environment_context>', '<permissions instructions>', '<user_instructions>', '<turn_aborted>')): continue
                        user(text_length(content), at)
                    elif role == 'assistant' and current and payload.get('channel') not in ('analysis', 'summary'):
                        append('agentMessage', text_length(content), at, 'commentary' if payload.get('phase', payload.get('channel')) == 'commentary' else 'final_answer')
                elif kind in ('function_call', 'custom_tool_call') and current:
                    row = append(tool_kind(payload.get('name'), provider), len(str(payload.get('arguments', payload.get('input', '')))), at)
                    row[5] = None
                    calls[payload.get('call_id')] = (row, at)
                elif kind in ('function_call_output', 'custom_tool_call_output'):
                    call = calls.get(payload.get('call_id'))
                    if call:
                        row, start = call; row[2] = min(MAX_TEXT, len(str(payload.get('output', '')))); row[5] = False
                        row[4] = min(3600000, max(0, at - start)) if at is not None and start is not None else 0
                elif kind == 'reasoning' and current: append('reasoning', 0, at)
            else:
                payload = record.get('message', {})
                if not isinstance(payload, dict): continue
                content = payload.get('content', [])
                if isinstance(content, str): content = [{'type': 'text', 'text': content}]
                if not isinstance(content, list): continue
                results = [b for b in content if isinstance(b, dict) and b.get('type') == 'tool_result']
                if kind == 'user' and not results and record.get('origin', {}).get('kind', 'human') == 'human' and text_length(content):
                    user(text_length(content), at)
                for block in content:
                    if not isinstance(block, dict): continue
                    if kind == 'assistant' and current and block.get('type') == 'text': append('agentMessage', len(block.get('text', '')), at)
                    elif kind == 'assistant' and current and block.get('type') == 'tool_use':
                        row = append(tool_kind(block.get('name'), provider), len(json.dumps(block.get('input', {}))), at)
                        row[5] = None
                        calls[block.get('id')] = (row, at)
                    elif block.get('type') == 'tool_result' and block.get('tool_use_id') in calls:
                        row, start = calls[block['tool_use_id']]; row[2] = min(MAX_TEXT, len(json.dumps(block.get('content', '')))); row[5] = block.get('is_error') is True
                        row[4] = min(3600000, max(0, at - start)) if at is not None and start is not None else 0
        if current and not partial: templates.append(current)
    # Reservoir over complete sampled turns, no source identifier or filename.
    eligible = [t for t in templates if len(t) > 1]
    if not eligible: raise ValueError('No complete sampled turns for ' + provider)
    rng = random.Random(71)
    chosen = rng.sample(eligible, min(48, len(eligible)))
    clipped = sum(len(t) > 96 for t in chosen)
    chosen = [t[:96] for t in chosen]
    result = {'sampledFiles': len(selected), 'availableFiles': len(files), 'bytesRead': total,
              'truncatedFiles': truncated, 'invalidRecords': invalid, 'observedStreamRecords': streams,
              'completeTurns': len(eligible), 'clippedTemplates': clipped, 'observedItemCounts': dict(sorted(counts.items())),
              'templates': chosen}
    privacy_scan(result)
    return result


def validate_profile(profile):
    if set(profile) != {'version', 'codex', 'claude'} or profile['version'] != 1: raise ValueError('Invalid shape profile')
    for provider in ('codex', 'claude'):
        source = profile[provider]
        if set(source) != {'sampledFiles', 'availableFiles', 'bytesRead', 'truncatedFiles', 'invalidRecords', 'observedStreamRecords', 'completeTurns', 'clippedTemplates', 'observedItemCounts', 'templates'}: raise ValueError('Invalid profile fields')
        for key, value in source.items():
            if key not in ('templates', 'observedItemCounts') and (type(value) is not int or value < 0): raise ValueError('Invalid profile count')
        if any(k not in KINDS or type(v) is not int or v < 0 for k, v in source['observedItemCounts'].items()): raise ValueError('Invalid profile kind')
        if not 1 <= len(source['templates']) <= 48: raise ValueError('Invalid template count')
        for template in source['templates']:
            if not 2 <= len(template) <= 96 or template[0][0] != 'userMessage': raise ValueError('Invalid turn shape')
            for row in template:
                if not isinstance(row, list) or len(row) != 7 or row[0] not in KINDS or row[6] not in ('commentary', 'final_answer') or (row[5] is not None and type(row[5]) is not bool): raise ValueError('Invalid shape row')
                if any(type(v) is not int or not 0 <= v <= cap for v, cap in zip(row[1:5], (MAX_TEXT, MAX_TEXT, 3600000, 3600000))): raise ValueError('Invalid shape measurement')
    privacy_scan(profile)


def profile_json(profile):
    """Keep each bounded numeric template on one reviewable line."""
    lines = ['{', '  "version": 1,']
    for index, provider in enumerate(('codex', 'claude')):
        lines.append('  ' + json.dumps(provider) + ': {')
        for key, value in profile[provider].items():
            if key != 'templates': lines.append('    ' + json.dumps(key) + ': ' + json.dumps(value, separators=(',', ':')) + ',')
        lines.append('    "templates": [')
        templates = profile[provider]['templates']
        lines.extend('      ' + json.dumps(t, separators=(',', ':')) + (',' if i < len(templates) - 1 else '') for i, t in enumerate(templates))
        lines.extend(['    ]', '  }' + (',' if index == 0 else '')])
    return '\n'.join([*lines, '}']) + '\n'


def prose(length, index, style='text'):
    if style == 'markdown':
        unit = f'### Todo review {index}\n\nThe list keeps due dates beside each task. Completed items retain their notes.\n\n| Task | State |\n| --- | --- |\n| Read a chapter | Open |\n| Water plants | Done |\n\n```javascript\nconst completed = false;\n```\n\n'
    else: unit = f'Todo review {index}: keep due dates, completion state and notes readable. The synthetic list uses local sample tasks.\n'
    return (unit * (max(1, length) // len(unit) + 1))[:max(1, length)]


def item_for(row, ident, index, provider):
    kind, length, output, _, duration, failed, phase = row
    item = {'type': kind, 'id': ident}
    # A missing result is not evidence of success; use a settled failure fixture.
    status = 'failed' if failed is not False else 'completed'
    if kind == 'userMessage': item['content'] = [{'type': 'text', 'text': prose(length, index)}]
    elif kind == 'agentMessage': item.update(text=prose(length, index, 'markdown'), phase=phase)
    elif kind == 'reasoning': item.update(summary=[], content=[])
    elif kind == 'commandExecution':
        command = 'node --test todos.test.js'
        if length > len(command) + 3:
            command += ' # ' + prose(length - len(command) - 3, index).replace('\n', ' ')
        item.update(command=command, cwd='/fixture/todo', commandActions=[{'type': 'unknown', 'command': command}], status=status, aggregatedOutput=prose(output, index), exitCode=1 if status == 'failed' else 0, durationMs=duration)
    elif kind == 'fileChange': item.update(status=status, changes=[{'path': 'todos.js', 'kind': {'type': 'update'}, 'diff': '@@ -1 +1 @@\n-const done = false;\n+const done = true;\n' + ('+// Synthetic todo detail\n' * (length // 25))}])
    elif kind == 'plan': item['text'] = prose(max(length, 120), index, 'markdown')
    elif kind in ('dynamicToolCall', 'mcpToolCall'):
        item.update(arguments={'note': prose(length, index)}, status=status, tool='read_fixture', durationMs=duration)
        if kind == 'mcpToolCall': item.update(server='fixture', result={'content': [{'type': 'text', 'text': prose(output, index)}]}, error={'message': 'Synthetic tool failure'} if status == 'failed' else None)
        else: item.update(contentItems=[{'type': 'inputText', 'text': prose(output, index)}], success=status != 'failed')
    elif kind == 'collabAgentToolCall': item.update(tool='spawnAgent', status='completed', senderThreadId='fixture-' + provider, receiverThreadIds=['fixture-child'], agentsStates={'fixture-child': {'status': 'completed', 'message': 'Checked the synthetic todo list.'}}, prompt=prose(length, index))
    elif kind == 'webSearch': item.update(query='synthetic todo accessibility', action={'type': 'search', 'query': 'synthetic todo accessibility', 'queries': None})
    elif kind == 'imageView': item['path'] = '/fixture/todo/preview.png'
    return item


def make_turns(profile, provider, count, seed):
    rng = random.Random(seed); turns = []; timing = {}
    for n in range(count):
        shape = deepcopy(rng.choice(profile[provider]['templates']))
        # Auditable coverage additions, never claimed as observed frequency.
        if n % 32 == 0:
            shape += [[kind, 180 if kind != 'agentMessage' else 32768, 320, 100, 200, False, 'final_answer'] for kind in ('plan', 'commandExecution', 'fileChange', 'mcpToolCall', 'collabAgentToolCall', 'imageView', 'agentMessage') if provider == 'codex' or kind != 'imageView']
        items = []
        for i, row in enumerate(shape):
            ident = f'{provider}-item-t{n:04}-i{i:03}'
            item = item_for(row, ident, n, provider)
            items.append(item); timing[ident] = {'gapMs': row[3], 'durationMs': row[4]}
        if n % 32 == 0:
            items[0]['content'] += [{'type': 'localImage', 'path': '/fixture/todo/preview.png'}, {'type': 'mention', 'name': 'notes.txt', 'path': '/fixture/todo/notes.txt'}]
        if not any(i['type'] == 'agentMessage' for i in items):
            ident = f'{provider}-reply-{n:04}'; items.append({'type': 'agentMessage', 'id': ident, 'text': 'The synthetic todo review is complete.', 'phase': 'final_answer'}); timing[ident] = {'gapMs': 0, 'durationMs': 0}
        status = 'failed' if n % 61 == 60 else 'interrupted' if n % 47 == 46 else 'completed'
        turns.append({'id': f'{provider}-turn-{n:04}', 'status': status, 'items': items, 'error': {'message': 'Synthetic interrupted work' if status == 'interrupted' else 'Synthetic connection failure'} if status != 'completed' else None})
    return turns, timing


def codex_replay(turns, timing, chunk_chars):
    replay = []; at = 0
    def emit(method, params, delay=0, request_id=None, direction='server'):
        nonlocal at
        at += delay
        message = {'method': method, 'params': params} if method else {'id': request_id, 'result': params}
        if method and request_id: message['id'] = request_id
        replay.append({'atMs': at, 'direction': direction, 'message': message})
    for n, turn in enumerate(turns):
        base = {'threadId': 'fixture-codex', 'turnId': turn['id']}
        turn_start = source_at = latest = at
        turn['startedAt'] = EPOCH + at // 1000
        emit('turn/started', {'threadId': base['threadId'], 'turn': {'id': turn['id'], 'status': 'inProgress', 'items': []}})
        for item in turn['items']:
            measured = timing[item['id']]
            source_at += measured['gapMs']; at = source_at
            text_delay = 0
            if item['type'] == 'agentMessage' and chunk_chars:
                chunks = (len(item['text']) + chunk_chars - 1) // chunk_chars
                # Logs retain a completed text record, not token timestamps.
                # Fit modeled chunks inside its preceding observed gap.
                text_delay = min(35, measured['gapMs'] // chunks)
                at -= chunks * text_delay
            started = deepcopy(item)
            if 'status' in started: started['status'] = 'inProgress'
            if item['type'] == 'agentMessage': started['text'] = ''
            if item['type'] == 'commandExecution': started.update(aggregatedOutput=None, exitCode=None, durationMs=None)
            if item['type'] == 'mcpToolCall': started.update(result=None, error=None, durationMs=None)
            if item['type'] == 'dynamicToolCall': started.update(contentItems=None, success=None, durationMs=None)
            if item['type'] == 'collabAgentToolCall': started['agentsStates'] = {'fixture-child': {'status': 'pendingInit'}}
            emit('item/started', {**base, 'item': started, 'startedAtMs': EPOCH * 1000 + at})
            if item['type'] in ('commandExecution', 'fileChange') and n % 32 == 0:
                request_id = 'approval-' + item['id']; method = 'item/' + item['type'] + '/requestApproval'
                params = {**base, 'itemId': item['id'], 'reason': 'Allow the synthetic todo change?'}
                if item['type'] == 'commandExecution': params.update(command=item['command'], cwd='/fixture/todo', availableDecisions=['accept', 'decline', 'cancel'])
                emit(method, params, request_id=request_id)
                emit(None, {'decision': 'accept'}, 250, request_id, 'client')
                emit('serverRequest/resolved', {'threadId': base['threadId'], 'requestId': request_id})
            if item['type'] == 'agentMessage':
                size = chunk_chars or len(item['text'])
                for start in range(0, len(item['text']), size): emit('item/agentMessage/delta', {**base, 'itemId': item['id'], 'delta': item['text'][start:start+size]}, text_delay)
            if item['type'] == 'plan': emit('turn/plan/updated', {**base, 'plan': [{'step': 'Review the todo list', 'status': 'completed'}, {'step': 'Check due dates', 'status': 'inProgress'}]})
            if item['type'] == 'fileChange': emit('turn/diff/updated', {**base, 'diff': item['changes'][0]['diff']})
            at = max(at, source_at + measured['durationMs'])
            if item['type'] == 'commandExecution': emit('item/commandExecution/outputDelta', {**base, 'itemId': item['id'], 'delta': item['aggregatedOutput']})
            emit('item/completed', {**base, 'item': deepcopy(item), 'completedAtMs': EPOCH * 1000 + at})
            latest = max(latest, at)
        at = latest
        if n % 59 == 58: emit('error', {**base, 'error': {'message': 'Synthetic transient failure'}, 'willRetry': True}, 200)
        turn.update(completedAt=EPOCH + at // 1000, durationMs=at - turn_start)
        emit('turn/completed', {'threadId': base['threadId'], 'turn': deepcopy(turn)})
    return sorted(replay, key=lambda row: row['atMs'])


def generate(profile, provider, count=320, seed=47, chunk_chars=96):
    validate_profile(profile)
    if provider not in ('codex', 'claude') or not 1 <= count <= 1000 or not 0 <= chunk_chars <= 4096: raise ValueError('Fixture bounds exceeded')
    turns, timing = make_turns(profile, provider, count, seed)
    if provider == 'claude':
        payload = json.dumps({'turns': turns, 'timing': timing, 'chunkChars': chunk_chars, 'epoch': EPOCH})
        process = subprocess.run(['node', str(ROOT / 'scripts/project-claude-fixtures.mjs')], input=payload, text=True, capture_output=True, timeout=90, cwd=ROOT)
        if process.returncode: raise ValueError('Offline Claude projection failed; no source content was printed')
        projected = json.loads(process.stdout); read = projected['threadRead']; replay = projected['replay']
    else:
        replay = codex_replay(turns, timing, chunk_chars)
        read = {'thread': {'id': 'fixture-codex', 'sessionId': 'fixture-session', 'projectId': 'fixture-project', 'cwd': '/fixture/todo', 'cliVersion': '0.162.0-alpha.17.2', 'createdAt': EPOCH, 'updatedAt': EPOCH + replay[-1]['atMs'] // 1000, 'ephemeral': True, 'modelProvider': 'openai', 'source': 'appServer', 'status': {'type': 'idle'}, 'preview': 'Synthetic todo project history', 'turns': turns}}
    bundle = {'version': 1, 'provider': provider, 'project': {'id': 'fixture-project', 'name': 'Todo study', 'cwd': '/fixture/todo'}, 'threadRead': read, 'replay': replay,
              'assets': [{'name': name, 'mimeType': 'image/png' if name.endswith('.png') else 'text/plain', 'byteSize': len(data), 'sha256': hashlib.sha256(data).hexdigest(), 'base64': base64.b64encode(data).decode()} for name, data in ASSETS.items()],
              'provenance': {'shape': 'sampled bounded complete turns', 'coverage': 'authored every 32 turns: plan, command, edit, tool, subagent, long reply and attachments; failed and interrupted outcomes plus approvals are authored', 'timing': 'observed item gaps and call durations; modeled chunks fit the preceding gap with a maximum delay of 35 ms; approval delays are authored', 'observedStreamRecords': profile[provider]['observedStreamRecords'], 'chunkChars': chunk_chars, 'maxChunkDelayMs': 35 if chunk_chars else 0, 'seed': seed}}
    validate_bundle(bundle)
    return bundle


def validate_bundle(bundle):
    privacy_scan(bundle)
    if len(bundle['assets']) != len(ASSETS): raise ValueError('Unexpected asset count')
    for asset in bundle['assets']:
        name = asset['name']; data = ASSETS.get(name)
        privacy_filename(name)
        if data is None or asset['base64'] != base64.b64encode(data).decode() or asset['sha256'] != hashlib.sha256(data).hexdigest() or asset['byteSize'] != len(data): raise ValueError('Asset integrity failed')
        if asset['mimeType'] != ('image/png' if name.endswith('.png') else 'text/plain'): raise ValueError('Asset MIME mismatch')
    turns = bundle['threadRead']['thread']['turns']
    if bundle['provider'] == 'codex':
        try: from jsonschema import Draft7Validator
        except ImportError as exc: raise ValueError('Install jsonschema for mandatory pinned-schema validation') from exc
        schema = json.loads(SCHEMA.read_text()); defs = schema['definitions']
        def check(name, obj):
            errors = Draft7Validator({'$ref': '#/definitions/' + name, 'definitions': defs}).iter_errors(obj)
            if next(errors, None) is not None: raise ValueError('Pinned schema rejected ' + name)
        check('ThreadReadResponse', bundle['threadRead'])
        variants = {v['properties']['method']['enum'][0]: v['properties']['params']['$ref'].rsplit('/', 1)[-1] for v in defs['ServerNotification']['oneOf']}
        for row in bundle['replay']:
            msg = row['message']; method = msg.get('method')
            if method in variants: check(variants[method], msg['params'])
            elif method and method not in ('item/commandExecution/requestApproval', 'item/fileChange/requestApproval'): raise ValueError('Unreviewed server request')
    seen = set(); active = {}; texts = {}; terminal = {}; ended = {}; requests = set(); previous = -1
    for row in bundle['replay']:
        if row['direction'] not in ('server', 'client'): raise ValueError('Invalid replay direction')
        if type(row['atMs']) is not int or row['atMs'] < previous: raise ValueError('Replay time moved backwards')
        previous = row['atMs']; msg = row['message']; method = msg.get('method'); params = msg.get('params', {})
        if row['direction'] == 'client':
            if msg.get('id') not in requests or msg.get('result') != {'decision': 'accept'}: raise ValueError('Unmatched approval answer')
            requests.remove(msg['id']); continue
        if method and method.endswith('/requestApproval'):
            if msg['id'] in requests: raise ValueError('Duplicate approval')
            if params.get('itemId') not in active.get(params.get('turnId'), set()): raise ValueError('Approval has no active item')
            requests.add(msg['id'])
        elif method == 'turn/started':
            if params['turn']['id'] in active: raise ValueError('Duplicate turn')
            active[params['turn']['id']] = set()
        elif method == 'item/started':
            item = params['item']; ident = item['id']
            if ident in seen or params['turnId'] not in active: raise ValueError('Duplicate or unowned item')
            seen.add(ident); active[params['turnId']].add(ident)
            if item['type'] == 'agentMessage': texts[ident] = item.get('text', '')
        elif method == 'item/agentMessage/delta':
            if params['itemId'] not in texts: raise ValueError('Delta before item')
            texts[params['itemId']] += params['delta']
        elif method == 'item/completed':
            item = params['item']; ident = item['id']
            if ident not in active.get(params['turnId'], set()):
                # Claude restates completed text when it becomes commentary.
                if terminal.get(ident, {}).get('type') != 'agentMessage': raise ValueError('Unmatched completion')
            else: active[params['turnId']].remove(ident)
            if ident in texts and texts[ident] != item['text']: raise ValueError('Stream differs from final text')
            terminal[ident] = item
        elif method == 'turn/completed':
            if params['turn']['id'] not in active: raise ValueError('Unknown terminal turn')
            if active.pop(params['turn']['id']): raise ValueError('Turn ended with unfinished items')
            ended[params['turn']['id']] = params['turn']
    if active or requests: raise ValueError('Unfinished replay lifecycle')
    if set(terminal) != {i['id'] for t in turns for i in t['items']}: raise ValueError('History item ownership differs')
    for turn in turns:
        if ended.get(turn['id']) != turn: raise ValueError('History turn differs from replay')
        for item in turn['items']:
            if terminal.get(item['id']) != item: raise ValueError('History differs from replay')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--sample', action='store_true', help='Read bounded private logs and save content-free shape profile')
    parser.add_argument('--codex-root', type=Path, default=Path.home() / '.codex/sessions')
    parser.add_argument('--claude-root', type=Path, default=Path.home() / '.claude/projects')
    parser.add_argument('--profile', type=Path, default=PROFILE)
    parser.add_argument('--output', type=Path, default=ROOT / '.local/session-fixtures')
    parser.add_argument('--sessions', type=int, default=24)
    parser.add_argument('--bytes-per-session', type=int, default=4 * 1024 * 1024)
    parser.add_argument('--turns', type=int, default=320)
    parser.add_argument('--seed', type=int, default=47)
    parser.add_argument('--chunk-chars', type=int, default=96, help='0 disables modeled chunking; otherwise at most 35 ms per chunk')
    args = parser.parse_args()
    try:
        os.umask(0o077)
        if args.sample:
            if not 3 <= args.sessions <= 60 or not 1024 <= args.bytes_per_session <= 8 * 1024 * 1024: raise ValueError('Sampling bounds exceeded')
            profile = {'version': 1, **{p: sample(getattr(args, p + '_root'), p, args.sessions, args.bytes_per_session) for p in ('codex', 'claude')}}
            validate_profile(profile); privacy_filename(args.profile.name)
            args.profile.parent.mkdir(parents=True, exist_ok=True)
            if args.profile.is_symlink(): raise ValueError('Profile must not be a symlink')
            args.profile.write_text(profile_json(profile))
            print(json.dumps({p: {k: v for k, v in profile[p].items() if k != 'templates'} for p in ('codex', 'claude')}, indent=2))
        else:
            profile = json.loads(args.profile.read_text()); bundles = {p: generate(profile, p, args.turns, args.seed, args.chunk_chars) for p in ('codex', 'claude')}
            # Validate all values and relative filenames before publishing any output.
            names = [p + '.json' for p in bundles]
            privacy_filename(args.output.name)
            for name in names:
                privacy_filename(name)
                if not re.fullmatch(r'(codex|claude)\.json', name): raise ValueError('Invalid output filename')
                if (args.output / name).is_symlink(): raise ValueError('Fixture must not be a symlink')
            args.output.mkdir(parents=True, exist_ok=True)
            report = {}
            for provider, bundle in bundles.items():
                data = json.dumps(bundle, separators=(',', ':')) + '\n'; path = args.output / (provider + '.json')
                path.write_text(data)
                report[provider] = {'turns': len(bundle['threadRead']['thread']['turns']), 'items': sum(len(t['items']) for t in bundle['threadRead']['thread']['turns']), 'replayEvents': len(bundle['replay']), 'bytes': len(data.encode()), 'privacy': 'passed'}
            print(json.dumps(report, indent=2))
    except (ValueError, OSError, subprocess.TimeoutExpired, KeyError, TypeError) as exc:
        # Paths and source fragments must not appear in diagnostics.
        print('Fixture generation failed: ' + (str(exc) if isinstance(exc, ValueError) and not isinstance(exc, json.JSONDecodeError) else type(exc).__name__), file=sys.stderr)
        return 1
    return 0

if __name__ == '__main__': sys.exit(main())

#!/usr/bin/env python3
"""Replay scrubbed provider sessions and diff their timelines against goldens.

    scripts/provider-replay-goldens.py            # replay all, diff, exit 1 on drift
    scripts/provider-replay-goldens.py --update   # rewrite goldens after a reviewed change
    scripts/provider-replay-goldens.py scrub claude SRC OUT

Fixtures live in tests/fixtures/provider-replay/<provider>/<name>.fixture.jsonl
beside <name>.golden.json. They are rebuilt from allowlisted fields of a real
log: every string is replaced by a numbered placeholder, ids and times are
renumbered, and unknown tool names collapse to tool-N. Nothing here sends to a
model, runs a recorded command or reads a provider home (only `scrub` does, and
only the file it is given). Only Claude replays, through the host's own projection. Codex, OpenCode and
Cursor are listed in MISSING with the reason: no host code can read their
on-disk logs without a live runtime, and a script-side normalizer would pass
whatever the host did.
"""
import argparse
import difflib
import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DIR = ROOT / 'tests' / 'fixtures' / 'provider-replay'
PROVIDERS = ('claude', 'codex', 'opencode', 'cursor')
# provider -> reason. These have no replay path through host code, and a script-side
# normalizer replaying a script-invented format could never fail on a host change.
MISSING = {
    'codex': 'host reads rollouts only via the Codex app-server (codex_activity.rs reads turn markers, not items); needs a Rust harness over scrubbed rollouts that keep turn_id',
    'opencode': 'host consumes ACP session updates (services/acp-runtime/projection.mjs TurnProjection), not OpenCode storage; needs scrubbed ACP update fixtures',
    'cursor': 'host consumes ACP session updates (TurnProjection), not Cursor store.db blobs; needs scrubbed ACP update fixtures',
}
EPOCH_MS = 1767225600000  # 2026-01-01T00:00:00Z; fixture times are offsets from it
TOOLS = {
    'claude': {'Bash', 'Read', 'Edit', 'Write', 'Grep', 'Glob', 'TodoWrite', 'WebFetch', 'WebSearch', 'Task', 'Agent', 'ExitPlanMode'},
    'codex': {'exec', 'exec_command', 'apply_patch', 'shell', 'update_plan', 'request_user_input_async', 'web_search'},
    'opencode': {'bash', 'read', 'edit', 'write', 'grep', 'glob', 'websearch', 'webfetch', 'task', 'todowrite', 'patch'},
    'cursor': set(),
}
LEAK = re.compile(r'/Users/|/home/|/private/|@[\w-]+\.\w{2,}|\b(?:sk|pk|ghp|xox[bp])[-_][A-Za-z0-9]{8,}|\b\d{1,3}(?:\.\d{1,3}){3}\b|\.ts\.net|Bearer\s', re.I)


class Scrubber:
    """Deterministic numbering so two scrubs of one log are byte-identical."""
    def __init__(self, provider):
        self.provider, self.ids, self.n = provider, {}, 0

    def text(self, kind):
        self.n += 1
        return f'Scrubbed {kind} {self.n}.'

    def id(self, raw, prefix):
        return self.ids.setdefault((prefix, raw), f'{prefix}-{len([k for k in self.ids if k[0] == prefix]) + 1}')

    def tool(self, name):
        return name if name in TOOLS[self.provider] else self.id(name, 'tool')

    def value(self, v, key=''):
        if isinstance(v, str):
            self.n += 1
            return f'/fixture/work/file-{self.n}' if key in ('file_path', 'path', 'cwd') else f'scrubbed {key or "value"} {self.n}'
        if isinstance(v, dict):
            return {k: self.value(x, k) for k, x in v.items() if re.fullmatch(r'[a-z_]{1,24}', k)}
        if isinstance(v, list):
            return [self.value(x, key) for x in v]
        return v if isinstance(v, (bool, int, float)) or v is None else None


def times(stamps):
    """Relative offsets only: the first record is t=0, gaps keep their size."""
    first = next((s for s in stamps if s is not None), 0)
    return [None if s is None else s - first for s in stamps]


def jl(rows):
    return ''.join(json.dumps(r, sort_keys=True) + '\n' for r in rows)


def iso(ms):
    from datetime import datetime, timezone
    return datetime.fromtimestamp((EPOCH_MS + (ms or 0)) / 1000, timezone.utc).strftime('%Y-%m-%dT%H:%M:%S.%f')[:-3] + 'Z'


def stamp_ms(s):
    from datetime import datetime
    return int(datetime.fromisoformat(s.replace('Z', '+00:00')).timestamp() * 1000) if isinstance(s, str) else None


# ---- scrub: real log -> owned fixture ---------------------------------------

def scrub_claude(src):
    s, out, raw = Scrubber('claude'), [], []
    for line in Path(src).read_text().splitlines():
        d = json.loads(line)
        if d.get('type') in ('user', 'assistant') and not d.get('isSidechain') and isinstance(d.get('message', {}).get('content'), (list, str)):
            raw.append(d)
    off = times([stamp_ms(d.get('timestamp')) for d in raw])
    for d, t in zip(raw, off):
        content = d['message']['content']
        blocks = [{'type': 'text', 'text': content}] if isinstance(content, str) else content
        new = []
        for b in blocks:
            k = b.get('type')
            if k == 'text':
                new.append({'type': 'text', 'text': s.text(d['type'])})
            elif k == 'thinking':
                new.append({'type': 'thinking', 'thinking': s.text('thinking') if b.get('thinking') else ''})
            elif k == 'tool_use':
                new.append({'type': 'tool_use', 'id': s.id(b['id'], 'toolu'), 'name': s.tool(b['name']), 'input': s.value(b.get('input') or {})})
            elif k == 'tool_result':
                new.append({'type': 'tool_result', 'tool_use_id': s.id(b['tool_use_id'], 'toolu'), 'content': s.text('tool output'),
                            **({'is_error': True} if b.get('is_error') else {})})
        if not new:
            continue
        msg = {'role': d['type'], 'content': new}
        if d['type'] == 'assistant':
            msg['id'] = s.id(d['message'].get('id') or d['uuid'], 'msg')
        row = {'type': d['type'], 'uuid': s.id(d['uuid'], 'uuid'), 'timestamp': iso(t), 'message': msg}
        if isinstance(d.get('toolUseResult'), dict):
            row['toolUseResult'] = s.value(d['toolUseResult'])  # host reads this beside the tool_result block
        out.append(row)
    return jl(out), raw


def source_strings(v, acc):
    if isinstance(v, str):
        if len(v) >= 8:
            acc.add(v)
    elif isinstance(v, dict):
        for x in v.values():
            source_strings(x, acc)
    elif isinstance(v, (list, tuple)):
        for x in v:
            source_strings(x, acc)
    return acc


def cmd_scrub(args):
    text, raw = {'claude': scrub_claude}[args.provider](args.src)
    bad = sorted({m.group(0) for m in LEAK.finditer(text)})
    # Lowercase single-word values are the log's own enums (tool_use, completed);
    # anything with spaces, digits, capitals or punctuation is content and must not survive.
    private = {x for x in source_strings(raw, set()) if x in text and not re.fullmatch(r'[a-z][a-z_-]*', x)}
    if bad or private:
        sys.exit(f'scrub refused: pattern hits {bad[:5]}; survivors {sorted(private)[:6]}. Fix the allowlist, not the check.')
    Path(args.out).parent.mkdir(parents=True, exist_ok=True)
    Path(args.out).write_text(text)
    print(f'{args.out}: {len(text.splitlines())} records from {len(raw)} source records, 0 source strings (>=8 chars) and 0 private patterns survive')


# ---- replay: fixture -> timeline --------------------------------------------

def claude_timeline(path):
    r = subprocess.run(['node', str(ROOT / 'scripts' / 'claude-native-turns.mjs'), str(path)], capture_output=True, text=True, timeout=60)
    if r.returncode:
        raise RuntimeError(f'host projection failed: {r.stderr.strip()[:400]}')
    return json.loads(r.stdout)


TIMELINES = {'claude': claude_timeline}


def render(timeline):
    return json.dumps(timeline, indent=2, sort_keys=True) + '\n'


def cmd_replay(args):
    failed, rows = 0, []
    for provider in PROVIDERS:
        fixtures = sorted((DIR / provider).glob('*.fixture.jsonl'))
        if not fixtures:
            rows.append((provider, '-', 'MISSING', MISSING.get(provider, 'no fixture committed')))
            continue
        for fx in fixtures:
            name = fx.name.removesuffix('.fixture.jsonl')
            golden = fx.with_name(f'{name}.golden.json')
            leaked = LEAK.findall(fx.read_text())
            if leaked:
                rows.append((provider, name, 'FAIL', f'private pattern in fixture: {leaked[:3]}'))
                failed += 1
                continue
            try:
                got = render(TIMELINES[provider](fx))
            except Exception as e:  # noqa: BLE001 - report which fixture broke
                rows.append((provider, name, 'FAIL', str(e)))
                failed += 1
                continue
            if args.update:
                golden.write_text(got)
                rows.append((provider, name, 'UPDATED', f'{len(got.splitlines())} lines'))
            elif not golden.exists():
                rows.append((provider, name, 'FAIL', 'no golden; review the output then run --update'))
                failed += 1
            elif golden.read_text() != got:
                diff = ''.join(list(difflib.unified_diff(golden.read_text().splitlines(True), got.splitlines(True), 'golden', 'replay', n=2))[:40])
                rows.append((provider, name, 'FAIL', 'timeline drifted:\n' + diff))
                failed += 1
            else:
                rows.append((provider, name, 'PASS', f'{len(got.splitlines())} lines'))
    for provider, name, status, note in rows:
        print(f'{status:8} {provider:9} {name:16} {note}')
    print(f'{sum(r[2] in ("PASS", "UPDATED") for r in rows)} ok, {failed} failed, {sum(r[2] == "MISSING" for r in rows)} missing')
    return 1 if failed else 0


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('--update', action='store_true', help='rewrite goldens from the current replay')
    sub = ap.add_subparsers(dest='cmd')
    sc = sub.add_parser('scrub', help='build a fixture from a real Claude Code JSONL log')
    sc.add_argument('provider', choices=['claude'])
    sc.add_argument('src')
    sc.add_argument('out')
    args = ap.parse_args()
    sys.exit(cmd_scrub(args) if args.cmd == 'scrub' else cmd_replay(args))


if __name__ == '__main__':
    main()

#!/usr/bin/env python3
"""Measure the reading position across an unfold, on an already acquired Duo lane.

Folded, the chat is scrolled up from its end; the first message under the title
and the gap between title bar and that message are recorded. After the unfold
the same message must still be the first one in view (the anchor is kept) and
its gap to the title bar must not be negative (nothing under the title).
"""
import json, os, subprocess, sys
from pathlib import Path
ROOT = Path(__file__).resolve().parents[4]
CTL = ROOT / 'scripts/wonderctl'
FIXTURES = ['subagent-fixture', 'chat-layout', 'chat-layout-unsaved', 'project-files-send', 'open-project-thread', 'rail-fixture']

def run(*args, env=None):
    r = subprocess.run([str(CTL), '--device', 'duo', *args], cwd=ROOT, capture_output=True, text=True, env=env)
    if r.returncode:
        sys.exit(f'{args}: {r.stderr}\n{r.stdout}')
    return r.stdout

def nodes(tree):
    for n in tree:
        yield n
        yield from nodes(n.get('children') or [])

def frame(n):
    f = n.get('frame') or {}
    return f.get('x', 0), f.get('y', 0), f.get('width', 0), f.get('height', 0)

def snapshot(display_x_min=0, display_x_max=1e9):
    tree = json.loads(run('tree', '--json'))
    all_nodes = list(nodes(tree))
    title = next(n for n in all_nodes if n.get('AXUniqueId') == 'conversation-title-menu')
    draft = next(n for n in all_nodes if n.get('AXUniqueId') == 'message-draft')
    tx, ty, tw, th = frame(title)
    dx, dy, dw, dh = frame(draft)
    header_bottom = ty + th
    texts = []
    for n in all_nodes:
        label = n.get('AXLabel') or ''
        x, y, w, h = frame(n)
        # Message text lives in the chat's column, between the title and the composer.
        if len(label) >= 24 and y + h > header_bottom - 200 and y < dy and w > 100 and n.get('AXUniqueId') not in ('conversation-title-menu', 'message-draft'):
            texts.append((y, y + h, label))
    texts.sort()
    first = next((t for t in texts if t[1] > header_bottom), None)
    return {'headerBottom': header_bottom,
            'first': first and {'label': first[2][:60], 'top': first[0], 'gap': first[0] - header_bottom},
            'texts': [{'label': t[2][:40], 'top': t[0], 'bottom': t[1]} for t in texts]}

def main():
    out = Path(sys.argv[1]) if len(sys.argv) > 1 else ROOT / '.local/evidence/WD-DUO-REGRESS/ui/unfold-anchor.json'
    run('hinge', '0'); run('orient', 'portrait')
    run('launch', *sum((['--fixture', f] for f in FIXTURES), []), '--arg=-wonder.sidebar.hidden', '--arg=YES')
    run('wait', '--id', 'message-draft', '--timeout', '30')
    run('swipe', 'down')
    folded_end = snapshot()
    run('hinge', '180'); run('orient', 'portrait')
    run('wait', '--id', 'message-draft', '--timeout', '30')
    unfolded = snapshot()
    result = {'folded': folded_end, 'unfolded': unfolded}
    out.write_text(json.dumps(result, indent=2))
    print(json.dumps(result, indent=2))

if __name__ == '__main__':
    main()

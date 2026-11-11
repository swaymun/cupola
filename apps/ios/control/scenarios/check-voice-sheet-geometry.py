#!/usr/bin/env python3
"""Check native AX trees recorded by composer-controls / voice capture scenarios.

Usage: check-voice-sheet-geometry.py LOG [LOG ...]
No fixture values or source strings substitute for the rendered frames.
"""
import json
import sys
from pathlib import Path


def walk(nodes):
    for node in nodes:
        yield node
        yield from walk(node.get('children', []))


count = 0
for filename in sys.argv[1:]:
    log = Path(filename).read_text()
    for section in log.split('tree --json\n')[1:]:
        start = section.find('[')
        if start < 0:
            continue
        tree, _ = json.JSONDecoder().raw_decode(section[start:])
        nodes = {n.get('AXUniqueId'): n for n in walk(tree)}
        if 'voice-sheet-computer' not in nodes:
            continue
        def frame(name):
            f = nodes[name]['frame']
            assert f['width'] > 0 and f['height'] > 0, (filename, name, f)
            return f
        title = frame('voice-sheet-computer')
        summary = frame('voice-confirmation-summary')
        cancel = frame('voice-confirmation-cancel')
        confirm = frame('voice-confirmation-confirm')
        end = frame('voice-end')
        # Insets between the computer identity and the reading area, never an
        # expanding spacer that steals height from the proposed message.
        gap = summary['y'] - title['y'] - title['height']
        assert 0 <= gap <= 40, (filename, 'unused space above message', gap)
        assert summary['y'] < cancel['y'], (filename, 'message must precede actions')
        assert confirm['y'] + confirm['height'] <= end['y'], (filename, 'Send overlaps End')
        print(json.dumps({'log': filename, 'title_to_message_gap': gap,
                          'summary_top': summary['y'], 'cancel_top': cancel['y'],
                          'send_bottom': confirm['y'] + confirm['height'], 'end_top': end['y']}))
        count += 1
assert count, 'No standalone voice AX tree found; record tree --json with the sheet open'
print(f'PASS: {count} native voice layouts')

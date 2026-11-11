#!/usr/bin/env python3
"""Check adjacent Settings Back/title frames from native scenario AX trees."""
import json
import sys
from pathlib import Path


def walk(nodes):
    for node in nodes:
        yield node
        yield from walk(node.get('children', []))


count = 0
for filename in sys.argv[1:]:
    for section in Path(filename).read_text().split('tree --json\n')[1:]:
        start = section.find('[')
        if start < 0:
            continue
        tree, _ = json.JSONDecoder().raw_decode(section[start:])
        nodes = {n.get('AXUniqueId'): n for n in walk(tree)
                 if n.get('frame', {}).get('width', 0) > 0}
        for back_id, title_id in [('settings-back-to-chats', 'settings-root-title'),
                                  ('settings-subsection-back', 'settings-subsection-title')]:
            if back_id not in nodes or title_id not in nodes:
                continue
            back, title = (nodes[key]['frame'] for key in (back_id, title_id))
            gap = title['x'] - back['x'] - back['width']
            midline = abs((back['y'] + back['height'] / 2) - (title['y'] + title['height'] / 2))
            assert back['width'] >= 44 and back['height'] >= 44, (filename, back_id, back)
            assert 0 <= gap <= 16 and midline <= 4, (filename, back_id, gap, midline)
            print(json.dumps({'log': filename, 'back': back_id, 'gap': gap, 'midline': midline}))
            count += 1
assert count, 'No Settings Back/title pairs; record tree --json with Settings open'
print(f'PASS: {count} native Settings Back/title pairs')

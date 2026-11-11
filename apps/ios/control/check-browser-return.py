#!/usr/bin/env python3
"""Compare the actual chat pixels around full-screen browser round trips."""
import argparse
import json
from pathlib import Path
from PIL import Image, ImageChops

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--crop', type=float, nargs=4, default=(.05, .15, .85, .72), metavar=('LEFT', 'TOP', 'RIGHT', 'BOTTOM'))
parser.add_argument('before', type=Path)
parser.add_argument('after', type=Path, nargs='+')
args = parser.parse_args()
before = Image.open(args.before).convert('RGB')
# Ignore system time, the composer and the minimap’s fading indicator.
# Compare the actual message pixels, with a selectable normalized crop.
width, height = before.size
left, top, right, bottom = args.crop
if not (0 <= left < right <= 1 and 0 <= top < bottom <= 1):
    parser.error('crop must be normalized LEFT TOP RIGHT BOTTOM bounds within 0...1')
crop = (int(width * left), int(height * top), int(width * right), int(height * bottom))
results = []
for path in args.after:
    after = Image.open(path).convert('RGB')
    if after.size != before.size:
        parser.error(f'{path}: capture dimensions changed; capture the same display and orientation')
    difference = ImageChops.difference(before.crop(crop), after.crop(crop))
    results.append(dict(before=str(args.before), after=str(path), crop=crop,
                        changedBounds=difference.getbbox(), sameChatPixels=difference.getbbox() is None))
print(json.dumps(results, indent=2))
if not all(result['sameChatPixels'] for result in results):
    raise SystemExit('Chat pixels changed. Inspect the captures and reading position before rerunning.')

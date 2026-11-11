#!/usr/bin/env python3
"""Run computer-entry-duo.wctl with active-display captures (#387).

Uses semantic production actions, never native Duo taps. Requires this unit's
own pooled slim simulator lane. Raw AX trees retain badges outside stale AX roots.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shlex
import subprocess
import time

ROOT = Path(__file__).resolve().parents[4]
CTL = ROOT / 'scripts/wonderctl'


def command(args, timeout=120):
    result = subprocess.run(args, cwd=ROOT, text=True, capture_output=True, timeout=timeout)
    if result.returncode:
        raise RuntimeError(f'{shlex.join(map(str, args))}\n{result.stdout}\n{result.stderr}')
    return result.stdout


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('output', type=Path)
    parser.add_argument('--scenario', type=Path, default=Path(__file__).with_name('computer-entry-duo.wctl'))
    parser.add_argument('--semantic-taps', action='store_true',
                        help='Launch the driver and invoke production actions; waits still use native AX')
    args = parser.parse_args()
    lane = json.loads(command([str(CTL), 'lane', 'status', '--kind', 'duo']))
    if not lane.get('held'):
        parser.error('Acquire an owned pooled Duo lane with wonderctl lane acquire --kind duo')
    args.output.mkdir(parents=True, exist_ok=True)
    ctl = [str(CTL), '--device', 'duo']
    device = command(ctl + ['device']).strip()
    if device != lane['held']:
        raise RuntimeError('The selected Duo differs from the owned pooled lane')
    command(ctl + ['install'])
    derived = Path(os.environ.get('WONDER_CONTROL_DERIVED', ROOT / '.local/build/control-sim'))
    binary = derived / 'Build/Products/Diagnostics-iphonesimulator/Wonder.app/Wonder'
    installed = Path(command(['xcrun', 'simctl', 'get_app_container', device, 'com.swaymun.wonder', 'app']).strip()) / 'Wonder'
    digest = lambda path: hashlib.sha256(path.read_bytes()).hexdigest()
    if digest(binary) != digest(installed):
        raise RuntimeError('Installed app differs from this checkout build')
    metadata = {'device': device, 'appSHA256': digest(installed),
                'head': command(['git', 'rev-parse', 'HEAD']).strip(),
                'evidence': 'Owned simulator fixtures and semantic actions; native Duo input unverified (#387)'}
    (args.output / 'metadata.json').write_text(json.dumps(metadata, indent=2) + '\n')
    (args.output / 'source.diff').write_text(command(['git', 'diff', 'HEAD', '--', 'apps/ios']))
    rows, driver, hinge = [], False, 0
    try:
        for line in args.scenario.read_text().splitlines():
            if not line.strip() or line.lstrip().startswith('#'):
                continue
            words = shlex.split(line)
            started = time.monotonic()
            print(line, flush=True)
            if words[0] == 'driver':
                driver = words[1] == 'on'
                output = ''
            elif words[0] == 'shot':
                name = words[1]
                # The cover is primary; the inner display is primary-1.
                command(['xcrun', 'simctl', 'io', device, 'screenshot',
                         '--display=' + ('primary' if hinge == 0 else 'primary-1'), str(args.output / (name + '.png'))])
                tree = command(ctl + ['tree', '--json'])
                (args.output / (name + '.tree.json')).write_text(tree)
                if name.endswith('-badge'):
                    queue = list(json.loads(tree))
                    badges = []
                    while queue:
                        node = queue.pop(0)
                        queue.extend(node.get('children', []))
                        if node.get('AXUniqueId') == 'computer-status-pill':
                            badges.append(node)
                    if len(badges) != 1 or '2 dialogs waiting' not in (badges[0].get('AXValue') or ''):
                        raise RuntimeError('Badge capture requires one native Computer entry with 2 dialogs waiting')
                if '-hinge-' in name and name.endswith('-whole-screen'):
                    queue = list(json.loads(tree))
                    viewports = []
                    while queue:
                        node = queue.pop(0)
                        queue.extend(node.get('children', []))
                        if node.get('AXUniqueId') == 'computer-viewport-appearances':
                            viewports.append(node.get('AXValue') or '')
                    if len(viewports) != 1 or 'laptop: true' not in viewports[0]:
                        raise RuntimeError('Hinge capture requires the computer-fold launch fixture; native Duo posture is unverified (#387)')
                state = command(ctl + ['--driver', 'state'])
                (args.output / (name + '.state.json')).write_text(state)
                output = name + '.png'
            else:
                if words[0] == 'hinge':
                    hinge = int(words[1])
                # The Duo's native rail owns Close; do not add a redundant button
                # just to satisfy the phone/iPad scenario's exit assertion.
                if args.semantic_taps and words[0] == 'wait' and '--id' in words:
                    at = words.index('--id') + 1
                    if words[at] == 'computer-session-close':
                        words[at] = 'computer-status-pill'
                        if '--label' in words:
                            words[words.index('--label') + 1] = 'View computer'
                        print('Duo native exit assertion: ' + shlex.join(words), flush=True)
                semantic = ((driver or (args.semantic_taps and words[0] in {'launch', 'tap'}))
                            and words[0] in {'launch', 'tap', 'wait', 'assert', 'dismiss', 'state', 'open-thread'})
                output = command(ctl + (['--driver'] if semantic else []) + words)
            rows.append({'step': line, 'platformStep': shlex.join(words), 'seconds': round(time.monotonic() - started, 3), 'output': output})
            (args.output / 'steps.json').write_text(json.dumps(rows, indent=2) + '\n')
    except Exception:
        command(['xcrun', 'simctl', 'io', device, 'screenshot',
                 '--display=' + ('primary' if hinge == 0 else 'primary-1'), str(args.output / 'failure.png')])
        raise
    print(f'PASS {args.scenario.name}: {len(rows)} steps', flush=True)


if __name__ == '__main__':
    main()

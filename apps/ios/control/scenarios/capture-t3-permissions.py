#!/usr/bin/env python3
"""Capture T3 permission UI on an already-leased pooled lane; no model sends.

Run from the checkout root after wonderctl build/install:
  python3 apps/ios/control/scenarios/capture-t3-permissions.py after ipad --output .local/evidence/t3-permissions
Repeat for iphone and duo. Duo captures both the folded cover and flat inner
display, using fixture navigation and native AX assertions while #387 limits
native input. `before` captures a baseline with the same fixture; point
WONDER_CONTROL_DERIVED at its preserved build to prevent automatic replacement.
"""
import argparse
import hashlib
import json
from pathlib import Path
import shlex
import subprocess


def run(*args):
    args = list(map(str, args))
    result = subprocess.run(args, text=True, capture_output=True, timeout=120)
    if result.returncode:
        raise RuntimeError(shlex.join(args) + '\n' + result.stdout + '\n' + result.stderr)
    return result.stdout


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('phase', choices=['before', 'after'])
    parser.add_argument('device', choices=['iphone', 'ipad', 'duo'])
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[4]
    ctl = [root / 'scripts/wonderctl', '--device', args.device]
    args.output.mkdir(parents=True, exist_ok=True)
    udid = run(*ctl, 'device').strip()
    fixtures = ['subagent-fixture', 'chat-layout', 'chat-layout-unsaved', 'send-states',
                'send-claude', 'send-held-by-app', 't3-code', 't3-code-running',
                't3-code-permission', 'open-project-thread']
    launch = [part for fixture in fixtures for part in ['--fixture', fixture]]
    poses = [('folded', 0, 'primary'), ('unfolded', 180, 'primary-1')] if args.device == 'duo' else [(args.device, None, 'primary')]
    metadata = {
        'phase': args.phase, 'device': args.device, 'udid': udid,
        'captureCheckoutHead': run('git', '-C', root, 'rev-parse', 'HEAD').strip(),
        'captures': [],
        'evidence': 'Simulator fixture responses; Duo fixture navigation and native AX assertions. Native Duo touches unverified (#387).',
    }
    display = 'primary'
    try:
        for pose, hinge, display in poses:
            if hinge is not None:
                run(*ctl, 'hinge', hinge)
                run(*ctl, 'orient', 'portrait')
            for scheme in ['light', 'dark']:
                run('xcrun', 'simctl', 'ui', udid, 'appearance', scheme)
                for surface in ['chat', 'details']:
                    flags = launch + (['--arg=-diagnostics-companion-details'] if surface == 'details' else [])
                    run(*ctl, '--driver', 'launch', *flags)
                    installed = Path(run('xcrun', 'simctl', 'get_app_container', udid, 'com.swaymun.wonder', 'app').strip()) / 'Wonder'
                    binary_hash = hashlib.sha256(installed.read_bytes()).hexdigest()
                    if metadata.setdefault('appSHA256', binary_hash) != binary_hash:
                        raise RuntimeError('Installed binary changed during capture; finish the build before rerunning')
                    target = 'thread-details-rename' if surface == 'details' else 'computer-waiting'
                    run(*ctl, 'wait', '--id', target, '--timeout', '30')
                    if args.phase == 'after':
                        if surface == 'chat':
                            run(*ctl, 'wait', '--contains', 'Approve in T3 Code on Synthetic fixture', '--timeout', '10')
                        else:
                            for identifier, text in [
                                ('thread-permission-mode', 'Accept edits · T3 Code'),
                                ('thread-permission-description', 'Edits files in its working folder without asking.'),
                                ('thread-approval-location', 'T3 Code on Synthetic fixture'),
                            ]:
                                run(*ctl, 'wait', '--id', identifier, '--contains', text, '--timeout', '10')
                    name = f'{args.phase}-{pose}-{scheme}-{surface}'
                    out = args.output / (name + '.png')
                    run('xcrun', 'simctl', 'io', udid, 'screenshot', *(['--display=' + display] if args.device == 'duo' else []), out)
                    (args.output / (name + '.tree.json')).write_text(run(*ctl, 'tree', '--json'))
                    metadata['captures'].append({'path': out.name, 'sha256': hashlib.sha256(out.read_bytes()).hexdigest()})
                    print(name, flush=True)
    except Exception:
        name = f'failure-{args.phase}-{args.device}'
        run('xcrun', 'simctl', 'io', udid, 'screenshot', *(['--display=' + display] if args.device == 'duo' else []), args.output / (name + '.png'))
        (args.output / (name + '.tree.json')).write_text(run(*ctl, 'tree', '--json'))
        raise
    finally:
        if args.device == 'duo':
            run(*ctl, 'hinge', 0)
        run('xcrun', 'simctl', 'ui', udid, 'appearance', 'light')
        (args.output / f'{args.phase}-{args.device}-metadata.json').write_text(json.dumps(metadata, indent=2) + '\n')
        (args.output / f'{args.phase}-{args.device}-checkout.diff').write_text(run('git', '-C', root, 'diff', 'HEAD', '--', 'apps/ios', 'apps/native'))


if __name__ == '__main__':
    main()

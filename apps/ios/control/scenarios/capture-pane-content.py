#!/usr/bin/env python3
"""Matched pane-content screenshots, using owned launch fixtures and wonderctl shot.

Acquire a pooled lane and install/build this checkout first. No host or model sends.
One invocation owns one device/posture; source and native AX accompany every shot.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess

ROOT = Path(__file__).resolve().parents[4]
BASE = ['subagent-fixture', 'chat-layout', 'chat-layout-unsaved', 'project-subagents',
        'terminal-available', 'project-files-send', 'open-project-thread', 'rail-fixture']
TARGETS = {'files': 'workspace-file-list', 'pull-requests': 'pull-request-row:128',
           'reply-changes': 'reply-changes', 'details': 'thread-details-rename',
           'terminal': 'terminal-output', 'agent-task': 'project-subagent-transcript',
           'code': 'message-draft'}


def walk(value):
    if isinstance(value, list):
        for child in value:
            yield from walk(child)
    elif isinstance(value, dict):
        yield value
        yield from walk(value.get('children', []))


def check_geometry(tree, screen, variant=None):
    nodes = list(walk(tree))
    def node(identifier):
        matches = [n for n in nodes if n.get('AXUniqueId') == identifier]
        assert matches, 'Missing ' + identifier
        return matches[0]['frame']
    checks = []
    if screen == 'terminal':
        assert not any('Speed up your typing' in (n.get('AXLabel') or '') for n in nodes), 'System keyboard tutorial is still covering the keyboard'
        frames = [node('terminal-key-' + key) for key in
                  ['ctrl', 'esc', 'tab', 'left', 'down', 'up', 'right', 'symbols', 'paste', 'hide']]
        output = node('terminal-output')
        for frame in frames:
            assert frame['width'] >= 43.5 and frame['height'] >= 43.5, ('Small key', frame)
            assert frame['y'] >= output['y'] + output['height'] - 1, ('Keys overlap terminal', frame, output)
        for i, a in enumerate(frames):
            for b in frames[i+1:]:
                assert (a['x'] + a['width'] <= b['x'] + 1 or b['x'] + b['width'] <= a['x'] + 1
                        or a['y'] + a['height'] <= b['y'] + 1 or b['y'] + b['height'] <= a['y'] + 1), 'Keys overlap'
        keyboard = [n['frame'] for n in nodes if n.get('AXLabel') in ('q', 'Q') and n.get('type') == 'Button']
        if keyboard:
            assert max(f['y'] + f['height'] for f in frames) <= min(f['y'] for f in keyboard) - 10, 'Terminal keys run behind the system keyboard'
        checks += ['10 keys have whole 44pt targets', 'keys do not overlap each other, terminal output or system letter keys']
    if screen == 'files':
        assert not any(n.get('AXUniqueId') == 'workspace-pane-title' for n in nodes), 'Redundant root Files heading'
        checks += ['no redundant root heading']
    if screen == 'details':
        node('thread-details-rename')
        assert not any(n.get('AXUniqueId') == 'thread-details-name' for n in nodes), 'Chat name repeats before editing'
        checks += ['rename is available without repeating the chat name']
    if screen == 'code':
        draft = node('message-draft')
        cue = [n for n in nodes if n.get('AXLabel') == 'Scroll code horizontally']
        if variant == 'light' and draft['width'] < 500:
            assert cue, 'Narrow overflowing code has no horizontal scroll cue'
            checks += ['narrow default-size code exposes a horizontal scroll cue']
        if variant == 'dark-large':
            code = [n for n in nodes if (n.get('AXLabel') or '').startswith('struct Trip')]
            assert code, 'Wrapped code is not independently accessible'
            assert all(n['frame']['width'] <= draft['width'] + 40 for n in code), 'Large-text code exceeds readable chat width'
            assert not cue, 'Large-text code still requires sideways scrolling'
            checks += ['large-text code fits chat width without horizontal scrolling']
    panes = [n['frame'] for n in nodes if n.get('AXUniqueId') == 'companion-pane']
    windows = [n['frame'] for n in nodes if n.get('type') == 'Application' and n.get('frame')]
    if panes or windows:
        pane = (panes or windows)[0]
        matched = 0
        identifiers = ('workspace-file-entry:', 'pull-request-row:', 'reply-file:', 'terminal-key-')
        for n in nodes:
            if (n.get('AXUniqueId') or '').startswith(identifiers):
                matched += 1
                # UIKit List reports the unclipped cell hit frame including
                # the inherited Duo safe-area width. Check rendered descendants
                # and the visible hit target, rather than calling that hidden
                # background a clipped filename. Other controls must fit whole.
                f = n['frame']
                descendants = [child for child in walk(n.get('children', []))
                               if child.get('type') in ('StaticText', 'Image') and child.get('frame')]
                frames = [child['frame'] for child in descendants] or [f]
                if (n.get('AXUniqueId') or '').startswith('workspace-file-entry:'):
                    visible_width = min(f['x'] + f['width'], pane['x'] + pane['width']) - max(f['x'], pane['x'])
                    assert visible_width >= 44 and f['height'] >= 44, ('File row loses its visible hit target', n)
                for frame in frames:
                    assert frame['x'] >= pane['x'] - 1 and frame['x'] + frame['width'] <= pane['x'] + pane['width'] + 1, ('Row/key content crosses pane edge', n)
        if matched:
            checks += ['file/PR/key content fits the pane or window; file rows retain whole visible targets']
    return checks


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('output', type=Path)
    parser.add_argument('--device', choices=['iphone', 'ipad', 'duo'], required=True)
    parser.add_argument('--posture', choices=['folded', 'unfolded'], default='folded')
    parser.add_argument('--source-head', help='Revision of a retained baseline build')
    parser.add_argument('--verify', action='store_true', help='Assert the updated content geometry in native AX')
    parser.add_argument('--screen', action='append', choices=TARGETS)
    parser.add_argument('--variant', action='append', choices=['light', 'dark-large'])
    args = parser.parse_args()

    def run(*command):
        result = subprocess.run([str(ROOT / 'scripts/wonderctl'), '--device', args.device, *command],
                                cwd=ROOT, capture_output=True, text=True)
        if result.returncode:
            raise RuntimeError(' '.join(command) + '\n' + result.stdout + result.stderr)
        return result.stdout

    args.output.mkdir(parents=True, exist_ok=True)
    run('install')
    if args.device == 'duo':
        run('hinge', '180' if args.posture == 'unfolded' else '0')
        run('orient', 'portrait')
    app = Path(os.environ.get('WONDER_CONTROL_DERIVED', ROOT / '.local/build/control-sim')) / 'Build/Products/Diagnostics-iphonesimulator/Wonder.app/Wonder'
    metadata = {'head': args.source_head or subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip(),
                'binarySHA256': hashlib.sha256(app.read_bytes()).hexdigest(),
                'device': args.device, 'posture': args.posture,
                'evidence': 'Simulator launch fixture, native accessibility, wonderctl shot; no physical/input claim'}
    prior = args.output / 'metadata.json'
    if prior.exists():
        assert json.loads(prior.read_text())['binarySHA256'] == metadata['binarySHA256'], 'Use a fresh directory for a different binary'
    prior.write_text(json.dumps(metadata, indent=2) + '\n')
    (args.output / 'source.diff').write_bytes(b'' if args.source_head else subprocess.check_output(['git', 'diff', 'HEAD', '--', 'apps/ios'], cwd=ROOT))
    failures = []

    def verify(tree, screen, variant, name):
        try:
            checks = check_geometry(json.loads(tree), screen, variant)
        except AssertionError as error:
            checks = {'failure': str(error)}
            failures.append(name + ': ' + str(error))
        (args.output / (name + '-checks.json')).write_text(json.dumps(checks, indent=2) + '\n')

    for variant in args.variant or ['light', 'dark-large']:
        for screen in args.screen or TARGETS:
            name = screen + '-' + variant
            if (args.output / (name + '.png')).exists():
                print(name + ' already captured', flush=True)
                if args.verify:
                    verify((args.output / (name + '.json')).read_text(), screen, variant, name)
                continue
            print(name, flush=True)
            fixtures = BASE + (['project-pull-requests'] if screen == 'pull-requests' else [])
            if screen == 'terminal':
                fixtures += ['terminal-preview']
            if screen == 'code':
                fixtures = ['subagent-fixture', 'chat-layout', 'chat-layout-unsaved', 'project-actions',
                            'project-speed', 'send-states', 'open-project-thread']
            command = ['--driver', 'launch']
            for fixture in fixtures:
                command += ['--fixture', fixture]
            if screen != 'code':
                command += ['--arg=-diagnostics-companion-' + screen]
            if screen == 'reply-changes':
                command += ['--arg=-diagnostics-app-store']
            arguments = ['-composerPills', 'computer,files,terminal,preview,attention,goal,agentTasks,git',
                         '-wonder.sidebar.hidden', 'YES', '-wonder.companionPane.flipped', 'NO',
                         '-UIPreferredContentSizeCategoryName',
                         'UICTContentSizeCategoryAccessibilityXXXL' if variant == 'dark-large' else 'UICTContentSizeCategoryL',
                         '-diagnostics-theme', 'cupola-dark' if variant == 'dark-large' else 'cupola-light',
                         '-AppleInterfaceStyle', 'Dark' if variant == 'dark-large' else 'Light']
            command += ['--arg=' + argument for argument in arguments]
            run(*command)
            run('wait', '--id', TARGETS[screen], '--timeout', '30')
            state = json.loads(run('--driver', 'state'))
            if state.get('alerts') == ['Unlock this device to open saved notification settings.']:
                run('--driver', 'tap', '--id', 'notification-error-dismiss')
            elif state.get('alerts'):
                raise RuntimeError('Unexpected alert: ' + repr(state['alerts']))
            run('wait', '--id', TARGETS[screen], '--timeout', '30')
            if screen == 'code':
                run('wait', '--contains', 'struct Trip', '--timeout', '30')
            if screen == 'terminal':
                keyboard_tree = run('tree', '--json')
                if 'Speed up your typing' in keyboard_tree:
                    run('tap', '--label', 'Continue')
                run('wait', '--id', 'terminal-key-ctrl', '--timeout', '15')
            tree = run('tree', '--json')
            (args.output / (name + '.json')).write_text(tree)
            (args.output / (name + '-state.json')).write_text(run('--driver', 'state'))
            result = run('shot', 'panes-' + name, *(['--display', 'primary-1']
                         if args.device == 'duo' and args.posture == 'unfolded' else []))
            shot = next(Path(line.strip()) for line in result.splitlines() if line.strip().endswith('.png'))
            shutil.copy2(ROOT / shot, args.output / (name + '.png'))
            if args.verify:
                verify(tree, screen, variant, name)
            (args.output / (name + '-launch.json')).write_text(json.dumps(command, indent=2) + '\n')

    if failures:
        raise AssertionError('Geometry checks failed after retaining all captures:\n' + '\n'.join(failures))


if __name__ == '__main__':
    main()

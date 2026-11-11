#!/usr/bin/env python3
"""Matched dismissal screenshots, using owned launch fixtures and wonderctl shot.

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
           'computer': 'computer-session-header', 'goal': 'goal-details-scroll',
           'context': 'context-usage-window', 'tasks': 'project-subagent-roster-list',
           'voice': 'voice-rail-panel', 'browser': 'browser-full-screen',
           'computer-sources': 'computer-session-connection-picker', 'new-computers': 'connection-options', 'effort': 'project-composer-effort-slider'}


def walk(value):
    if isinstance(value, list):
        for child in value:
            yield from walk(child)
    elif isinstance(value, dict):
        yield value
        yield from walk(value.get('children', []))


def check_geometry(tree, screen, variant=None, state=None, unfolded=False):
    nodes = list(walk(tree))
    def frame(identifier):
        values = [n.get('frame') for n in nodes if n.get('AXUniqueId') == identifier]
        return next((v for v in values if v and v['width'] > 0), None)
    def right(f): return f['x'] + f['width']
    checks = []
    app = next((n['frame'] for n in nodes if n.get('type') == 'Application' and n.get('frame')), None)
    expand = frame('companion-pane-expand') or frame('companion-pane-flip')
    if state and state.get('pane'):
        assert expand, 'An open companion pane must expose its size or display-swap control'
    if expand:
        # Other toolbar controls must precede the size action, at every text size.
        for identifier in ['terminal-menu', 'computer-session-more', 'thread-details-done',
                           'reply-changes-done', 'pull-requests-done']:
            other = frame(identifier)
            if other and abs(other['y'] - expand['y']) < 20:
                assert right(other) <= expand['x'] + 2, (identifier, 'must precede expand', other, expand)
        pane_frame = frame('companion-pane')
        if pane_frame:
            assert right(expand) <= right(pane_frame) + 2, ('trailing control outside pane', expand, pane_frame)
        checks.append({'placement': 'size or display-swap control is trailing and contained',
                       'nativeAXBounds': expand, 'hitArea': 'AX bounds do not establish UIKit touch expansion'})
    for identifier in ['goal-done', 'voice-rail-close', 'pull-requests-done', 'project-subagent-sheet-done', 'terminal-done']:
        assert not frame(identifier), ('redundant dismiss control', identifier)
    checks.append('no redundant Done on toggle-owned panes or gesture presentations')
    if not frame('companion-pane') and screen in ['details', 'reply-changes']:
        assert not frame('thread-details-done') and not frame('reply-changes-done'), 'sheet has redundant Done'
        checks.append('sheet omits Done')
    sheet_screens = ['details', 'agent-task', 'terminal', 'pull-requests', 'reply-changes', 'goal']
    if app and screen in sheet_screens and not frame('companion-pane'):
        # Gesture sheets close by swiping down; no grabber line is drawn in place of Done.
        grabbers = [n['frame'] for n in nodes if n.get('AXLabel') == 'Sheet Grabber' and n.get('frame')]
        assert not grabbers, ('gesture sheet draws a grabber', screen, grabbers[0])
        checks.append('sheet draws no grabber')
    # Unfolded Duo spans two displays; the AX application frame there is not the window the popover uses.
    if screen == 'goal' and not unfolded:
        scroll = frame('goal-details-scroll')
        assert scroll and app, 'Goal details must be on screen'
        assert scroll['x'] >= app['x'] - 1 and right(scroll) <= right(app) + 1, ('Goal body wider than the display', scroll, app)
        assert scroll['y'] >= app['y'] - 1 and scroll['y'] + scroll['height'] <= app['y'] + app['height'] + 1, ('Goal body taller than the display', scroll, app)
        checks.append('Goal body fits inside the display')
        if variant == 'dark-large' and app['width'] >= 800:
            # A regular-width window sizes the popover to its content: every control is visible without scrolling (8 points of AX line-box slack).
            for identifier in ['goal-edit', 'goal-pause', 'goal-remove', 'goal-metadata-time']:
                inner = frame(identifier)
                assert inner and inner['y'] + inner['height'] <= scroll['y'] + scroll['height'] + 8, (identifier, 'needs scrolling at large text', inner, scroll)
            checks.append('large-text Goal popover shows every control without scrolling')
    if screen == 'computer':
        close, menu = frame('computer-session-close'), frame('computer-session-more')
        if close and menu:
            assert close['x'] >= right(menu) - 2, ('Close is not trailing', close, menu)
            checks.append('full-screen Close is trailing')
    if screen == 'browser':
        close, share = frame('browser-done'), frame('browser-share')
        assert close and share, 'browser needs Close and Share'
        if abs(close['y'] - share['y']) < 20:
            assert close['x'] >= right(share) - 2, 'browser Close is not trailing'
        else:
            viewer = frame('browser-full-screen')
            assert viewer and close['x'] >= viewer['x'] + viewer['width'] / 2 and close['y'] < share['y'], 'Duo Close must be top trailing, above its side rail'
        checks.append('browser Close is trailing')
    return checks


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('output', type=Path)
    parser.add_argument('--device', choices=['iphone', 'ipad', 'duo'], required=True)
    parser.add_argument('--posture', choices=['folded', 'unfolded'], default='folded')
    parser.add_argument('--expanded', action='store_true')
    parser.add_argument('--source-head', help='Revision of a retained baseline build')
    parser.add_argument('--verify', action='store_true', help='Assert the updated content geometry in native AX')
    parser.add_argument('--verify-only', action='store_true', help='Recheck retained native AX without accessing a simulator')
    parser.add_argument('--screen', action='append', choices=TARGETS)
    parser.add_argument('--variant', action='append', choices=['light', 'dark-large'])
    args = parser.parse_args()
    if args.verify_only:
        assert json.loads((args.output / 'metadata.json').read_text())['device'] == args.device
        failures = []
        captures = list(args.output.glob('*-state.json'))
        for capture in captures:
            name = capture.name.removesuffix('-state.json')
            screen = name.removesuffix('-dark-large').removesuffix('-light').removesuffix('-expanded')
            try:
                variant = 'dark-large' if '-dark-large' in name else 'light'
                checks = check_geometry(json.loads((args.output / (name + '.json')).read_text()), screen, variant=variant, unfolded=args.posture == 'unfolded',
                    state=json.loads((args.output / (name + '-state.json')).read_text()))
            except AssertionError as error:
                checks = {'failure': str(error)}
                failures.append(name + ': ' + str(error))
            (args.output / (name + '-checks.json')).write_text(json.dumps(checks, indent=2) + '\n')
        assert captures, 'No retained captures to verify'
        assert not failures, '\n'.join(failures)
        print(f'{len(captures)} retained native AX captures passed')
        return
    if args.device == "duo" and args.expanded and args.verify:
        print("Not applicable: upstream hinge layout keeps each tool on one display; Duo has no expand action.")
        return

    def run(*command):
        for attempt in range(3):
            try:
                result = subprocess.run([str(ROOT / 'scripts/wonderctl'), '--device', args.device, *command],
                                        cwd=ROOT, capture_output=True, text=True,
                                        timeout=60 if command[0] == 'shot' else None)
                break
            except subprocess.TimeoutExpired:
                if attempt == 2: raise
                # Evidence leases are sharded. A fresh timestamp chooses a new
                # owned output path while retaining the storage lock protection.
                print('Screenshot timed out; retrying a fresh evidence path', flush=True)
        if result.returncode:
            raise RuntimeError(' '.join(command) + '\n' + result.stdout + result.stderr)
        return result.stdout

    args.output.mkdir(parents=True, exist_ok=True)
    run('install')
    if args.device == 'duo':
        run('activate')
        run('hinge', '180' if args.posture == 'unfolded' else '0')
        run('orient', 'portrait')
        run('activate')
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
            checks = check_geometry(json.loads(tree), screen, variant, json.loads((args.output / (name + '-state.json')).read_text()),
                                    unfolded=args.posture == 'unfolded')
        except AssertionError as error:
            checks = {'failure': str(error)}
            failures.append(name + ': ' + str(error))
        (args.output / (name + '-checks.json')).write_text(json.dumps(checks, indent=2) + '\n')

    for variant in args.variant or ['light', 'dark-large']:
        for screen in args.screen or TARGETS:
            if screen == 'voice' and args.device != 'duo':
                continue  # The Diagnostics rail controller exists only on the Duo.
            name = screen + ('-expanded' if args.expanded else '') + '-' + variant
            if (args.output / (name + '.png')).exists():
                print(name + ' already captured', flush=True)
                if args.verify:
                    verify((args.output / (name + '.json')).read_text(), screen, variant, name)
                continue
            print(name, flush=True)
            unfold_effort = screen == 'effort' and args.device == 'duo' and args.posture == 'unfolded'
            if unfold_effort:
                # Fold with the app foregrounded: folding over Home can leave
                # the cover showing Home while AX still returns app nodes (#387).
                run('activate')
                run('hinge', '0')
                run('orient', 'portrait')
                run('activate')
            fixtures = BASE + (['project-pull-requests'] if screen == 'pull-requests' else [])
            if screen == 'terminal':
                fixtures += ['terminal-preview']
            if screen in ['computer', 'computer-sources']:
                fixtures += ['computer-session-fixture', 'computer-session-available-fixture', 'computer-menu-fixture']
            if screen == 'computer-sources': fixtures += ['companion-computer', 'computer-sources-open']
            if screen == 'new-computers':
                fixtures = [f for f in BASE if f != 'open-project-thread'] + ['new-chat-computers-open', 'fresh-agent-defaults']
            if screen == 'goal': fixtures += ['goal-fixture']
            if screen == 'effort': fixtures += ['project-provider-switch', 'context-usage']
            if screen == 'context': fixtures += ['context-usage', 'context-usage-open']
            if screen == 'tasks':
                fixtures += ['project-claude-tasks', 'task-roster-open']
                if args.device == 'duo': fixtures += ['task-roster-in-bar']
            if screen == 'voice': fixtures += ['voice-preview', 'project-actions', 'project-speed', 'send-states']
            if screen == 'browser':
                fixtures = ['read-preview', 'send-preview', 'project-files-conversation-preview', 'dev-server-preview']
            command = ['--driver', 'launch']
            for fixture in fixtures:
                command += ['--fixture', fixture]
            if screen in ['files', 'terminal', 'pull-requests', 'details', 'reply-changes', 'agent-task', 'computer']:
                command += ['--arg=-diagnostics-companion-' + screen]
            if args.expanded: command += ['--arg=-diagnostics-companion-expanded']
            if screen == 'voice': command += ['--arg=-diagnostics.voiceRailPrototype', '--arg=YES']
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
            if screen == 'effort':
                run('wait', '--id', 'project-composer-effort', '--timeout', '30')
                run('tap', '--id', 'project-composer-effort')
                if unfold_effort:
                    run('wait', '--id', 'project-composer-effort:high' if variant == 'dark-large' else TARGETS[screen], '--timeout', '30')
                    run('hinge', '180')
                    run('orient', 'portrait')
            if screen in ['goal', 'voice']:
                entry = 'goal-status-pill' if screen == 'goal' else 'voice-rail-entry'
                run('--driver', 'wait', '--id', entry, '--timeout', '30')
                run('--driver', 'tap', '--id', entry)
            if screen == 'browser':
                run('--driver', 'wait', '--id', 'preview-pages-menu', '--timeout', '30')
                run('--driver', 'tap', '--id', 'preview-pages-menu')
                run('--driver', 'tap', '--id', 'preview-page-5173')
            run('wait', '--id', 'project-composer-effort:high', '--timeout', '30') if screen == 'effort' and variant == 'dark-large' else run('wait', '--id', TARGETS[screen], '--timeout', '30')
            state = json.loads(run('--driver', 'state'))
            if args.expanded:
                assert (state.get('pane') or {}).get('expanded') is True, 'Fixture did not open an expanded pane'
            if state.get('alerts') == ['Unlock this device to open saved notification settings.']:
                run('--driver', 'tap', '--id', 'notification-error-dismiss')
            elif state.get('alerts'):
                raise RuntimeError('Unexpected alert: ' + repr(state['alerts']))
            if screen == 'terminal':
                keyboard_tree = run('tree', '--json')
                if 'Speed up your typing' in keyboard_tree:
                    run('tap', '--label', 'Continue')
                run('wait', '--id', 'terminal-key-ctrl', '--timeout', '15')
            tree = run('tree', '--json')
            (args.output / (name + '.json')).write_text(tree)
            (args.output / (name + '-state.json')).write_text(json.dumps(state, indent=2) + '\n')
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

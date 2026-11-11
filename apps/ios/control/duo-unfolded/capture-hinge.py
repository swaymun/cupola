#!/usr/bin/env python3
"""Owned launch fixtures and native captures, on an already acquired pooled lane."""
import argparse, hashlib, json, os, shutil, subprocess, time
from pathlib import Path
ROOT = Path(__file__).resolve().parents[4]
CTL = ROOT / 'scripts/wonderctl'
p = argparse.ArgumentParser()
p.add_argument('--device', choices=['iphone','ipad','duo'], required=True)
p.add_argument('--phase', choices=['before','after'], required=True)
p.add_argument('--build-info', type=Path, help='Successful build sourceHead and sha256; required for baseline captures')
p.add_argument('--state', action='append')
p.add_argument('--output', type=Path, default=ROOT / '.local/wd-hinge-evidence')
p.add_argument('--replace', action='store_true')
p.add_argument('--theme', action='append', choices=['light','dark'])
p.add_argument('--large-text', action='store_true')
p.add_argument('--posture', action='append', choices=['folded','unfolded'])
a = p.parse_args()
env = dict(os.environ)
if a.phase == 'before': env['WONDER_CONTROL_DERIVED'] = str(ROOT / '.local/wd-hinge-baseline')
def run(*args):
    r = subprocess.run([str(CTL), '--device', a.device, *args], cwd=ROOT, env=env, capture_output=True, text=True)
    if r.returncode: raise RuntimeError(f'{args}: {r.stderr}\n{r.stdout}')
    return r.stdout.strip()
base = ['subagent-fixture','chat-layout','chat-layout-unsaved','project-subagents','terminal-available','project-files-send','open-project-thread','rail-fixture']
states = ['chat','chat-hidden','chats','details','details-expanded','pull-requests','pull-requests-expanded','reply-changes','reply-changes-expanded','agent-task','agent-task-expanded','files','terminal','computer','settings','settings-voice']
binary = Path(env.get('WONDER_CONTROL_DERIVED', ROOT / '.local/build/control-sim')) / 'Build/Products/Diagnostics-iphonesimulator/Wonder.app/Wonder'
sha = hashlib.sha256(binary.read_bytes()).hexdigest()
if a.phase == 'before' and a.build_info is None:
    p.error('--phase before requires --build-info from the preserved baseline build')
provenance = json.loads((a.build_info or ROOT / '.local/wd-hinge-after-build.json').read_text())
assert provenance.get('sourceHead'), 'Build provenance must name its source revision'
assert provenance['sha256'] == sha, 'Record the successful build revision and binary hash before capturing'
run('install')
for posture in (a.posture or ['folded','unfolded']) if a.device == 'duo' else [a.device]:
    if a.device == 'duo':
        env['WONDER_CONTROL_INPUT'] = 'axe' if posture == 'folded' else 'rig'
        run('hinge', '0' if posture == 'folded' else '180'); run('orient','portrait')
    for theme in a.theme or ['light','dark']:
        out = a.output / a.phase / (('duo-' + posture) if a.device == 'duo' else a.device) / (theme + ('-ax5' if a.large_text else ''))
        out.mkdir(parents=True, exist_ok=True)
        previous = out / 'build.json'
        if previous.exists() and list(out.glob('*.png')):
            for capture in out.glob('*.png'):
                metadata = capture.with_name(capture.stem + '-build.json')
                if not metadata.exists(): metadata.write_text(previous.read_text())
        requested = a.state or [state for state in states if posture != 'folded' or not state.endswith('-expanded')]
        previous_frame = None
        previous_state = None
        for state in requested:
            if not a.replace and all((out / (state + suffix)).exists() for suffix in ['.png','.json','-state.json','-launch.json']): continue
            print(a.phase, a.device, posture, theme, state, flush=True)
            args = ['--driver','launch']
            fixtures = base + (['sidebar-open'] if state == 'chats' else []) + (['project-pull-requests'] if state.startswith('pull-requests') else []) + (['terminal-preview'] if state == 'terminal' else []) + (['computer-session-fixture','computer-session-available-fixture','computer-menu-fixture'] if state == 'computer' else [])
            for f in fixtures: args += ['--fixture', f]
            extra = ['-wonder.sidebar.hidden', 'NO' if state in ['chat','chats'] else 'YES', '-wonder.companionPane.flipped','NO', '-diagnostics-theme', 'cupola-' + theme, '-AppleInterfaceStyle',theme.title(), '-composerPills','computer,files,terminal,preview,attention,goal,agentTasks,git']
            if a.large_text: extra += ['-UIPreferredContentSizeCategoryName', 'UICTContentSizeCategoryAccessibilityXXXL']
            if state.startswith(('details','pull-requests','reply-changes','agent-task','files','terminal','computer')):
                extra += ['-diagnostics-companion-' + state.removesuffix('-expanded')]
                if state.startswith('reply-changes'): extra += ['-diagnostics-app-store']
                if state.endswith('-expanded'): extra += ['-diagnostics-companion-expanded']
            if state.startswith('settings'): extra += ['-diagnostics-settings-open']
            unfolded_voice = a.device == 'duo' and posture == 'unfolded' and state == 'settings-voice'
            if unfolded_voice and a.phase == 'after': extra += ['-diagnostics-settings-voice']
            args += ['--arg=' + v for v in extra]
            try:
                if unfolded_voice and a.phase == 'before':
                    run('hinge','0'); run('orient','portrait'); env['WONDER_CONTROL_INPUT'] = 'axe'
                run(*args)
                semantic = json.loads(run('--driver','state'))
                if semantic.get('alerts') == ['Unlock this device to open saved notification settings.']:
                    run('--driver','tap','--id','notification-error-dismiss')
                target = 'settings-add-computer' if state.startswith('settings') else 'sidebar-cupola' if state == 'chats' else ('thread-details-rename' if a.phase == 'after' else 'thread-details-name') if state.startswith('details') else 'pull-requests' if state.startswith('pull-requests') else 'reply-changes' if state.startswith('reply-changes') else 'project-subagent-transcript' if state.startswith('agent-task') else 'workspace-file-list' if state == 'files' else 'terminal-output' if state == 'terminal' else 'computer-session-container' if state == 'computer' else 'message-draft'
                run('wait','--id',target,'--timeout','30')
                if state == 'terminal' and 'Speed up your typing' in run('tree','--json'):
                    run('tap','--label','Continue')
                if state == 'settings-voice':
                    if not (unfolded_voice and a.phase == 'after'):
                        run('reveal','up','--id','settings-voice')
                        run('tap','--id','settings-voice')
                    if unfolded_voice and a.phase == 'before':
                        run('hinge','180'); run('orient','portrait'); env['WONDER_CONTROL_INPUT'] = 'rig'
                    run('wait','--id','voice-key-computer','--timeout','60')
                time.sleep(0.6)
                tree = run('tree','--json')
                (out / (state + '.json')).write_text(tree)
                shot_args = ['shot', 'wd-hinge-' + a.phase + '-' + posture + '-' + theme + '-' + state]
                if a.device == 'duo': shot_args += ['--display', 'primary' if posture == 'folded' else 'primary-1']
                shot = run(*shot_args).splitlines()[-1]
                frame_hash = hashlib.sha256((ROOT / shot).read_bytes()).hexdigest()
                # Chat/hidden/list and restored expansion can intentionally match.
                identity = lambda name: 'chat' if name.startswith('chat') else name.removesuffix('-expanded')
                if previous_state and identity(state) != identity(previous_state):
                    assert frame_hash != previous_frame, f'Simulator framebuffer did not update between {previous_state} and {state}; inspect the capture and restart only the owned lane'
                previous_frame, previous_state = frame_hash, state
                shutil.copy2(ROOT / shot, out / (state + '.png'))
                (out / (state + '-state.json')).write_text(run('--driver','state'))
                (out / (state + '-launch.json')).write_text(json.dumps(args))
                (out / (state + '-build.json')).write_text(json.dumps(provenance, indent=2))
                (out / (state + '-failure.txt')).unlink(missing_ok=True)
            except Exception as e:
                (out / (state + '-failure.txt')).write_text(str(e))
                print(str(e)[-900:], flush=True)
                # Preserve the failed state before any subsequent fixture launch.
                try:
                    (out / (state + '-failure-state.json')).write_text(run('--driver','state'))
                    failure_shot = ['shot', 'wd-hinge-failure-' + state]
                    if a.device == 'duo': failure_shot += ['--display', 'primary' if posture == 'folded' else 'primary-1']
                    shot = run(*failure_shot).splitlines()[-1]
                    shutil.copy2(ROOT / shot, out / (state + '-failure.png'))
                finally:
                    raise
        assert hashlib.sha256(binary.read_bytes()).hexdigest() == sha, 'Build changed during capture'
        (out / 'build.json').write_text(json.dumps({**provenance, 'phase':a.phase, 'device':a.device, 'posture':posture},indent=2))

#!/usr/bin/env python3
"""Capture K3b in caller-owned pooled lanes; validate real fixture pixels.

No live host/model calls or native Duo taps. Half-fold uses K2's native
90-degree/landscape-left pose plus the explicit horizontal-fold fixture.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shlex
import shutil
import signal
import subprocess
import time

ROOT = Path(__file__).resolve().parents[4]
CTL = ROOT / 'scripts/wonderctl'
VISION = r'''
import Foundation
import Vision
import ImageIO
let url = URL(fileURLWithPath: CommandLine.arguments[1])
let request = VNRecognizeTextRequest()
request.recognitionLevel = .accurate
request.recognitionLanguages = ["en-US"]
request.minimumTextHeight = 0.002
try VNImageRequestHandler(url: url).perform([request])
let text = (request.results ?? []).compactMap { $0.topCandidates(1).first?.string }.joined(separator: " ")
print(text)
let joined = text.lowercased().filter { $0.isLetter || $0.isNumber }
guard joined.contains("tripnotes") else { exit(2) }
if CommandLine.arguments.count > 2 && !joined.contains("incontrol") { exit(3) }
'''


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('output', type=Path)
    parser.add_argument('--device', required=True, choices=('iphone', 'duo'))
    parser.add_argument('--record', action='store_true')
    parser.add_argument('--keyboard', action='store_true', help='capture the active keyboard and its Clipboard accessory')
    args = parser.parse_args()
    if not os.environ.get('WONDER_CONTROL_LANE_OWNER_PID'):
        parser.error('Acquire a pooled lane with WONDER_CONTROL_LANE_OWNER_PID first')
    if args.output.exists() and any(args.output.iterdir()):
        parser.error('Preserve prior evidence; choose a fresh output directory')
    args.output.mkdir(parents=True, exist_ok=True)
    args.output = args.output.resolve()
    rows = []
    base = [str(CTL), '--device', args.device]

    def run(command, timeout=180):
        start = time.monotonic()
        result = subprocess.run(command, cwd=ROOT, text=True, capture_output=True, timeout=timeout)
        rows.append(dict(command=shlex.join(command), code=result.returncode, seconds=round(time.monotonic()-start, 3),
                         output=result.stdout, error=result.stderr))
        (args.output / 'commands.json').write_text(json.dumps(rows, indent=2) + '\n')
        if result.returncode:
            raise RuntimeError(rows[-1])
        return result.stdout.strip()

    def ctl(*words):
        return run(base + list(words))

    swift = args.output / 'check-picture.swift'
    validator = args.output / 'check-picture'
    swift.write_text(VISION)
    run(['swiftc', str(swift), '-o', str(validator)])

    def check_picture(path, controlling=False):
        # Independent native OCR checks pixels, not the app's semantic state.
        text = run([str(validator), str(path)] + (['controlling'] if controlling else []))
        path.with_suffix('.ocr.txt').write_text(text + '\n')

    def launch(flags, theme):
        words = ['--driver', 'launch']
        for flag in flags:
            words += ['--fixture', flag]
        ctl(*words, '--arg=-diagnostics-theme', '--arg=cupola-' + theme,
            '--arg=-AppleInterfaceStyle', '--arg=' + theme.title())

    udid = ctl('device').splitlines()[-1]
    ctl('install')
    common = ['computer-session-fixture', 'computer-session-available-fixture', 'computer-reconnect',
              'computer-dock', 'no-hardware-keyboard']
    if args.device == 'duo':
        # As in K2: establish an active inner scene before changing displays.
        ctl('hinge', '180')
        ctl('orient', 'portrait')
        launch(common, 'light')
        ctl('--driver', 'assert', '--path', 'computer.hasPicture', '--equals', 'true', '--timeout', '30')
    postures = [('iphone', None, 'portrait')] if args.device == 'iphone' else [
        ('folded', 0, 'portrait'), ('open', 180, 'portrait'), ('half-folded', 90, 'landscape-left')]
    platforms = [('mac', []), ('linux', ['linux-host']), ('windows', ['computer-windows-host'])]
    if args.record:
        platforms = platforms[:1]
        postures = [('iphone', None, 'portrait')] if args.device == 'iphone' else [
            ('open', 180, 'portrait'), ('folded', 0, 'portrait')]
    captures = []
    started = time.monotonic()
    for posture, angle, orientation in postures:
        display = 'primary-1' if angle is not None and angle != 0 else 'primary'
        if angle is not None:
            ctl('hinge', str(angle))
            ctl('orient', orientation)
            run(['xcrun', 'simctl', 'io', udid, 'enumerate'])
            time.sleep(2)
        for platform, platform_flags in platforms:
            for theme in (['light'] if args.record else ['light', 'dark']):
                name = f'{platform}-{posture}-{theme}' + ('-recording' if args.record else '')
                flags = common + platform_flags
                if posture == 'half-folded': flags += ['computer-fold']
                flags += ['computer-full-screen'] if args.record else ['computer-control-active']
                if args.keyboard: flags += ['computer-keyboard']
                recorder = None
                recording_log = None
                try:
                    if args.record:
                        recording_path = args.output / (name + '.record.log')
                        recording_log = recording_path.open('w')
                        record = ['xcrun', 'simctl', 'io', udid, 'recordVideo', '--codec=h264']
                        if args.device == 'duo': record += ['--display=' + display]
                        recorder = subprocess.Popen(record + [str(args.output / (name + '.mp4'))],
                                                    stdout=recording_log, stderr=recording_log)
                        deadline = time.monotonic() + 30
                        while recorder.poll() is None and 'Recording started' not in recording_path.read_text() and time.monotonic() < deadline:
                            time.sleep(0.1)
                        if recorder.poll() is not None or 'Recording started' not in recording_path.read_text():
                            raise RuntimeError('Recorder did not start; inspect ' + str(recording_path))
                    launch(flags, theme)
                    ctl('--driver', 'assert', '--path', 'computer.hasPicture', '--equals', 'true', '--timeout', '30')
                    ctl('--driver', 'assert', '--path', 'computer.dockAvailable', '--equals', 'true', '--timeout', '15')
                    if args.record: ctl('signal', 'computer-dock-take-control')
                    ctl('--driver', 'assert', '--path', 'computer.controlling', '--equals', 'true', '--timeout', '15')
                    accessories = ['computer-session-clipboard', 'computer-session-key-escape', 'computer-session-microphone'] if args.keyboard else ['computer-session-microphone']
                    for identifier in ['computer-session-done', 'computer-session-mini-dock', 'computer-session-keyboard'] + accessories:
                        ctl('wait', '--id', identifier, '--timeout', '15')
                    ctl('wait', '--id', 'computer-viewport-appearances', '--contains', 'appearances: 1; keyboards: 1;', '--timeout', '15')
                    shot = ctl('shot', name, *(['--display', display] if args.device == 'duo' else []))
                    source = Path(shot.splitlines()[-1]); source = source if source.is_absolute() else ROOT / source
                    target = args.output / (name + '.png'); shutil.copy2(source, target)
                    (args.output / (name + '.ax.json')).write_text(ctl('tree', '--json') + '\n')
                    state = json.loads(ctl('--driver', 'state'))
                    (args.output / (name + '.state.json')).write_text(json.dumps(state, indent=2) + '\n')
                    assert state['computer']['hasPicture'] and state['computer']['controlling']
                    if args.keyboard: assert state['computer']['keyboardPresented']
                    check_picture(target, controlling=args.record)
                    if recorder and recorder.poll() is not None:
                        raise RuntimeError('Recorder exited before control assertions')
                    captures.append(dict(name=name, path=str(target), device=args.device, udid=udid, platform=platform,
                                         posture=posture, theme=theme, fixtures=flags, orientation=orientation,
                                         sha256=hashlib.sha256(target.read_bytes()).hexdigest()))
                finally:
                    if recorder:
                        if recorder.poll() is None:
                            recorder.send_signal(signal.SIGINT)
                            recorder.wait(timeout=30)
                        recording_log.close()
                        if recorder.returncode != 0: raise RuntimeError('Recorder failed: ' + name)
                if args.record:
                    video = args.output / (name + '.mp4')
                    probe = json.loads(run(['ffprobe', '-v', 'error', '-select_streams', 'v:0', '-count_frames',
                                           '-show_entries', 'stream=width,height,nb_read_frames:format=duration', '-of', 'json', str(video)]))
                    assert int(probe['streams'][0]['nb_read_frames']) > 0 and float(probe['format']['duration']) > 1
                    end = args.output / (name + '-end.png')
                    count = int(probe['streams'][0]['nb_read_frames'])
                    run(['ffmpeg', '-v', 'error', '-y', '-i', str(video), '-vf', f'select=eq(n\\,{count-1})', '-frames:v', '1', str(end)])
                    check_picture(end, controlling=True)
                    captures[-1].update(video=str(video), probe=probe, decodedEnd=str(end), videoSHA256=hashlib.sha256(video.read_bytes()).hexdigest())
                print('PASS ' + name, flush=True)
    manifest = dict(head=run(['git', 'rev-parse', 'HEAD']), seconds=round(time.monotonic()-started, 2),
                    evidence='Synthetic host/apps; explicit horizontal fold; native pixels checked with Vision; no physical recording-indicator claim', captures=captures)
    (args.output / 'manifest.json').write_text(json.dumps(manifest, indent=2) + '\n')
    print(f'PASS: {len(captures)} captures in {manifest["seconds"]}s', flush=True)


if __name__ == '__main__':
    main()

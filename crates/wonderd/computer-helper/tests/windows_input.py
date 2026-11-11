#!/usr/bin/env python3
"""Windows console acceptance: real WGC/WebRTC, consent, click and text.

Run in the interactive console session (not SSH's session 0), with the native
helper and computer-parity-receiver built from this tree. Only this run's
fixture windows and its helper's own consent dialog are operated on.
"""
import argparse
import ctypes
from ctypes import wintypes
import json
import os
from pathlib import Path
import subprocess
import sys
import threading
import time
import uuid

from parity import Peer, wait_for


def allow_owned_dialog(pid):
    """Answer only the consent button belonging to the spawned test helper."""
    user = ctypes.WinDLL('user32', use_last_error=True)
    callback = ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)
    user.GetWindowThreadProcessId.argtypes = [wintypes.HWND, ctypes.POINTER(wintypes.DWORD)]
    user.GetWindowTextW.argtypes = [wintypes.HWND, wintypes.LPWSTR, ctypes.c_int]
    user.SendMessageW.argtypes = [wintypes.HWND, wintypes.UINT, wintypes.WPARAM, wintypes.LPARAM]
    user.SendMessageW.restype = ctypes.c_ssize_t
    buttons = []

    @callback
    def child(hwnd, _):
        text = ctypes.create_unicode_buffer(256)
        user.GetWindowTextW(hwnd, text, len(text))
        if text.value == 'Allow':
            buttons.append(hwnd)
        return True

    @callback
    def window(hwnd, _):
        owner = wintypes.DWORD()
        user.GetWindowThreadProcessId(hwnd, ctypes.byref(owner))
        if owner.value == pid:
            user.EnumChildWindows(hwnd, child, 0)
        return True

    user.EnumWindows(window, 0)
    if len(buttons) != 1:
        return False
    user.SendMessageW(buttons[0], 0x00F5, 0, 0)  # BM_CLICK, native consent UI
    return True


def window_at(x, y):
    """Describe the top-level window under a screen point (failure diagnosis)."""
    user = ctypes.WinDLL('user32', use_last_error=True)
    user.WindowFromPoint.argtypes = [wintypes.POINT]
    user.WindowFromPoint.restype = wintypes.HWND
    user.GetAncestor.argtypes = [wintypes.HWND, wintypes.UINT]
    user.GetAncestor.restype = wintypes.HWND
    top = user.GetAncestor(user.WindowFromPoint(wintypes.POINT(int(x), int(y))), 2)
    title = ctypes.create_unicode_buffer(256)
    name = ctypes.create_unicode_buffer(256)
    pid = wintypes.DWORD()
    user.GetWindowTextW(top, title, 256)
    user.GetClassNameW(top, name, 256)
    user.GetWindowThreadProcessId(top, ctypes.byref(pid))
    return {'hwnd': top, 'title': title.value, 'class': name.value, 'pid': pid.value}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--helper', required=True)
    parser.add_argument('--receiver', required=True)
    parser.add_argument('--ffmpeg', default='ffmpeg')
    parser.add_argument('--out', type=Path, required=True)
    args = parser.parse_args()
    args.out.mkdir(parents=True, exist_ok=False)
    state = args.out / 'fixture-state.json'
    summary = {'surface': 'Windows console / native helper / libwebrtc test client', 'checks': {}}
    peer = fixture = owner = None
    stopped = threading.Event()
    heartbeat = None

    def require(name, value):
        summary['checks'][name] = bool(value)
        if not value:
            raise RuntimeError(name)

    def fixture_state():
        try:
            return json.loads(state.read_text())
        except (FileNotFoundError, PermissionError, json.JSONDecodeError):
            return {}

    try:
        fixture = subprocess.Popen([sys.executable, str(Path(__file__).with_name('live_fixture.py')),
                                    '--state', str(state)])
        require('fixture_ready', wait_for(lambda: fixture_state().get('ready')))
        # A small console desktop stacks fixture window B over A, and the
        # helper's always-on-top Stop notice (top centre, 150 px) covers the
        # top of A. The helper rightly refuses covered points, so lay the
        # fixture out clear of both.
        state.with_suffix('.command.json').write_text(json.dumps(
            {'id': 'place', 'action': 'place', 'windows': {'A': [40, 200], 'B': [600, 400]}}))
        require('fixture_placed', wait_for(lambda: fixture_state().get('commandID') == 'place'
                                           and abs(fixture_state()['windows']['A']['bounds'][1] - 200) < 12
                                           and fixture_state()['windows']['B']['bounds'][0] > 560))
        discovery = subprocess.run([args.helper, '--discover-sources'], input='{"allWindows":true}\n',
                                   capture_output=True, text=True, encoding='utf-8', timeout=15, check=True)
        inventory = json.loads(discovery.stdout.splitlines()[0])
        source = next(s for s in inventory['sources'] if s.get('windowTitle') == 'Computer parity A')
        # Like wonderd: a separate input-owner process injects input, and the
        # capture helper reaches it by loopback address and shared handshake.
        cookie = uuid.uuid4().hex
        owner = subprocess.Popen([args.helper, '--input-owner'], stdout=subprocess.PIPE, text=True,
                                 env={**os.environ, 'WONDER_COMPUTER_USE_HANDSHAKE': cookie})
        address = owner.stdout.readline().strip()
        require('input_owner_started', address)
        peer = Peer([args.helper], args.receiver, args.out / 'peer', cookie=cookie,
                    env={'WONDER_INPUT_OWNER': address})
        summary['helperVersion'] = subprocess.check_output([args.helper, '--version'], text=True).strip()
        require('prepare', peer.request('capture.prepare', {**peer.session, 'viewerCount': 1,
                                                           'videoQuality': 'standard'})['accepted'])
        require('pick', peer.request('capture.pick', {**peer.session, 'sourceID': source['id']})['accepted'])
        status = peer.request('capture.status')
        lease = {**peer.session, 'sourceID': source['id'], 'geometryRevision': status['geometryRevision'],
                 'leaseID': 'win-input-lease', 'requestID': 'win-input-request'}
        consent = {}
        thread = threading.Thread(target=lambda: consent.update(peer.request('control.consent', lease)))
        thread.start()
        require('native_consent_dialog', wait_for(lambda: allow_owned_dialog(peer.helper.pid)))
        thread.join(timeout=15)
        require('consent_granted', consent.get('granted'))
        require('activate', peer.request('control.activate', lease)['activated'])

        def renew():
            while not stopped.wait(2):
                try:
                    if not peer.request('control.heartbeat', lease)['renewed']:
                        return
                except Exception:
                    return
        heartbeat = threading.Thread(target=renew)
        heartbeat.start()
        first = {**lease, 'sequence': 1, 'actions': [{'type': 'text', 'text': 'must not appear'}]}
        summary['beforeStart'] = peer.request('control.input', first)
        require('input_before_capture_refused', summary['beforeStart'].get('accepted') is False
                and summary['beforeStart'].get('reason') == 'capture_not_live')
        require('start', peer.request('capture.start', peer.session)['accepted'])
        require('capture_live', wait_for(lambda: peer.request('capture.status')['state'] == 'capturing', 20))
        require('decoded_video', wait_for(lambda: any(s.get('k') == 'content_frame' for s in peer.stats), 20))
        sequence = 0

        def send(actions):
            nonlocal sequence
            sequence += 1
            result = peer.request('control.input', {**lease, 'sequence': sequence, 'actions': actions})
            require('input_' + str(sequence), result.get('accepted'))

        def click(target):
            row = fixture_state()['windows']['A']
            ox, oy, w, h = row['bounds']
            x, y = row['targets'][target]
            send([{'type': 'pointer', 'phase': phase, 'button': 'left',
                   'x': (x - ox) / (w - 1), 'y': (y - oy) / (h - 1)} for phase in ['down', 'up']])

        click('button')
        require('click_reached_desktop', wait_for(lambda: fixture_state()['windows']['A']['clicks'] == 1))
        click('entry')
        text = 'WIN-INPUT live native control'
        send([{'type': 'text', 'text': text}])
        require('text_reached_desktop', wait_for(lambda: fixture_state()['windows']['A']['text'] == text))
        summary['liveStatus'] = peer.request('capture.status')
        summary['fixtureAfterInput'] = fixture_state()
        with (args.out / 'screenshot.log').open('w') as log:
            subprocess.run([args.ffmpeg, '-hide_banner', '-loglevel', 'error', '-f', 'gdigrab', '-i', 'desktop',
                            '-frames:v', '1', '-y', str(args.out / 'desktop.png')],
                           stdout=log, stderr=subprocess.STDOUT, timeout=20, check=True)
        require('desktop_screenshot', (args.out / 'desktop.png').stat().st_size > 0)
        require('capture_stop', peer.request('capture.stop', peer.session)['accepted'])
        summary['afterStop'] = peer.request('control.input', {**lease, 'sequence': sequence + 1,
                                                            'actions': [{'type': 'text', 'text': 'must not appear'}]},
                                           allow_error=True)
        require('input_after_stop_refused', summary['afterStop'].get('accepted') is False)
        time.sleep(.2)
        require('no_late_text', fixture_state()['windows']['A']['text'] == text)
    except Exception as error:
        summary['error'] = str(error)
        if peer:
            summary['failureStatus'] = peer.request('capture.status')
        try:
            summary['failureUnderEntry'] = window_at(*fixture_state()['windows']['A']['targets']['entry'])
        except Exception as diagnosis:
            summary['failureUnderEntry'] = str(diagnosis)
    finally:
        stopped.set()
        if heartbeat:
            heartbeat.join(timeout=16)
        if peer:
            peer.close()
        if owner:
            owner.terminate()
        if fixture:
            fixture.terminate()
            fixture.wait(timeout=10)
        (args.out / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n')
        print(json.dumps(summary, indent=2))
    return int('error' in summary)


if __name__ == '__main__':
    raise SystemExit(main())

#!/usr/bin/env python3
"""Timed capture run: the helper captures the live display and a libwebrtc
receiver decodes it. Prints helper-sent and receiver-decoded fps and helper CPU.
Needs motion on the display (see the README) and a parity-tests build.
VM evidence; never targets owner chats.
"""
import argparse
import json
import os
from pathlib import Path
import sys
import time

sys.path.insert(0, str(Path(__file__).resolve().parent))
import parity  # noqa: E402

try:
    import psutil
except ImportError:
    psutil = None


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--helper', required=True)
    parser.add_argument('--receiver', required=True)
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--seconds', type=float, default=30)
    parser.add_argument('--quality', default='auto')
    parser.add_argument('--max-fps', type=int, help='videoMaxFramesPerSecond request')
    parser.add_argument('--then-max-fps', type=int, help='after the window, lower the ceiling live with capture.setQuality and measure again')
    parser.add_argument('--expect-codec', choices=['video/H264', 'video/VP8'])
    parser.add_argument('--expect-mode', choices=['hardware', 'software'])
    parser.add_argument('--expect-cap', type=int)
    args = parser.parse_args()
    peer = parity.Peer([args.helper], args.receiver, args.out)
    result = {'quality': args.quality, 'maxFps': args.max_fps}
    try:
        peer.request('capabilities')
        extra = {'videoMaxFramesPerSecond': args.max_fps} if args.max_fps else {}
        peer.request('capture.prepare', {**peer.session, 'viewerCount': 1, 'videoQuality': args.quality, **extra})
        display = 'display:1' if os.name == 'nt' else 'display:x11'
        assert peer.request('capture.pick', {**peer.session, 'sourceID': display})['accepted']
        assert peer.request('capture.start', peer.session)['accepted']
        assert parity.wait_for(lambda: any(s.get('k') == 'content_frame' for s in peer.stats), 30), 'no frame'
        time.sleep(5)  # let the rate controller settle before the window
        status = peer.request('capture.status')
        result['framesPerSecondReported'] = status.get('configuration', {}).get('framesPerSecond')
        proc = psutil.Process(peer.helper.pid) if psutil else None
        cpu0 = sum(proc.cpu_times()[:2]) if proc else 0
        rx0 = next((s for s in reversed(peer.stats) if s.get('k') == 'rx_stats'), {})
        seq0 = peer.request('capture.status')['frameSequence']
        t0 = time.monotonic()
        if psutil:
            psutil.cpu_percent()
        time.sleep(args.seconds)
        dt = time.monotonic() - t0
        seq1 = peer.request('capture.status')['frameSequence']
        rx1 = next((s for s in reversed(peer.stats) if s.get('k') == 'rx_stats'), {})
        result.update(
            seconds=round(dt, 1),
            capturedFps=round((seq1 - seq0) / dt, 1),
            decodedFps=round((rx1.get('frames_decoded', 0) - rx0.get('frames_decoded', 0)) / dt, 1),
            helperCpuPercentOfOneCore=round((sum(proc.cpu_times()[:2]) - cpu0) / dt * 100, 1) if proc else None,
            systemCpuPercent=psutil.cpu_percent() if psutil else None,
            cores=os.cpu_count(),
            codec=rx1.get('codec'),
            width=rx1.get('width'), height=rx1.get('height'),
        )
        # The helper's own sender stats (WONDER_COMPUTER_STATS lines on stderr).
        sent = []
        for line in (args.out / 'helper.stderr.log').read_text().splitlines():
            if line.startswith('WONDER_COMPUTER_STATS '):
                v = json.loads(line.split(' ', 1)[1])
                if 'fps' in v:
                    sent.append(v)
        if sent:
            tail = sent[-10:]
            result['senderFpsLast10'] = [round(v['fps'] or 0, 1) for v in tail]
            result['senderLimit'] = tail[-1].get('limit')
            result['targetBitrate'] = tail[-1].get('targetBitrate')
            result['encoder'] = tail[-1].get('encoder')
            result['hardware'] = tail[-1].get('hardware')
            result['encoderCap'] = tail[-1].get('encoderCap')
        if args.then_max_fps:
            peer.request('capture.setQuality', {**peer.session, 'videoMaxFramesPerSecond': args.then_max_fps})
            time.sleep(8)
            sent_before = len(sent) if sent else 0
            seq0 = peer.request('capture.status')['frameSequence']
            rx0 = next((s for s in reversed(peer.stats) if s.get('k') == 'rx_stats'), {})
            time.sleep(15)
            seq1 = peer.request('capture.status')['frameSequence']
            rx1 = next((s for s in reversed(peer.stats) if s.get('k') == 'rx_stats'), {})
            lines = [json.loads(l.split(' ', 1)[1]) for l in (args.out / 'helper.stderr.log').read_text().splitlines()
                     if l.startswith('WONDER_COMPUTER_STATS ')]
            tail = [v for v in lines if 'fps' in v][-5:]
            result['afterSetQuality'] = dict(maxFps=args.then_max_fps, capturedFps=round((seq1 - seq0) / 15, 1),
                decodedFps=round((rx1.get('frames_decoded', 0) - rx0.get('frames_decoded', 0)) / 15, 1),
                senderFpsLast5=[round(v['fps'] or 0, 1) for v in tail], targetBitrate=tail[-1].get('targetBitrate') if tail else None)
    finally:
        peer.close()
    print(json.dumps(result))
    (args.out / 'summary.json').write_text(json.dumps(result, indent=2) + '\n')
    if args.expect_codec:
        assert (result.get('codec') or '').startswith(args.expect_codec), result
    if args.expect_mode:
        assert result.get('hardware') == (args.expect_mode == 'hardware'), result
    if args.expect_cap is not None:
        assert result['framesPerSecondReported'] == args.expect_cap, result
    assert result['decodedFps'] > 0 and result['capturedFps'] > 0, result


if __name__ == '__main__':
    main()

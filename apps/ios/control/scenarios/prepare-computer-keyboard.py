#!/usr/bin/env python3
"""Prime Apple's first-use keyboard intro in an owned simulator lane.

Run before computer-full-screen-keyboard.wctl; this changes only the lane's
OS first-use state. The scenario still launches its own explicit app fixtures.
"""
import argparse
import json
import os
import subprocess
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[4]
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--device', choices=('iphone', 'ipad'), required=True)
args = parser.parse_args()
os.environ['WONDER_CONTROL_REQUIRE_LANE'] = '1'

def ctl(*words, capture=False):
    return subprocess.run([str(ROOT / 'scripts/wonderctl'), '--device', args.device, *words],
                          cwd=ROOT, check=True, text=True, capture_output=capture, timeout=120).stdout

def nodes(value):
    if isinstance(value, dict):
        yield value
        for child in value.get('children', []):
            yield from nodes(child)
    elif isinstance(value, list):
        for child in value:
            yield from nodes(child)

ctl('launch', '--fixture', 'computer-session-fixture', '--fixture', 'computer-session-available-fixture',
    '--fixture', 'computer-control-active', '--fixture', 'computer-keyboard', '--fixture', 'no-hardware-keyboard')
# The fixture raises the system keyboard as control starts.
ctl('wait', '--id', 'computer-session-key-escape', '--timeout', '30')
deadline = time.monotonic() + 60
while time.monotonic() < deadline:
    raw = ctl('tree', '--json', capture=True)
    evidence = ROOT / '.local/evidence/launch-wave/K2'
    evidence.mkdir(parents=True, exist_ok=True)
    (evidence / ('keyboard-prime-' + args.device + '-last.ax.json')).write_text(raw)
    current = list(nodes(json.loads(raw)))
    labels = [node.get('AXLabel') or '' for node in current]
    if any('Speed up your typing by sliding your finger' in label for label in labels):
        print('Apple keyboard introduction: dismissing Continue', flush=True)
        ctl('tap', '--label', 'Continue')
        ctl('wait', '--label', 'Continue', '--gone', '--timeout', '30')
        print('OS introduction dismissed; the scenario launches a fresh input owner', flush=True)
        break
    elif any(node.get('AXUniqueId') == 'delete' for node in current):
        print('Native keyboard ready; lane first-use state is primed', flush=True)
        break
    else:
        time.sleep(0.1)
else:
    print(ctl('shot', 'keyboard-prime-' + args.device + '-failed', capture=True), flush=True)
    raise SystemExit('Native keyboard did not become ready; inspect the owned lane before retrying')
ctl('terminate')

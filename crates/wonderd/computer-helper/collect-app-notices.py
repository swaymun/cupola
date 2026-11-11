#!/usr/bin/env python3
"""Copy audited release notices into the app's offline acknowledgements resources.

Run once per target with --build pointing to release-manifest.py's output.
License payloads are preserved verbatim; no dependencies or texts are downloaded.
"""
import argparse
import hashlib
import json
from pathlib import Path
import re
import tomllib

HERE = Path(__file__).resolve().parent
TARGETS = {'linux-x86_64': 'Linux', 'windows-x86_64': 'Windows'}


def collect(build, out):
    manifest = json.loads((build / 'wonder-webrtc-build.json').read_text(encoding='utf-8'))
    target = manifest.get('target')
    if target not in TARGETS or any(manifest.get(key) != value for key, value in {
        'sourceBuilt': True, 'verificationOnly': False, 'rtcUseH264': True,
        'h264Software': 'bundled-openh264', 'ciscoBinaryLicense': False,
        'rtcUseH265': False, 'forbiddenSymbolsFound': False,
    }.items()):
        raise SystemExit('Use an audited H264/VP8 source release manifest for Linux or Windows x64.')
    if not re.fullmatch('[0-9a-f]{40}', manifest.get('webrtcRevision', '')):
        raise SystemExit('The release manifest needs its full WebRTC source revision.')
    payloads = {}
    for name, key in [('LICENSE.md', 'licenseSha256'), ('RUST_NOTICES.md', 'rustNoticesSha256')]:
        data = (build / name).read_bytes()
        if hashlib.sha256(data).hexdigest() != manifest.get(key):
            raise SystemExit('Audited notice hash mismatch: ' + name)
        payloads[name] = data
    inventory = json.loads((build / 'rust-inventory.json').read_text(encoding='utf-8'))
    locked = {(p['name'], p['version']) for p in tomllib.loads((HERE / 'Cargo.lock').read_text())['package']}
    if inventory['unresolved'] or any((p['name'], p['version']) not in locked for p in inventory['packages']):
        raise SystemExit('Resolve the Rust notices against the current helper Cargo.lock first.')
    title = TARGETS[target] + ' computer sharing'
    header = (f'# {title}\n\n'
              'These components run on the connected computer. The phone app includes\n'
              'their full acknowledgements for offline reading.\n\n'
              f"Google WebRTC source revision: {manifest['webrtcRevision']}\n\n"
              'LiveKit Rust bindings: libwebrtc 0.3.52 / webrtc-sys 0.3.49\n\n'
              f"Native notice SHA-256: {manifest['licenseSha256']}\n\n"
              f"Rust notice SHA-256: {manifest['rustNoticesSha256']}\n\n"
              '## Google WebRTC and native dependencies\n\n').encode()
    data = header + payloads['LICENSE.md'] + b'\n\n## Rust dependencies\n\n' + payloads['RUST_NOTICES.md']
    out.mkdir(parents=True, exist_ok=True)
    destination = out / ('ComputerSharing-' + TARGETS[target] + '.md')
    destination.write_bytes(data)
    print(f'{destination}: {len(data)} bytes, SHA-256 {hashlib.sha256(data).hexdigest()}')
    return destination


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--build', type=Path, required=True)
    parser.add_argument('--out', type=Path, default=HERE / 'notices')
    args = parser.parse_args()
    collect(args.build, args.out)

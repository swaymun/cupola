#!/usr/bin/env python3
"""Sign the finished bundle inside out. Never silently fall back to ad-hoc."""
import os
from pathlib import Path
import plistlib
import re
import subprocess
import sys


def run(*args):
    return subprocess.check_output(args, text=True, stderr=subprocess.STDOUT)


def identity():
    matches = re.findall(r'\b([A-F0-9]{40}) "([^"]+)"', run('security', 'find-identity', '-v', '-p', 'codesigning'))
    requested = os.environ.get('WONDER_APP_SIGNING_IDENTITY')
    default_prefix = 'Developer ID Application:' if os.environ.get('WONDER_NOTARIZE') == '1' else 'Apple Development:'
    candidates = [(sha, name) for sha, name in matches if
                  (requested in (sha, name) if requested else name.startswith(default_prefix))]
    if len(candidates) != 1:
        raise SystemExit('Select one valid certificate with WONDER_APP_SIGNING_IDENTITY. Ad-hoc signing is not supported.')
    sha, name = candidates[0]
    if not name.startswith(('Apple Development:', 'Developer ID Application:')):
        raise SystemExit('Use an Apple Development or Developer ID Application identity.')
    if os.environ.get('WONDER_NOTARIZE') == '1' and not name.startswith('Developer ID Application:'):
        raise SystemExit('Notarization requires a Developer ID Application certificate.')
    return sha, name


def sign(app, sha, name):
    app = app.resolve()
    if not (app / 'Contents/Info.plist').is_file():
        raise SystemExit('Expected a complete macOS app bundle.')
    timestamp = '--timestamp' if name.startswith('Developer ID Application:') else '--timestamp=none'
    def codesign(path, identifier=None, preserve=False):
        args = ['codesign', '--force', '--sign', sha, '--options', 'runtime', timestamp]
        if identifier:
            args += ['--identifier', identifier]
        if preserve:
            args += ['--preserve-metadata=entitlements']
        subprocess.run(args + [str(path)], check=True)

    # Preserve the vendor helper entitlements; sign leaves before their bundles.
    framework = app / 'Contents/Frameworks'
    if framework.exists():
        paths = sorted((p for p in framework.rglob('*') if not p.is_symlink()),
                       key=lambda p: len(p.parts), reverse=True)
        for path in paths:
            if path.is_file() and 'Mach-O' in run('file', '-b', str(path)):
                codesign(path, preserve=True)
            elif path.is_dir() and path.suffix in ('.app', '.xpc', '.framework'):
                codesign(path, preserve=True)
    with (app / 'Contents/Info.plist').open('rb') as stream:
        info = plistlib.load(stream)
    if info.get('CFBundleExecutable') != 'cupola':
        raise SystemExit('Only the windowless CLI bundle (scripts/package-cli-macos.sh) is signed here.')
    if not name.startswith('Developer ID Application:'):
        raise SystemExit('The CLI bundle requires the existing Developer ID Application identity.')
    bundle_id = info['CFBundleIdentifier']
    if bundle_id not in ('com.saimun.wonder', 'com.saimun.wonder.cli-staging'):
        raise SystemExit('Unsupported CLI bundle identity.')
    executables = {
        'MacOS/wonderd': bundle_id + '.daemon',
        'Helpers/WonderComputerUse': bundle_id + '.computer-use',
        # The same identity: one Screen Recording grant covers both paths.
        'Helpers/WonderComputerUseDiscovery': bundle_id + '.computer-use',
    }
    for relative, identifier in executables.items():
        path = app / 'Contents' / relative
        if not path.is_file():
            raise SystemExit(f'Missing required executable: {relative}')
        codesign(path, identifier)
    # Node and the SDK's native executable retain their vendor JIT entitlements.
    # Inspect Mach-O magic, not filename extensions (the SDK binary is `claude`).
    magic = {b'\xfe\xed\xfa\xce', b'\xce\xfa\xed\xfe', b'\xfe\xed\xfa\xcf', b'\xcf\xfa\xed\xfe',
             b'\xca\xfe\xba\xbe', b'\xbe\xba\xfe\xca', b'\xca\xfe\xba\xbf', b'\xbf\xba\xfe\xca'}
    for directory in ['node', 'claude-runtime']:
        for path in (app / 'Contents/Resources' / directory).rglob('*'):
            if path.is_file() and not path.is_symlink():
                with path.open('rb') as binary:
                    if binary.read(4) in magic:
                        codesign(path, preserve=True)
    codesign(app)
    subprocess.run(['codesign', '--verify', '--deep', '--strict', str(app)], check=True)
    print(f'Signed and verified {app} with {name}')


if __name__ == '__main__':
    sha, name = identity()
    if sys.argv[1:] == ['--check-identity']:
        print(sha)
    elif len(sys.argv) == 2:
        sign(Path(sys.argv[1]), sha, name)
    else:
        raise SystemExit('Usage: sign-macos-app.py APP | --check-identity')

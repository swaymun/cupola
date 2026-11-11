#!/usr/bin/env python3
"""Build the pinned H.264/VP8 release library on a Linux or Windows build VM.

Requires the platform C++/SDK build prerequisites used by webrtc-sys. This is
build tooling, never an installed Cupola component or a Python distribution.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys

WEBRTC = '51c367e209f8f12b39bd8dd0ca70bf588f42cb48'
DEPOT = 'd38909eb68d85b91259f4bf20fe5ebcd38461bc2'


def git(directory, *args):
    return subprocess.check_output(['git', '-C', str(directory), *args], text=True).strip()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--out', type=Path, required=True, help='owned build directory on the VM')
    parser.add_argument('--upstream', type=Path, help='webrtc-sys 0.3.49 libwebrtc directory')
    parser.add_argument('--vs-install', help='Windows Visual Studio 2022 C++ installation')
    args = parser.parse_args()
    if sys.platform not in ['linux', 'win32']:
        raise SystemExit('Build this library on linux-build-host or windows-build-host, not the Mac.')
    windows = sys.platform == 'win32'
    upstream = args.upstream
    if upstream is None:
        upstream = Path(__file__).resolve().parent/'vendor/webrtc-sys/libwebrtc'
    if not (upstream/'build_linux.sh').is_file() or not (upstream/'build_windows.cmd').is_file():
        raise SystemExit('Supply the pinned webrtc-sys libwebrtc build scripts with --upstream.')
    root = args.out.resolve()
    root.mkdir(parents=True, exist_ok=True)
    if (root/'src/.git').exists() and git(root/'src', 'rev-parse', 'HEAD') != WEBRTC:
        raise SystemExit('Existing source has a different revision; select another owned --out directory.')
    shutil.copytree(upstream, root, dirs_exist_ok=True)
    config = root/'.gclient'
    config.write_text(config.read_text(encoding='utf-8').replace('m150_release', WEBRTC), encoding='utf-8')
    depot = root/'depot_tools'
    if not depot.exists():
        subprocess.run(['git', 'clone', '--no-checkout', 'https://chromium.googlesource.com/chromium/tools/depot_tools.git', str(depot)], check=True)
        subprocess.run(['git', '-C', str(depot), 'checkout', '--detach', DEPOT], check=True)
    if git(depot, 'rev-parse', 'HEAD') != DEPOT:
        raise SystemExit('Existing depot_tools differs from the pinned revision; use a fresh --out.')
    script = root/('build_windows.cmd' if windows else 'build_linux.sh')
    text = script.read_text(encoding='utf-8')
    text = text.replace('rtc_use_h264=false', 'rtc_use_h264=true').replace('rtc_use_h265=true', 'rtc_use_h265=false')
    text = text.replace('rtc_libvpx_build_vp9=true', 'rtc_libvpx_build_vp9=false').replace('enable_libaom=true', 'enable_libaom=false')
    if windows:
        text = text.replace('rtc_use_h264=true', 'rtc_use_h264=true rtc_use_h265=false')
        text = text.replace('ninja.exe -C', 'ninja.exe -j4 -C')
        text = text.replace('rem build', 'if errorlevel 1 exit /b 1\nrem build')
        text = text.replace('rem copy static library', 'if errorlevel 1 exit /b 1\nrem copy static library')
        text = text.replace('rem generate license', 'if errorlevel 1 exit /b 1\nrem generate license')
        text = text.replace('rem generate ninja for release',
                            'python3 src/build/util/lastchange.py -s src --filter="" -o src/build/util/LASTCHANGE\n'
                            'if errorlevel 1 exit /b 1\nrem generate ninja for release')
    else:
        text = text.replace('ninja -C', 'ninja -j4 -C')
        text = text.replace('# generate ninja files',
                            'python3 src/build/util/lastchange.py -s src --filter="" -o src/build/util/LASTCHANGE\n'
                            '# generate ninja files')
    script.write_text(text, encoding='utf-8')
    env = {**os.environ, 'DEPOT_TOOLS_UPDATE':'0', 'DEPOT_TOOLS_WIN_TOOLCHAIN':'0',
           'PATH':str(depot)+os.pathsep+os.environ.get('PATH','')}
    # Pinned depot_tools does not run its updater, so bootstrap its pinned
    # Python/CIPD tools explicitly (gn otherwise fails on a fresh Linux host).
    bootstrap = (['cmd.exe', '/d', '/c', str(depot/'bootstrap/win_tools.bat')]
                 if windows else ['bash', str(depot/'ensure_bootstrap')])
    subprocess.run(bootstrap, cwd=depot, env=env, check=True)
    if args.vs_install:
        env['vs2022_install'] = args.vs_install
    command = (['cmd.exe', '/d', '/c', str(script)] if windows else ['bash', str(script)]) + ['--arch', 'x64', '--profile', 'release']
    with (root/'source-build.log').open('w', encoding='utf-8') as log:
        subprocess.run(command, cwd=root, env=env, stdout=log, stderr=subprocess.STDOUT, check=True)
    if git(root/'src', 'rev-parse', 'HEAD') != WEBRTC:
        raise SystemExit('Fetched WebRTC source differs from the pinned revision.')
    artifact = root/('win-x64-release' if windows else 'linux-x64-release')
    clang = root/'src/third_party/llvm-build/Release+Asserts/bin'/('clang-cl.exe' if windows else 'clang')
    provenance = dict(webrtcRevision=WEBRTC, depotToolsRevision=DEPOT,
                      buildScriptSha256=hashlib.sha256(script.read_bytes()).hexdigest(),
                      patchSha256={p.name:hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted((root/'patches').glob('*.patch'))},
                      clangVersion=subprocess.check_output([str(clang), '--version'], text=True, encoding='utf-8').strip())
    (artifact/'source-build-provenance.json').write_text(json.dumps(provenance, indent=2)+'\n', encoding='utf-8')
    print('Built pinned release library: '+str(artifact))


if __name__ == '__main__':
    main()

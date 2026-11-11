#!/usr/bin/env python3
"""Run the standalone helper's owning checks on Linux (local Lima on a Mac).

Only this crate's source is transferred. The dedicated local VM has no mounts;
its toolchain, registry and target directory survive stops and later checks.
--host retains explicit SSH checks; landing never selects a remote host.
Development prebuilts may be used here; release-manifest.py owns the release gate.
"""
import argparse
import contextlib
import hashlib
import io
import json
import os
from pathlib import Path
import platform
import runpy
import shlex
import shutil
import subprocess
import sys
import tarfile

HERE = Path(__file__).resolve().parent


def local(sdk):
    env = dict(os.environ)
    env['CARGO_BUILD_JOBS'] = str(min(4, max(1, int(env.get('CARGO_BUILD_JOBS', '4')))))
    if sdk:
        env['LK_CUSTOM_WEBRTC'] = sdk
    if platform.system() == 'Linux' and 'CXX' not in env and shutil.which('clang++-21'):
        env['CXX'] = 'clang++-21'
    cargo = shutil.which('cargo') or str(Path.home() / '.cargo/bin' / ('cargo.exe' if os.name == 'nt' else 'cargo'))
    command = [cargo] + (['+stable-x86_64-pc-windows-msvc'] if os.name == 'nt' else [])
    for args in (['fmt', '--', '--check'], ['test', '--locked']):
        subprocess.run(command + args, cwd=HERE, env=env, check=True)


def snapshot():
    # Fixed build inputs, with neither evidence nor arbitrary untracked files.
    paths = [HERE / name for name in ('Cargo.toml', 'Cargo.lock', 'build.rs', 'check.py', '.cargo/config.toml')]
    paths += sorted((HERE / 'src').rglob('*.rs'))
    paths += sorted((HERE / 'tests').glob('*.rs'))
    paths += sorted(p for p in (HERE / 'vendor').rglob('*') if p.is_file())
    snapshot = io.BytesIO()
    with tarfile.open(fileobj=snapshot, mode='w') as archive:
        for path in paths:
            data = path.read_bytes()
            entry = tarfile.TarInfo(path.relative_to(HERE).as_posix())
            entry.size = len(data)
            archive.addfile(entry, io.BytesIO(data))
    payload = snapshot.getvalue()
    return payload, hashlib.sha256(payload).hexdigest()


GUEST_CHECK = '''import os,pathlib,subprocess,sys,tarfile
base=pathlib.Path.home()/'.cache/wonder/computer-helper-check'
root=base/sys.argv[1];root.mkdir(parents=True,exist_ok=True)
with tarfile.open(fileobj=sys.stdin.buffer,mode='r|') as archive:
    archive.extractall(root,filter='data')
env=dict(os.environ,CARGO_TARGET_DIR=str(base/'target'),CARGO_INCREMENTAL='1')
subprocess.run([sys.executable,str(root/'check.py'),'--local']+(['--sdk',sys.argv[2]] if len(sys.argv)>2 else []),env=env,check=True)
'''


def remote(host, sdk):
    payload, sha = snapshot()
    command = ['python3', '-c', GUEST_CHECK, sha] + ([sdk] if sdk else [])
    if host == 'linux-build-host':
        # The prepared GCP image keeps Rust in /opt, outside non-login SSH PATH.
        command = ['bash', '-lc', shlex.join(command)]
    print(f'Checking exact helper source {sha} on {host}', flush=True)
    result = subprocess.run(['ssh', '-o', 'BatchMode=yes', '--', host, shlex.join(command)], input=payload)
    # Name the failure: the land log keeps only this line, not the output above.
    if result.returncode == 255:
        sys.exit(f'Could not reach {host} over SSH (exit 255). Start it or fix the connection, then rerun.')
    if result.returncode:
        sys.exit(f'The helper checks failed on {host} (exit {result.returncode}); the cargo output above names the failure.')


def lima(sdk):
    import fcntl
    instance = 'wonder-helper-check'
    cache = Path.home() / '.cache/wonder/computer-helper-check'
    cache.mkdir(parents=True, exist_ok=True)
    # Serialize first creation/start and checks from concurrent worktrees. No
    # host directories are mounted and no existing personal VM is reconfigured.
    with (cache / 'lima.lock').open('a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        template = {
            'minimumLimaVersion': '2.2.0', 'base': ['template:_images/ubuntu-26.04'],
            'cpus': 4, 'memory': '6GiB', 'disk': '35GiB', 'mounts': [],
            'containerd': {'system': False, 'user': False},
            'portForwards': [{'guestPortRange': [1, 65535], 'ignore': True}],
            'provision': [
                {'mode': 'system', 'script': '#!/bin/sh\nset -eu\nexport DEBIAN_FRONTEND=noninteractive\n'
                 'apt-get update -q\napt-get install -y --no-install-recommends build-essential '
                 'ca-certificates curl unzip pkg-config libglib2.0-dev libssl-dev libva-dev python3 clang-21\n'},
                {'mode': 'user', 'script': '#!/bin/sh\nset -eu\n'
                 'if [ ! -x "$HOME/.cargo/bin/cargo" ]; then\n'
                 ' curl -fsSL https://sh.rustup.rs -o /tmp/wonder-helper-rustup.sh\n'
                 ' sh /tmp/wonder-helper-rustup.sh -y --profile minimal --default-toolchain stable\nfi\n'
                 '"$HOME/.cargo/bin/rustup" component add rustfmt\n'},
            ],
        }
        config = cache / 'lima.yaml'
        config.write_text(json.dumps(template, indent=2))
        try:
            instances = subprocess.run(['limactl', 'list', '--json'], text=True, capture_output=True,
                                       check=True, timeout=30)
            found = next((json.loads(line) for line in instances.stdout.splitlines()
                          if json.loads(line).get('name') == instance), None)
            if not found or found.get('status') != 'Running':
                source = instance if found else str(config)
                subprocess.run(['limactl', 'start', '--tty=false', '--name=' + instance,
                                '--timeout=20m', source], check=True, timeout=1230)
        except (OSError, ValueError, subprocess.SubprocessError) as error:
            print(f'environment unavailable: local helper VM: {error}; inspect limactl list and '
                  f'limactl start {instance}, then retry check.py', file=sys.stderr)
            raise SystemExit(75) from None
        # Existing dedicated check VMs predate the hardware H264 build inputs.
        # Upgrade only this owned VM; never install packages on an SSH host.
        subprocess.run(['limactl', 'shell', '--workdir', '/tmp', instance, '--',
                        'bash', '-lc', 'pkg-config --exists libva || '
                        '(sudo apt-get update -q && sudo apt-get install -y --no-install-recommends libva-dev)'],
                       check=True, timeout=300)
        payload, sha = snapshot()
        print(f'Checking exact helper source {sha} in local Lima {instance}', flush=True)
        command = ['python3', '-c', GUEST_CHECK, sha] + ([sdk] if sdk else [])
        result = subprocess.run(['limactl', 'shell', '--workdir', '/tmp', instance, '--',
                                 'bash', '-lc', shlex.join(command)], input=payload)
        if result.returncode:
            raise SystemExit(result.returncode)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--host', help='Explicit SSH host (never selected by landing)')
    parser.add_argument('--sdk', help='Optional sealed source SDK path on the test host')
    parser.add_argument('--local', action='store_true')
    args = parser.parse_args()
    if platform.system() == 'Darwin' and not args.local and not args.host:
        lima(args.sdk)
    elif platform.system() == 'Darwin' and not args.local:
        manager = HERE.parents[2] / 'scripts/vms/gcp-linux'
        lease = runpy.run_path(str(manager))['lease'] if args.host == 'linux-build-host' else contextlib.nullcontext
        with contextlib.ExitStack() as stack:
            try:
                stack.enter_context(lease())
            except (OSError, ValueError, KeyError, RuntimeError, subprocess.SubprocessError) as error:
                print(f'environment unavailable ({args.host}): {error}', file=sys.stderr)
                raise SystemExit(75) from None
            remote(args.host, args.sdk)
    elif platform.system() in ('Linux', 'Windows'):
        local(args.sdk)
    else:
        parser.error('The native helper runs on Linux and Windows; omit --local to use Lima on a Mac.')

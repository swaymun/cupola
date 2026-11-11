#!/usr/bin/env python3
"""Verify the linked WebRTC inputs and collect the helper's complete notices.
Run on the target build VM after building the helper with LK_CUSTOM_WEBRTC.
No downloads, publication, signing or environment changes occur here.
"""
import argparse
import hashlib
import json
from pathlib import Path
import re
import shutil
import struct
import subprocess
import tomllib

HERE = Path(__file__).resolve().parent

def digest(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()

def binary_target(path):
    with path.open('rb') as stream:
        header = stream.read(64)
        if header[:5] == b'\x7fELF\x02' and header[5] == 1:
            return {62: 'linux-x86_64', 183: 'linux-aarch64'}.get(struct.unpack_from('<H', header, 18)[0])
        if header[:2] == b'MZ':
            stream.seek(struct.unpack_from('<I', header, 60)[0])
            signature = stream.read(6)
            if signature[:4] == b'PE\0\0':
                return {0x8664: 'windows-x86_64', 0xaa64: 'windows-aarch64'}.get(struct.unpack_from('<H', signature, 4)[0])
    return None

def rust_notices(target, out):
    triple = {'linux-x86_64':'x86_64-unknown-linux-gnu', 'windows-x86_64':'x86_64-pc-windows-msvc',
              'linux-aarch64':'aarch64-unknown-linux-gnu', 'windows-aarch64':'aarch64-pc-windows-msvc'}[target]
    selected = set()
    tree = subprocess.check_output(['cargo', 'tree', '--locked', '--manifest-path', str(HERE/'Cargo.toml'),
                                    '--target', triple, '-e', 'normal', '--prefix', 'none', '--format', '{p}'], text=True, encoding='utf-8')
    for line in tree.splitlines():
        match = re.match(r'([^ ]+) v([^ ]+)', line)
        if match: selected.add(match.groups())
    metadata = json.loads(subprocess.check_output(['cargo', 'metadata', '--locked', '--format-version', '1',
                          '--manifest-path', str(HERE/'Cargo.toml'), '--filter-platform', triple], text=True, encoding='utf-8'))
    texts, records, missing = {}, [], []
    upstream=json.loads((HERE/'upstream/index.json').read_text(encoding='utf-8'))
    # Preserve notices supplied by upstream packages; never invent copyright text.
    for package in metadata['packages']:
        key = package['name'], package['version']
        if key not in selected or (not (package.get('source') or '').startswith('registry+') and key[0] != 'webrtc-sys'):
            continue
        directory = Path(package['manifest_path']).parent
        manifest = tomllib.loads((directory/'Cargo.toml').read_text(encoding='utf-8'))['package']
        paths = {p for p in directory.iterdir() if p.is_file() and p.name.lower().startswith(('license','licence','copying','notice'))}
        if manifest.get('license-file'): paths.add(directory / manifest['license-file'])
        hashes = []
        for path in sorted(paths):
            if path.is_file():
                data = path.read_bytes()
                sha = hashlib.sha256(data).hexdigest()
                texts[sha] = data
                hashes.append(sha)
        if not hashes:
            entry=upstream.get('@'.join(key))
            if entry:
                vcs=json.loads((directory/'.cargo_vcs_info.json').read_text(encoding='utf-8'))
                if vcs['git']['sha1']!=entry['revision']:raise SystemExit('Upstream notice revision mismatch: '+key[0])
                path=HERE/'upstream'/entry['file']
                if digest(path)!=entry['sha256']:raise SystemExit('Upstream notice hash mismatch: '+key[0])
                data=path.read_bytes();texts[entry['sha256']]=data;hashes.append(entry['sha256'])
        if not hashes: missing.append('@'.join(key))
        records.append(dict(name=key[0], version=key[1], license=package.get('license'),
                            source=package['source'], licenseTextHashes=hashes))
    (out/'rust-inventory.json').write_text(json.dumps(dict(packages=records, unresolved=missing), indent=2)+'\n')
    if missing: raise SystemExit('Missing full Rust license text: '+', '.join(missing))
    with (out/'RUST_NOTICES.md').open('w', encoding='utf-8') as stream:
        stream.write('# Native computer helper Rust dependency notices\n\n')
        for record in sorted(records, key=lambda p:(p['name'],p['version'])):
            stream.write(f"- {record['name']} {record['version']} ({record['license']}), https://crates.io/crates/{record['name']}/{record['version']}\n")
        for sha, data in sorted(texts.items()):
            stream.write(f'\n---\n\nText SHA-256: {sha}\n\n')
            stream.write(data.decode('utf-8')+'\n')

def verify_codecs(text, symbols):
    """Fail closed on the approved H264/VP8 native codec contract."""
    for key, value in (('rtc_use_h264', 'true'), ('rtc_use_h265', 'false')):
        if not re.search(r'^\s*'+key+r'\s*=\s*'+value+r'\s*$', text, re.M):
            raise SystemExit(f'Release requires explicit {key}={value}')
    # H264's decoder uses Chromium's FFmpeg subset; OpenH264 is source-built
    # and bundled. Neither is covered by Cisco's separate binary-download offer.
    if not re.search(rb'\bWelsCreateSVCEncoder\b', symbols):
        raise SystemExit('Source H264 release is missing its OpenH264 software fallback')
    # FFmpeg's shared profiles.c includes a descriptive HEVC profile-name table
    # even with its parser/decoder disabled. Permit only that data symbol; code
    # or any other HEVC symbol still fails this gate.
    implementations = re.sub(rb'(?m)^[0-9a-fA-F]+\s+[DRdr]\s+ff_hevc_profiles[ \t]*$', b'', symbols)
    if re.search(rb'\b(?:x264_|x265_|ff_hevc|ff_h265)', implementations):
        raise SystemExit('An unapproved x264/x265/HEVC implementation remains in the library')

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--library', type=Path, required=True)
    parser.add_argument('--helper', type=Path, required=True)
    parser.add_argument('--target', choices=['linux-x86_64','linux-aarch64','windows-x86_64','windows-aarch64'], required=True)
    parser.add_argument('--revision', required=True)
    parser.add_argument('--llvm-nm', required=True)
    parser.add_argument('--out', type=Path, required=True)
    args = parser.parse_args()
    if not re.fullmatch('[0-9a-f]{40}',args.revision): raise SystemExit('Pin the full WebRTC source revision')
    version=subprocess.check_output([str(args.helper.resolve()),'--version'],text=True, encoding='utf-8')
    if 'verificationOnly=false' not in version:raise SystemExit('Release helper must be built without parity-tests')
    capabilities = json.loads(subprocess.check_output([str(args.helper.resolve()), '--codec-capabilities'], text=True, encoding='utf-8'))
    offered = {c['mimeType'].lower() for c in capabilities['codecs']}
    if not {'video/h264', 'video/vp8'} <= offered:
        raise SystemExit('The linked helper must actually offer H264 and VP8')
    if binary_target(args.helper) != args.target: raise SystemExit('Helper architecture does not match target')
    gn = args.library/'args.gn'
    text = gn.read_text(encoding='utf-8')
    library = args.library/'lib'/('webrtc.lib' if args.target.startswith('windows') else 'libwebrtc.a')
    library_hash=digest(library)
    binding=re.search(r'\bWONDER_LINKED_WEBRTC_SHA256=([0-9a-f]{64})\b',version)
    if not binding or binding[1]!=library_hash:
        raise SystemExit('Helper was built with a different or unverified SDK. Seal the source SDK and rebuild the helper.')
    symbols = subprocess.check_output([args.llvm_nm, '--defined-only', str(library)], stderr=subprocess.DEVNULL)
    verify_codecs(text, symbols)
    args.out.mkdir(parents=True,exist_ok=True)
    shutil.copy2(gn,args.out/'args.gn')
    shutil.copy2(args.library/'LICENSE.md',args.out/'LICENSE.md')
    rust_notices(args.target,args.out)
    manifest = dict(target=args.target,sourceBuilt=True,verificationOnly=False,webrtcRevision=args.revision,
                    rtcUseH264=True,rtcUseH265=False,forbiddenSymbolsFound=False,
                    h264Software='bundled-openh264',ciscoBinaryLicense=False,
                    librarySha256=library_hash,helperSha256=digest(args.helper),
                    licenseSha256=digest(args.out/'LICENSE.md'),gnArgsSha256=digest(gn),
                    rustNoticesSha256=digest(args.out/'RUST_NOTICES.md'))
    (args.out/'wonder-webrtc-build.json').write_text(json.dumps(manifest,indent=2)+'\n')
    print('Verified source library, helper target, and complete notices: '+str(args.out))

if __name__ == '__main__': main()

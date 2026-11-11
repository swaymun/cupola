#!/bin/sh
# Install a verified release for this user. No sudo, service changes or sign-ins.
set -eu
# Everything runs inside main, called on the last line: a truncated `curl | sh`
# download is a syntax error and runs nothing.
main() {
# The installer is Python; fail with the exact fix before it runs. Apple's own
# python3 (Xcode or the Command Line Tools) is 3.9, which is enough.
python_help='Cupola installation needs Python 3.9 or newer. On macOS: xcode-select --install (its Python 3.9 works), or brew install python. On Ubuntu or Debian: sudo apt-get install -y python3 (Ubuntu 22.04 and newer include 3.10+; Debian 11 and newer include 3.9+).'
command -v python3 >/dev/null 2>&1 || { echo "python3 not found. $python_help" >&2; exit 1; }
python3 -c 'import sys; sys.exit(sys.version_info < (3, 9))' >/dev/null 2>&1 || { echo "python3 is $(python3 -c 'import platform; print(platform.python_version())' 2>/dev/null || echo unusable); $python_help" >&2; exit 1; }
exec python3 - "$@" <<'PY'
import argparse, hashlib, json, os, platform, re, shutil, stat, subprocess, sys, tarfile, tempfile, urllib.error, urllib.parse, urllib.request, zipfile
from pathlib import Path, PurePosixPath

def fail(message):
    raise SystemExit(message)

def linux_glibc():
    try:
        value = os.confstr('CS_GNU_LIBC_VERSION') or ''
    except (AttributeError, OSError, ValueError):
        value = ''
    match = re.fullmatch(r'glibc ([0-9]+(?:\.[0-9]+)+)', value)
    return match[1] if match else None

def require_glibc(minimum):
    found = linux_glibc()
    if found is None:
        fail(f'Cupola needs Linux with glibc {minimum} or newer; this system\'s glibc version could not be determined. Nothing installed.')
    if tuple(map(int, found.split('.'))) < tuple(map(int, minimum.split('.'))):
        fail(f'This Linux is too old: Cupola needs glibc {minimum} or newer (found {found}). Nothing installed.')

def trusted(url):
    p = urllib.parse.urlsplit(url)
    if p.username or p.password or not (p.scheme == 'https' or (p.scheme == 'http' and p.hostname in ('localhost', '127.0.0.1', '::1'))):
        fail('Release URLs must use HTTPS (HTTP is allowed only on loopback for local acceptance).')
    return url

class Redirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, url):
        return super().redirect_request(req, fp, code, msg, headers, trusted(url))

opener = urllib.request.build_opener(Redirect())
def fetch(url, target=None):
    with opener.open(trusted(url), timeout=60) as response:
        if target is None:
            data = response.read(1024 * 1024 + 1)
            if len(data) > 1024 * 1024: fail('Release manifest exceeds 1 MiB.')
            return json.loads(data)
        digest = hashlib.sha256()
        with target.open('wb') as out:
            for chunk in iter(lambda: response.read(1024 * 1024), b''):
                digest.update(chunk)
                out.write(chunk)
        return digest.hexdigest()

parser = argparse.ArgumentParser(description='Install Cupola without administrator rights; user data is preserved.')
# CUPOLA_* names win; the older WONDER_* names still work.
setting = lambda name: os.environ.get(f'CUPOLA_{name}') or os.environ.get(f'WONDER_{name}')
# The newest GitHub release holds stable.json and the archives side by side.
DEFAULT_FEED = 'https://github.com/swaymun/cupola/releases/latest/download'
PIN_HINT = 'GitHub releases hold only flat files, so pin a release with --feed https://github.com/swaymun/cupola/releases/download/vX.Y.Z (the feed folder of that release).'
parser.add_argument('--feed', default=setting('UPDATE_URL') or DEFAULT_FEED, help='Release folder URL (default: the newest GitHub release)')
parser.add_argument('--channel', choices=['stable', 'nightly'], default='stable')
parser.add_argument('--version', help='Only for feeds with versions/X.json; on GitHub use --feed .../releases/download/vX.Y.Z')
parser.add_argument('--prefix', default=setting('INSTALL_DIR') or str(Path.home() / '.local/share/cupola'))
parser.add_argument('--bin-dir', default=setting('BIN_DIR') or str(Path.home() / '.local/bin'))
parser.add_argument('--app-dir', default=setting('APP_DIR') or str(Path.home() / 'Applications'), help='macOS only: the folder that receives Cupola.app (default: ~/Applications)')
parser.add_argument('--staging', action='store_true', help='Accept only the isolated macOS CLI staging identity')
args = parser.parse_args()
valid = lambda v: bool(re.fullmatch(r'[0-9][A-Za-z0-9.+-]{0,63}', v))
if args.version and not valid(args.version): fail('Invalid version.')
if args.version and args.feed.rstrip('/') == DEFAULT_FEED: fail(f'--version needs a feed with versions/{args.version}.json, which GitHub releases cannot hold. {PIN_HINT}')
system = {'Darwin': 'macos', 'Linux': 'linux'}.get(platform.system())
arch = {'arm64': 'aarch64', 'aarch64': 'aarch64', 'x86_64': 'x86_64', 'AMD64': 'x86_64'}.get(platform.machine())
if not system or not arch: fail('This platform is unsupported; Windows uses install.ps1.')
prefix, bin_dir = Path(args.prefix).expanduser().absolute(), Path(args.bin_dir).expanduser().absolute()
if prefix.is_symlink() or prefix == Path.home() or len(prefix.parts) < 3: fail('Choose a dedicated installation directory.')
app_dir = Path(args.app_dir).expanduser().absolute()
receipt = prefix / 'install.json'
if prefix.exists() and any(prefix.iterdir()) and not receipt.is_file(): fail(f'Refusing an unowned directory: {prefix}')
if receipt.exists():
    old = json.loads(receipt.read_text())
    if old.get('owner') != 'wonder-cli-v1' or old.get('prefix') != str(prefix): fail('Invalid installation receipt.')
    fail('Cupola is already installed here. Use cupola update, or uninstall before reinstalling.')
for name in (['cupola'] if system == 'macos' else ['cupola', 'wonderd']):
    link = bin_dir / name
    if link.exists() or link.is_symlink(): fail(f'Refusing to replace an existing command: {link}')
if system == 'macos':
    existing = app_dir / 'Cupola.app'
    if existing.exists() or existing.is_symlink(): fail(f'Cupola.app is already installed at {existing}. Update it with brew upgrade --cask cupola or cupola update, or remove it first. Nothing installed.')
base = args.feed.rstrip('/') + '/'
manifest_url = urllib.parse.urljoin(base, f'versions/{args.version}.json' if args.version else f'{args.channel}.json')
try:
    manifest = fetch(manifest_url)
except (urllib.error.URLError, OSError, ValueError) as error:
    fail(f'Could not read the release manifest {manifest_url}: {getattr(error, "reason", error)}. Nothing installed.' + (f' {PIN_HINT}' if args.version else ''))
version = manifest.get('version', '')
if not valid(version) or (args.version and version != args.version): fail('The manifest does not match the requested version.')
artifact = manifest.get('artifacts', {}).get(f'{system}-{arch}', {})
sha = artifact.get('sha256', '')
if not re.fullmatch('[0-9a-f]{64}', sha): fail(f'No SHA256-verified artifact for {system}-{arch}.')
url = urllib.parse.urljoin(manifest_url, artifact['url'])
prefix.parent.mkdir(parents=True, exist_ok=True)
with tempfile.TemporaryDirectory(prefix='.cupola-install-', dir=prefix.parent) as temporary:
    stage = Path(temporary)
    archive, unpacked = stage / 'download', stage / 'unpacked'
    unpacked.mkdir()
    if fetch(url, archive) != sha: fail('SHA256 mismatch; nothing installed.')
    if system == 'macos':
        # ditto preserves signed framework symlinks. Validate every entry first.
        with zipfile.ZipFile(archive) as bundle:
            for item in bundle.infolist():
                p = PurePosixPath(item.filename)
                if p.is_absolute() or '..' in p.parts or '\\' in item.filename: fail('Unsafe ZIP entry.')
                if stat.S_ISLNK(item.external_attr >> 16):
                    target = bundle.read(item).decode()
                    resolved = os.path.normpath(str(p.parent / target))
                    if target.startswith('/') or resolved.startswith('../'): fail('Unsafe ZIP link.')
        subprocess.run(['/usr/bin/ditto', '-x', '-k', str(archive), str(unpacked)], check=True)
        app = unpacked / 'Cupola.app'
        bundle_id = 'com.saimun.wonder.cli-staging' if args.staging else 'com.saimun.wonder'
        requirement = f'anchor apple generic and identifier "{bundle_id}" and certificate leaf[subject.OU] = "8KKNVD7758"'
        subprocess.run(['/usr/bin/codesign', '--verify', '--deep', '--strict', '-R', '='+requirement, str(app)], check=True)
        subprocess.run(['/usr/bin/xcrun', 'stapler', 'validate', str(app)], check=True)
        subprocess.run(['/usr/sbin/spctl', '--assess', '--type', 'execute', str(app)], check=True)
        import plistlib
        info = plistlib.loads((app / 'Contents/Info.plist').read_bytes())
        if info.get('CFBundleIdentifier') != bundle_id or info.get('CFBundleExecutable') != 'cupola' or info.get('CFBundleVersion') != version or info.get('LSUIElement') is not True:
            fail('The signed app is not the requested windowless CLI release.')
        executable = app / 'Contents/MacOS/cupola'
    else:
        with tarfile.open(archive, 'r:gz') as bundle:
            # Release files have no links or device nodes; reject traversal before extraction.
            for item in bundle.getmembers():
                p = PurePosixPath(item.name)
                if p.is_absolute() or '..' in p.parts or not (item.isfile() or item.isdir()): fail('Unsafe tar entry.')
            bundle.extractall(unpacked)
        executable = unpacked / 'cupola'
        if not (unpacked / 'wonderd').is_file(): fail('Release has no wonderd.')
        compatibility = unpacked / 'linux-compatibility.json'
        if compatibility.is_file():
            try:
                minimum = json.loads(compatibility.read_text())['minimumGlibc']
                if not isinstance(minimum, str) or not re.fullmatch(r'[0-9]+(?:\.[0-9]+)+', minimum):
                    raise ValueError('invalid glibc version')
            except (ValueError, KeyError, TypeError):
                fail('The Linux release has invalid compatibility metadata. Nothing installed.')
            require_glibc(minimum)
    try:
        result = subprocess.run([str(executable), 'version', '--json'], capture_output=True, text=True, timeout=30)
    except (OSError, subprocess.TimeoutExpired) as error:
        fail(f'The Cupola release could not run: {error}. Nothing installed.')
    if result.returncode:
        diagnostic = result.stderr.strip() or result.stdout.strip() or 'No diagnostic output.'
        if system == 'linux':
            # Older releases have no compatibility metadata. Preserve their loader
            # diagnostics too, without turning a missing symbol into a traceback.
            required = '\n'.join(line for line in diagnostic.splitlines() if 'weak version' not in line)
            needed = re.findall(r'GLIBC_([0-9]+(?:\.[0-9]+)+)', required)
            if needed:
                require_glibc(max(needed, key=lambda value: tuple(map(int, value.split('.')))))
        fail(f'The Cupola release could not run (exit {result.returncode}):\n{diagnostic}\nNothing installed.')
    try:
        reported_version = json.loads(result.stdout).get('version')
    except (ValueError, AttributeError):
        fail('The release executable returned an invalid version response. Nothing installed.')
    if reported_version != version: fail('The release executable reports a different version.')
    uninstaller = (app / 'Contents/Resources/uninstall.sh') if system == 'macos' else unpacked / 'uninstall.sh'
    if not uninstaller.is_file(): fail('Release lacks its uninstaller; nothing installed.')
    # All checks passed. Only now create installation state and PATH entries.
    prefix.mkdir(exist_ok=True)
    bin_dir.mkdir(parents=True, exist_ok=True)
    if system == 'macos':
        # The app lives in ~/Applications like any Mac app; the install folder keeps only the receipt and uninstaller.
        app_dir.mkdir(parents=True, exist_ok=True)
        destination = app_dir / 'Cupola.app'
        shutil.move(str(app), destination)
        targets = {'cupola': destination / 'Contents/MacOS/cupola'}
    else:
        destination = prefix / 'versions' / version
        destination.parent.mkdir(exist_ok=True)
        shutil.move(str(unpacked), destination)
        targets = {name: destination / name for name in ['cupola', 'wonderd']}
    for name, target in targets.items():
        (bin_dir / name).symlink_to(target)
    # The release must contain the uninstaller, so curl|sh remains self-contained.
    source = (destination / 'Contents/Resources/uninstall.sh') if system == 'macos' else destination / 'uninstall.sh'
    shutil.copy2(source, prefix / 'uninstall.sh')
    (prefix / 'uninstall.sh').chmod(0o700)
    receipt.write_text(json.dumps(dict(owner='wonder-cli-v1', prefix=str(prefix), binDir=str(bin_dir), platform=system, channel=args.channel, feed=base, **(dict(app=str(destination)) if system == 'macos' else {}), targets={k: str(v) for k,v in targets.items()}), indent=2) + '\n')
    receipt.chmod(0o600)
print(f'Installed Cupola {version} ({args.channel}) in {destination if system == "macos" else prefix}.')
if str(bin_dir) not in os.environ.get('PATH', '').split(os.pathsep):
    print(f'{bin_dir} is not on this shell\'s PATH yet. Run: export PATH="{bin_dir}:$PATH"' + (' (Ubuntu\'s ~/.profile adds it to new login shells).' if system == 'linux' else ''))
print('Next: cupola init; cupola service install (starts the host now' + (' and at boot' if system == 'linux' else ' and at every login') + '); cupola pair')
if system == 'linux':
    # Claude and OpenCode run through Node; Codex does not need it. Advise, don't block.
    node = shutil.which('node')
    try: node_major = int(subprocess.run([node, '--version'], capture_output=True, text=True, timeout=10).stdout.strip().lstrip('v').split('.')[0]) if node else 0
    except (OSError, ValueError, subprocess.TimeoutExpired): node_major = 0
    if node_major < 22:
        print('Node.js 22 or newer is needed for Claude and OpenCode (not for Codex), and ' + ('found ' + str(node_major) if node_major else 'was not found') + '. Install it from https://nodejs.org/en/download (or: curl -fsSL https://deb.nodesource.com/setup_22.x | sudo -E bash - && sudo apt-get install -y nodejs).')
print(f'Uninstall (keeps conversations, pairing and sign-ins): {prefix}/uninstall.sh')
PY
}
main "$@"

#!/bin/sh
# Remove only a receipt-owned installation. Never remove ~/.cupola or sign-ins.
set -eu
exec python3 - "$0" "$@" <<'PY'
import json, os, shutil, subprocess, sys
from pathlib import Path
prefix = Path(sys.argv[1]).absolute().parent
if len(sys.argv) != 2:
    raise SystemExit('Usage: uninstall.sh (stops no services; run cupola stop and service uninstall first)')
receipt = json.loads((prefix / 'install.json').read_text())
if receipt.get('owner') != 'wonder-cli-v1' or receipt.get('prefix') != str(prefix) or prefix.is_symlink():
    raise SystemExit('Invalid Cupola installation receipt; nothing removed.')
name = 'cupola' if 'cupola' in receipt['targets'] else 'wonder'
command = Path(receipt['targets'][name])
# macOS keeps the app outside the install folder; its PATH link still belongs to this install.
app = Path(receipt['app']) if receipt.get('app') else None
if app and (app.name != 'Cupola.app' or app.is_symlink() or str(app) != str(command).split('/Contents/')[0]):
    raise SystemExit('Invalid Cupola installation receipt; nothing removed.')
owned = [prefix] + ([app] if app else [])
link = Path(receipt['binDir']) / name
if link.is_symlink() and any(link.resolve().is_relative_to(root.resolve()) for root in owned):
    command = link.resolve()
def default_data_dir(home):
    # As data_home::default_dir: ~/.wonder while it still holds the data
    # (anything but the link to ~/.cupola), otherwise ~/.cupola.
    cupola, legacy = home/'.cupola', home/'.wonder'
    def canonical(path):
        try: return path.resolve(strict=True)
        except OSError: return None
    if not (legacy.is_symlink() or legacy.exists()): return cupola
    return cupola if legacy.is_symlink() and canonical(legacy) == canonical(cupola) else legacy
data = os.environ.get('CUPOLA_DATA_DIR') or os.environ.get('WONDER_DATA_DIR')
state = (Path(data) if data else default_data_dir(Path.home())) / 'headless'
if not (state/'config.json').exists() and (state/'supervisor.pid').exists():
    raise SystemExit('A host PID remains without configuration. Stop that host before removing Cupola.')
if not command.exists() and (state/'config.json').exists():
    raise SystemExit('Could not locate the installed CLI to check the host. Restore its PATH link before uninstalling.')
if command.exists() and (state/'config.json').exists():
    result = subprocess.run([str(command), 'status', '--json'], capture_output=True, text=True)
    if result.returncode: raise SystemExit('Could not check the host. Stop it and uninstall its service before removing Cupola.')
    status = json.loads(result.stdout)
    if status.get('running') or status.get('supervisorPid'):
        raise SystemExit(f'Cupola is running. Run {name} stop and {name} service uninstall first, then retry.')
if receipt.get('platform') == 'linux' and command.exists() and (state/'config.json').exists():
    # A Tailscale route left behind keeps pointing at whatever listens on the port next.
    unpublished = subprocess.run([str(command), 'unpublish'], capture_output=True, text=True)
    if unpublished.returncode:
        reason = (unpublished.stderr or unpublished.stdout).strip()
        print(f"Warning: could not remove Cupola's Tailscale route ({reason}). If `tailscale serve status` still lists it, remove it with `tailscale serve --https=PORT off`.", file=sys.stderr)
for name, target in receipt['targets'].items():
    link = Path(receipt['binDir']) / name
    # Updates change a versioned target; only unlink commands still inside this install.
    if link.is_symlink() and any(link.resolve().is_relative_to(root.resolve()) for root in owned): link.unlink()
if app and app.exists(): shutil.rmtree(app)
shutil.rmtree(prefix)
print('Cupola uninstalled. Conversations, pairing, settings and provider sign-ins were kept.')
PY

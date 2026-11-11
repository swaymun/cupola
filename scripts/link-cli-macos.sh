#!/usr/bin/env bash
# Only link an already installed signed bundle; never replace a different CLI.
set -euo pipefail
APP=${1:?Usage: link-cli-macos.sh /installed/Cupola.app BIN_DIRECTORY [LEGACY_APP]}
BIN_DIR=${2:?Supply the directory on PATH (for example ~/.local/bin)}
ROOT=$(cd "$(dirname "$0")/.." && pwd)
"$ROOT/scripts/verify-cli-macos.sh" "$APP" --notarized
python3 - "$APP" "$BIN_DIR" "${3:-}" <<'LINK'
from pathlib import Path
import os, sys
app = Path(sys.argv[1]).resolve(strict=True)
target = app / 'Contents/MacOS/cupola'
directory = Path(sys.argv[2]).expanduser()
directory.mkdir(parents=True, exist_ok=True)
link = directory / 'cupola'
# The coordinator stops/unregisters the old host and retains its signed bundle
# for rollback first. A previous bundle ("Cupola CLI.app", or "Wonder CLI.app"
# with its retired `wonder` command) loses only its exact owned PATH symlinks,
# never a file or another installation's command; the old bundle may already
# be gone. No alias is created for the retired name.
legacy = directory / 'wonder'
previous = Path(sys.argv[3]).expanduser().absolute() if len(sys.argv) > 3 and sys.argv[3] else None
if previous:
    old_target = previous / 'Contents/MacOS/wonder'
    if legacy.is_symlink() and legacy.resolve() != old_target.resolve():
        raise SystemExit(f'Refusing to remove another installation’s command: {legacy}')
    if legacy.exists() and not legacy.is_symlink():
        raise SystemExit(f'Refusing to remove an existing file: {legacy}')
if link.is_symlink():
    if link.resolve() == target:
        pass
    elif previous and link.resolve() == (previous / 'Contents/MacOS/cupola').resolve():
        link.unlink()
        link.symlink_to(target)
    else:
        raise SystemExit(f'Refusing to replace another CLI: {link}')
elif link.exists():
    raise SystemExit(f'Refusing to replace an existing file: {link}')
else:
    link.symlink_to(target)
if previous and legacy.is_symlink():
    legacy.unlink()
print(f'Linked {link} -> {target}')
if str(directory.resolve()) not in [str(Path(p).resolve()) for p in os.get_exec_path()]:
    print(f'Add {directory} to PATH before running cupola.')
LINK

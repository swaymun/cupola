#!/usr/bin/env bash
# Read-only structural/signature validation; no helper, provider or model starts.
set -euo pipefail
APP=${1:?Usage: verify-cli-macos.sh Cupola.app [--notarized]}
python3 - "$APP" <<'VERIFY'
from pathlib import Path
import plistlib, sys
app = Path(sys.argv[1])
info = plistlib.loads((app / 'Contents/Info.plist').read_bytes())
assert info['CFBundleIdentifier'] in ('com.saimun.wonder', 'com.saimun.wonder.cli-staging')
assert info['CFBundleExecutable'] == 'cupola'
assert info['LSUIElement'] is True
assert info.get('CFBundleIconFile') == 'Cupola', 'Finder shows the Cupola icon'
assert all(not key.startswith('SU') for key in info), 'The CLI bundle has no Sparkle updater'
for path in ['MacOS/cupola', 'MacOS/wonderd', 'Helpers/WonderComputerUse', 'Helpers/WonderComputerUseDiscovery',
        'Helpers/wonder-computer-use', 'Frameworks/WebRTC.framework/WebRTC',
        'Resources/node/bin/node', 'Resources/claude-runtime/main.mjs',
        'Resources/acp-runtime/main.mjs', 'Resources/WebRTC-LICENSE.md', 'Resources/Cupola.icns']:
    assert (app / 'Contents' / path).is_file(), path
for path in ['MacOS/WonderHost', 'MacOS/WonderMenu', 'MacOS/WonderMacBridge',
        'Frameworks/Sparkle.framework', 'Resources/wonder-tunnel']:
    assert not (app / 'Contents' / path).exists(), path
assert not list(app.glob('Contents/**/*.app')), 'Only one app identity may ship'
print('CLI bundle structure verified')
VERIFY
codesign --verify --deep --strict "$APP"
INFO=$(codesign -dvv "$APP" 2>&1)
[[ "$INFO" == *'Authority=Developer ID Application:'* && "$INFO" == *'TeamIdentifier=8KKNVD7758'* ]] || { echo 'Existing Developer ID team required' >&2; exit 1; }
BUNDLE_ID=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$APP/Contents/Info.plist")
for helper in WonderComputerUse WonderComputerUseDiscovery; do
  HELPER_INFO=$(codesign -dvv "$APP/Contents/Helpers/$helper" 2>&1)
  [[ "$HELPER_INFO" == *"Identifier=$BUNDLE_ID.computer-use"* && "$HELPER_INFO" == *'Info.plist=not bound'* ]] || { echo "Computer helper identity changed: $helper" >&2; exit 1; }
  [[ $(otool -l "$APP/Contents/Helpers/$helper") == *'@loader_path/../Frameworks'* ]] || { echo "Missing WebRTC runpath: $helper" >&2; exit 1; }
done
# One Screen Recording grant covers both: the same designated requirement.
[[ "$(codesign -d -r- "$APP/Contents/Helpers/WonderComputerUse" 2>&1 | grep designated)" == "$(codesign -d -r- "$APP/Contents/Helpers/WonderComputerUseDiscovery" 2>&1 | grep designated)" ]] || { echo 'Discovery helper requirement differs' >&2; exit 1; }
codesign -d -r- "$APP"
codesign -d -r- "$APP/Contents/Helpers/WonderComputerUse"
if [[ ${2:-} == --notarized ]]; then
  xcrun stapler validate "$APP"
  spctl --assess --type execute "$APP"
fi
echo "CLI bundle signatures verified: $APP"

#!/usr/bin/env bash
# Build the phase-2 CLI bundle in staging. Never install or alter the GUI app.
# WONDER_BUILD_VERSION=1.0.130 scripts/package-cli-macos.sh .local/release/cli/Cupola.app
set -euo pipefail
cd "$(dirname "$0")/.."
APP=${1:?Usage: package-cli-macos.sh STAGING/Cupola.app}
VERSION=${WONDER_BUILD_VERSION:?Set a numeric, monotonic WONDER_BUILD_VERSION}
export WONDER_RELEASE_VERSION=$VERSION
[[ "$VERSION" =~ ^[0-9]+([.][0-9]+)*$ ]] || { echo 'Build version must be numeric' >&2; exit 2; }
[[ $(uname -s) == Darwin ]] || { echo 'Build and sign on macOS' >&2; exit 2; }
python3 - "$APP" <<'CHECK'
from pathlib import Path
import sys
app = Path(sys.argv[1]).resolve()
if app.name != 'Cupola.app' or app.exists() or any(p == app or p in app.parents for p in
        [Path('/Applications'), Path.home() / 'Applications']):
    raise SystemExit('Use a new staging/Cupola.app; installed apps are never build output.')
CHECK
# A fixed isolated identity is available only for acceptance on this Mac.
export WONDER_CLI_STAGING=${WONDER_CLI_STAGING:-0}
[[ $WONDER_CLI_STAGING == 0 || $WONDER_CLI_STAGING == 1 ]] || { echo 'WONDER_CLI_STAGING must be 0 or 1' >&2; exit 2; }
export WONDER_NOTARIZE=1
export WONDER_APP_SIGNING_IDENTITY=${WONDER_APP_SIGNING_IDENTITY:-'Developer ID Application: YOUR NAME (YOUR_TEAM_ID)'}
export WONDER_APP_SIGNING_IDENTITY="$(python3 scripts/sign-macos-app.py --check-identity)"
export CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-4}
if (( CARGO_BUILD_JOBS > 4 )); then export CARGO_BUILD_JOBS=4; fi
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-.local/build/rust}
cargo build --locked --release -p wonderd --features experimental-headless --bin cupola --bin wonderd
python3 scripts/verify-no-blind-pairing.py "$CARGO_TARGET_DIR/release/cupola"
swift build -c release --package-path native/computer-use --jobs 4
HELPER_BUILD=$(swift build -c release --package-path native/computer-use --show-bin-path)
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Helpers" "$APP/Contents/Resources" "$APP/Contents/Frameworks"
python3 - "$APP" "$VERSION" <<'PLIST'
from pathlib import Path
import os, plistlib, sys
app = Path(sys.argv[1])
staging = os.environ['WONDER_CLI_STAGING'] == '1'
info = dict(CFBundleIdentifier='com.saimun.wonder.cli-staging' if staging else 'com.saimun.wonder', CFBundleExecutable='cupola',
    CFBundleName='Cupola CLI Staging' if staging else 'Cupola',
    CFBundleDisplayName='Cupola CLI Staging' if staging else 'Cupola', CFBundlePackageType='APPL',
    CFBundleInfoDictionaryVersion='6.0', CFBundleVersion=sys.argv[2],
    CFBundleShortVersionString=sys.argv[2], CFBundleIconFile='Cupola', LSUIElement=True, LSMinimumSystemVersion='14.0',
    NSLocalNetworkUsageDescription='Cupola connects directly to devices you pair with this Mac for secure live screen viewing.',
    NSScreenCaptureUsageDescription='Cupola shares this Mac’s screen with a device you paired, only while Computer View is open.',
    NSAppleEventsUsageDescription='Cupola follows the browser window you choose to share with a paired device.')
with (app / 'Contents/Info.plist').open('wb') as stream:
    plistlib.dump(info, stream)
PLIST
cp "$CARGO_TARGET_DIR/release/cupola" "$CARGO_TARGET_DIR/release/wonderd" "$APP/Contents/MacOS/"
cp "$HELPER_BUILD/WonderComputerUse" "$APP/Contents/Helpers/"
# Source discovery runs from its own path: the screen capture service tells
# clients apart by executable path (see helper_binary in headless/macos.rs).
cp "$HELPER_BUILD/WonderComputerUse" "$APP/Contents/Helpers/WonderComputerUseDiscovery"
ln -s ../MacOS/cupola "$APP/Contents/Helpers/wonder-computer-use"
FRAMEWORK=native/computer-use/.build/artifacts/webrtc/WebRTC/WebRTC.xcframework/macos-x86_64_arm64/WebRTC.framework
ditto "$FRAMEWORK" "$APP/Contents/Frameworks/WebRTC.framework"
cp native/computer-use/.build/checkouts/WebRTC/LICENSE.md "$APP/Contents/Resources/WebRTC-LICENSE.md"
install_name_tool -add_rpath '@loader_path/../Frameworks' "$APP/Contents/Helpers/WonderComputerUse"
install_name_tool -add_rpath '@loader_path/../Frameworks' "$APP/Contents/Helpers/WonderComputerUseDiscovery"
cp LICENSE THIRD_PARTY_NOTICES.md "$APP/Contents/Resources/"
# The staging identity keeps a blue icon apart from the real app.
ICONS=apps/ios/Wonder/Assets.xcassets/AppIcon.appiconset
[[ $WONDER_CLI_STAGING == 0 ]] || ICONS=assets/macos/CupolaStaging.appiconset
scripts/macos-app-icon.sh "$APP/Contents/Resources/Cupola.icns" "$ICONS"
cp uninstall.sh "$APP/Contents/Resources/"
mkdir -p "$APP/Contents/Resources/Licenses"
cp licenses/RUST-NOTICES.txt licenses/rust-dependencies.json licenses/Rust-COPYRIGHT.html.gz "$APP/Contents/Resources/Licenses/"
python3 scripts/package-claude-runtime.py "$APP/Contents/Resources"
python3 scripts/sign-macos-app.py "$APP"
scripts/verify-cli-macos.sh "$APP"
echo "Signed CLI bundle: $APP (notarization and installed behavior are separate gates)."

#!/usr/bin/env bash
# Install a finished release over /Applications/Cupola.app and restart the host.
# Refuses a bundle that release-cli-macos.sh did not finish: notarization must be
# Accepted, the ticket stapled and Gatekeeper must call it notarized. Preserves
# ~/.wonder and ~/.cupola; never touches the Mac mini or publishes anything.
set -euo pipefail
cd "$(dirname "$0")/.."
VERSION=${1:?Usage: install-cli-macos.sh VERSION (from .local/release/cli-macos-VERSION)}
OUT=${RELEASE_DIR:-.local/release/cli-macos-$VERSION}
[[ "$OUT" = /* ]] || OUT="$PWD/$OUT"
APP="$OUT/Cupola.app"
DEST=/Applications/Cupola.app
fail() { echo "Not installed: $*" >&2; exit 2; }
[[ -d "$APP" ]] || fail "$APP is missing; run scripts/release-cli-macos.sh $VERSION"
[[ -f "$OUT/Cupola-$VERSION.zip" ]] || fail "the release did not finish (no Cupola-$VERSION.zip)"
python3 -I - "$OUT/notarization.json" <<'ACCEPTED' || fail "notarization was not accepted"
import json, sys
assert json.load(open(sys.argv[1])).get('status') == 'Accepted'
ACCEPTED
xcrun stapler validate "$APP" >/dev/null 2>&1 || fail "no notarization ticket is stapled"
spctl -a -t exec -vv "$APP" 2>&1 | grep -q 'source=Notarized Developer ID' || fail "Gatekeeper does not see a notarized Developer ID app"
BUILT=$(defaults read "$APP/Contents/Info.plist" CFBundleVersion)
[[ "$BUILT" == "$VERSION" ]] || fail "bundle version $BUILT is not $VERSION"

cupola stop >/dev/null 2>&1 || true
rm -rf "$DEST"
ditto "$APP" "$DEST"
cupola service restart
ORIGIN=$(cupola status --json | python3 -I -c 'import json,sys; print((json.load(sys.stdin).get("host") or {}).get("publicOrigin") or "")')
for _ in $(seq 1 60); do
  [[ -n "$ORIGIN" ]] && curl -fsS -o /dev/null "$ORIGIN/readyz" 2>/dev/null && break
  sleep 2
  ORIGIN=$(cupola status --json | python3 -I -c 'import json,sys; print((json.load(sys.stdin).get("host") or {}).get("publicOrigin") or "")')
done
[[ $(defaults read "$DEST/Contents/Info.plist" CFBundleVersion) == "$VERSION" ]] || fail "installed version differs"
curl -fsS -o /dev/null "$ORIGIN/readyz" || { echo "Installed $VERSION but $ORIGIN/readyz is not ready" >&2; exit 1; }
echo "Installed Cupola $VERSION (notarized, stapled); $ORIGIN/readyz is ready."
cupola devices | tail -n +2 | wc -l | xargs echo "Paired devices:"

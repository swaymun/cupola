#!/usr/bin/env bash
# Read-only source-build preflight. Does not install software or change a tailnet.
set -euo pipefail
cd "$(dirname "$0")/.."
missing=0
for command in cargo swift xcodebuild; do
  if command -v "$command" >/dev/null 2>&1; then
    echo "$command: installed"
  else
    echo "$command: missing (see DEVELOPMENT.md)"; missing=1
  fi
done
runtime="${WONDER_CODEX_BIN:-/Applications/ChatGPT.app/Contents/Resources/codex-cli/bin/codex}"
if [[ -z "${WONDER_CODEX_BIN:-}" && ! -f "$runtime" ]]; then
  runtime="/Applications/ChatGPT.app/Contents/Resources/codex"
fi
if [[ -x "$runtime" ]]; then
  version="$("$runtime" --version)"
  echo "Codex runtime: $version (Wonder checks its protocol compatibility at startup)"
else
  echo 'Codex runtime: install the supported ChatGPT app (see INSTALL.md)'; missing=1
fi
if (( missing )); then exit 1; fi
echo 'Build prerequisites found. Follow DEVELOPMENT.md to select your signing identity and build.'

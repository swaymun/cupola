#!/bin/sh
# Runs example_client.py against a throwaway wonderd: its own HOME, data
# directory and CODEX_HOME, no provider runtime (WONDER_CODEX_BIN=false) and
# no network beyond loopback. Never touches ~/.wonder or a running host.
#
#   tests/contracts/rpc/run-example.sh [target/debug]
set -eu
bin=$(cd "${1:-target/debug}" && pwd)
here=$(cd "$(dirname "$0")" && pwd)
root=$(mktemp -d)
daemon=
cleanup() {
    [ -n "$daemon" ] && kill "$daemon" 2>/dev/null && wait "$daemon" 2>/dev/null
    rm -rf "$root"
}
trap cleanup EXIT INT TERM

port=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
mkdir -p "$root/home" "$root/data/headless" "$root/codex"
printf '{"tunnel":"tailscale","httpsPort":8444,"listenAddr":"127.0.0.1:%s"}' "$port" \
    > "$root/data/headless/config.json"
head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n' > "$root/data/headless/loopback-capability"

HOME="$root/home" CODEX_HOME="$root/codex" WONDER_DATA_DIR="$root/data" \
WONDER_LOG_DIR="$root/data/logs" WONDER_LISTEN_ADDR="127.0.0.1:$port" \
WONDER_LOOPBACK_CAPABILITY=$(cat "$root/data/headless/loopback-capability") \
WONDER_CODEX_BIN=/bin/false \
    "$bin/wonderd" > "$root/wonderd.log" 2>&1 &
daemon=$!
for _ in $(seq 100); do
    curl -fsS "http://127.0.0.1:$port/healthz" > /dev/null 2>&1 && break
    sleep 0.1
done
curl -fsS "http://127.0.0.1:$port/healthz" > /dev/null || { cat "$root/wonderd.log"; exit 1; }

# One Project to list, made through the existing HTTP route (fixture setup).
mkdir -p "$root/home/example-project"
curl -fsS -o /dev/null -X POST "http://127.0.0.1:$port/api/v1/projects" \
    -H "x-wonder-loopback-capability: $(cat "$root/data/headless/loopback-capability")" \
    -H 'content-type: application/json' \
    -d "{\"requestId\":\"$(python3 -c 'import uuid; print(uuid.uuid4())')\",\"name\":\"Example\",\"folders\":[\"$root/home/example-project\"]}"

HOME="$root/home" WONDER_DATA_DIR="$root/data" \
    python3 "$here/example_client.py" --cupola "$bin/cupola" --seconds "${SECONDS_TO_FOLLOW:-2}"

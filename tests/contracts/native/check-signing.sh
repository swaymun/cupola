#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../../.."
work_dir="$(mktemp -d "${TMPDIR:-/tmp}/wonder-signing.XXXXXX")"
trap 'rm -rf "$work_dir"' EXIT
cargo run --locked -q -p wonder-api --example signing_fixture > "$work_dir/rust.json"
cmp tests/contracts/native/signing-v1.json "$work_dir/rust.json"
swift tests/contracts/native/verify-signing.swift "$work_dir/rust.json" "$work_dir/swift.json"
cargo run --locked -q -p wonder-api --example signing_fixture -- "$work_dir/swift.json"

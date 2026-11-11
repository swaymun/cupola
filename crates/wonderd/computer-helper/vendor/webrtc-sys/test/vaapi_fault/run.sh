#!/bin/sh
# Compiles the production VAAPI wrapper unchanged against a mock libva and
# injects vaMapBuffer/vaRenderPicture failures under ASan+UBSan (Linux, needs
# libva-dev and clang++). A failed map must make Encode return false (software
# fallback), never dereference a null pointer or leak VA buffers.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
src="$here/../../src/vaapi"
out=$(mktemp -d)
trap 'rm -rf "$out"' EXIT
"${CXX:-clang++}" -std=c++17 -g -fsanitize=address,undefined -fno-sanitize-recover=undefined \
  -I"$src" -I"$here/stubs" "$here/fault_injection_test.cpp" -o "$out/test"
"$out/test"

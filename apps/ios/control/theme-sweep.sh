#!/bin/bash
# Usage: theme-sweep.sh [--scenario NAME] DEVICE [APPEARANCE] -- THEME_ID...
# Run from the repo root (WONDER_CONTROL_MINI=1 runs on the Mac mini). Runs a theme scenario once
# per theme (the copy lives under the synced tree so a remote host can read it) and names
# the screenshots theme-<id>-<step>. NAME defaults to theme-appearance (sidebar, chat,
# picker); theme-surfaces covers New Chat, Files and the file viewers; font-appearance
# takes MESSAGE+CODE font ids such as serif+jetBrains. With APPEARANCE
# (light|dark) the simulator's appearance is set first, for the "wonder" theme that
# follows the device.
set -u
scenario=theme-appearance
if [ "$1" = "--scenario" ]; then scenario=$2; shift 2; fi
device=$1; shift
appearance=""
if [ "$1" != "--" ]; then appearance=$1; shift; fi
shift
src=apps/ios/control/scenarios/$scenario.wctl
tmpdir=apps/ios/control/.sweep; mkdir -p "$tmpdir"
if [ -n "$appearance" ]; then
  udid=$(scripts/wonderctl --device "$device" device | tail -1)
  if [ "${WONDER_CONTROL_MINI:-}" = 1 ]; then
    ssh "${WONDER_MINI_HOST:-second-mac}" "xcrun simctl ui $udid appearance $appearance"
  else
    xcrun simctl ui "$udid" appearance "$appearance"
  fi
fi
# A scenario names the id it runs by default in a "# sweep-default: ID" line (nord if
# absent); that id is replaced in its --arg and in its screenshot names.
default=$(sed -n 's/^# sweep-default: //p' "$src"); default=${default:-nord}
for id in "$@"; do
  name=$id; [ -n "$appearance" ] && name="$id-$appearance"
  sed -e "s/-$default-/-$name-/g" -e "s/--arg $default/--arg $id/" "$src" > "$tmpdir/$name.wctl"
  scripts/wonderctl --device "$device" run "$tmpdir/$name.wctl" 2>&1 | tail -1
done
rm -rf "$tmpdir"

# Driving Cupola from the command line

`scripts/wonderctl` launches the Diagnostics build of Cupola in a dedicated simulator,
jumps to a known state with Diagnostics fixtures, taps and types by accessibility
identifier, and captures evidence. Semantic steps need no screenshot model, and a
scenario file reruns the same way every time. Run `scripts/wonderctl --help` for every
command and `scripts/wonderctl fixtures` for the launch flags the app understands.

- **Scenarios** are in `apps/ios/control/scenarios/*.wctl`: one command per line
  (`tap`, `type`, `wait`, `assert`, `shot`, ...). `scripts/run-scenarios` runs the whole
  feature map on one simulator; `scripts/check-scenario-selectors` checks that every
  element a scenario taps or waits on still exists in the app.
- **Devices** (`--device`): `iphone`, `ipad`, `duo` (a two-screen foldable simulator,
  folded or unfolded with the `hinge` command) and `iphone-27`.
- **Lanes:** `scripts/wonderctl lane acquire --kind iphone|ipad|duo` takes a dedicated
  simulator clone, so parallel runs on one Mac do not share a device;
  `lane release` deletes it.
- **Driver:** the loopback test driver and its semantic state compile only under
  `WONDER_DIAGNOSTICS`. `wonderctl --driver` supplies a random per-launch token; its
  requests never use production pairing or transport. State includes visible UI text
  and must stay local. Release builds exclude it: `scripts/check-ios-profiles.py`
  checks an exported app.
- **Previews:** `scripts/wonderctl preview <screen>` renders offline fixture screens on
  all four device layouts, with `--before REF` for a before and after.

Evidence (captures, `.xcresult` bundles) is written under `.local/control/`, which is
git-ignored. Real captures still need a human look.

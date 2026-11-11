# Testing Cupola on iPhone and iPad

| Scheme | Configuration | Use |
| --- | --- | --- |
| `Wonder` | `Release` | Distribution builds; no recorder, fixtures or test driver |
| `Diagnostics` | `Diagnostics` | Simulators, scenarios and UI tests |

Both include the Notification Service, Share, Widget and Notification Content
extensions.

## Swift packages and snapshots

```sh
swift test --package-path apps/native           # pairing, protocol, caches, rendering
swift test --package-path apps/ios              # presentation snapshot tests
```

Snapshot references live in `apps/ios/SnapshotTests/References`.

## Simulator scenarios

`scripts/wonderctl` launches the Diagnostics build in a dedicated simulator, jumps to
a known state with launch fixtures, taps and types by accessibility identifier and
captures evidence. A scenario (`apps/ios/control/scenarios/*.wctl`) replays the same
steps every time. It needs Xcode, a simulator runtime and [AXe](https://github.com/cameroncooke/AXe).
See [CONTROL.md](CONTROL.md).

```sh
scripts/wonderctl doctor                              # what is installed and ready
scripts/wonderctl select-tests --files apps/ios/Wonder/TerminalSheet.swift
scripts/wonderctl lane acquire --kind iphone
scripts/wonderctl run apps/ios/control/scenarios/first-launch.wctl
scripts/wonderctl lane release --kind iphone
```

Layouts to cover for a UI change: iPhone, iPad, and a two-screen foldable (the
`duo` device) folded and unfolded. Check selectors with `scripts/check-scenario-selectors`.

A build, a skipped step or a tap is not behavior proof: look at the captures.
Hardware-only behavior (camera, Secure Enclave, notifications, folding hinge) is
unverified until it runs on that hardware; label it that way.

# Native computer-use boundary

`WonderComputerUse` is a separately built, permission-gated unlocked Computer
Use service. In the signed Mac app it ships as the plain executable
`Contents/Helpers/WonderComputerUse`, with no bundle or Info.plist of its own.
The CLI bundle sets `CFBundleExecutable=cupola` and `LSUIElement=true`; it has no
windows, nested helper app or in-app updater. Its
`Contents/Helpers/wonder-computer-use` symlink runs the bundled CLI, which uses
`open -g -n -a <this Cupola.app> --args ...` before executing the native helper.
The LaunchServices-launched CLI stays alive while its native helper child runs,
so ScreenCaptureKit keeps a live app principal instead of a replaced executable.
LaunchServices gives the process Cupola's privacy attribution even when the CLI
was invoked from a terminal or LaunchAgent. A mode-0700 temporary directory,
mode-0600 Unix socket and per-session token reconnect the existing JSONL
stdin/stdout stream. A separate authenticated lifetime connection lets the
LaunchServices session kill and reap the real helper's process group when the
client proxy is killed, even if the helper ignores stdin or SIGTERM. Closing
JSONL input alone still lets the helper drain queued replies before exiting.
Who can reach the helper (it holds the Screen Recording and Accessibility
grants): the session token is a mode-0600 file in the private socket directory,
never a process argument, so `ps` shows only the socket path. The proxy and the
LaunchServices session each check the executable of the process on the other end
of the socket (`LOCAL_PEERPID`, then `proc_pidpath`) against the Cupola host's own
`Contents/MacOS/cupola` before any token is read or sent; another program of the
same user that finds the socket is skipped, and a session never sends its token
to a non-host listener. The helper itself exits unless its parent process is the
bundled `Contents/MacOS/cupola` beside it, so launching `WonderComputerUse`
directly or from a copied binary is refused. Debug builds, and release builds made
with `swift build -c release -Xswiftc -DCUPOLA_DEV_HELPER` (for `scripts/computer-soak`
and `scripts/verify-computer-use.mjs`), skip that last check and are never shipped.
The shared logic is `computer_sessions::helper_channel` (tested on Linux) and
`HelperCaller` (Swift tests).
No listener is exposed on the network. The executable keeps the stable `com.saimun.wonder.computer-use`
signing identifier for update checks. It communicates over JSONL on standard
input/output so `wonderd` can supervise it without exposing a network listener.

`cupola permissions status` launches only `--permissions` (preflight APIs, no
request flags). `cupola permissions grant` separately opens the LaunchServices
helper with `--request-screen --request-input` and the Screen Recording and
Accessibility panes in System Settings. The returned status is the current
preflight result; run status again after enabling Cupola. The same launcher
supports `--watch-dialogs`, `--discover-sources`, capture and control without
changing the daemon protocol.

Build a Developer ID signed staging bundle with
`WONDER_BUILD_VERSION=<numeric version> scripts/package-cli-macos.sh <staging>/Cupola.app`.
`scripts/release-cli-macos.sh <version> <existing notarytool Keychain profile>`
notarizes, staples and verifies it without installing or publishing. Neither
script creates signing credentials. `scripts/link-cli-macos.sh <installed>/Cupola.app
<bin directory>` creates the PATH symlink without copying the signed executable.

`scripts/verify-cli-macos-runtime.py <bundle> --output <report.json>` checks the
real LaunchServices bridge and helper EOF. Add
`--window-source app:<bundle-id>#<window-number>` only for an owned capture
fixture with existing grants: it requires increasing frame metadata from that
exact window, stores no pixels and never opens a permission prompt. This guards
against ScreenCaptureKit application-connection failures that preflight and
source discovery alone cannot detect.

For isolated on-Mac acceptance, set `WONDER_CLI_STAGING=1` when packaging.
This uses the fixed `com.saimun.wonder.cli-staging` app identity, its own helper
signing identifier and LaunchAgent label. Install that bundle under a distinct
name in `~/Applications`, and always supply a separate `WONDER_DATA_DIR`.
The updater requires the candidate to match the current bundle identity, so a
staging update cannot replace production or reset production permission rows.
Remove only the staging bundle and its TCC rows after acceptance; keep the
installed production bundle untouched during this isolated acceptance.

The phase-2 update input is a local, staged bundle:
`WONDER_UPDATE_BUNDLE=<staged>/Cupola.app cupola update [--check] [--version X]`.
It validates Developer ID team, bundle identity, Gatekeeper and the stapled
notarization ticket, prevents downgrade, and requires the host to be stopped.
It compares the app and helper designated requirements before replacement.
Unchanged requirements cause no TCC mutation; a changed requirement resets only
ScreenCapture, Accessibility and AppleEvents for `com.saimun.wonder`. Distribution
feeds, checksum/Sigstore download verification and channel selection belong to
phase 4; a staged bundle's channel cannot be verified here.

`cupola service install|status|restart|uninstall` manages only
`~/Library/LaunchAgents/com.saimun.wonder.cli.plist`. The login job uses
`open -g -n -a <this Cupola.app>`, carries an allowlisted private environment,
and runs in the current user's Aqua session. It starts at login, without a
KeepAlive loop that would undo an explicit `cupola stop`. Uninstall removes
only a matching service and leaves the app and paired-device data in place.

Supported actions are deliberately narrow: `status`, `screenshot`, the
authenticated `capture.*` family, and the authenticated `control.*` lease/input
family. Input exists only as `control.input` under an active control lease;
the older lease-free `click`, `type`, `key` and `focusApp` commands were
removed. Every input is bounded and fails closed unless macOS Accessibility is
trusted. When a window is shared, input fails with `window_covered`,
`window_not_focused` or `window_not_found` rather than reaching another
window, and Accessibility reads of the shared app run off the helper's main
run loop with a total time budget, so a busy app cannot stall heartbeats. Screenshots and ScreenCaptureKit capture fail closed
unless Screen Recording is available. The service never enables locked use,
elevates privileges, or accepts arbitrary shell commands.

## Capture boundary

The capture owner is a separate `WonderComputerUseCore` library target backed by
ScreenCaptureKit. It supports `capture.prepare`, `capture.sources`,
`capture.pick`, `capture.start`, `capture.pause`, `capture.resume`,
`capture.stop`, `capture.status`, `capture.viewerCount`, and
`capture.sourceRemoved`. Each capture command is handshake-gated and carries the
session ID and generation where it acts on an existing session. Duplicate stops
are safe for the same identity; a stale identity is rejected.

The viewer can request 1280x720, 1920x1080, 2560x1440, or Auto (up to 2560x1440
with balanced WebRTC adaptation). Older clients continue to request 1280x720. All modes cap at
15 fps, disable audio, and use a ScreenCaptureKit
queue depth of two. Frames are handled on a dedicated capture queue and handed to
the pinned stasel/WebRTC M153 `RTCVideoSource(forScreenCast: true)` through a
bounded latest-frame sink. Superseded frames are dropped so a slow encoder cannot
accumulate latency. H264 is encoded at level 5.1 so VideoToolbox accepts frames
larger than 1280x720 instead of WebRTC falling back to software VP8.
`WONDER_LATENCY_BENCH=1 swift test --filter ScreenLatencyBenchmarkTests`
measures loopback latency per setting. Pixel payloads are never persisted, logged, or placed in chat
events; only bounded metadata is emitted.

`capture.pick` selects the saved setup display when connected, or the Mac's main
display when none is selected. An explicit source request uses that display or
window. An agent's app target, `app:<bundle id, name or path>` with an optional
`#<window number>`, shares that window when it belongs to the app, otherwise the
app's frontmost visible window, and fails with `window_not_found` when the app has
none. If neither the selected nor the main display is available on macOS 14 and newer,
the system sharing picker lets the person choose. A request from a
phone reports **Choose what to share on your Mac** until that OS-owned action is
complete. Selection never bypasses TCC. The daemon mediates authenticated,
in-memory offer/answer and bounded trickle-ICE signaling. Direct LAN ICE is used
with an empty ICE-server list in this checkpoint; there is no LiveKit, token
broker, public helper listener, TURN service, or WebRTC remote input/data
channel. Control input is delivered through the authenticated daemon/helper
pipe only. The paired owner, conversation, host, and generation bind every
signaling operation.

Each successful action also returns a bounded accessibility observation: the
frontmost app, focused window, and focused element metadata such as role,
title, description, identifier, and whether its value is settable. Text field
contents are not returned; only value type/length metadata is included. This
lets the agent verify what the desktop reports after an action without making
the driver a general-purpose screen reader or collecting typed secrets.

Build it with:

```text
swift build -c release --package-path native/computer-use
```

Use `--dry-run` for protocol and permission checks; it validates inputs without
posting pointer, keyboard, or application-focus events.

When `computer_use_enabled` is on and the configured executable is present and
executable, `wonderd` supervises one persistent helper for the host. It stores a
session as preparing, starts the child with a fresh per-invocation handshake,
and sends `capture.prepare` followed by `capture.pick`. The system picker stays
Mac-owned and TCC-gated. Helper responses and lifecycle events are bounded,
session/generation checked, and folded into the durable session row. Duplicate
requests for the same device/request reuse the worker; a different owner cannot
take an active provider session. Delete, revocation, helper failure, and daemon
shutdown stop, kill if necessary, and reap the child, releasing any lease.

Direct invocations without the daemon handshake are rejected, including
`status`; `stop` remains available only to terminate the supervised child
process. The helper only publishes to the authenticated daemon-mediated peer.
The iOS renderer and remote-control gestures use the same authenticated lease
boundary. Native input is delivered only when the owner has enabled **Allow
control from paired devices** in local Cupola Settings. Stopping the lease,
disabling the preference, losing the capture binding, backgrounding the
session, or expiry releases held input and hides the local control surface.

Control key presses with modifiers emit a complete native chord, preserving
arrow-key flags and left/right modifier identity. Explicit held modifiers also
apply to pointer and scroll events until released. `ControlCoreTests` checks the
native events without posting input; live shortcut acceptance still requires a
Mac with the corresponding keyboard shortcuts enabled.

The package script copies the helper into `Contents/Helpers`, adds the
`@loader_path/../Frameworks` rpath for WebRTC, signs it before the app, and
rejects a nested helper app or the older `Resources` layout in finished
artifacts. Runtime execution and Accessibility/Screen Recording approval remain
explicit acceptance gates; ordinary builds do not invoke an action.

Run the focused native checks with:

```text
swift test --package-path native/computer-use
python3 -m py_compile scripts/sign-macos-app.py
```

`scripts/verify-cli-macos.sh` checks a finished bundle's helper signing identity,
WebRTC rpath and layout without installing or launching it.

### Headless Simulators

The viewer menu lists booted devices through authenticated
`GET /api/v1/computer/sources/simulators`. The daemon runs this same helper with
`--simulators`, bounded to eight seconds, one enumeration at a time and 32
results. No Xcode or an unsupported CoreSimulator runtime returns unavailable;
ordinary ScreenCaptureKit sharing is unaffected.

A `simulator:<UUID>` source uses the selected Xcode's native framebuffer, without
opening Simulator.app, booting a device or taking over another consumer. The
optional Objective-C bridge dynamically checks private Apple selectors documented
in the [idb CoreSimulator headers](https://github.com/facebook/idb/tree/fcd8934dd93fe4b6095a5789beb1d84174a8a994/PrivateHeaders/CoreSimulator); this
is not a public Apple compatibility promise. Native shutdown notifications end
the exact source. Framebuffer pixels are copied before encoding, capped at 15
frames/second and carried through the existing bounded video queue. Pause,
quality, session-generation and viewer lifetime use the same capture owner.
Simulator viewing does not grant or require Mac Screen Recording. Whole-screen/window
permission checks remain intact.

Simulator input (`SimulatorInputDriver`, `WonderSimulatorInput.m`) goes only to
the session's exact `simulator:<UUID>` and never posts Mac events, so it needs
neither Accessibility nor a Mac-side approval; it still needs the phone's "Take
control" lease (identity, sequence, dedupe, revocation) and the owner's
`cupola computer control` switch. On a simulator session the daemon sends
`capabilities` with that `sourceID`; the helper connects and answers
`simulatorControl` (`provider: simulator-hid-v1`, supported `buttons`). With
CoreSimulator 1155.4 or later (Xcode 27) the injector sends XPC events to the
guest's `dtuhidd` digitizer service, the transport idb documents for those
toolchains; earlier toolchains use SimulatorKit's `SimDeviceLegacyHIDClient`
and `IndigoHIDMessageFor*` builders. Every private class, selector and symbol
is checked first; anything missing reports the simulator as view only.
Control actions add `touch` (normalized point, `down`/`move`/`up`, optional
`delayMs` up to 1 s) and `button` (`home`, `lock`, `volumeUp`, `volumeDown`);
`key` and `text` reuse Mac control's names, typed as US-keyboard HID usages.
Other text is refused (`text_unsupported_characters`) so the phone can offer
paste. Debug builds measure the injector with
`WonderComputerUse --simulator-input-probe <UUID> [--transport dtuhid|indigo]`,
one JSON action array per input line; Release builds do not contain it.

### Dialogs waiting on the Mac

While any thread's turn runs, `wonderd`
(`crates/wonderd/src/mac_dialogs.rs`) keeps one helper running with `--watch-dialogs`. Once a second it reads the
window list and prints `{"dialogs":[…]}` whenever the set of open system
dialogs changes: windows owned by `UserNotificationCenter` (privacy prompts
such as Automation), `universalAccessAuthWarn` (Accessibility),
`SecurityAgent` (passwords) and `CoreServicesUIAgent` (opening downloads).
Only owner, window number and bounds are read, so it needs no Screen
Recording. Closing standard input ends it. The watcher only reports windows;
it never focuses or answers a dialog. A pending dialog keeps it alive after
the turn ends, until closure or 30 seconds of failed verification. Healthy quiet
streams retain their last snapshot. The host journals `mac_dialogs` count changes,
including zero, over its existing event stream. Initial/reconnect and notification
tap reads recover snapshots; phone foreground presence and count reads do not
activate the watcher, and the phone has no dialog-polling timer.

The phone's counted computer notification explicitly starts a session with
source `mac-dialog`.
The daemon selects a currently pending window and calls
`--raise-dialog <windowId> <owner>` before starting its exact window capture.
The helper rechecks both ID and owner, raises only the matching Accessibility
window, and reports `raised`, `gone` or `unavailable`. It never presses a button
or substitutes another window/display. Raising needs existing Accessibility
access; capture separately needs Screen Recording. A closed dialog clears its
count and returns HTTP 410, so the phone refreshes the computer indicator and
shows a plain note. The initial window view is view-only; Whole screen uses
the existing explicit control flow for phone input. The notification itself
never responds to a dialog.

`--automation <bundle id>` prints whether Cupola may send Apple events to that
app (`granted`, `needsConsent`, `denied`, `notRunning`) using
`AEDeterminePermissionToAutomateTarget` without asking, so Cupola can explain a
permission on the phone before anything shows the macOS prompt. It never
launches the app.


### Session clipboard

The phone's computer-view menu has a persisted **Share Mac clipboard** switch,
initially off. With that switch on and a foreground viewer subscribed, the helper
polls the pasteboard change count every 250 ms on a worker actor. The first poll
baselines existing contents; it sends only new text copies. Subscriptions expire
three seconds after the device stops polling, and ending the session ends its helper.
Clipboard text is returned only in authenticated signaling replies, never stored in
session history or logged. Unsupported helpers keep video working and report sharing
as unavailable on the phone.

Safety rules informed by [Sill](https://github.com/Saffsanity/sill) (Apache-2.0):
check all item types before reading text; skip concealed, transient, autogenerated,
legacy password-manager, Universal Clipboard and Cupola-owned markers; accept only
nonempty UTF-8 text up to 256 KiB. On macOS 15.4+, background reads require
`NSPasteboard.accessBehavior == .alwaysAllow`; ask, deny and unknown states do not
read even the change count. Returning permission baselines copies made while blocked.

Manual **Copy from Mac** applies the same type checks before reading. It follows the
person's tap, so with the default or Ask setting it may let macOS show its paste
prompt. Helper requests run serially on the main thread, so that read runs on its own
queue and the tap answers at once ("choose Allow Paste on the Mac, then tap again");
the next tap collects the result if the pasteboard is unchanged (single use, 60 s).
Always Deny, a declined prompt, a private item, no text or more than 8,192 characters
come back as a reason the phone shows while control stays active.
These markers protect content identified by the copying app; unmarked text cannot
be reliably classified as a password.

The phone writes received text with `.localOnly` and a ten-minute expiration.
Phone-to-Mac input uses the system `PasteButton`, so the app does not automatically
read the phone pasteboard. Tests use named private pasteboards only.

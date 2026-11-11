# Native Windows/Linux computer helper

This standalone Rust executable speaks the existing JSON-lines computer helper
protocol. macOS continues to use the Swift helper. The daemon and phone session
protocols are unchanged. Linux and Windows use native sharing by default where
the archive includes it; archives without a native SDK (currently ARM64) retain
the Python default. To choose explicitly:

```sh
export WONDER_COMPUTER_BACKEND=native # python selects the fallback
cupola doctor
cupola service install             # saves this selection for the service
```

On Windows use `$env:WONDER_COMPUTER_BACKEND = 'python'` (or `'native'`). Restart the host after
changing the choice. `WONDER_COMPUTER_USE_BIN` remains a development override.
An invalid backend or absent selected helper fails with a repair message; it
never silently changes backends. The native helper is beside `cupola` in the
complete installation. It needs no Python, pip, venv or separate helper download.
The Python backend continues to use system Python and the packaged requirements
files. Cupola bundles no interpreter.

Linux capture requires an X11 display (the host starts Xvfb/openbox), with
MIT-SHM and XComposite. Input uses xdotool, and clipboard uses xclip. Pure Wayland
capture/input is outside this unit. Windows uses Windows.Graphics.Capture and
SendInput in the signed-in user's interactive desktop. Windows can refuse input
to elevated windows or a secure desktop; the helper releases the lease on input
failure. Window views retain exact source identity and the covered-point/focus
guards. A closed or missing window never changes into whole-screen capture.

## Linux desktop on boat

A boat box's desktop is real X11 (Xorg `:0` under lightdm + openbox, 1920x1080,
Composite and MIT-SHM present), and `boat desktop ID` streams it with Sunshine
(`stream.html?...width=1920&height=1080`). The native helper captures `:0`
directly, so what the owner sees there is what the phone gets. The box ships
clang 18 and libwebrtc needs clang 21. From the repo root on your machine:

```sh
scripts/vms/boat fork NAME --base HEAD
scripts/vms/boat ssh NAME 'cd /tmp && curl -fsSL https://apt.llvm.org/llvm.sh -o llvm.sh && sudo bash llvm.sh 21'
scripts/vms/boat ssh NAME 'export CXX=clang++-21 CC=clang-21; cargo build --locked -p wonderd --features experimental-headless --bins && (cd crates/wonderd/computer-helper && cargo build --locked --release --features parity-tests --bins) && python3 -m venv /tmp/venv && /tmp/venv/bin/pip -q install -r scripts/live-fixtures-requirements.txt'
scripts/vms/boat ssh NAME 'DISPLAY=:0 /tmp/venv/bin/python scripts/computer-soak --isolated --sessions 20 --daemon target/debug/wonderd --helper crates/wonderd/computer-helper/target/release/wonder-computer-helper --receiver crates/wonderd/computer-helper/target/release/computer-parity-receiver'
scripts/vms/boat ssh NAME 'export WONDER_COMPUTER_DISPLAY=:0 DISPLAY=:0 WONDER_COMPUTER_USE_BIN=$PWD/crates/wonderd/computer-helper/target/release/wonder-computer-helper; target/debug/cupola init --tunnel none --origin https://HOST --computer && target/debug/cupola start && target/debug/cupola pair'
scripts/vms/boat stop NAME   # or delete
```

Step 4 is the 20-session soak with the libwebrtc answerer; step 5 brings up a
pairable host on the real desktop. `WONDER_COMPUTER_DISPLAY=:0` makes `cupola`
share that display; without it the host starts its own private Xvfb `:1`
(which needs `sudo apt-get install xvfb`) and the phone sees that empty screen.
Use the default `--source` (the main display); the helper lists windows only,
not a `display` source. Run commands with `DISPLAY=:0` (no `XAUTHORITY` needed).

- Apps started from an ssh shell can drop typed keys: GTK tries ibus without
  the session bus. Start test targets with `GTK_IM_MODULE=simple`; apps started
  inside the desktop are unaffected.
- The X11 contract test (`x11_identity_...`) needs bare Xvfb (`Xvfb :77 &`,
  `DISPLAY=:77`); under the desktop's openbox its raw window is never mapped.
- Reachability: the box offers only host candidates (its public IPv6 and a
  private 192.168.x address). A phone without IPv6 (or Tailscale on the box)
  cannot reach it; there is no TURN. A soak or the answerer on the box itself
  always works.
- `tools/input_probe.py HELPER OUTDIR` drives `control.input`
  (move, click, type, Enter) at a zenity dialog and checks the pointer and the
  typed text.

## Build and checks

Development builds may use the pinned LiveKit prebuilt. They are **not release
artifacts**: release packaging requires a source SDK, its matching binary hash,
codec flags, actual H264/VP8 capabilities, and complete notices.

```sh
cd crates/wonderd/computer-helper
CARGO_BUILD_JOBS=4 CXX=clang++-21 cargo test --locked
CARGO_BUILD_JOBS=4 CXX=clang++-21 cargo build --locked --release
# With this unit's isolated Xvfb DISPLAY/XAUTHORITY:
cargo test --locked x11_identity_movement_cover_and_closed_window_contracts -- --ignored
# Test-only libwebrtc answerer; never packaged:
cargo build --locked --release --features parity-tests --bin computer-parity-receiver
```

Build Windows in this directory with `cargo +stable-x86_64-pc-windows-msvc`;
`.cargo/config.toml` selects the static MSVC CRT required by libwebrtc. The main
Windows host still uses gnullvm. Do not link the MSVC library into that host.
Keep target output on the build VM, with `CARGO_BUILD_JOBS=4`.

`python3 check.py` runs the standalone fmt/unit checks on Linux or Windows. From
the Mac it snapshots only the helper build inputs over SSH to `linux-build-host` and
runs there, using a reusable content-addressed directory and Cargo cache under
`~/.cache/wonder/computer-helper-check/`. Use `--host HOST` for a different Linux
test host and `--sdk PATH` for its sealed source SDK. This bounded check needs no
display, simulator, installed host or Python backend. The display-dependent X11
case remains an explicitly ignored integration check run on the isolated VM
fixture; a unit-check pass does not claim live capture/control parity.

The publisher offers constrained-baseline H264 and VP8/RTX, marks the track as
video rather than screencast, and scales its bitrate budget with resolution and
frame rate. `WONDER_COMPUTER_STATS=1` writes content-free encoder/capture counters only in
the `parity-tests` benchmark build. Release helpers sample sender stats once per second while a peer is alive to
apply the hardware/codec rate ceiling; they log encoder changes, not per-frame
statistics. Full frame statistics remain verification-only. The daemon forwards exactly one fixed
negotiated-codec summary and discards other helper diagnostics. Receiver cadence,
loss/drops and actual simulator Choppy intervals are separate acceptance checks.


## Encoder selection and rate ceiling

Linux x64 uses the SDK's dynamically loaded VAAPI backend. Windows x64 uses
NVENC through Direct3D11 and the system `nvEncodeAPI64.dll`; it needs a compatible
NVIDIA driver, without a CUDA installation. Windows next tries a hardware-only
Media Foundation H264 transform, allowing compatible Intel/AMD drivers. Media
Foundation is loaded dynamically so missing Windows Server/N media components
do not prevent startup. Hardware initialization or encode failure switches to
source-built OpenH264 through WebRTC's fallback wrapper, retaining the negotiated
H264 codec. VP8 remains available for receivers without H264. The active sender
implementation, rather than driver presence, determines the rate ceiling.

The ceiling starts at 30. A requested `videoMaxFramesPerSecond` above 30 takes
effect only after outbound stats show newly encoded H264 frames and
`powerEfficientEncoder=true`. The sender's negotiated level and current size
can lower that cap; the steps are 60, 30, 15. Software encoding remains at 30.
A new peer resets the cap. A software fallback lowers it again. The receiver's
request carries its display/power ceiling, and a quality change updates both
capture and sender limits. Resolution is retained. At 60 fps the conservative
bitrate ceilings are about 4.5/9/12 Mbit/s for 720p/1080p/1440p; these budgets
are estimates pending measurements on real GPU hosts.

The helper logs `encoder codec=... implementation=... mode=hardware|software`
and rate changes to stderr. This is actual sender evidence. Listing an encoder
or offering H264 alone does not raise the cap. The daemon's existing VP8-only
log filter may discard H264 summaries; inspect the helper log for verification.

A desktop-independent native codec probe exercises real encoding, decoding and
repeated teardown, including Windows over SSH:

```sh
cargo build --locked --release --features parity-tests --bins
target/release/computer-encoder-probe --require-software # no-GPU host
# --software forces the software selector; omit --require-software on a GPU host.
```

For an actual X11 capture, run `tests/fps_measure.py` with `--max-fps 60
--expect-codec video/H264 --expect-mode software --expect-cap 30` on a no-GPU
host. Add `--then-max-fps 15` to exercise a live ceiling change. GPU acceptance
uses a moving 1080p display, `--expect-mode hardware --expect-cap 60`, and reports
measured decoded fps separately from the requested rate.

## H264 licensing decision (owner approved 2026-11-02)

This source build bundles OpenH264 and Chromium's FFmpeg H264 decoder subset.
It does **not** download Cisco's separately distributed binary. OpenH264's BSD
source license is distinct from AVC patent coverage; Cisco's royalty arrangement
for its separately downloaded binary does not cover our bundled source build.
That binary arrangement also requires end-user control, attribution and the
full binary notice. Hardware driver/API access is not itself a statement that
all product distribution or AVC use is patent-licensed. Preserve the actual
source-build notices, including FFmpeg's LGPL terms, and the corresponding
source/relinking obligations for distribution. Packaging records
`h264Software=bundled-openh264` and `ciscoBinaryLicense=false` explicitly.

Sources: [Cisco FAQ](https://www.openh264.org/faq.html),
[Cisco binary conditions](https://www.openh264.org/BINARY_LICENSE.txt),
[NVIDIA API guide](https://docs.nvidia.com/video-technologies/video-codec-sdk/13.0/nvenc-video-encoder-api-prog-guide/index.html).

## Release library and notices

Use the `webrtc-sys 0.3.49` source build scripts/patches with the pinned
webrtc-sdk m150 revision recorded in the build evidence. Build off the Mac.
`build-webrtc.py` pins that source and depot_tools revision, applies the release
flags to the upstream scripts, uses four Ninja jobs and records provenance:

```sh
# Linux build VM, with the Chromium/WebRTC C++ build prerequisites installed:
python3 build-webrtc.py --out "$HOME/.local/build/wonder-webrtc-h264"
# Windows build VM, with VS 2022 C++, SDK 10.0.26100.0 and its Debugging Tools:
py -3 build-webrtc.py --out C:/build/wonder-webrtc-h264 --vs-install "C:/Program Files (x86)/Microsoft Visual Studio/2022/BuildTools"
```

The Windows source checkout needs substantially more disk than the final
artifact. Use the provisioned build VM/storage; never build it on the Mac.
These SDK/compiler tools are build prerequisites, not app prerequisites.
Set `rtc_use_h264=true`, `rtc_use_h265=false`, `is_debug=false`, tests/tools/
examples off, and retain LiveKit's libc++ ABI settings. Linux disables VP9/libaom
build options; the product offers constrained-baseline H264 (packetization mode
1, level 5.1) followed by VP8. Install `libva-dev` before building the Linux x64
helper; its absence is a build error, so release builders cannot silently omit
VAAPI. The scoped LiveKit native fork is documented in `vendor/README.md`. Regenerate full `LICENSE.md` from the actual GN
build graph. Preserve source/depot_tools/toolchain revisions, patches, `args.gn`,
logs and output hashes. Point `LK_CUSTOM_WEBRTC` at that complete artifact
layout; never fall back to the automatic prebuilt in a release command. First
give it a content-addressed path, which also invalidates webrtc-sys's Cargo cache
when the library changes:

```sh
export LK_CUSTOM_WEBRTC="$(python3 seal-webrtc.py --library /path/to/linux-x64-release)"
# PowerShell: $env:LK_CUSTOM_WEBRTC = py -3 seal-webrtc.py --library C:/path/to/win-x64-release
```

The helper records that library's SHA-256 in its binary and `--version` output.
The verifier and packager require it to match the audited library. An unsealed
custom SDK, automatic prebuilt, stale binary or mismatched notices cannot pass.

After building the production helper, run `release-manifest.py` with the library
layout, helper, full source revision, target and its llvm-nm. It checks binary
architecture, actual H264/VP8 capabilities, explicit H264 enablement/H265
disablement, the OpenH264 software fallback, absence of x264/x265/HEVC implementations
(the shared descriptive HEVC profile-name table is allowed),
and all normal Rust dependency license texts (including the native binding fork). Missing notices fail the
command. It emits the hash-bound manifest and complete notices into the selected
output directory. Set `WONDER_COMPUTER_BUILD` to that directory and
`LK_CUSTOM_WEBRTC` to its SDK before running the native CLI release script.
On Linux, `scripts/release-cli-linux.sh` builds both the helper and host inside
its Jammy/glibc 2.35 container, with Clang 21 for the SDK's hermetic libc++.
The helper output is `.local/build/computer-helper-release-jammy/release/wonder-computer-helper`.
Generate the audited manifest from that baseline-built helper before packaging;
a manifest from a newer-libc host build will correctly fail the binary hash check.
`package-cli-release.py` requires matching helper/manifest/notices whenever
native is supplied. With neither SDK variable set, release scripts omit native
and retain the default Python helper. This keeps ARM64 archives buildable before
their SDK is available. The archive's `computer-backends.json` records the
default and native availability, also exposed as `nativeComputerHelper` in the
release manifest. `cupola doctor` explains when native is unavailable and how
to use the included Python backend. No Python interpreter is bundled.

The root THIRD_PARTY_NOTICES links to full Linux/Windows notices, also bundled in
the existing phone Settings > Acknowledgements screen. Regenerate them from each
audited build's manifest, native texts and Rust inventory:

```sh
python3 collect-app-notices.py --build /path/to/linux-release-inputs
python3 collect-app-notices.py --build /path/to/windows-release-inputs
```

The collector verifies the notice hashes and locked Rust versions, then preserves
both payloads verbatim in `notices/`. Release acceptance still requires recorded
side-by-side parity on both VMs and the simulator. The `parity-tests` feature also enables the test-only `benchmark30` quality.
Its helper reports `verificationOnly=true`; manifest generation and packaging
reject it. The normal helper retains only the product quality table. A
production helper, unit checks or receiver-only run cannot authorize removing Python. See `plans/computer-single-app.md`.

For the existing simulator window-control scenario, launch `tests/live_fixture.py
--state PATH --text-only`: the center and short trackpad pan stay within an
editable text surface on both platforms. The ordinary fixture retains its
separate click/scroll controls for the VM protocol probe. After recording, run
`tests/live_fixture.py --state SAVED_STATE --verify-window-control TOKEN` to
require the exact `TOKEN-first` and `TOKEN-second` in their own windows. A UI
test reporting successful taps is not the stored-input acceptance check.

For phone clipboard parity, use that same text-only fixture and write a unique
`{"id":"RUN","action":"clipboard","text":"TOKEN"}` to the state path with
its suffix replaced by `.command.json`. Wait for the exported `commandID` to
match before testing. In the real phone app, select window A, take control,
recenter the trackpad and tap the text field. Copy from the computer, then Paste
from Phone. Require the token in A's saved text; successful menu taps alone do
not prove a round trip. Send Tab/Escape and a two-finger scroll, check the saved
keys and nonzero scroll, then close, terminate and reopen the app and require
Live again. Run the same steps and recordings for both backends on both VMs.
Keep scripted-scenario failures separate from passing XCTest/VM checks; they do
not authorize removing Python while the required scenario gate remains open.

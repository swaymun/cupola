# CLI distribution (private preparation)

No channel feed, tap or GitHub release is published by these scripts. The owner
must approve publication in the run's `gates.md`; the coordinator owns that file.
Until then, supply an existing private HTTPS feed with `WONDER_UPDATE_URL` or
`--feed` / `-Feed`. There is deliberately no guessed public download endpoint.

## Install and remove

Download `install.sh` from the approved distribution location and run:

```sh
sh install.sh --feed https://YOUR-PRIVATE-FEED --channel stable
# Or: --channel nightly, or --version 1.2.3
export PATH="$HOME/.local/bin:$PATH"
cupola init
cupola service install   # starts the host now and at every login
cupola pair
```

The script needs Python 3.10+; Linux also needs Node 22+ for the bundled runtime
bridges. macOS uses the Node runtime inside the signed app. Installation runs no
model, signs in to nothing, changes no shell profile and registers no service.
The current macOS verifier also needs `xcrun stapler` from installed Apple developer
tools. The installer does not install Python, Node or those tools; this is a
one-command application install with prerequisites, not a bare-OS bootstrap.
`cupola service install` is the one start command (`cupola start` runs the host only until you log out). No installer uses
sudo or an administrator service. Linux starts at boot only if systemd lingering
is already enabled (enabling it may require the owner's administrator).

On Linux the default install is `~/.local/share/cupola`, with PATH symlinks in
`~/.local/bin`; `--prefix` / `--bin-dir` override both. On macOS the script puts
`Cupola.app` in `~/Applications` (`--app-dir` overrides it), links `cupola` into
`~/.local/bin`, and keeps only its receipt and uninstaller in `~/.local/share/cupola`. The installation receipt
remembers its feed and paths, so later `cupola update` needs no environment setup.
The macOS CLI stays inside a single Developer ID signed, notarized, stapled,
windowless `Cupola.app`. The PATH symlink points into that app; launch and
permission prompts use LaunchServices and the same signed identity after updates.
Homebrew is the usual way to install on a Mac; the cask and the release ZIP's
`INSTALL.txt` put the app in `/Applications/Cupola.app`, and `install.sh` puts it
in `~/Applications/Cupola.app` without administrator rights. An existing
`~/.local/share/wonder` installation keeps working and updating in place.

Windows (PowerShell 5.1+, normal user, **experimental preview**):

```powershell
irm https://getcupola.app/install.ps1 | iex
```

If getcupola.app is unreachable: `irm https://github.com/swaymun/cupola/releases/latest/download/install.ps1 | iex`.

A piped script cannot take parameters, so each has an environment variable
(`CUPOLA_*`, or the older `WONDER_*`): `UPDATE_URL` (the feed, default the newest
GitHub release), `CHANNEL`, `VERSION`, `INSTALL_DIR`, `NO_PATH`. To pass parameters,
save the script and run it: `powershell -NoProfile -File install.ps1 -Feed https://YOUR-PRIVATE-FEED -Channel stable`.

Windows uses `%LOCALAPPDATA%\Programs\Cupola`, versioned binaries and immutable
`.cmd` launchers reading one atomically replaced version selector, so it requires
neither elevation nor Developer Mode symlinks. It adds
its own `bin` to the user's PATH for new terminals (`-NoPath` or `CUPOLA_NO_PATH=1` opts out). Node 22+
is required. Computer viewing uses the included native helper only when the archive says
`nativeComputerHelper` is true; the preview archives do not include it (no Windows WebRTC SDK
is sealed yet), so they use the Python helper: install x64 Python 3.12+ and
`pip install -r <prefix>\versions\<release>\computer-windows\requirements.txt`,
or set `WONDER_COMPUTER_PYTHON`. That helper shows the screen and types for the phone but
cannot ask for consent on the PC, so it reports control as unavailable.
Install and sign in to the providers you use, and make their CLI commands available
on the host's PATH. Windows npm installations commonly put those commands in
`%APPDATA%\npm`; an SSH session may need that directory added to its process PATH
before starting Cupola. The Cupola installer adds its own command, not provider CLIs.
Windows artifacts are currently **unsigned**; SHA256 verifies downloaded content,
but does not establish an Authenticode publisher. Signing choice remains an owner
gate (Trusted/Artifact Signing versus OV/EV).

Known gaps in the Windows preview: pairing over the same Wi-Fi is refused (use
Tailscale or another tunnel), voice is unavailable, Git actions are missing, the host
needs a signed-in desktop session for computer view (it starts at sign-in through a
Task Scheduler logon task, not a Windows service), and the
executables are unsigned, so Defender and SmartScreen may warn.

Remove the service and stop the host before removing an installation:

```sh
cupola service uninstall
cupola stop
~/.local/share/cupola/uninstall.sh
```

On Windows, run `powershell -ExecutionPolicy Bypass -File "$env:LOCALAPPDATA\Programs\Cupola\uninstall.ps1"`.
Uninstallers remove only their receipt-owned application and PATH entries. They
keep conversations, pairing, settings, provider sign-ins and other user files.
They refuse to remove a running host. They never reset TCC grants or revoke devices.

## Upgrade from the previous beta name

The public command is now `cupola`; there is no compatibility alias. Keep
`~/.wonder`, `%LOCALAPPDATA%\Wonder`, install receipts, all `WONDER_*` settings,
pairing and provider sign-ins. Do not copy or rename these data directories.
Let active work finish, back up the data and installed release, then use the old
command to stop and unregister the old host:

```sh
wonder stop
wonder service uninstall
```

For a script-managed install, run its existing `uninstall.sh` (or `uninstall.ps1`
on Windows), then the new installer with the same prefix, bin directory, feed and
channel. The uninstaller keeps the data; the new command reuses it. The new
uninstaller also reads legacy receipts and removes only their owned PATH links.
Reinstalling never starts a host or registers a service.

For Homebrew, after the approved Cupola tap artifacts become available, remove
the old cask/formula before installing the new one. The packages conflict because
both own the same daemon and signed identity:

```sh
# macOS
brew uninstall --cask wonder
brew install --cask swaymun/tap/cupola
# Linux
brew uninstall wonder
brew install swaymun/tap/cupola
```

For the manually installed Mac CLI bundle, retain the old signed bundle as a
rollback copy, install the complete notarized release at
`/Applications/Cupola.app`, then run:

```sh
scripts/link-cli-macos.sh /Applications/Cupola.app "$HOME/.local/bin" "/Applications/Wonder CLI.app"
```

The optional final argument removes only a symlink to that exact old bundle's
CLI and replaces only a `cupola` symlink into it; pass
`"/Applications/Cupola CLI.app"` to move from that earlier name. A regular file or a link to another installation is preserved and fails the
operation. Retire the old bundle at its original location after retaining its
rollback copy; never rewrite its signature or change its bundle ID.

For each route, run `cupola service install` to register the new executable path,
then `cupola status --json`, `cupola devices --json` and `cupola doctor`.
Compare saved devices and existing chats with the backup. macOS permissions keep
the same signed bundle identity; verify the actual grants after installation.
To roll back, stop/unregister the new host, remove only its installer-owned files
and command, restore the old signed release and its service path, and keep the
same data. Do not leave both names on PATH or both services registered.

This is a local preparation and migration contract. It does not publish a tap,
create a downloadable release, install on this computer or change any store record.

## Updates and integrity

`cupola update --check --channel stable --json` checks a manifest;
`cupola update --channel nightly` selects the other channel. `--version X` selects
`versions/X.json`. All artifacts carry a SHA256 digest in the HTTPS manifest;
redirects must also use HTTPS. Loopback HTTP is allowed solely for local acceptance.
An optional Sigstore bundle may be published beside an archive; it is not currently
verified and must not be described as required provenance. HTTPS protects the
manifest; SHA256 alone does not authenticate a compromised feed.

Channel files have this shape (target keys: `linux-x86_64`, `linux-aarch64`,
`macos-aarch64`, `macos-x86_64`, `windows-x86_64`, `windows-aarch64`):

```json
{"version":"1.2.3","artifacts":{"linux-x86_64":{"url":"cupola-1.2.3-linux-x86_64.tar.gz","sha256":"<64 lowercase hex digits>"}}}
```

An idle running host restarts after verification; active agents require
confirmation (`--yes` supplies it; JSON/noninteractive use defers restart).
Computer sharing or an in-flight send defers the restart even with `--yes`.
Running background commands must finish before their agent can be paused;
`--yes` does not bypass that refusal. Let those commands finish, then retry.
Resumption and exactly-once queued delivery are separate live acceptance checks;
an installed download alone does not prove either. macOS swaps the complete signed
app and keeps one previous bundle; Windows selects a new version directory without
overwriting loaded executables. Both wait for `/readyz` after a running-host update
and restore the previous release if readiness fails. Windows keeps the new release
and the one it updated from (for rollback) and removes older version directories. Platform acceptance is recorded separately
in the unit report; source implementation is not live-turn or phone evidence.

Current live acceptance found a Codex continuation that reported missing earlier
context and did not finish its task, despite returning a continuation receipt.
Until that path is fixed, let active replies finish before updating. A successful
restart does not establish successful task continuation.

## Linux computer viewing

Computer view shares an **X11** screen; it does not capture a Wayland desktop and
says "Computer view needs an X11 session" there. Either set
`WONDER_COMPUTER_DISPLAY=:0` (the X11 desktop already running; set it before
`cupola service install` so the service keeps it), or let Cupola start its private
Xvfb display, which needs `sudo apt-get install xvfb openbox`. xdotool and xclip
add control and copy and paste; `cupola doctor` names only what the chosen setup lacks. Linux archives with a native helper use it by default,
without Python setup. Archives without a native SDK retain Python sharing (the Linux x86_64 preview archives are
of this kind): Debian and Ubuntu refuse `pip install` into the system Python, so
create the environment Cupola looks for,
`sudo apt-get install -y python3-venv && python3 -m venv ~/.local/share/cupola/computer-venv && ~/.local/share/cupola/computer-venv/bin/pip install -r ~/.local/share/cupola/versions/*/computer-linux/requirements.txt`
(`cupola doctor` prints the exact commands for your paths). Cupola uses
`<install>/computer-venv` automatically; `WONDER_COMPUTER_PYTHON` overrides it. The archive's
`computer-backends.json` and `cupola doctor` describe its available backend.

On Wayland, native desktop capture through Cua Driver depends on the desktop's
xdg-desktop-portal ScreenCast/RemoteDesktop implementation. The logged-in user may
have to select a monitor/window and approve each portal session; restore tokens
and input support depend on the compositor. An SSH session cannot silently grant
these permissions. Cupola's private Xvfb view is distinct from that portal path;
the installer does not promise unattended capture of a Wayland desktop.
See the upstream [ScreenCast session and restore-token contract](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.ScreenCast.html)
and [RemoteDesktop interface](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.RemoteDesktop.html).

## Build artifacts locally

Use a clean committed checkout on each native platform:
Build/packaging tools include Rust, npm and Python 3.11+ (Windows: `py -3`).

```sh
scripts/release-cli-linux.sh 1.2.3          # Linux VM
scripts/release-cli-macos.sh 1.2.3          # Mac; existing ASC upload config
```

Windows: `powershell -File scripts/release-cli-windows.ps1 -Version 1.2.3`.
Set `LK_CUSTOM_WEBRTC` and `WONDER_COMPUTER_BUILD` for Linux and Windows x64 to
include native screen sharing ([computer-helper/README.md](../crates/wonderd/computer-helper/README.md));
without them the archive keeps Python sharing as its default.
These scripts build native binaries, package locked production runtime dependencies,
retain the source commit and logs, and emit an archive, SHA256 file and platform
JSON fragment under `.local/release/`. They never publish or install artifacts.
The Mac script submits its archive privately to Apple's notarization service.
macOS notarization uses `WONDER_ASC_CONFIG` or
`~/.config/wonder/app-store-connect/upload.json` (`privateKeyPath`, `keyId`,
`issuerId`), like the TestFlight lane. An existing notarytool Keychain profile
may instead be supplied as the second argument. It creates no credentials and
never prints private key contents. Its signature,
staple and Gatekeeper checks must all pass before a fragment is emitted.

Copy each same-version fragment and archive into a local staging folder and run:

```sh
python3 scripts/prepare-cli-feed.py --channel stable --out .local/release/feed PATH/PLATFORM.json ...
```

The feed assembler rehashes every archive and rejects duplicate targets or mixed
versions. Prepare nightly separately. Keep releases private until the owner gate
is explicitly cleared. The `homebrew-tap` directory is the prepared tap layout;
its generator takes this exact verified manifest and an owner-approved HTTPS URL.

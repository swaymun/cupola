# Set up Cupola on this Mac (for AI agents and people)

Goal: install the Cupola app on this Mac, start it, and reach the pairing step,
where the **person** pairs their iPhone or iPad. If you are an AI agent (Codex,
Claude Code, …), do the steps in order, run each check, and stop at every
**Needs the person** line. Never approve a pairing yourself. Setup is finished
only when the final check in [step 6](#6-final-check) passes.

Setup needs two things: the Cupola app on this Mac and the Cupola app on the
phone. The phone pairs over the same Wi-Fi as the Mac. Tailscale (to reach the
Mac away from home) and the ChatGPT desktop app are optional.

On Linux (x86_64) instead of a Mac? Go to [Linux](#linux).

Conventions:

- Run every command with `--json` when you want to read the result. Output is
  JSON on stdout (`pair --json` prints one JSON object per line); prompts go to stderr.
- A step is done only when its **Check** passes. If it fails, read the `fix`
  text, apply it if it does not need the person, and re-run the check.

## 0. Check first

| Needs the person | Why |
| --- | --- |
| A Mac with Apple silicon on macOS 14 or later | Cupola for Mac is built for it. |
| The Cupola app on an iPhone or iPad with iOS/iPadOS 26 or later | They scan the pairing code in it. |
| The phone on the same Wi-Fi as this Mac while pairing | The phone connects to the Mac directly on the home network. |
| An agent they already use (Codex, Claude Code, …) signed in on this Mac | Cupola runs their agents; their plan and limits still apply. |

Optional, only if the person asks for it:

| Optional | What it adds |
| --- | --- |
| [Tailscale](https://tailscale.com/download) on this Mac and the phone, same Tailscale network, with MagicDNS and HTTPS certificates on in its admin console | The phone reaches this Mac when it is not on the same Wi-Fi. |
| The ChatGPT desktop app | Following its ChatGPT threads on the phone, and its own computer use. Without it, agents use [Cua Driver](https://cua.ai/docs/cua-driver/quickstart) to work in this Mac's apps. Codex works with the Codex CLI (npm or Homebrew) or with the ChatGPT desktop app. |
| Screen sharing (see [step 3](#3-check-prerequisites-with-doctor)) | Viewing and controlling this Mac's screen from the phone. It is off unless the person requests it. |

Ask the person these questions together, once, at the start, and keep checking
the Mac while you wait:

1. Is the Cupola app installed on your iPhone or iPad, and is it on iOS/iPadOS 26
   or later (Settings > General > About)?
2. Is the phone on the same Wi-Fi as this Mac?
3. Do you also want to reach this Mac away from home with Tailscale?

If they answer no to 1 or 2, tell them what is missing and continue with the
Mac; pairing in step 5 waits for it. Ask only for a missing download or a
system approval that prevents the next step.

Check this Mac:

```sh
uname -m                     # arm64
sw_vers -productVersion      # 14 or later
xcode-select -p              # a path
```

If `uname -m` is not `arm64` or macOS is older than 14, stop and tell the
person this Mac is not supported. If `xcode-select -p` fails, **Needs the
person:** run `xcode-select --install` and finish the Command Line Tools
installation; it provides the `python3` the checks below use. Do not run
`python3` before then: on a fresh Mac it only opens that installation prompt.

## 1. Install

First run `cupola version --json`. If it prints a version, this person already
has Cupola installed: skip to step 2. If `/Applications/Cupola.app` exists but
`cupola` is not found, skip to the PATH part below.

With [Homebrew](https://brew.sh) (`brew --version` works), the usual way:

```sh
brew install --cask swaymun/tap/cupola
```

Homebrew checks the download's SHA-256, puts `Cupola.app` in Applications and
`cupola` on `PATH`. Naming the cask in `install` is what lets Homebrew trust
and load it; no separate `brew trust` is needed (measured on Homebrew 7.0.12).
Only if it still refuses with "untrusted tap", run
`brew trust --cask swaymun/tap/cupola` and retry. A `depends_on macos:`
deprecation warning from the tap is harmless. If the cask is not found, or
there is no Homebrew, use the download below.

The download: `Cupola-<version>.zip` and `SHA256SUMS` from
[the latest release](https://github.com/swaymun/cupola/releases/latest). Do not
invent a download URL. In the folder holding both files, check the
SHA-256 before opening the archive; on a mismatch, stop and tell the person:

```sh
set -e
ZIP=$(ls Cupola-*.zip)
grep " $ZIP\$" SHA256SUMS | shasum -a 256 --check --strict
STAGE=$(mktemp -d)
ditto -x -k "$ZIP" "$STAGE"
APP="$STAGE/Cupola.app"
codesign --verify --deep --strict "$APP"
spctl --assess --type execute "$APP"
test ! -e /Applications/Cupola.app
ditto "$APP" /Applications/Cupola.app
```

`codesign` checks the app's signature is intact; `spctl` checks Apple
notarized it. If either fails, install nothing and tell the person. If
`/Applications/Cupola.app` already exists, stop and ask the person: it may be a
running installation, and upgrading it follows the archive's `INSTALL.txt`.
Keep the app at that path with that name: the login service and the `cupola`
command point at it.

Put `cupola` on `PATH` if it is not there yet (the Homebrew cask already did;
never replace an existing `cupola`):

```sh
mkdir -p ~/.local/bin
test -e ~/.local/bin/cupola || ln -s /Applications/Cupola.app/Contents/MacOS/cupola ~/.local/bin/cupola
export PATH="$HOME/.local/bin:$PATH"
```

Check: `cupola version --json` prints a version. If new Terminal windows do not
find `cupola`, add `export PATH="$HOME/.local/bin:$PATH"` to `~/.zprofile`.

## 2. Initialize

For an existing installation, first run `cupola doctor --json`. If the
`initialized` check is `ok`, preserve its configuration and continue at step 3.
Run `init` only for a new or uninitialized host: re-running it replaces the
name, ports and screen-sharing settings and fails if the host is running.

```sh
cupola init --json
```

`init` enables same-Wi-Fi connections by default, so the phone can connect to
this Mac directly. The connection uses TLS with this Mac's own key, which the
phone checks when it pairs. Check: `init` exits 0 and
`cupola lan status --json` prints `"enabled": true`.

Before starting, check whether the loopback port 3777 and the home network
port 3778 are already in use (`lsof -nP -iTCP -sTCP:LISTEN`). Never stop
another host. If necessary use other ports, for example
`cupola init --listen 127.0.0.1:13877 --json` and `cupola lan on --port 13878 --json`.

Add `--computer` to `init` only if the person wants to view and control this
Mac's screen from the phone (experimental; see step 3). Use `--name "<name>"`
for the name the phone shows.

Tailscale, only if the person wants it: `cupola init` also sets up this Mac's
Tailscale name for the phone to use away from home, once Tailscale is signed
in. Check it with `tailscale status --json` (or
`/Applications/Tailscale.app/Contents/MacOS/Tailscale status --json` when
`tailscale` is not on `PATH`): `BackendState` is `Running`,
`CurrentTailnet.MagicDNSEnabled` is `true` and `CertDomains` is not empty.
Otherwise **Needs the person:** sign in to Tailscale and turn on MagicDNS and
HTTPS certificates in the Tailscale admin console (DNS). Also check
`tailscale serve status` and never replace another Serve entry; if port 443
is taken, re-run `init` with `--https-port 8477` (before starting the host).

## 3. Check prerequisites with `doctor`

```sh
cupola doctor --json
```

Output: `{"ok": bool, "checks": [{"id", "status", "summary", "fix"}]}`.

- `status` is `ok`, `warning`, `failed` or `unavailable`. The command exits 1
  when any check is `failed`; `ok` is false then. `warning` does not block pairing
  by itself. Before starting, `running` and `service` warnings are expected;
  continue to step 4.
- `fix` is `null` when `status` is `ok`. Otherwise it is either a command to run
  yourself, or text starting with `Needs the person:`. Tell the person that
  text verbatim, wait for them, then re-run `doctor`.
- Without Tailscale, the `tailscale` check is a `warning` when the same-Wi-Fi
  connection is reachable. It concerns reaching the Mac away from home and
  does not block pairing on the same Wi-Fi. If it is `failed`, check the
  home-network connection in step 4 and re-run `doctor` once it is reachable.

Check ids:

| id | Meaning | Typical fix |
| --- | --- | --- |
| `initialized` | `cupola init` was run | `cupola init` |
| `daemon` | the host is inside Cupola.app | reinstall |
| `running` | the host is running | `cupola start` |
| `service` | the host starts at login | `cupola service install` |
| `home-network`, `firewall` | the phone can connect on the same Wi-Fi | follow `fix` |
| `tailscale` | optional: signed in and allowed to publish | Needs the person: sign in, enable MagicDNS and HTTPS certificates |
| `computer` | screen sharing (only with `--computer`) | Needs the person: grant permissions in System Settings |
| `screenRecording`, `accessibility` | screen sharing (only with `--computer`) | Needs the person: grant the named permission in System Settings |
| `cua-driver` | lets agents use apps on this Mac (only with `--computer`) | run the install command in `fix` |
| `providers` | at least one agent provider (Codex, Claude, …) is installed | `cupola providers` shows install steps |

Only if screen sharing was requested: `cupola permissions status --json`
lists `screenRecording` and `accessibility`. Needs the person: they run
`cupola permissions grant` (or you run it for them while they watch) and enable
Cupola in System Settings > Privacy & Security.

## 4. Start the host

```sh
cupola service install --json   # Start now and at every login; verify running below.
cupola status --json
```

If the person does not want Cupola to start at login, use `cupola start --json`
instead; the `service` warning then describes automatic startup and does not
block pairing. Service registration can finish before startup. If the
`running` doctor check is not `ok`, run `cupola start --json`, then recheck.
Allow up to a minute; if it is still stopped, inspect `cupola logs --json` and
resolve the error.

macOS may ask once whether Cupola may accept incoming network connections.
**Needs the person:** choose Allow, or the phone cannot connect on the Wi-Fi.

Check: `cupola doctor --json` has `running` = `ok`, and `cupola lan status --json`
has `"state": "ready"` with at least one address in `origins`. If `state` is
`unavailable`, follow its `detail` (for example, the Mac is not on a Wi-Fi or
Ethernet network). With Tailscale, the first start can take up to a minute to
issue the HTTPS certificate.

Cupola keeps its data in `~/.cupola`; `CUPOLA_DATA_DIR` chooses another
directory. A host set up by an earlier release keeps its data in `~/.wonder`
until it next starts with no host running from it. That start moves the
directory to `~/.cupola`, leaves `~/.wonder` as a link to it and updates the
login service, so paired phones, Projects and the host's identity stay as they
were. If both directories hold data, the host does not start and changes
neither; keep the one in use and move the other aside. The older `WONDER_*`
setting names still work.

## 5. Pair (the person approves)

Start only once step 4's check passes and the person has confirmed the Cupola
app on their phone and that the phone is on the same Wi-Fi. Run plain `pair`
**without `--yes`**, so the person confirms on both devices. It falls back to
the same Wi-Fi when Tailscale is unavailable. A phone paired this way also uses Tailscale away from
home when it is set up. Ask the person whether they want the QR code shown in
the chat (see the trade-off below). If they have not answered yet, omit `--qr`
and reach the pairing code first; ask the QR question alongside it. Keep this
command running in the background, with stdout and stderr in private files,
and read its output:

```sh
cupola pair --json --timeout 300 --qr "$HOME/cupola-pair.png"
```

`--qr` is optional; a path ending in `.svg` writes an SVG, anything else a PNG.
Choose a new private path; do not use a file that already exists. Copy this
launcher into a shell call. It starts a separate session with closed stdin and
private logs, so it survives the agent's shell call ending:

```sh
python3 - <<'PY'
import json, pathlib, subprocess, tempfile
directory = pathlib.Path(tempfile.mkdtemp(prefix="cupola-pair-"))
command = ["cupola", "pair", "--json", "--timeout", "300"]
# Only after QR chat consent: command += ["--qr", str(directory / "qr.png")]
with (directory / "events.jsonl").open("xb") as output, (directory / "stderr").open("xb") as errors:
    process = subprocess.Popen(command, stdin=subprocess.DEVNULL,
                               stdout=output, stderr=errors, start_new_session=True)
print(json.dumps({"pid": process.pid, "eventLog": str(directory / "events.jsonl"),
                  "errorLog": str(directory / "stderr")}))
PY
```

The first time, iOS asks the person whether Cupola may find devices on local
networks. **Needs the person:** choose Allow; the phone cannot reach this Mac
without it. After `connected`, the phone offers Add Project: they pick New
folder to start.

Read the returned `eventLog` in a later shell call. Check for an `offer` event
and that the PID is still running (`kill -0 <pid>`). If it exits without an
offer, read both `eventLog` and `errorLog` and fix the cause. With `--json`, a
CLI failure is an `{"error":{"code","message"}}` object in `eventLog`;
usage errors are in `errorLog`. `nohup ... &` alone can die with an
agent's shell session; use the launcher above.

Closed stdin is **not** a denial. The command waits for the explicit
`--approve`/`--deny` step below. Do not pipe a guessed answer or add `--yes`.

Events, one JSON object per line:

| `event` | Fields | What you do |
| --- | --- | --- |
| `offer` | `offerId`, `url`, `origin`, `code`, `expiresAtMs`, `qrPath` (only with `--qr`), `hostKeyPin` | Show the person the QR image at `qrPath` (or give them the `url`). Tell them: open Cupola > Add computer and scan it (or paste the link). The offer expires at `expiresAtMs` (5 minutes). |
| `verification` | `deviceId`, `label`, `hostInstallationId` | The phone has claimed the offer. Ask the person: "Please read me the verification text shown on your iPhone." The host does not reveal its comparison text to the agent. |
| `connected` | `deviceId`, `label` | Done. |
| `rejected` | `deviceId`, `label` | The phone was refused. Offer to run `pair` again. |
| `cancelled` | `offerId` | The offer was withdrawn. |

When the person reads out the text, pass their
answer as a quoted argument (substitute the `deviceId` from `verification`):

```sh
cupola pair --approve <deviceId> --code '<text read from phone>' --json  # emits "connected"
```

`--code` is required, and the daemon checks it before approving, including
direct RPC/HTTP confirmations. A missing or mismatched code never approves the
phone. JSON output, pending-device responses, logs, status files and RPC events
contain neither the text nor the challenge needed to reconstruct it. A yes
without the phone text is insufficient: ask them to read it from their phone.

Only a non-JSON `cupola pair` with both input and output attached to a terminal
accepts a typed answer. Enter the verification text read from the phone (a blank
line or `no` rejects it). The terminal displays the matched text only after the
daemon has accepted that answer. It never reveals the expected text in advance,
and a typed `yes` cannot approve a phone. Redirected output and JSON mode always
wait for the explicit `--approve --code` / `--deny` command instead.

The signed challenge stays private in host process memory for signature checks;
it is never returned by host-side pending APIs or persisted in setup state.
This is an output/API boundary, not protection against a compromised host that
can inspect or modify the daemon's memory or executable.

On their no, or any doubt (the text differs, they did not start pairing):

```sh
cupola pair --deny <deviceId> --json        # emits "rejected"
```

After approval the waiting `pair` finishes. After denial it may keep waiting;
withdraw the offer with `cupola pair --cancel <offerId> --json` and stop its
process if still running. If it already ended, the offer is gone and
`--approve` fails with "no phone is waiting"; run `pair` again. The QR file is
deleted when a phone claims it, when `--cancel` succeeds, or when the waiting
command exits (including timeout). To withdraw early:
`cupola pair --cancel <offerId> --json`. An external cancellation can leave the
waiting process alive until its timeout; stop it too and remove its private logs.
The QR is removed by the cancellation command without waiting for that process.

Trade-off for the QR: it holds a single-use secret that expires in 5 minutes.
Showing it in an AI chat sends it to that AI provider, which may retain the
chat; the secret itself is usable only during that window. The verification
step reduces the risk of another phone claiming it: approve only after the
person reads out the text on their own phone and it matches the host's text. Without
`--qr`, give the person the `url` from the `offer` event: in Cupola > Add
computer they can paste it as a pairing link.

Never use `--yes` for an agent-run pairing: it approves whatever phone claims
the code first.

## 6. Final check

After `connected`, check the whole setup once:

```sh
python3 - <<'PY'
import json, subprocess
def run(*args):
    result = subprocess.run(["cupola", *args, "--json"], capture_output=True, text=True)
    try:
        return json.loads(result.stdout)
    except ValueError:
        return {}
doctor, status, lan, devices = run("doctor"), run("status"), run("lan", "status"), run("devices")
host = status.get("host") or {}
checks = {
    "hostRunning": status.get("supervisorPid") is not None and bool(host),
    "sameWiFi": lan.get("state") == "ready" and bool(lan.get("origins")),
    "phonePaired": any(d.get("revokedAt") is None for d in devices.get("devices") or []),
}
problems = [f"{c['id']}: {c['summary']} -> {c['fix']}" for c in doctor.get("checks", [])
            if c.get("status") in ("failed", "warning")]
print(json.dumps({"ok": all(checks.values()), **checks, "doctorProblems": problems}, indent=2))
PY
```

Check: `ok` is `true`. `doctorProblems` may still list warnings (for example
`service` when you used `cupola start`) and, without Tailscale, the `tailscale`
check; tell the person what each one means, but they do not block the phone on
the same Wi-Fi. If the person wanted Tailscale, the `tailscale` check must be
`ok` too. If a check is `false`, fix it as in steps 3 and 4 and run this again.
Then tell the person setup is done and ask them to open the Mac in Cupola on
their phone. Do not report success before this check passes.

## Linux

One command installs a verified Cupola host for your user (no sudo, no service
changes, no sign-in):

```sh
curl -fsSL https://getcupola.app/install.sh | sh
```

If getcupola.app is unreachable, use the same script attached to the release:
`curl -fsSL https://github.com/swaymun/cupola/releases/latest/download/install.sh | sh`.

AI agents: check the installer before running it. Download it, compare its
SHA-256 with this guide's pin, and stop on a mismatch:

```sh
INSTALL_SH_SHA256=728948281dcd053ce746a76e1535856cd880e4147d87b8376a98f17641c7bdee
curl -fsSL https://getcupola.app/install.sh -o install.sh
echo "$INSTALL_SH_SHA256  install.sh" | sha256sum --check --strict
sh install.sh
```

The installer fetches the newest [GitHub release](https://github.com/swaymun/cupola/releases/latest),
checks the archive's SHA-256 and that it runs before changing anything, unpacks
it under `~/.local/share/cupola` and links `cupola` and `wonderd` into
`~/.local/bin`. To pin a release, add
`--feed https://github.com/swaymun/cupola/releases/download/vX.Y.Z` (pass it as
`sh install.sh --feed …`, or `sh -s -- --feed …` in the one-liner).

Needs, on a stock Ubuntu or Debian install:

| Needs | Install |
| --- | --- |
| x86_64 Linux (aarch64 has no verified artifact yet) with glibc 2.35 or newer (Ubuntu 22.04+, Debian 12+) | The installer checks and stops before installing anything. |
| `curl` and `ca-certificates` (not in Docker's base images) | `sudo apt-get install -y curl ca-certificates` |
| Python 3.10 or newer, to run the installer | `sudo apt-get install -y python3` |
| Node.js 22 or newer, only for Claude and OpenCode (Codex does not need it); the installer prints a note if it is missing | See [nodejs.org](https://nodejs.org/en/download) |

Use a dedicated non-root account. Then, in a new shell (or after
`export PATH="$HOME/.local/bin:$PATH"`):

```sh
cupola version --json
cupola init
cupola service install   # starts the host now and at boot
cupola doctor --json
cupola pair
```

The host takes up to about 30 seconds to start listening: `cupola doctor` and
`cupola pair` say it is not running until `cupola status --json` shows a
`host`. Without Tailscale, same-Wi-Fi pairing still works. Use `cupola start`
only to run the host until it is stopped or you log out.

**Needs the person:** read the verification text on the phone and approve the
pairing, as in [step 5](#5-pair-the-person-approves). Never approve it yourself.
`cupola stop` stops the host; `~/.local/share/cupola/uninstall.sh` removes
Cupola and keeps conversations, pairing and sign-ins.

## When something fails

| Symptom | Do |
| --- | --- |
| `cupola` not found | Link it onto `PATH` as in step 1. |
| `pair` says home network connections are off | Run `cupola lan on --json`, then `cupola service restart --json` (or `cupola stop` and `cupola start`). |
| The phone cannot connect on the Wi-Fi | Check both are on the same network and that macOS allowed Cupola's incoming connections (the `firewall` doctor check). |
| Offer expired | Run `pair` again; it makes a new code and QR. |
| Phone shows a different verification text | `--deny`, then start again; do not approve. |

If the problem is not in this table, run `cupola triage`: it writes one redacted
file of facts about this computer and prints how to give it to a coding agent
with the public playbook ([TRIAGE.md](TRIAGE.md)) to diagnose it.

Treat pairing URLs, codes, QR images and their logs as temporary secrets. Do not
commit or publish them. The CLI removes the QR on normal exit; remove your
private log files after the offer ends. If the CLI is forcibly killed, delete
its QR too. Never claim a phone connected just because an offer was printed.

More: [INSTALL.md](INSTALL.md) for the Mac app, [crates/wonderd/RPC.md](crates/wonderd/RPC.md) for the JSON-RPC interface.

# Synthetic Project thread fixtures

`generate-session-fixtures.py` measures local session **shapes**, then replaces
all content with deterministic todo-project text. It never imports a real thread,
writes a database, starts a runtime, sends a message, executes a tool, or accesses
a model. This is the offline fixture producer; it does not install fixtures into
the app or implement the live nightly runner.

Use Python with `jsonschema` installed and Node (on the owner Mac, `python3.12`):

```sh
python3.12 scripts/test-session-fixtures.py
python3.12 scripts/generate-session-fixtures.py
```

The default writes `.local/session-fixtures/codex.json` and `claude.json`, each
with 320 turns. `--turns 600 --seed 47` creates a longer repeatable history.
`--chunk-chars 0` disables modeled text chunking. Identical profile, options and
runtime projection source produce identical output. Generated bundles stay
private under `.local/`; commit the compact shape profile, not large replay JSON.

To remeasure this Mac's logs:

```sh
python3.12 scripts/generate-session-fixtures.py --sample
```

Sampling reads `~/.codex/sessions` and `~/.claude/projects`; override with
`--codex-root` and `--claude-root` for a wholly synthetic source. It selects up to
24 files per provider: recent files, large files, and a spread of sorted paths.
Each read stops at 4 MiB; the partial final record and final incomplete turn are
discarded. The profile retains up to 48 complete turn templates, each capped at
96 items. Text/result lengths cap at 65,536 characters and individual gaps/call
durations at one hour. `--sessions` (3–60) and `--bytes-per-session` (1 KiB–8 MiB)
change the sampling bounds. These are bounded convenience samples, **not** an
unbiased population measurement. Active logs may change between samples.

The checked-in profile records 23 Codex and 22 Claude sampled files. No token
stream records were observed. It contains only fixed item-kind/phase enums,
integer counts and measurements, and a result-error flag. Each shape row is:

```text
[kind, textOrArgumentChars, resultChars, gapFromPreviousItemMs,
 callDurationMs, resultFailed, phase]
```

`resultFailed: null` means no paired result was present in the bounded source;
the generated tool is a settled failure, never a fabricated observed success.
Late results are paired with their original call even after a newer user turn.
Unknown tool names map to the reviewed generic tool category; no source name,
argument, text, session ID, timestamp, directory or filename enters the profile.
Reasoning contributes an empty Codex marker; private reasoning is never copied.
Claude thinking is not sampled yet, although the runtime now projects each
finished thinking block as a reasoning row like Codex's.

## Integration boundary

Each bundle contains:

- `threadRead`: the actual `thread/read` result shape, with hundreds of turns.
  Codex uses the checked-in `0.162.0-alpha.17.2` experimental schema. Claude uses
  the existing `Sessions.describe` pure projection and its `source: "sdk"`
  envelope; Codex-only required fields are not invented for Claude.
- `replay`: `{atMs, direction, message}` records in timestamp order. `message`
  is the normal App Server notification, server request, or client response.
  Feed server records into the existing runtime event boundary and client
  records into the pending-request response path. No retired conversation
  adapters or database fixture rows are produced.
- `project`: synthetic Project metadata. `/fixture/todo` is a placeholder;
  the Diagnostics adapter must bind it to its owned workspace.
- `assets`: generated PNG/text bytes with MIME type, size and SHA-256. Decode
  and verify them through the ordinary attachment admission path before display;
  localImage/mention inputs alone do not establish daemon-authenticated previews.
- `provenance`: sampled shape, authored coverage and modeled timing distinctions.

`project-claude-fixtures.mjs` constructs only synthetic SDK blocks and passes
them through the real `TurnProjection`. It does not construct `Sessions`, call
`Sessions.create`, import the SDK, or start a bridge. The subagent activity shape
matches `Bridge.child`; its referenced child is an inert identifier, with no
child transcript or session created. The replay host below supplies authored
child details if a scenario opens them. Codex subagent calls likewise contain a
synthetic child reference. History paging should slice these existing turns in
the daemon's normal history adapter; this script does not invent a second paging protocol.

The sampled item starts and paired tool durations retain their relative timing,
including overlapping calls. Text records in these logs are completed messages,
not token traces. Modeled chunks fit inside the preceding observed gap, at most
35 ms apart; a zero gap yields simultaneous chunks. `--chunk-chars 0` retains
whole-message delivery. This data cannot establish actual token latency. The
future live fixture runner is responsible for that measurement.

Every 32nd turn adds **authored** rare coverage: plans, commands, edits, tools,
subagents, a long Markdown/code/table reply, image/file inputs, and approvals.
Failed/interrupted turns and Codex retry notifications are authored, too. These
additions are identified in provenance and are not claimed as sampled frequencies.
The fixture covers the runtime-supported types for each provider; for example,
Claude exposes ordinary non-shell tools as MCP items, not Codex web-search items.

## Verification and privacy

Before publishing either bundle, the generator checks both bundles in full:

- Codex history and notification payloads against the owning pinned JSON schema.
  That schema does not define server approval parameters; those are checked for
  active-item ownership and matched responses, and use the existing supported
  command/file approval shapes. This is not an independent full approval schema.
- Stable item IDs, ordered time, paired approvals, settled turn/item lifecycles,
  exact streaming-text reconstruction and identical replay/history items.
- Sensitive values and output filenames: credentials, tokens, API keys, email,
  phone numbers, home paths, IP/host names and Tailscale addresses. Asset encodings
  and hashes are exempt only when they match the exact generated fixture assets.
  The scanner is a pattern guard; the stronger privacy boundary is that sampling
  retains only reviewed enums and bounded numbers, and generation synthesizes all
  strings. Tests use entirely fictional sensitive sentinels.

The default output is not an app resource and cannot enter a Release binary.
A future Diagnostics integration must maintain the existing Release exclusion
guards. No simulator, paired-host or real-model behavior is established here.

## Replay through the runtime transport

`scripts/replay-session-fixture.py` replaces the retired database importer. It
accepts only a privacy-scanned, schema-validated generated bundle. It never reads
raw provider sessions, writes Cupola rows, executes recorded commands, starts a
model, or touches the owner's provider home. Each preparation requires a new
private directory under `.local/`; a prepared launcher can be reused. Preparation
runs the full schema/privacy/lifecycle checks and records the exact bundle SHA-256.
Runtime startup verifies that digest before parsing. Repeating the full schema
walk during startup exceeded the provider handshake deadline on large Claude
fixtures; the digest keeps modified bytes from entering a previously validated run.

### Paired loopback latency benchmark

Use the existing isolated HTTP runner with `--benchmark-samples 20` and fresh
`--bundles`. It reports nearest-rank p50/p95, every raw sample, one separate cold
attachment/hydration sample, and the daemon SHA-256. Install `websocket-client`
alongside the runner's existing `jsonschema` and `cryptography` dependencies.

```sh
python3.12 scripts/test-session-replay-http.py \
  --daemon .local/build/release/wonderd --bundles .local/session-fixtures \
  --output .local/latency/before --benchmark-samples 20
```

Each run owns a fresh database, paired identity, provider homes and synthetic
runtime launchers. It measures warm Chats catalog, Project threads, thread
detail/history, older history pages, Send receipt/first streamed fixture text,
and authenticated reconnect to an actual replayed turn. The receiver runs while
the HTTP send is in flight, so a slow receipt does not hide an earlier delta.
Reconnect must deliver the known turn from its pre-send checkpoint; a socket
upgrade alone is not success. Samples are bounded to 1–50 per provider and the
fixture must reserve that many turns. No models or real sessions are used.

The synthetic runtime writes only its turn-admission and first-text timestamps
under each prepared provider's private `runtime-timing/` directory. All processes use the same
machine's monotonic clock. These boundaries separate client-to-runtime admission,
synthetic replay, and runtime-through-daemon-to-client return. They do not isolate
SQLite, network or individual daemon functions; production gains need profiling
of the measured slow phase. The first-text number includes accelerated replay and
any fixture approval wait, so it is **not model inference latency**. Use the same
fixture bytes, runtime speed, binary configuration and sample count before/after.

This is a Python loopback client. iOS rendering, TLS and Tailscale are unmeasured;
there is no relay in this path. Use the existing Diagnostics recorder and
`analyze-ios-diagnostics.py` for phone measurements, retaining their stated proxy
limits. No instrumentation is added to the shipped daemon or app.

### Native Claude history projection

To profile the production Claude bridge without a model or provider account:

```sh
mkdir -p .local/claude-history
node scripts/benchmark-claude-history.mjs > .local/claude-history/report.json
```

Install the existing Claude runtime dependencies first (`npm ci` in
`services/claude-runtime`). An optional first argument selects a different
checkout, so the same benchmark can compare two revisions. It creates a fresh
temporary native transcript and SDK read fixture, then deletes them on exit.
The fixed workload has 255 turns, 20 command calls per turn and 5610 projected items;
its content is entirely synthetic. Twenty complete walks report raw samples
and nearest-rank p50/p95 for projection, turn pages and item pages. The first
walk includes cold transcript preparation; later walks reuse unchanged history.
For CPU attribution, add Node's `--cpu-prof` and a private `--cpu-prof-dir`.
Compare latency runs with the same profiling flags, since profiling adds cost.

This isolates the real bridge's projection and cursor validation. SDK transcript
reading is supplied by the fixture; network, daemon, iOS rendering and model
latency are excluded. The script is not bundled with the Mac runtime.

### Prepare a replay runtime

```sh
python3.12 scripts/replay-session-fixture.py \
  --bundle .local/session-fixtures/codex.json \
  --output .local/session-replay/codex-original --history-turns 256
```

The printed launcher speaks JSONL over stdin/stdout. Use it as a test
`LaunchConfig.codex_bin` for the Project runtime. For
Claude, prepare `claude.json` the same way, then use Python as
`BridgeLaunchConfig.node_bin` and the launcher as its entrypoint. This is a
**synthetic runtime**, including its version/schema and code-mode test handshake;
it establishes no installed-provider availability or model behavior. It uses the
existing test code-mode helper and checked-in pinned schemas without weakening
Cupola's runtime verification.

The host serves the real `thread/list`, `thread/read`, `thread/resume`,
`thread/turns/list`, and `thread/items/list` methods. Codex paging supports
ascending and descending pages, `notLoaded` turns, item anchors, and reverse cursors. Codex
cursors are scoped to their thread/turn/filter and kept in a bounded process-local
cache. Claude mirrors its bridge: newest turns first, items ordered within each
turn, opaque cursors invalidated by changed content, and no reverse/item anchors.
Unknown threads, unsupported operations and arbitrary sends fail closed.

`--history-turns 256` exposes the first 256 turns and reserves the remainder for
replay. Omit it for a read-only history with all turns. Each `turn/start` must
use the `threadId` written in `config.json` and exactly this fixture-only input:

```json
[{"type":"text","text":"Replay the next recorded turn."}]
```

The host maps authored thread/session placeholders to deterministic UUIDs and
Claude runtime IDs to `claude-<uuid>`, preserving the separate native session ID.
Project HTTP attachment validates those actual provider identity shapes. A sent
`clientUserMessageId` is retained on the replayed user item for receipt correlation.

A start returns the next recorded turn ID and sends its events through the usual
notification/server-request transport. Its relative event offsets are unchanged
at the default `--speed 1`; inter-turn human idle time is excluded because the
caller explicitly starts each turn. Completed-message chunk timing remains
modeled as described above. Approval records are sent to the client and the host
waits for its actual matching response. It never answers for the client. A
missing answer times out after 30 seconds; a different decision or exact-turn
interrupt settles the replay as interrupted, never as the recorded success.
Delays from late approval answers extend subsequent delivery. No output command
or patch is executed.

An authored child transcript supplies the generator's `fixture-child` reference
with an exact parent/cwd and native spawn metadata. Claude additionally exposes
its actual bridge `thread/backgroundTasks/list` and `thread/backgroundTask/read`
shapes. This small child is authored coverage, not a measured child transcript.
PNG/text assets are decoded to the prepared workspace only after the producer's
MIME/byte-size/SHA-256 checks. Exact `/fixture/todo` paths are rebound to that
workspace. The host does not invent `wonderArtifact` records: daemon-authenticated
previews use normal attachment admission in the HTTP probe below; the replay
process itself does not create daemon file records.

For a bounded transport/inbox check, prepare **separate accelerated fixtures**:

```sh
python3.12 scripts/test-replay-session-fixture.py
python3.12 scripts/replay-session-fixture.py \
  --bundle .local/session-fixtures/codex.json \
  --output .local/session-replay/codex-check --history-turns 256 --speed 10000
CARGO_TARGET_DIR=.local/build/rust cargo run -p wonderd --example session_replay -- \
  "$PWD/.local/session-replay/codex-check/replay-runtime" codex "$(command -v python3.12)"
# Repeat preparation/probe with claude.json, a fresh output and provider claude.
```

The Rust example uses the shipped `AppServerClient`, runtime handshake and
`wonderd::ingestion::notification_sink`, then reads and acknowledges the real
SQLite inbox in a temporary test database. It checks hundreds of turns and every
item page, runtime provenance, client approval responses, exact reconstruction
of streamed text, child ownership, and final history equality. Its report labels
acceleration and zero model calls. It does **not** prove Project HTTP projection,
authenticated media previews, app rendering, real token latency or live SDK
execution. The host and example are development tools, not app resources or
Release launch paths. The default replay keeps original offsets; accelerated
results are never a latency baseline.

## Isolated Project HTTP acceptance

`test-session-replay-http.py` uses the real daemon executable and normal provider
catalog, Project creation/attachment, history projector, dispatch worker,
approval signatures and attachment storage. It requires Python `jsonschema` and
`cryptography`; it has no new production dependencies. Build the Project-only
daemon first, then run:

```sh
CARGO_TARGET_DIR=.local/build/rust cargo build -p wonderd --bin wonderd
python3.12 scripts/test-session-replay-http.py \
  --daemon .local/build/rust/debug/wonderd \
  --bundles .local/session-fixtures \
  --output .local/session-replay/http-check
```

The output must be new and beneath `.local/`. The runner prepares both synthetic
providers with 255 history turns at **10,000x**, starts wonderd on an unused
loopback port, and supplies fresh data/log/provider directories. It creates an
owned pairing offer, confirms its generated device locally and signs the real
session challenge. All subsequent Project requests use that session; consequential
approval responses use its P-256 key. It does not insert fixture database rows.
The HTTPS origin/Host pair models the local reverse-proxy boundary; the probe
connects over loopback HTTP and does **not** verify Tailscale or TLS.

For each provider it checks all 255 turns and item IDs through bounded HTTP
history pages, exact assistant text, a completed recorded send, one signed
approval followed by Stop on the next coverage turn, and the owned child
transcript. The image and text assets go through authenticated upload and
retrieval with exact MIME/size/SHA-256 checks; an unauthenticated request must be
rejected. These are **download/admission checks**, not native preview screenshots.
No raw provider sessions, real chats or model calls are used. Each daemon and its
owned process group stop even when a check fails. Private databases and logs
remain in the output for inspection; ephemeral keys and session tokens are not
printed or written to the report.

A subsequent check can reuse the validated provider launchers while keeping a
fresh daemon database:

```sh
python3.12 scripts/test-session-replay-http.py \
  --daemon .local/build/rust/debug/wonderd \
  --prepared .local/session-replay/http-check \
  --output .local/session-replay/http-recheck
```

The report includes the daemon binary SHA-256 and per-provider counts. It does
not establish WebSocket delivery, native UI rendering/preview presentation,
real-model streaming, real token latency, or live SDK execution. Those remain
separate acceptance surfaces. The transport/inbox probe above verifies recorded
deltas; the HTTP check verifies their downstream history and delivery state.

For native simulator checks, the same runner can hold a host at original pacing:

```sh
python3.12 scripts/test-session-replay-http.py \
  --daemon .local/build/rust/debug/wonderd \
  --bundles .local/session-fixtures \
  --output .local/session-replay/native \
  --serve-native "$WONDER_REPLAY_ORIGIN" --port 4415 --serve-seconds 1800
```

Set the origin to an unused HTTPS port on this Mac's Tailscale name. Inspect
existing Serve entries first, then point only that unused port at the loopback
address in `ready.json`. The runner never changes Tailscale configuration. Pair
Cupola normally using the private `pairing-offer.json`; compare the
phone's verification text before confirming its exact pending device through
the host's normal owner endpoint. The offer and `control.json` contain private
credentials: keep them under `.local/` and never paste them into logs or a PR.
Expired offers can be renewed through the same pairing endpoint.

The host creates both Projects and their image/text assets, then waits for the
bounded serving interval. SIGINT/SIGTERM also stops its owned daemon group and
retains the report. Remove only the Tailscale entry created for this run when
finished. A `served` report means the host stayed available, not that a native
test passed. Record native screenshots, streaming, paging and approval results
separately. Reusing `--prepared` in this mode requires speed 1. No live model is
called by either serving mode.

## Daemon resource soak

`soak-daemon.py` runs a release `wonderd` on fresh private data with the replay
runtimes and repeats one cycle: reopen both Project threads (catalog, detail,
two history pages, child transcript), stream one recorded turn per provider with
its signed approvals, reconnect the event socket five times from just before the
newest event (each connection must authenticate and replay that event),
pair/revoke/forget a second device by link, and start and end a computer session
against a fake capture helper, which must launch and answer `capture.prepare`
(the session reaches `awaitingSource`; a failed helper errors the soak). No
model, provider home or owner data is used.
On macOS the fixture also answers the daemon's dialog watcher with an empty
snapshot; those launches are separate from capture preparation receipts.

```sh
python3 scripts/generate-session-fixtures.py --turns 120 --output .local/soak-bundles
python3 scripts/soak-daemon.py --daemon target/release/wonderd \
  --bundles .local/soak-bundles --output .local/soak/$(date +%s)
```

The default 3 warm-up and 40 measured cycles take about 2.5 minutes in a 4-CPU
Linux container; each cycle consumes one reserved turn per provider, so `--cycles` must
not exceed the bundle's turns minus `--history-turns` (60) and warm-up. After
warm-up it compares the last sample with the first: RSS growth and second-half
RSS slope, open files, threads, child processes (every descendant of the daemon
or of a member of its process group, including children that start their own
session, zombies included; on Linux the soak is a child subreaper, so orphans
whose parent exited still count; on macOS the private per-run `TMPDIR` identifies
escaped processes even after their parents exit), logical database growth per
streamed turn, log
growth (the log directory plus the captured stdout/stderr in `daemon.log`),
devices still paired and unended
computer sessions. Each step must also have completed once per measured cycle
(`completed:*`), so a step that silently did nothing fails instead of passing.
The compared macOS endpoints wait for idle work and dialog watcher shutdown
within ten seconds. All descendants still count; a normal transient watcher
cannot fail a clean run or hide one leaked child behind an elevated baseline.
At the end the soak sends SIGTERM to the daemon alone: any of its descendants
still running after it exits fails `orphansAfterShutdown`, and the soak kills
them and errors if one survives. The daemon must exit with code zero; a crash,
shutdown error or forced SIGKILL fails `daemonExitCode`. Interrupting an active
daemon run with SIGTERM or Ctrl-C also writes its error report and cleans up its owned
processes before exiting unsuccessfully. `--self-test` covers the counter and this
cleanup with leaky and tidy stand-in daemons.
It also covers early parent exit, failed or missing helper preparation, silent
or incorrect event replay, skipped work and excessive captured output.
Exit 1 names each failed check; `--budget NAME=VALUE` overrides one budget.
`report.json` keeps every sample with per-table row counts and `dbstat` bytes.
Budgets were calibrated on Linux (glibc, two malloc arenas, see `wonderd`'s
`main.rs`). Keep the budgets fixed when verifying a regression. The database
budget assumes these seeded fixtures; report changes to fixture size or seed
alongside the results.

## Bounded real-runtime checks

`run-live-fixtures.py` uses the same paired-device HTTP/signature helper as the
replay probe and the normal authenticated WebSocket. It creates its own daemon
state, a generated todo Git repository and a fresh copy of the reviewed public
Cupola repository pinned to `98eae5337d82157081dedab322f5091f856a8564`.
The todo commit is deterministic and resets before each scenario. No provider
session log text is used. Browsing files come only from the reviewed public repo.

Use an isolated Python environment for the development-only dependencies, then
run the offline guards and read-only preflight. The nightly setup creates the
same environment at `.local/nightly/venv` in the main checkout.

```sh
python3.12 -m venv .local/live-fixtures-venv
.local/live-fixtures-venv/bin/python -m pip install -r scripts/live-fixtures-requirements.txt
.local/live-fixtures-venv/bin/python scripts/run-live-fixtures.py --check-dependencies
.local/live-fixtures-venv/bin/python scripts/test-live-fixtures.py
.local/live-fixtures-venv/bin/python scripts/run-live-fixtures.py \
  --daemon /Applications/Cupola.app/Contents/MacOS/wonderd \
  --codex /Applications/ChatGPT.app/Contents/Resources/codex-cli/bin/codex \
  --public-checkout .local/public-source/public-checkout \
  --budget .local/live-fixtures/budget.json \
  --output .local/live-fixtures/preflight-1
```

Model work additionally requires `--execute` and prior owner authorization.
`--provider codex|claude|all` selects the measured providers independently;
`--scenario edit|stop|all` selects the short task (default `edit`). The edit asks
for a due-date field and test; the stop task requests a short sleep and interrupts
the active turn. The edit checks streamed reply deltas over the authenticated
WebSocket, runtime tool/diff records, signed approval resolution and actual Git
changes, never exact generated text. Stop must produce an interrupted turn.
For Codex, a test-only stdout observer records file-change events from the App
Server process owning the turn. It forwards requests and responses unchanged;
no second process resumes or runs the thread. A separate App Server read cannot
see all pending items. The runner matches the approval's thread, turn and item
IDs and checks every source and rename destination against the two toy files
before sending the signed approval through Cupola. Display diffs alone are
insufficient because they omit rename destinations. An event not yet recorded
remains unapproved within the existing 90-second deadline. Records stay private
under `native-file-changes/`, capped at 64 items and 1 MiB per native frame.
Unknown, broad, command or network approvals fail closed. Claude's native
approval verification is not implemented; a live edit that needs it reports an
infrastructure failure. Do not substitute another Claude model.
Recorded replay remains the deterministic owner for specific approval forms.

The budget file is mandatory, locked and atomically saved. It defaults to
**three attempted sends across all invocations that share it**. When four turns
are explicitly authorized, `--turn-cap 4` extends the same ledger while retaining
all attempts; it never allows a fifth. Attempts count before submission, so an
uncertain HTTP response or rerun cannot refill it. Reuse that file for the entire
nightly run: for example `edit` on both providers and `stop` on one provider.
Do not delete/recreate it to bypass the cap or raise it without authorization. `all` needs two remaining slots per
selected provider. All live threads reserve metadata first and save exact IDs
for interruption/archive cleanup; no existing thread is sent to or archived.
Archive uses the normal Project API: Codex archives its provider session; Claude
archives only the Cupola conversation and leaves its native transcript intact.
`cleanupOutcomes.archiveScope` distinguishes those cases. It never deletes a
provider transcript or archives a thread not recorded in `owned-threads.json`.
A normal failure attempts that cleanup before stopping the owned process group.
Each thread's cleanup has a 15-second wall-clock deadline, including stalled HTTP
responses. A cleanup failure is retained for recovery; it does not prevent
temporary credential deletion or the daemon shutdown attempt. Completed scenario
checks remain in the report if a later scenario fails.
After a hard kill or incomplete cleanup, repeat the command with the same
`--output` and `--budget`, replacing `--execute` with `--recover`. This submits no
new message and stops/archives only saved IDs. An unstarted reservation with no
provider session is reported separately; it cannot be archived in Codex and is
never retried. Keep unfinished run evidence instead of silently restarting it.

Read-only preflight checks subscription login, exact models and usage before
creating a live thread. Only `gpt-6-luna` and `claude-haiku-5-5` (or its dated
snapshot) qualify; an alias qualifies only if the runtime reports that resolved model. Use low effort
when the exact model supports it; a model without an effort control sends none.
Unavailable models, exhausted/unknown usage, auth failures, startup failures and
rate limits are `infrastructure_failed` (exit 1), never a skip or pass. One
unavailable provider does not prevent the other selected provider's checks, but
the aggregate result remains an infrastructure failure. A preflight-only success
is `preflight_only`, never live acceptance.

All evidence stays in the new `.local/` output directory with private file modes.
`report.json` records attempted turns and per-provider outcomes; `catalog.json`,
`events.json`, request statuses and daemon logs are private diagnostic evidence.
The daemon uses fresh Cupola data, a private Codex home containing only a temporary
copy of subscription auth (deleted during cleanup), and the bundled Claude SDK's
normal subscription Keychain login. Claude retains its normal user settings and
stores its own new transcripts under the user's Claude home; its provider state
is not fully isolated. It never reads the owner's Cupola database.
The generated Projects and sessions belong to this run. It does not configure
Tailscale, install a mobile app, prove native rendering or claim TLS/tailnet
latency. These scripts and generated data are not app resources or Release paths.

## Provider replay goldens

`scripts/provider-replay-goldens.py` replays scrubbed recordings of real Claude
Code, Codex, OpenCode and Cursor sessions and diffs each timeline against a
committed `*.golden.json` (`tests/fixtures/provider-replay/<provider>/`). No
model, recorded command or provider home is touched.

```sh
scripts/provider-replay-goldens.py            # replay all; exit 1 on drift or a leaked pattern
scripts/provider-replay-goldens.py --update   # after reviewing an intended change
scripts/provider-replay-goldens.py scrub codex ~/.codex/sessions/.../rollout-*.jsonl \
  tests/fixtures/provider-replay/codex/new.fixture.jsonl
```

`scrub` rebuilds the log from allowlisted fields: every string becomes a
numbered placeholder, ids and times are renumbered (offsets from a fixed epoch),
unknown/MCP tool names become `tool-N`, tool input keeps only short key names
with placeholder values, and paths become `/fixture/work/file-N`. It refuses to
write if any source string with spaces, digits, capitals or punctuation (8+
chars) survives or a private pattern (home paths, email, tokens, IPs, hostnames)
matches; replay re-scans the committed fixtures. OpenCode's source is
`opencode.db:<session id>`; Cursor's is a chat's `store.db`.

Claude replays through the host's own `nativeTurns` projection
(`scripts/claude-native-turns.mjs`). Codex, OpenCode and Cursor use this
script's normalizer: the host receives those through their runtimes, so the
goldens pin the recorded shapes (message, reasoning, tool, abort) rather than
host code. A provider with no fixture prints `MISSING` with its reason.

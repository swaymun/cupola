# Cupola protocol authority — v1, revision 2

September 5, 2026. Native migration gates 01–02. This package defines the wire contract; Rust handlers implement it. Changes to either must pass the same response fixtures.

## Authority and compatibility

- HTTP: `schemas/wonder-http-v1.json`, with the endpoint's named `$defs` schema as the root. The file's outer envelope is a schema catalog, **not** a wrapper emitted by HTTP handlers.
- WebSocket: `schemas/wonder-websocket-v1.json`. Optional correlation fields are nullable and use camelCase. Event data uses each Rust event variant's existing field names; do not apply a blanket casing conversion.
- Signing: `crates/wonder-api/src/pairing.rs`; normative byte/signature vectors in `tests/contracts/native/signing-v1.json`.
- Runtime: root `compatibility-manifest.json` records exact known schema hashes and the local additive-compatibility policy. A newer Codex runtime must preserve the bundled stable and experimental contracts and pass the code-mode probe. Cupola API v1 does not imply compatibility with arbitrary runtime versions.
- Revision 1 corrects previously omitted `conversationSnapshot.thread` and WebSocket correlation properties. It does not change the emitted response shape or the URI version.

The snapshot route `GET /api/v1/conversations/{id}` returns `conversationSnapshot`; a successful send/steer returns `messageReceipt`. The checked-in fixtures come from the real Axum handler against an isolated SQLite store and a fake stdio App Server. They contain synthetic IDs and may contain temporary test paths. Never regenerate them from an owner's live daemon.

Producer tests are strict about fields to catch drift. Consumers tolerate additional object properties. Removing fields, changing existing meanings or introducing mandatory behavior requires a new major contract. Additive display kinds may be introduced within v1. New write actions require an explicit reviewed client implementation; do not derive write authority from an unknown string or an HTTP success alone.

## Liveness and execution readiness (revision 2)

`GET /healthz` remains a public liveness probe (`200`, `{"status":"ok"}`). It does not authorize execution. `GET /readyz` returns `executionReadiness` with `200` when ready and `503` during recovery, storage failure, stopped/stalled projection, or substantial notification backlog. The authenticated `GET /api/v1/host/status` returns `hostStatus`, adding the same `execution` object; its existing `state` is degraded when execution or the public route is unavailable.

Consumers preserve drafts and disable new sends while `execution.ready` is false; `detail` explains recovery in user language. Missing execution readiness requires a host update, not an assumption of readiness. New sends, retries, steering, and manual/scheduled automation admission pause during recovery. Reading history and interrupting work remain available. A small, progressing inbox is normal; backlog above 256 envelopes or no projection progress for five seconds is degraded. The durable inbox, not this threshold or the observer broadcast, owns delivery.

Runtime restart never submits a new turn. Authoritative retained items/turns reconcile existing messages; incomplete history keeps readiness degraded. Outcomes that cannot be confirmed remain `uncertain` and the ordinary retry endpoint rejects them. Approval identities are opaque and scoped to their issuing runtime; their raw JSON-RPC IDs must never be sent to another runtime or inferred from display text.

## Unknown data and unsupported actions

An unfamiliar transcript item becomes a non-interactive “Unsupported activity” entry; preserve identity/order and surrounding messages, sanitize its content, and never interpret its payload as an executable action, approval or rendered HTML. The current daemon maps unknown runtime items to `type: unknown`; existing projection tests exercise this boundary.

An unknown state-bearing event is **not** equivalent to an ignorable display item. Native clients must preserve their last committed cursor and local intent, pause writes, request a snapshot once, and show “Update Cupola to continue syncing” if the event is still unsupported. Do not loop, acknowledge it as applied, or silently advance the cursor. The current Rust enum rejects unknown event variants; native client integration of this policy belongs to gate 09. Missing required fields or invalid types also stop application of the batch. Additional fields on known variants are tolerated.

No generic “forward request” mechanism is permitted. Unsupported HTTP methods fail (405); unknown approval/tool responses fail before runtime forwarding. New native clients must disable any action whose contract/runtime support is absent or unknown, preserve the attempted draft, and show “Update Cupola on your Mac and this device to use this action.” API version mismatch permits local cached reading only. Do not claim capability negotiation is implemented: native capability discovery is a prerequisite to enabling its write controls.

## Session and signature contract

Proposed native choice: reuse `__Host-wonder_session` over HTTPS, scoped to the paired origin, with `x-wonder-csrf` on non-GET requests and the exact public `Origin`. Keep an isolated native session per paired host; store identity and session material in Keychain, never UserDefaults, URLs, logs or shared browser storage. Reject cross-origin credential forwarding, including redirects and preview fetches. The existing server accepts this cookie form; no bearer-token alternative is implied.

Pairing uses P-256, public JWK coordinates as unpadded base64url, SHA-256 ECDSA and a 64-byte raw `r || s` signature (CryptoKit `rawRepresentation`, not DER). Canonical transcripts are UTF-8, LF-separated, ordered exactly as the Rust module, decimal unsigned timestamps, with no trailing LF or normalization. The action body hash is the existing action-specific canonical semantic body; it is not arbitrary JSON reserialization. Session binding is the issued CSRF value. All signed identifiers/origins come from the authenticated pairing/action flow, not display text.

Offer/challenge expiry, nonce consumption, host/device/origin binding, durable session insertion and revocation stay authoritative on the daemon. Signature interoperability tests do not establish secure Keychain storage or real-device pairing acceptance.

## Host recovery endpoint

The host supervisor, not the daemon, serves a three-command recovery endpoint so a paired phone can act when the daemon has stopped, gave up restarting or hangs. Types: `crates/wonder-api/src/rescue.rs`; schemas: `rescue*` in `wonder-http-v1.json`; the rules a request must meet: `crates/wonderd/src/rescue.rs`; the supervisor half: `crates/wonderd/src/headless/rescue.rs`.

- **Where:** `https://<home network address>:<home port + 1>` (3779 by default) on the addresses and TLS host key of the home network route, only while home network connections are on. It is not on the Tailscale route, and nothing relays it. macOS and Linux supervisors serve it; the Windows service has no home network listener, so there is nothing to reach.
- **Requests:** `POST /api/v1/rescue/{status|restart|diagnostics}` with a `rescueRequest` JSON body (no other fields). The device signs the existing `wonder-action-v1` transcript with `action` = `rescue.<command>`, `target` = the request path, `bodySha256` = SHA-256 of the empty string, `sessionBinding` = `rescue-v1`, `expectedState` empty, and the host's installation id (`RescueCommand::transcript_bytes`). `issuedAtMs` must be within 60 s of the host's clock and `actionNonce` (16 to 128 characters of `A-Za-z0-9_-`) is single use.
- **Answers:** `status` returns `rescueStatus`; `restart` returns `rescueRestartResponse` and starts the daemon (clearing a give-up); `diagnostics` returns `rescueDiagnostics` (status plus the newest redacted daemon log lines). Failures are `rescueError`: 401 `unauthorized` (unpaired, revoked, expired, bad signature or replay: one answer for all), 401 `stale` (clock), 400 `bad_request`, 429 `rate_limited` (restart: 3 per 10 minutes, all devices together; `retryAfterSeconds`).
- **States:** `running`, `starting` (including waiting out a crash backoff), `gaveUp` (crashed past the restart limit; the supervisor stays up for this endpoint), `stopped` (host stopping).

## Snapshot limitations

Revision 1 describes today's actual snapshot. `thread.hydrated` is a projection field, not proof of an atomic replay boundary. `thread.nextCursor` is not a host replay cursor. Gate 04 must introduce and validate a consistent host epoch/cursor with transactional snapshot/replay; native live synchronization remains blocked until then. Do not invent those guarantees by adding schema fields without changing storage/publication.

### Stop action identity

`POST /api/v1/conversations/{conversationId}/turns/{turnId}/interrupt` accepts
`{ownerActionId: string}`. The native request boundary creates a UUID once per
Stop invocation, before HTTP or sealed-relay transmission. Retries of that action
must reuse its body and ID. IDs contain 1–128 ASCII letters, digits, `-` or `_`.
An empty body, `{}`, or a null ID remains compatible with older clients.

The daemon preserves the ID for Claude's `turn/interrupt`; internal stops and
automatic update handoffs omit it. The bridge soft-interrupts the first Stop,
ignores replays, and closes the owned query on the second distinct owner action
while that turn is still active. Automatic/legacy stops neither count as owner
taps nor escalate. The state ends with the run: stale turns cannot kill a newer
reply or its spared background tasks. Codex and ACP retain their existing stop
parameters and behavior. A 202 response acknowledges the request, not the turn's
completion; the turn's terminal event remains authoritative.

`tests/contracts/native/stop-actions.json` is shared by the native client,
daemon/schema and bridge tests. The bridge tests hold query output and interrupt
acknowledgement independently, without sending to a model.

## Checks

From the repository root:

```sh
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
bash tests/contracts/native/check-signing.sh
```

`jsonschema` is a development-only validator with default network/file resolution disabled. Tests validate saved fixtures and freshly emitted handler responses, and reject a snapshot missing `thread`. To intentionally refresh the synthetic HTTP fixtures, run the named handler test with `WONDER_CONTRACT_FIXTURE_DIR` set to the absolute path of `tests/contracts/native`; review the resulting diff. Do not automatically refresh expected fixtures in ordinary test runs.


## Step 05 local history

`GET /api/v1/conversations/{id}` and `/history` now return the latest local page,
with `limit=1..100` (default 100). Pass `thread.nextCursor` as `before` to load
older entries. A page counts canonical user messages, assistant messages and
saved typed activities together. Responses stay chronological within the page;
merge by message/item identity, because a turn can cross page boundaries. Invalid,
wrong-conversation or unsupported-version cursors return 400; cursors survive
restart and do not depend on replay retention. An empty page is valid after deletion.

Each page is one committed SQLite read. Its `lastSequence` covers that page, not
unfetched history. Do not advance a whole-client replay cursor based on a later
page alone. Keep the initial page cursor while fetching older pages; on a replay
invalidation discard/refetch affected cached pages. This is live pagination, not
an immutable historical export. `thread.hydrated=false` honestly identifies local
projections; it does not mean the page is unreadable.

`POST /history/refresh` starts independent runtime hydration and returns 202
`{state:"refreshing",detail:null}`. It never performs a model turn or resumes a
thread. `GET /history/refresh` returns idle/refreshing/completed/failed and a
user-readable failure detail. At most two refreshes run per daemon, with one
per conversation; saturation returns retryable 429. Failed or abandoned refreshes
can be requested again. Saved history remains available. Fresh runtime history
fills missing entries and does not overwrite existing live projections.

The existing PWA is deliberately outside this migration step and has not been
adapted to pagination. Native scrolling, position restoration and cached display
remain native-client work.

### Native read acknowledgements

`PATCH /api/v1/conversations/{id}` accepts `markRead: true` only with the displayed snapshot's `hostEpoch` and `readThroughSequence` (its `lastSequence`). Send it only while the conversation's newest displayed content is actually visible in the active scene. The store clears unread state atomically only when no newer invalidation for that conversation exists and the snapshot epoch/retention boundary remains valid. A delayed, repeated or stale acknowledgement cannot clear newer unseen work. The endpoint returns 204 without a summary; clients must not clear newer local unread state from an older acknowledgement. It accepts no title, pin or archive fields. Those settings belong to the Project conversation routes and do not imply reading.

### Computer text and clipboard input

The existing `text` action accepts 1–4096 Unicode scalar values. The
`clipboard` action with `pasteFromPhone` accepts 0–8192 Unicode scalar values
and inserts them into the focused Mac application without changing the Mac
clipboard. Both permit newline (`\n`) and tab (`\t`), and reject other Unicode
control characters. Native clients, the daemon and the Mac helper enforce the
same limits. `fixtures/computer-clipboard-v1.json` records the whitespace and
control-character boundary cases.

### Mac clipboard sharing

While the phone's Share Mac clipboard switch is on and the full computer view is
active, `POST /api/v1/computer/sessions/{id}/signaling` carries
`clipboardSubscriptionId` (1–128 bytes, no control characters) and
`clipboardCursor` (the last sequence the phone applied). A host that can share its
clipboard answers with an ephemeral `clipboard` object: `subscriptionId`,
`sequence`, `status` (`off`, `watching`, `permissionRequired`, or `unavailable` when
its helper did not answer this poll) and, only while `watching` that subscription,
nonempty plain `text` of at most 256 KiB UTF-8. Older clients omit both fields and
older hosts omit `clipboard`; neither breaks video.
The text is never stored in session events, and the Mac never sends items marked
concealed, transient, remote-clipboard or Cupola's own write.
# Native reply UI

Agent text can carry a versioned, fixed SwiftUI component catalog in a `wonder-ui`
fence. See [native-reply-ui.md](native-reply-ui.md) and
[the v1 schema](schemas/wonder-ui-v1.json). Rendering grants no execution authority;
buttons and forms prepare an ordinary user draft for explicit review and Send.

### Compact tool output

Native clients request `X-Wonder-History-View: compact`. Compact pages omit
`payload.id` and `payload.text` when they exactly duplicate the item envelope;
read `item.id` and `item.text` first. Distinct provider values remain intact.

On stored history pages, string `output` for command and function-call results
is a UTF-8 prefix of at most 2,048 bytes. `payload.outputBytes` is the full saved,
sanitized UTF-8 byte count and `payload.outputTruncated` identifies a preview.
Expanding a truncated row reads
`GET /api/v1/conversations/{conversationId}/items/{itemId}/output?turnId={turnId}`.
The response follows `toolOutput` in the HTTP schema. It returns the complete
saved text, subject to the same redaction and ingestion limits as ordinary
history. Missing, hidden or reset items are unavailable (404); this read never
starts or resumes a provider. The paired session still authorizes the route.

Consumers keep this expanded text only while the row is open, cancel on closing
or changing connections, and display a recoverable failure if the read fails.
Older hosts omit the preview fields and keep sending full output. Unstored
history, agent-task transcripts, structured tool results and noncompact pages
retain their existing full-output behavior. The synthetic shared contract is
`tests/contracts/native/tool-output.json`.

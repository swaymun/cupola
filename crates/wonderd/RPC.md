# Cupola RPC

Cupola RPC is how any program drives a Cupola host: the `cupola` CLI, a desktop
app, your own scripts and agents, and later the iOS app. It is JSON-RPC 2.0 over
two transports, with one method table, one schema and the host's existing
authentication. Like Codex's app-server or Pi's RPC mode, it is a protocol, not an
agent: the host runs Codex, Claude Code, OpenCode and Cursor; Cupola RPC lets you list
Projects and threads, send messages, answer approvals, pair devices and follow
what happens.

- Schema: [`tests/contracts/rpc/wonder-rpc-v1.schema.json`](../../tests/contracts/rpc/wonder-rpc-v1.schema.json),
  also printed by `cupola rpc --schema`. It is generated from the Rust types in
  [`src/rpc/mod.rs`](src/rpc/mod.rs); a test fails when the two differ.
- Example client (98 lines of Python, no dependencies):
  [`tests/contracts/rpc/example_client.py`](../../tests/contracts/rpc/example_client.py);
  [`run-example.sh`](../../tests/contracts/rpc/run-example.sh) runs it against a
  throwaway host.

## Quick start

On the computer that runs the host (`cupola start`):

```sh
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientInfo":{"name":"me"}}}' \
  '{"jsonrpc":"2.0","id":2,"method":"devices.list"}' \
  '{"jsonrpc":"2.0","id":3,"method":"threads.read","params":{"threadId":"nope"}}' \
  | cupola rpc
```

```json
{"jsonrpc":"2.0","id":1,"result":{"authority":"owner","protocolVersion":1,"methods":["initialize","auth.challenge",…],…}}
{"jsonrpc":"2.0","id":2,"result":[]}
{"jsonrpc":"2.0","id":3,"error":{"code":-32004,"message":"Not found.","data":{"httpStatus":404}}}
```

`cupola rpc` exits once its input ends and every request has been answered,
including the `null`-ID errors for lines the host could not read.
Keep its input open to keep receiving event notifications.

## HTTP batch archive

`GET /api/v1/projects` advertises `archiveVersion: 3` when the host accepts
`POST /api/v1/projects/{projectId}/threads/archive` (the existing owner/device
authentication applies). Send `{"threads":[{"reference":"codex:…","conversationId":"…"}]}`.
`conversationId` is optional for a provider-only catalog row. The host validates
an attached row's reference and Project ownership (or overlapping folder membership) before changing it. Saved conversations in the Project remain archivable after their folder is removed.

The request must contain 1–50 distinct references; invalid size or duplicates
return 422 before any writes. Items run in request order with a 12-second
whole-batch deadline. HTTP 200 returns `{"results":[{"reference":"codex:…",
"conversationId":"…"},{"reference":"claude:…","error":"…"}]}` in the same
order, including a failure for each unconfirmed or unattempted item after the
deadline. A lost response may follow a completed archive; retry is idempotent.
Each item retains the single-thread working/pending-send/review/update guards.
Codex threads using this computer's sign-in archive in Codex too; other providers
archive in Cupola. Clients remove only confirmed successes and retain failures
for retry. Older hosts can use a bounded, ordered batch of the existing attach
and conversation PATCH operations. No JSON-RPC method is added here.

## Transports

| Transport | Who | Framing | Authentication |
|---|---|---|---|
| `cupola rpc` (stdio) | Programs on the host computer, as its owner | One JSON message per line (UTF-8, `\n`), both ways | The loopback capability in the host's data directory (`~/.cupola/headless/loopback-capability`), read by `cupola` itself. Anyone who can read that file is the owner. |
| `GET /api/v1/rpc` (WebSocket) | Paired devices, over the host's public HTTPS origin; the local owner on loopback | One JSON message per text frame | Paired device: the `__Host-wonder_session` cookie and exact `Origin` on the upgrade, then the session's CSRF token and a signed challenge in `initialize`. Owner: the `x-wonder-loopback-capability` header. |
| `GET /api/v1/rpc/sealed` (WebSocket) | Paired devices on any trusted origin; the only RPC on the relay address | Two JSON handshake text frames, then one sealed JSON message per binary frame | Exact `Origin` and **no cookie** on the upgrade; the host proves its pinned key in the handshake; the session is the first sealed frame; then `auth.challenge` and `initialize` as on `/api/v1/rpc`. See [Sealed transport](#sealed-transport). |

Claude's file tools and commands cannot open the data directory, so the
capability file stays closed to them; on macOS that includes its
`/System/Volumes/Data` spelling.

`cupola rpc` is a relay to the loopback WebSocket, so both transports have the
same behavior. Responses can arrive in any order; match them by `id`. Binary
frames are refused.

## Messages

Requests, responses and notifications are JSON-RPC 2.0:

```json
{"jsonrpc":"2.0","id":7,"method":"threads.list","params":{"projectId":"…"}}
{"jsonrpc":"2.0","id":7,"result":{"threads":[…],"nextCursor":null,"partial":[]}}
{"jsonrpc":"2.0","method":"event","params":{"subscriptionId":"…","event":{…}}}
```

- `id` is a number or string. A request without one is a notification; the host
  defines none from clients and ignores them.
- `params` is an object (or omitted for methods without params). Unknown params
  are refused with `-32602`, so a typo never silently changes meaning.
- Batches (JSON arrays) are not supported in version 1.
- A method that answers with no body (a route's `204`) returns `{}`.

## Sessions: `initialize`

Every connection starts with `initialize`. Before it, only `initialize` and
`auth.challenge` are served; anything else returns `-32090`.

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{
  "protocolVersion": 1,
  "clientInfo": {"name": "my-client", "version": "0.3"},
  "capabilities": {"compactView": false},
  "auth": {"deviceId":"…","csrfToken":"…","challengeId":"…","signature":"…"}
}}
```

The result names the protocol version, the host (`serverInfo`, `hostEpoch`,
`hostInstallationId`), who you are (`authority`: `owner` or `device`, with
`deviceId`), every method and notification this host serves, and `capabilities`:
`compactView`, `eventBatches` (`events.subscribe` accepts `batch: true`; see
Events), `historyCompression` (the codecs `http.request` may use for stored
history, today `["deflate-raw"]`) and the limits `maxInFlight` and
`maxRequestBytes`. Use an optional feature only when the host advertises it:
older hosts refuse the unknown field. `serverInfo.version` is the host's release
(what `cupola version` prints), not a protocol version.

**Versioning.** `protocolVersion` is a major version. A host that does not speak
the client's version answers `-32093` with `data.supported`. Within a major version
changes are additive only: new methods, new optional params, new result fields
and new notifications. Clients ignore unknown result fields and enable a feature
only when its method is listed in `initialize`'s `methods`. Removing or changing
the meaning of anything is a new major version, served alongside the old one for a
compatibility window. The method results reuse the HTTP API's v1 contract
(`serverInfo.apiVersion`), so its compatibility rules
([`packages/protocol/README.md`](../../packages/protocol/README.md)) apply to them.

**Capabilities.** `capabilities.compactView` asks for the compact conversation view
(the HTTP API's `x-wonder-history-view: compact`) in `threads.read` and events.

### Authentication per transport

- **Owner (`cupola rpc`, or loopback with the capability).** No `auth`. The owner
  can do what the owner can do over loopback HTTP today, including pairing. It
  cannot answer approvals: today only a paired device can, with a signature.
  `auth.challenge` is refused (`-32003`): only a paired device signs in with one.
- **Paired device (`/api/v1/rpc` over the public origin).** The upgrade request
  must carry the device's session cookie and the exact public `Origin`, as for
  `/api/v1/events`. Then:
  1. `auth.challenge` returns a one-time challenge (the same as
     `GET /api/v1/events/challenge`).
  2. `initialize` with `auth`: the device ID, the CSRF token issued with the
     session, the challenge ID and the device key's signature over the challenge
     transcript (P-256, raw `r||s`, unpadded base64url; see
     `crates/wonder-api/src/pairing.rs` and `tests/contracts/native/signing-v1.json`).

  Each later call carries the session and CSRF token into the route, so the
  session is checked on every call. The socket closes as soon as the device is
  revoked or its session expires.

### Sealed transport

The relay route is a Cloudflare Tunnel, whose edge ends TLS and could read
plaintext. So the relay address serves `/api/v1/rpc/sealed` and nothing else:
the host answers `404` to every other device path there (pairing, events,
artifacts, plain RPC), and the tunnel's ingress rule forwards no other path.
MCP and its sign-in routes keep their own boundary instead, which accepts only
the host's tailnet address or loopback and answers `403` to any other `Host`,
the relay address included (`503` while the tailnet address is not ready). The edge
sees ciphertext only (`crates/wonderd/src/rpc/sealed.rs`; design in
[RELAY.md](RELAY.md)). Phones pair on the
home network or Tailscale; the relay origin then appears in `host.status`
`routes`.

Each route accepts only its own origin: on the relay address (by `Host`) only
the relay origin, on a home network listener only that listener's origin, and
on loopback neither a home origin nor the relay origin for another address.
At most 32 sealed handshakes run at once, and on the relay address one client
(Cloudflare's `CF-Connecting-IP`, an IPv6 address by its /64) holds at most 4 of
them; past either limit the upgrade gets `503` with `Retry-After`. A frame
larger than a socket's cap (257 KiB of request plus sealing; the plaintext
socket likewise) ends the connection as soon as its header is read. A
connection's unread answers and event frames are held to 128 MiB; an answer that
does not fit is replaced by a `BUSY` (-32092) error, and the call can be repeated
once the client reads, while event frames wait for room (the journal keeps them). The sealed session retains the home listener's identity
through authentication and every routed call, just as the plaintext socket
does. A client-supplied origin cannot supply that listener identity. The handshake:

1. The client opens the socket with its exact `Origin` and no cookie, and sends
   `{"v":"wonder-sealed-v1","clientKey":<X25519 public key, base64url>}`.
2. The host answers `{"serverKey":<X25519 public key>,"proof":{"hostKey","signature"}}`.
   `proof` is a `HostProof` (`crates/wonder-api/src/pairing_protocol.rs`) for the
   client's origin and the nonce `base64url(SHA-256("wonder-sealed-v1\n" ||
   clientKey || serverKey))`. The client checks it against the host key pin from
   pairing (`verify_host_proof`) and stops on a mismatch.
3. Each side derives two ChaCha20-Poly1305 keys with HKDF-SHA256 (salt: that
   SHA-256; input: the X25519 secret; info: `wonder-sealed-v1 client to host` and
   `wonder-sealed-v1 host to client`). Every later frame is binary: the message
   sealed with the 12-byte nonce `0000 || u64 big-endian frame number` in its
   direction, starting at 0, with no associated data.
4. The client's first sealed frame is `{"session":<session token>}`. A session
   the host does not accept gets one sealed JSON-RPC error (`id: null`, -32001)
   and the socket closes. Then the protocol is exactly the one above:
   `auth.challenge` (its origin is the accepted socket route), `initialize` with `auth`,
   calls and events.
5. A device whose session has ended (or was refused in step 4) renews it here,
   because the relay address serves nothing else to renew through. Its first
   sealed frame is instead `{"renew":{"deviceId":<id>}}`. The host answers
   `{"challenge":<Challenge>}`: a one-time challenge for that device and the
   origin this socket arrived on, as `POST /api/v1/pairing/session/refresh-challenge`
   issues. The device signs its canonical transcript with its paired key and
   sends `{"challengeId","signature"}`; the host checks it exactly as
   `POST /api/v1/pairing/session` does (confirmed, not revoked, pairing not
   expired, the paired key) and answers
   `{"renewed":{"sessionToken","csrfToken","deviceId","hostInstallationId","expiresAtMs"?}}`.
   The socket then continues as one opened with that session; the device
   saves it and runs `auth.challenge` and `initialize` with the new CSRF token.
   Nothing else is read before the signed answer, only the challenge issued on
   this socket is accepted, and renewal shares the handshake's slot and 10
   second limit. A refusal is the same sealed `-32001` error and close; it means
   the device must pair again. A host without renewal closes the socket
   without answering `renew`; a client should then say that connecting on the
   home network or Tailscale will sign it in again, not that it must pair.

A frame that does not open (altered, reordered, replayed, cut short) or a text
frame after the handshake closes the socket; reconnect and resume events from
the last cursor. The handshake must finish within 10 seconds.

The plan called for Noise IK with the device key; the phone's device key is a
Secure Enclave signing key that cannot do key agreement, so the device proves
itself with its session and challenge signature inside the channel instead.
Clients must not send the session cookie, or any other credential, on the
relay address outside this channel.

The phone reaches the rest of its HTTP API through the channel with
`http.request`: the call runs with the channel's session and CSRF token
through the same router and checks as on the home network, and returns the
route's status and body. `compact: true` asks for the compact conversation view
for that call. `responseCompression: "deflate-raw"` (only when `initialize`
listed it in `historyCompression`) compresses stored-history reads; the result
then has `bodyEncoding` and `uncompressedBytes`, and other calls stay
uncompressed. The result also carries the route's `contentType`, `errorCode`
and, for a phone-sized picture, `previewSha256`. It is refused on every other connection, and each
request and answer is bounded by the frame limits above (256 KB in, 16 MB
out). Resumable attachment uploads work through `http.request` too: use
chunks of at most 128 KiB decoded so base64 and the request envelope fit the
256 KiB inbound frame. Home network and Tailscale HTTP allow 4 MiB chunks.

**Host proof over HTTP.** Every pairing link carries `hostKey=<pin>`, the
base64url SHA-256 of the host key's SPKI (`pairing.offer` also returns it as
`hostKeyPin`). On any route, before it sends a session, a phone can call
`GET /api/v1/host/proof?nonce=<32 hex digits>` with that route's exact `Origin`
and no credential. The host answers the same `HostProof` for the origin the
request matched, never another one, and that nonce. The nonce must be hex, so
this route can never sign a sealed handshake nonce (43 base64url characters)
under the same transcript. Each origin gets at most 60 proofs a minute; past
that the answer is `429` with `Retry-After`.

Pairing a new device stays an owner decision (`pairing.*`). Claiming an offer and
creating a session remain the public HTTP routes the phone uses today
(`/api/v1/pairing/code`, `/claim`, `/session`), because they happen before the
device has a session. They are not served on the relay address.

Before calling `pairing.confirm`, ask the person to read the verification text
from their phone and pass it as `code` with the pending `deviceId`. The daemon
refuses missing or incorrect text. `pairing.pending` returns device metadata
only; neither RPC events nor `cupola pair --json` disclose the text or its
challenge. An interactive `cupola pair` terminal asks for the phone's text and
shows the matched text only after the daemon accepts it.

## Errors

Errors are JSON-RPC errors. `message` is user language and safe to show as is.

| Code | Meaning |
|---|---|
| -32700 | Not JSON |
| -32600 | Not a JSON-RPC 2.0 request, a batch, a binary frame, or too large |
| -32601 | Unknown method |
| -32602 | Invalid params (missing, wrong type or unknown field) |
| -32603 | Internal error |
| -32090 | Call `initialize` first |
| -32091 | Already initialized |
| -32092 | Busy: too many requests in flight, or too many subscriptions |
| -32093 | Unsupported protocol version (`data.supported` lists the host's) |
| -32001 … -32099 | The route refused with HTTP 4xx: code is `-32000 - (status - 400)`. 401 → -32001 (not signed in, or a signature is missing or wrong), 403 → -32003 (not allowed from this connection), 404 → -32004, 409 → -32009 (conflict: changed, already resolved, idempotency conflict), 410 → -32010 (gone for good: a list cursor expired, so list again; a pairing offer, code or challenge expired or was already used, so start again; a picture or file the message names no longer exists; a computer dialog already closed), 412 → -32012 (a stale approval), 413 → -32013 (too large) |
| -32100 … -32199 | The route failed with HTTP 5xx: `-32100 - (status - 500)`. 503 → -32103 (busy, starting or recovering: retry later) |

A route refusal has `data.httpStatus`, and `data.body` when the route sent a JSON
body, for example `{"error":"idempotency_conflict"}`.

## Methods

Each method is one fixed call to an existing HTTP route of the host, and returns
that route's body. Its rules (validation, authorization, readiness gates) are the
route's: the method table is an allowlist into the router, never a generic
forwarder. The one exception is `http.request`, served only to a paired device
inside the sealed channel, which reaches only that device's own HTTP API. Params and results are in the schema; results reuse the definitions of
[`wonder-http-v1.json`](../../packages/protocol/schemas/wonder-http-v1.json).

Authority: **any** (owner or paired device), **owner** (local owner only),
**signed** (paired device with an action signature), **owner or signed**,
**sealed device** (a paired device inside the sealed channel).

| Method | Authority | Route | What it does |
|---|---|---|---|
| `initialize` | any | — | Opens the session (above). |
| `auth.challenge` | any | `GET /api/v1/events/challenge` | One-time challenge for a paired device's `initialize`. The owner needs none and is refused. |
| `host.status` | any | `GET /api/v1/host/status` | Name, platform, public origin, and whether agents can run now (`execution.ready`). |
| `host.providers` | owner | `GET /api/v1/host/providers` | Each provider runtime on this computer and whether it runs. |
| `media.orphans` | owner | `GET /api/v1/host/media/orphans` | Preview files no conversation names any more and unchanged for `olderThanDays` (default 7): counts, bytes and folders. Changes nothing. |
| `media.prune` | owner | `POST /api/v1/host/media/orphans/prune` | Deletes exactly what `media.orphans` lists, then preview folders left empty. |
| `host.name` | any | `GET /api/v1/host/name` | The name devices show for this computer. |
| `host.rename` | any | `PUT /api/v1/host/name` | Renames it; empty or `null` returns to the computer's own name. |
| `devices.list` | any | `GET /api/v1/devices` | Paired devices, including disconnected ones. |
| `devices.revoke` | owner or signed | `POST /api/v1/devices/{deviceId}/revoke` | Disconnects a device at once. The owner disconnects any device; a paired device signs `device.revoke` (`signed`) and can disconnect only itself (`403` otherwise). |
| `devices.rename` | owner | `PATCH /api/v1/devices/{deviceId}` | Renames a device in the list (`label`). |
| `devices.forget` | owner | `POST /api/v1/devices/{deviceId}/forget` | Removes a disconnected device from the list. |
| `pairing.offer` | owner | `POST /api/v1/pairing/offers` | A pairing offer: link, code and the host key pin the link carries. |
| `pairing.cancel` | owner | `POST /api/v1/pairing/offers/{offerId}/cancel` | Withdraws an unclaimed offer. |
| `pairing.pending` | owner | `GET /api/v1/pairing/pending` | Devices that claimed an offer and wait for a decision. |
| `pairing.confirm` | owner | `POST /api/v1/pairing/pending/{deviceId}/confirm` | Lets a waiting device connect. |
| `pairing.reject` | owner | `POST /api/v1/pairing/pending/{deviceId}/reject` | Refuses it. |
| `relay.status` | any | `GET /api/v1/relay` | Whether the owner allowed the relay here, its state and address. |
| `relay.set` | owner | `PUT /api/v1/relay/settings` | `allowed: true` lets a phone turn the relay on; `false` stops the tunnel and releases the address. |
| `relay.enable` | owner or signed | `POST /api/v1/relay/enable` | Starts the relay with a phone's one-time Worker grant (`grant`, `label`). A device signs `relay.enable` with expected state `allowed`; refused with `403` until the owner allows the relay. |
| `providers.list` | any | `GET /api/v1/providers` | Codex, Claude Code, OpenCode, Cursor: sign-in, models, capabilities. `refresh` checks again. |
| `providers.reconnect` | owner | `POST /api/v1/host/providers/{provider}/reconnect` | Finds and checks a provider again, so it reads a new sign-in. |
| `voice.status` | any | `GET /api/v1/voice` | Whether voice is set up. |
| `voice.setKey` | any | `PUT /api/v1/voice/openai-key` | Saves the OpenAI key voice uses (Keychain on macOS, a 0600 file on Linux). |
| `voice.removeKey` | any | `DELETE /api/v1/voice/openai-key` | Removes it. |
| `voice.dictionary` | owner | `GET /api/v1/voice/dictionary` | The owner's voice words. |
| `voice.updateDictionary` | owner | `PUT /api/v1/voice/dictionary` | `add` / `remove` words; sent to the transcriber when the next voice session starts. |
| `update.work` | owner | `GET /api/v1/host/update/work` | What a restart would interrupt (`running`, `blocksUpdate`). |
| `update.prepare` | owner | `POST /api/v1/host/update/prepare` | Pauses work durably for a restart; refuses if more replies run than `consentedRunning`. |
| `update.cancel` | owner | `POST /api/v1/host/update/cancel` | Lets the host take work again after a restart that did not happen. |
| `projects.list` | any | `GET /api/v1/projects` | Projects and pinned threads. |
| `threads.list` | any | `GET /api/v1/projects/{projectId}/threads` | A page of a Project's threads, newest first (`cursor`, `limit` 1–30). |
| `models.list` | any | `GET /api/v1/agent-options` | The models each provider offers here; `isDefault` marks the one a new thread uses. |
| `threads.create` | any | `POST /api/v1/projects/{projectId}/threads` | Starts a thread in a Project with its first message; idempotent like `threads.send`. |
| `threads.attach` | any | `POST /api/v1/projects/{projectId}/threads/attach` | Gives a listed provider thread without a `conversationId` one. |
| `threads.delegate` | any | `POST /api/v1/conversations/{threadId}/delegations` | Starts a task as a child of a thread, shown in it as an agent task (`wonder_delegate`, as the owner). The child's access, approval and speed are at most the parent's. OpenCode and Cursor run under the owner's own settings, which Cupola cannot lower, so only a parent at Full access (or one under such settings itself) may start or direct them; any other parent gets `permission_escalation`, as `wonder_thread_send` does. |
| `threads.read` | any | `GET /api/v1/conversations/{threadId}` | A thread's messages, newest page first; page back with `before`. |
| `threads.notifications` | any | `GET /api/v1/conversations/{threadId}/notifications` | Reads this computer's stored per-chat notification choice. |
| `threads.moveToWorktree` | any | `POST /api/v1/project-conversations/{threadId}/worktree` | Moves an idle thread onto a new git worktree and branch beside its repository (`branch` optional); returns the new `cwd` and `branch`. The thread keeps its history. |
| `threads.setNotifications` | any | `PUT /api/v1/conversations/{threadId}/notifications` | Persists `notificationLevel`: `default`, `follow` or `mute`; returns the saved choice. |
| `threads.send` | any | `POST /api/v1/conversations/{threadId}/messages` | Sends a message; idempotent (below). `takeOver: true` is Send now on a thread another app on the computer has open (`heldBy` in `/history/activity`): the host stops that app's verified process for the thread, then sends; resending a queued message's `clientMessageId` with it takes that message over. A refusal leaves the message queued with a notice. |
| `threads.queue` | any | `GET /api/v1/conversations/{threadId}/queue` | Messages waiting in a thread's queue, next first. |
| `queue.edit` | any | `POST /api/v1/conversations/{threadId}/queue/{messageId}` | Changes (`body`) or cancels (`cancel`) a queued message, or sends it now into the running turn (`expectedTurnId`). |
| `approvals.list` | any | `GET /api/v1/approvals` | Requests waiting for a decision. |
| `approvals.resolve` | signed | `POST /api/v1/approvals/{approvalId}/resolve` | Answers one (below). |
| `events.checkpoint` | any | `GET /api/v1/sync/checkpoint` | The current event cursor. |
| `events.subscribe` | any | — | Streams host events from a cursor. |
| `events.unsubscribe` | any | — | Ends a subscription. |
| `http.request` | sealed device | — | One of the device's own HTTP API calls (`method`, `path`, `body`), answered with `status`, `errorCode` and the base64 `body` whatever the status; `compact` and `responseCompression` as above. Only inside `/api/v1/rpc/sealed`; not sockets, pairing claims or offers, or MCP. A paired device's session renewal (`/api/v1/pairing/session` and its `refresh-challenge`) is allowed. |

A thread's ID in `threads.read` and `threads.send` is its `conversationId` from
`threads.list` or `projects.list`. Uploads, steering a running turn directly,
interrupting, settings and the other routes of the HTTP API are not RPC methods
yet; see the migration plan below.

### Sending: idempotency

`threads.notifications` takes `{ "threadId": "<conversationId>" }`.
`threads.setNotifications` adds `"notificationLevel": "default" | "follow" | "mute"`.
Both return `{ "notificationLevel": "..." }`; the HTTP PUT body contains only
`notificationLevel`. Unknown chats return 404; unknown levels or
extra fields are rejected (HTTP 422 / RPC invalid params). The authenticated
owner or a paired device can read/write; choices survive host restarts.

`default` preserves today's finished, failed, question and approval pushes.
`follow` also delivers child-task finished pushes to the verified parent chat
(default already delivers parent completions, failures and requests for input).
`mute` suppresses all pushes routed to the chat, including queued retries, and
excludes its work/questions/approvals from the Live Activity rows this Mac reports. It does not
resolve, delete or hide in-app approvals/questions. Muting a chat while the card
shows it drops that chat's row once. Pushes skipped or cancelled while muted
are not replayed on unmute; the in-app list is the record. Foreground suppression,
expiry, verified ownership and device revocation still apply to Follow.

`threads.send` takes `threadId`, `body`, and `idempotencyKey`, a UUID the client
makes once per message and reuses for every retry of that message (after a
timeout, a dropped connection or a restart). The host keys the message by the
sender and the key:

- the same key and body again returns the first receipt (`wonderMessageId` is the
  same) and never sends twice;
- the same key with a different body is `-32009` with
  `data.body = {"error":"idempotency_conflict"}`.

The receipt's optional `queued` is true when the message waits in the
conversation's queue (behind a running turn) rather than being sent now.

The sender is the session, never a param: a paired device sends as itself, and
the owner sends as the computer's desktop identity (`wonder-desktop`). Sends are
refused with `-32103` while the host recovers (`host.status` `execution.ready`
false); keep the draft and retry.

### Threads T3 Code has open

A Claude or Codex thread open in T3 Code on the computer (`heldBy.app` is
`"T3 Code"`) carries `heldBy.t3Code` in `/history/activity`:

- `"sends"`: the owner connected the host to T3 Code and exactly one T3 thread
  matched. A send is not queued: the host passes it to T3 Code's MCP tool
  `t3_thread_send` (`mode: "auto"`, which starts, steers or queues a turn there,
  `clientRequestId` = the message's `wonderMessageId`) and marks it `completed`;
  nothing on the computer is stopped. `heldBy.waits`, `openElsewhere`,
  `tasksOnMac` and `sendStopsMacReply` are false, also while a turn runs there.
  The thread itself stays held: rewind and review refuse it as for any thread
  running on the computer. `t3_thread_send` carries text alone: a message with
  attachments, skills or a `projectModel` other than the thread's goes the usual
  way, waiting for the thread there (and the messages after it wait behind it).
  The host saves the send's intent before calling T3 Code. When T3 Code's answer
  is lost (a timeout, a server error, a host restart mid-call) the message is
  `uncertain` and is not sent again blindly: the host reads the T3 thread (T3
  Code names the message `message:mcp:<grant>:thread-send:<clientRequestId>`;
  a queued one is matched by its text) and marks it `completed`, or, when T3
  Code never got it, queues it again to go with the same `clientRequestId`
  (which T3 Code applies once per grant).
- `"cantSend"`: as `"sends"`, but the next waiting message has what T3 Code's
  send can't carry, named in `heldBy.t3CodeLimit` (`"attachments"`, `"skills"`
  or `"model"`). It waits as with `"connect"`, and `heldBy.waits` and
  `openElsewhere` are true. Nothing fails.
- `"connect"`: T3 Code is not connected (or no longer accepts the host's sign-in).
  A send waits as for any app holding the thread; Send now takes over a Claude
  thread (`heldBy.canTakeOver`), never the codex app-server T3 Code runs.
- `"notFound"`: connected, but no single T3 thread is known to be this one (T3
  Code's tools do not name the session behind a thread; the host matches folder,
  provider and messages), or T3 Code refused the last message in the past 30
  seconds: a send waits as above. The host never makes a send, its receipt or
  `/history/activity` wait for T3 Code: the first look at the thread (or a send)
  starts a lookup in the background, bounded by a timeout, whose answer later
  calls read. A failed lookup is retried after a wait that doubles (5 seconds up
  to 5 minutes), a thread with no match after 30 seconds, and a match is looked
  up again after 5 minutes. When a lookup finds the thread, waiting messages go
  into it without another send.

Connecting is HTTP only for now (not an RPC method): `GET /api/v1/t3-code`
returns `{installed, running, reachable, state, error, expiresAt, launches}`.
`reachable: false` records a stopped app or failed connection independently of
authorization; `null` means unknown. `GET /api/v1/t3-code?refresh=true` performs
a bounded, read-only capabilities check without authorizing or sending. `state` is one of
`disconnected`, `waiting` (T3 Code's approval page is open in the computer's
browser), `connected` or `expired`; `POST /api/v1/t3-code/connect` starts T3
Code's OAuth sign-in for outside agents (registration, PKCE, a loopback redirect
to the host) and opens its approval page; `DELETE /api/v1/t3-code` forgets the
grant (kept in the login Keychain) and cancels a waiting approval. T3 Code
issues no refresh token; after its grant expires the owner connects again.

The new-chat picker explicitly requests `GET /api/v1/t3-code?catalog=true`
for `launchCatalog` (or `launchCatalogError`). `?search=<model name>` also
requests that catalog through T3's supported capabilities search. Ordinary
status reads do not call the live catalog tool; Settings refresh retains its
single bounded reachability probe. Launch validates the selected catalog
options again before creating a thread.

New threads in T3 Code are HTTP only too (`launches: true` in the status):
`POST /api/v1/projects/{id}/t3-code/threads` (`{title, message, folderId?,
planMode?}`) starts one with `t3_thread_launch` in the T3 project for the
folder and returns `{threadId}`. T3 Code has no retry key, so a lost answer
(504) is never launched again. Neither the launch nor `t3_thread_read` reports
the agent's session, so the host names no Project row for the new thread; it
shows in the Project once its agent's session does.

### Approvals

`approvals.list` returns each pending approval with its `actionNonce`.
`approvals.resolve` takes the decision and the signed action fields the HTTP route
takes (`actionNonce`, `expectedState: "pending"`, `idempotencyKey`, `issuedAtMs`
within a minute of the host's clock, `signature`). The device signs the action
transcript for `approval.resolve` with target
`/api/v1/approvals/{approvalId}/resolve`, bound to its session's CSRF token, exactly
as for HTTP. The local owner cannot resolve approvals (`-32001`), as over HTTP today.

## Computer key names

A computer-session `key` input action names one key (at most 32 characters,
case-insensitive) with a `phase` of `down`, `up` or `press` and a modifier mask
(shift 1, control 2, option 4, command 8, capslock 16). There is no Super bit:
send Win/Super as its own `down`/`up`. Every helper reads the same names; the
Mac helper is `native/computer-use` (`ControlInputTranslator.keyCode`), the
Windows and Linux helpers are `computer-helper/src/platform/{windows,linux}.rs`
(`virtual_key`, `keysym`). The Python helpers in `native/computer-use-linux` and
`native/computer-use-windows` are fallbacks that read the same table; the Linux
daemon-launched helper is the Rust one (the Python Linux helper is its
fallback, and the Python Windows helper cannot control the computer).

| Name | Mac | Windows | Linux (X11 keysym) |
| --- | --- | --- | --- |
| letters, digits, `- = [ ] \ ; ' , . / \`` | yes | yes | yes |
| `return`/`enter`, `tab`, `space`, `delete`/`backspace`, `forwarddelete`, `escape`/`esc` | yes | yes | yes |
| `left right up down home end pageup pagedown` | yes | yes | yes |
| `f1`–`f12` | yes | yes | yes |
| `f13`–`f19` | yes | yes (VK 0x7C–0x82) | `F13`–`F19` |
| `shift`, `control`/`ctrl`, `option`/`alt`, `command` | yes (`ctrl`/`alt` are aliases) | Ctrl/Alt (`command` is Ctrl) | Control/Alt (`command` is Control) |
| `rightshift`, `rightcontrol`, `rightcommand` | yes | yes | yes |
| `capslock` | yes | yes | yes |
| `win`, `super` | ignored | Windows key | `Super_L` |
| `rightoption` | Right Option | AltGr: Right Alt, with Control added | `ISO_Level3_Shift` |
| `insert` | Help key (code 114) | VK_INSERT | `Insert` |
| `printscreen` | ignored | VK_SNAPSHOT | `Print` |
| `menu` | ignored | VK_APPS | `Menu` |
| `volumeup`, `volumedown`, `mute` | NX system key | VK_VOLUME_* | `XF86AudioRaiseVolume`, `XF86AudioLowerVolume`, `XF86AudioMute` |
| `playpause`, `next`, `previous` | NX system key | VK_MEDIA_* | `XF86AudioPlay`, `XF86AudioNext`, `XF86AudioPrev` |
| `brightnessup`, `brightnessdown` | NX system key | ignored | ignored |

"Ignored" means the helper accepts the name and does nothing: that action is
skipped and the rest of the batch runs. A name outside this table, or any `f`
number above 19, is `invalid_input`. The Mac helper has no per-action error
channel yet, so an ignored key reports success.

`rightoption` on Windows presses Control only when this helper has to pair it
(no Control latched or requested); AltGr up releases that paired Control and
never a Control the sender latched. On Linux it is `ISO_Level3_Shift`, which
only changes characters on a layout with a third level; plain US layouts show
no effect.

## Events

Host events are the journal behind `/api/v1/events`: every committed change, in
one sequence per host epoch. A cursor is `{hostEpoch, sequence}`.

1. Call `events.checkpoint` and keep its `hostEpoch` and `lastSequence`.
2. Read the state you show (`projects.list`, `threads.read`, …).
3. `events.subscribe` with `after` set to that cursor. Every event committed after
   it arrives as an `event` notification, in order, then new ones as they commit.
4. After applying each event, save its `hostEpoch` and `sequence` as your cursor.
   To resume after a disconnect, subscribe again with `after` set to it.

```json
{"jsonrpc":"2.0","id":9,"method":"events.subscribe","params":{"after":{"hostEpoch":"8213…","sequence":41}}}
{"jsonrpc":"2.0","id":9,"result":{"subscriptionId":"5f0e…","cursor":{"hostEpoch":"8213…","sequence":41}}}
{"jsonrpc":"2.0","method":"event","params":{"subscriptionId":"5f0e…","event":{"hostEpoch":"8213…","sequence":42,"event":{"type":"message_state","data":{…}},…}}}
```

Without `after`, a subscription starts at the current head. The event is a
`hostEventEnvelope` (`packages/protocol/schemas/wonder-websocket-v1.json`).

**Batches.** When `initialize` lists `eventBatches`, `events.subscribe` with
`batch: true` delivers events as `events.batch` notifications instead:
`{"subscriptionId":…,"events":[…]}`, several committed events in sequence
order, the same envelopes `event` carries. Save the last one's cursor after
applying the whole batch. Without `batch`, every event is its own `event`
notification, as before.

`events.ended` with `reason: "resync"` means the cursor cannot be replayed: the
host restarted with a new epoch, or the event is older than the replay window.
Read state again from step 1. Treat an unknown `reason` the same way.

Delivery is from durable storage, so a slow client is never dropped: events wait
in the journal until it reads them. A connection has up to 8 running
subscriptions; one that ended with `events.ended` no longer counts.

## Limits

- 32 requests in flight per connection (`-32092` beyond); 256 KiB per request.
  A request just over that is answered `-32600` under a `null` ID; a WebSocket
  frame more than 1 KiB over it closes the connection (a transport limit).
  `cupola rpc` answers any oversized line itself with that `-32600` and keeps
  the session.
- The host pings the WebSocket every 15 seconds and closes it after 45 seconds
  without any frame from the client (WebSocket libraries answer pings
  automatically). `cupola rpc` handles this itself.
- A client that stops reading stops having its requests read: the host never
  waits on a full outgoing queue, and revocation or session expiry still closes
  the connection at once. Keep reading while you wait on anything else.
- The host never times out a route call; set your own timeout. Provider discovery and reconnect
  may need extra time on a cold start.

## Driving a turn from the command line: `cupola chat`

`cupola chat` is a client of these methods, for agents and scripts that test
Cupola's orchestration without the phone. It runs nothing itself: the host's
provider runs the turn, exactly as for a message from the iPhone, and the
thread shows in the Project there.

```sh
cupola chat -p "Summarize the README" --project "Overnight fixtures 2026-10-18" --json
cupola chat -p "Now the tests" --thread <conversationId> [--provider codex|claude|opencode|cursor] [--model M]
cupola chat queue --thread <conversationId>
cupola chat send-now <queuedMessageId> --thread <conversationId>
cupola chat approve|deny <approvalId>
```

1. `events.subscribe` (from the head), so no event of the turn is missed.
2. A new thread (the default; `--project` by name or ID): `projects.list`,
   `models.list` (the provider's `isDefault` model unless `--model`; the
   provider is `--provider`, else the Project's `lastFamily`, else Codex), then
   `threads.create`. An existing thread (`--thread`): `threads.send`, with
   `projectModel` when `--provider` or `--model` switches it. Each message has a
   fresh `idempotencyKey`; a send that times out is retried once with it.
3. The message is followed by its turn: the receipt's `codexTurnId`, else the
   `turnId` of the message's own `message_state`. While it waits in the queue
   (no turn yet) only its own `message_state` counts; the thread streams the
   turn ahead. Once it has a turn, every event of that turn counts and the
   turn's terminal `message_state` ends it, even when they name another
   message: a message sent into a running reply (Send now) joins that turn
   (Codex) or starts its own (Claude), and the turn's events carry the message
   that started it. Back in the queue (`accepted_by_wonder`) it has no turn
   again. `approval_opened`, `approval_resolved` and a new turn re-read
   `approvals.list`; it shows the approvals whose `params.turnId` is its turn.
4. A lost connection resubscribes with `after` set to the last event read,
   so the host replays what it missed. On `events.ended` (`resync`) it
   subscribes from now first, then reads the message with `threads.read`
   (its state, `deliveryError` and `codexTurnId`), so an end that committed
   in between is never missed, and says that some of the reply may be
   missing. A call that has to open a new connection (the old one closed)
   ends the subscription, so the CLI resumes after its cursor on the new one
   and reads `approvals.list` again. It retries while the host restarts.
   `--timeout` bounds every call while following: a host that stops
   answering ends the command at `--timeout` with `end` `timedOut` (75).

`--json` prints one object per line: `sent` (`threadId`, `messageId`,
`created`, `queued`), `text` (a piece of the reply), `message` (a complete
agent message), `tool` (`category`: `command`, `fileChange`, `tool`, `plan`,
`reasoning`; `state`; `detail`), `approval` (`approvalId`, `kind`, `summary`,
`decisions`), `approvalResolved`, `state` (delivery state), `notice`, and last
`end` (`status`, `exitCode`, `detail`). Exit status: 0 completed, 1 failed, 2
usage, 3 interrupted, 69 unavailable, 75 `--timeout` passed while the turn
still runs.

`send-now` is the iPhone's "Send now": `threads.queue` for the message's
revision, `threads.read` for the running turn, then `queue.edit` with
`expectedTurnId`. `approve` and `deny` call `approvals.resolve`; the host
answers the local owner `-32001` because an approval needs a paired device's
signature ([Approvals](#approvals)), so they exit 69 and say to answer on the iPhone. Approvals
still show in the stream with their IDs.

## Changing the protocol

1. Add or change the types and the method table in `src/rpc/mod.rs`. A method
   names exactly one existing route; put new behavior in the route, not here.
2. Regenerate the schema: `WONDER_UPDATE_RPC_SCHEMA=1 cargo test -p wonderd
   rpc::tests::schema` (or `cupola rpc --schema > tests/contracts/rpc/wonder-rpc-v1.schema.json`).
3. Add the method's example params in `src/rpc/tests.rs` and its row above (the
   tests check both).
4. Keep changes additive within version 1.

## Phase 2: the iOS app on Cupola RPC (plan)

Today the iOS app (`apps/native` WonderPairing and `apps/ios`) calls about 90
distinct HTTP routes and keeps `/api/v1/events` open. Moving it onto Cupola RPC
gives it one authenticated socket for calls and events, one error model and one
schema, and lets the HTTP surface shrink to pairing, uploads and media.

**What stays HTTP.** Pairing before a session exists (`/api/v1/pairing/claim`,
`/code`, `/session`, `/session/refresh-challenge`), file upload and download
(`/conversations/{id}/files…`, `/workspace/media`, `/workspace/file`), the
dev-preview proxy, terminal and computer-session sockets and WebRTC signaling,
diagnostics batches, `/healthz` and `/readyz`. These carry bytes or run before
authentication, and do not fit JSON-RPC well.

**Order.** Each step adds methods (thin, as in phase 1), switches the app's calls
for one area behind a capability check on `initialize`'s `methods`, and keeps the
HTTP routes:

1. Session and events: open `/api/v1/rpc` with `auth.challenge` and
   `initialize`; replace `/api/v1/events` and `/sync/checkpoint` with
   `events.subscribe` / `events.checkpoint`, with batching (`batch: true`,
   `events.batch`) to match today's coalesced frames.
2. Chats: `projects.list`, `threads.list`, `threads.attach`, `threads.read`,
   `threads.send`, `threads.create`, `threads.queue`, `queue.edit` (exist); add
   `threads.update` (`PATCH /project-conversations/{id}`), `threads.detail`,
   `threads.fork`, `threads.steer` (`/turns/{id}/steer`), `threads.interrupt`,
   `threads.retry` (`/messages/{id}/retry`), `threads.activity`
   (`/history/activity`), `threads.refresh`, `threads.archived`, `threads.goal`,
   `queue.reorder`, `questions.list`/`answer`,
   `threads.rewind`, `search`.
3. Approvals and attention: `approvals.list`/`resolve` (exist); push
   (`push.config`, `push.register`, `push.revoke`, `push.presence`,
   `push.route`, `liveActivity.register`).
4. Settings and providers: `providers.*` (exist) plus `providers.reconnect`,
   account usage, `settings.*` (default models, thread tools, chat detail, custom
   instructions, file restore, Cupola skills), `agentOptions`, `host.name`/
   `rename`, `voice.*`, `connectedApps.list`, automations (`automations.*`).
5. Workspace reads: `workspace.roots`, `directory`, `search`, git status,
   summary, changes, diff, repositories and repository actions, pull requests,
   subagents and transcripts, composer commands, skills.
6. Devices: `devices.list`, `devices.revoke` (signed), `devices.rename` (signed),
   `workspace.reset` (signed).

**Compatibility window.** The host keeps every HTTP route the app uses until the
last TestFlight build that uses it has expired (90 days after its upload) and the
owner agrees to remove it. The app falls back to HTTP for any method a host does not
list, so a new app works with an older host and the reverse. The HTTP authority and
the RPC schema share result definitions, so the fixtures in
`packages/protocol/fixtures` keep testing both. Removing a route follows AGENTS.md:
the route, its tests, smoke checks and inventory entries together.

Added provider accounts are no longer supported. `providers.list` keeps the
computer's own sign-in status available to paired phones, and
`GET /api/v1/account/usage?agentFamily=codex` (or `claude`) keeps its usage.
The capability fields `accounts` and `accountSignInOnPhone` remain `false` for
older clients. New-thread `accountId`, when sent, must be `"default"`.
Legacy added-account threads retain their original identities and saved history,
open read-only with an explanation, and cannot continue under the computer's
sign-in. The old provider homes are left on disk.


## Computer dock

`GET /api/v1/computer/dock` (owner device) feeds the phone's "Windows & apps"
menu: `{"available": Bool, "pinned": [App], "recent": [App]}` with
`App = {"id", "name", "running", "iconURL"?}`. At most 24 pinned and 8 recent (a Linux or Windows helper lists installed apps in `pinned`, up to 64);
an app is listed once, pinned before recent. `iconURL` (relative, present only
when the computer has an icon) is `GET /api/v1/computer/apps/icon?id=…`, whose
answer is `{"iconPNG": base64}`, a 64 px PNG of at most 32 KB. `available` is
false when the computer has no dock to read (for example no display).
`POST /api/v1/computer/sessions/{id}/apps/open` with `{"bundleId": id, …}` opens
or brings forward any listed app and needs the same control lease as before.

| Computer | Pinned | Recent | App `id` |
|---|---|---|---|
| Mac | `com.apple.dock` `persistent-apps` | Running Dock apps, then `recent-apps` if `show-recents` is on | Bundle identifier |
| Linux | Installed GUI apps, running first then alphabetical: `.desktop` entries from `XDG_DATA_HOME`, `XDG_DATA_DIRS`, Flatpak and Snap, minus `NoDisplay`/`Hidden`, non-`Application` and `OnlyShowIn`/`NotShowIn` mismatches for `XDG_CURRENT_DESKTOP`. No desktop pins are read (they do not exist on XFCE, Cinnamon, MATE, i3 or Wayland compositors) | Open windows whose app has no desktop entry, most recently used first; excluded desktop entries stay absent even while running | Desktop file ID; a running app with no desktop file uses its `WM_CLASS` and cannot be opened from the dock |
| Windows | Taskbar `.lnk` pins resolved with `IShellLink`; Store (UWP) pins are skipped | Programs with windows open now, including minimized windows, in the window list’s front-to-back order | `pin-<SHA-256 of shortcut stem>` (pins), `exe-<SHA-256 of lowercase executable name>` (running only) |

Linux and Windows are answered by the computer helper (`--dock`, one JSON line)
and opened by `--launch-app ID`; Windows focuses an existing window or opens
the original pinned shortcut (preserving arguments and working directory) from
the helper process, never the input owner. Windows one-shot dock commands do not
start an input owner. A locked, secure, or noninteractive Windows desktop answers
`available: false`; launch/focus is refused there. Icons from the helper are
re-encoded by the host, so only valid PNGs reach the phone. Linux icons are PNG files from the
icon theme (vector-only icons have none); Windows icons are the shell's.

## Resumable conversation attachments

The existing `POST /api/v1/conversations/{conversationId}/files` remains the
single-shot path for images and small files (8 MiB decoded, 12 MiB request).
Large videos use the following authenticated HTTP routes, also available through
the sealed channel's `http.request`. Uploads never send a model message.

1. `POST .../files/uploads` with `{clientUploadId, name, mimeType, byteSize,
   sha256}`. The client generates and retains the UUID with its draft and computes
   the SHA-256 over the entire original file. Repeating the same request resumes
   it; reusing the identity with different metadata returns 409.
2. `PATCH .../files/uploads/{uploadId}` with `{offset, contentBase64}`. Use
   `offset = receivedBytes` and at most `maxChunkBytes` (4 MiB) decoded per chunk;
   sealed requests use at most 128 KiB. Each successful response reports the
   durable `receivedBytes`, `totalBytes`, `expiresAt` (Unix milliseconds), and
   `state: "uploading"`. Advance UI byte progress only after that receipt.
3. After any interruption or lost response, `GET` the same upload URL. The host
   persists the acknowledged prefix across restarts, trims an unacknowledged
   write before appending, and accepts exact retries wholly within that prefix.
   Gaps, overlapping suffixes and differing retries return 409; fetch progress
   and resume from the returned byte offset. GET does not extend the deadline.
4. `POST .../files/uploads/{uploadId}/complete` after all bytes arrive. The host
   streams SHA-256 and MIME checks, atomically publishes a complete ordinary
   conversation attachment, and returns `state: "complete"` with `file` in the
   existing conversation-file shape. Its ID goes into `attachmentIds` when the
   owner sends. Retrying completion or creation after a lost receipt returns the
   same file. No partial attachment is listed or sent.
5. `DELETE .../files/uploads/{uploadId}` cancels immediately and removes staging
   bytes. Stop local chunk work before cancelling; requests serialize by upload
   identity, so an in-flight chunk cannot recreate a cancelled session. Cancellation
   is idempotent. A small cancellation marker lasts 24 hours so a delayed create
   retry cannot resurrect the session; an explicit new attachment uses a new UUID.
   A completed upload returns 409 here; remove that draft attachment
   with `DELETE .../files/{fileId}`, which still protects sent files.

Videos have an explicit **1 GiB (1,073,741,824 byte)** limit, inclusive. This
allows about 28.6 Mb/s for a five-minute recording; actual screen-recording sizes
vary with codec and content. Images and other files retain the 8 MiB limit. A
larger declaration is refused before bytes are stored, with HTTP 413 and a plain
message naming the limit. At most four sessions per conversation may be active.
Each accepted chunk renews a 24-hour idle deadline. A startup sweep and a
one-minute background sweep remove expired session metadata and partial files,
including interrupted assembly; while the host is stopped, cleanup occurs when
it next starts. Cancellation removes them immediately. Staging is private,
contained within the existing owned Project media storage, separate from complete
attachment records; no new database table or migration is needed.

Begin refuses unsupported video MIME types with 415 before creating a session.
Accepted video types are MP4, QuickTime MOV, M4V, 3GPP/3GPP2, WebM, Matroska and AVI.
Completion refuses a SHA-256 mismatch with 422 and a MIME/container mismatch with
415, removing partial bytes in both cases. MP4/M4V, 3GP and QuickTime MOV require a
matching `ftyp` box and brands (HEIF/AVIF still images cannot masquerade as video).
The complete `ftyp` box is checked in bounded memory, including extended sizes
and compatible brands past 4 KiB. ISO3 and 3GP brands are recognized.
WebM/Matroska require their EBML document type, and AVI requires its RIFF signature.
Unrecognized `video/*` container types are refused with a plain format message.
The small-file endpoint applies the same video signature checks.

Chunk writes and assembly hold only their upload identity guard during bulk I/O.
The media reset fence covers short metadata/publication steps; a reset generation
check prevents a stale writer from publishing after a workspace reset. Staging
and final attachment files are created owner-only (0600 on Unix).

There is **no video processing**: no duration inspection, thumbnail extraction,
keyframes, transcript or transcoding. Video stays a file for the agent's own
tools. The daemon streams a verified private copy into its existing sent-file
storage. Codex and Claude receive the existing “Attached file … is available at
…” text path note; ACP receives the existing `localFile` with path, MIME, size and
SHA-256, verifies it in bounded memory, and gives its provider a text path note.
The large file never becomes an inline provider blob, and does not require ACP
embedded-context support. Provider slash-command attachment restrictions remain.
T3 Code's existing signed attachment upload streams the verified private snapshot
from an open file descriptor. Its own existing 50 MiB file limit still determines
whether T3 Code can carry a send; larger files keep the existing attachment-limit
notice/wait behavior while T3 Code owns the thread. They are never buffered for T3.

Private video snapshots use `.wonder/sent/<fileId>.<sendId>/<name>`, preserving
the checked bytes separately from the stored original. Every send still makes a
copy. They stay while their source record exists; after the record is removed,
the existing age-protected media orphan report/prune reclaims the copies too.
Workspace reset removes them immediately along with the conversation's media.
This cleanup is explicit orphan pruning or reset, not an automatic per-turn purge.
Older random-named send folders remain until workspace reset because their
source-record ownership cannot be reconstructed safely.
The phone UI, picker, progress controls and retry/cancel wiring belong to
W3-VIDEO-I; host contracts do not prove that phone flow.

# ACP runtime

The OpenCode and Cursor adapters share ACP connection lifecycle code. Model
discovery, launch arguments and provider modes belong to each adapter. Adding an
agent does not require a second send/stream/stop implementation, but it still
requires explicit store, daemon, launcher, native identity and contract
registration. Cursor selects the shared entrypoint with
`node main.mjs --provider cursor --state-dir <private-directory>` and keeps its
own `cursor-runtime` journal and `cursor-` thread IDs.

## Owners and extension points

- `agent.mjs` owns ACP JSON-RPC framing, request IDs, deadlines, native
  `session/cancel`, permission responses and exact child-process shutdown.
  Supply `command`, `args`, `env`, `cwd` and a user-facing `label`; there is no
  implicit `acp` command-line argument or OpenCode error label. Keep this layer
  independent of catalogs, model syntax and mode names.
- `catalog.mjs` owns the explicit OpenCode and Cursor adapters: CLI discovery,
  authentication, native model inventory/default, model validation, unsupported
  policy choices and provider work modes. OpenCode uses `build`/`plan`; Cursor
  uses `agent`/`plan`. Those values are not ACP constants.
- Work mode and permission policy are separate contracts. A work mode picks how
  the agent approaches a turn (OpenCode's mode selects one of its agents;
  Cursor's Ask is a question-and-answer chat, which Cupola never selects); it
  is not a permission level. Plan is not read-only: under OpenCode's default
  rules its plan agent can still run shell commands, so Cupola calls it a
  planning mode and promises no restriction. Permission policy is
  `permissions: "native"`: neither agent offers a per-session setter, and
  Cupola invents none (no launch flags, environment or config rewrites, which
  would reach every chat sharing the process or folder). Approval cards appear
  only when the agent's own rules ask. The neutral stored values
  (`workspace`/`ask`) pass and make no access guarantee; any other access or
  approval choice, or a non-boolean Plan choice, fails before work rather than
  being mapped onto a work mode. Enforced profiles are later work (F3-U2/U3/U5).
- `bridge.mjs` translates Cupola requests to native ACP methods. Attach with
  native `session/resume` when advertised; otherwise use advertised
  `session/load` and exclude its replay from the newly accepted turn. Configure
  only advertised options/categories and verify the returned configuration.
- `session-commands.mjs` retains optional native `available_commands_update`
  capability state on one owned ACP peer. It has no provider/model/mode policy;
  use the same owner for another ACP adapter's native session notifications.
- `projection.mjs` translates native updates into the existing Cupola items.
  Preserve native tool kind, IDs, content and ordering; never infer completion
  from output text. The selected adapter supplies the family and provider copy;
  generic tool items and errors retain that identity.
- `sessions.mjs` journals durable create/turn receipts and Cupola-visible items;
  it is not an independent agent status detector. Each provider needs its own
  family namespace and storage root. Never parse another agent's logs for a
  lifecycle answer its protocol already provides.
- `main.mjs` composes the selected provider launch/catalog with these owners.
  The daemon family enum is in `crates/wonder-store/src/runtime_bindings.rs`;
  `crates/wonderd/src/claude.rs` owns bridge launch and routing despite its name.
  Provider status lives in `provider_runtime.rs`, Project admission in
  `projects.rs`, and native model/provider presentation in
  `apps/native/Sources/WonderPairing` and `apps/ios/Wonder`. Packaging and the
  public source inventory must include a new provider's adapter. These are
  explicit integration steps, not a dynamic provider/plugin registry.

## Send, stream and stop contract

Persist acceptance before issuing `session/prompt`; repeat delivery returns
that receipt and never runs it twice. A stop during setup waits for the in-flight
control operation to settle, then skips the remaining configuration and prompt.
A stop after prompt submission sends native `session/cancel` and cancels pending
permission requests. Continue accepting streamed updates until the original
prompt response reports its stop reason. Stop waits up to ten seconds across
the cancel write and acknowledgement, then reports an unconfirmed stop if the
turn still has no terminal outcome. Its successful reply includes the exact
turn ID and confirmed outcome; a response which completed before cancellation must
remain completed.

A response deadline or cancel notification does not prove completion. For an
admitted prompt, `AcpAgent.request` invokes `onTimeout` to persist the unresolved
state and keeps that original request pending. A later native response can
release the same turn. If it never arrives, keep admission blocked; do not kill
an otherwise healthy shared process or report idle. Process-loss recovery must
observe that exact owned process's exit. Restarted journals also retain
unresolved admission rather than guessing from a missing response.

## Native session command reads

`thread/commands/list` is a Cupola bridge-local method, **not a standard ACP
request**. The daemon serves it to the phone's composer `/` list as
`GET /api/v1/conversations/{id}/commands`. Pass an existing Cupola
`threadId`. The result contains `sessionId` (the native ID), `peerId` (one
owned-process lifetime), `revision` and `commands`:

- `availability: "ready"`: the latest complete native replacement. `commands: []`
  confirms an empty catalog; native row order and metadata are preserved.
- `availability: "unknown"`: the live attached peer has not advertised a valid
  catalog, or its previous catalog was invalidated/evicted. `commands` is null;
  a rejected replacement has `reason: "invalidCatalog"`, otherwise reason is null.
- `availability: "unavailable"`: there is no live owned attachment. `peerId`,
  `revision` and `commands` are null, including cold or expired sessions.

Reading makes no native request, starts no process, creates/resumes/loads no
session, and does not extend idle lifetime. Catalogs never enter the durable
journal or turn projection. A caller must fence results by thread/native session
and `peerId`; revisions are local to that peer, not durable catalog identities.

New-session notifications may precede their receipts: a bounded pending entry
can bind only to the exact returned native ID and an outstanding create that
existed when the notification arrived. Resume/load reserves the requested native
ID and publishes its catalog only after attachment succeeds. Failed/timed-out
setup, process loss, replacement and shutdown discard those pending entries.

Each replacement allows at most 512 rows and 256 KiB of JSON, with bounded name,
description and input-hint strings. Invalid replacements clear prior rows rather
than truncating or retaining stale choices. A peer retains at most 64 completed
catalogs, 32 pending unknown IDs and 32 attachment/create attempts of each kind;
eviction means unknown until a new complete native update arrives.

This bridge read does not add a phone picker or invocation API. OpenCode 1.18.27's
command branch omits image/binary parts, including for `/compact` and unknown
commands. The OpenCode adapter therefore rejects slash commands combined with
attachments before native prompt submission, retaining the failed turn's input
so the user can edit it. It follows native text/resource ordering, without
changing the input, executing commands itself or restricting the shared ACP
transport. Text-only commands and ordinary messages with attachments still use
the native prompt path. ACP offers no capability for command attachments; review
this restriction against the native implementation when updating OpenCode.

A leading provider-handoff context can prevent slash recognition. That
combination remains unverified; catalog retention does not enable a picker or
emulate invocation. Commands remain ordinary native prompt text.

## Cupola's thread tools

The daemon sends Cupola's Project thread tools (`dynamicTools`) with
`thread/start` and every `turn/start`, as it does for Claude. `tool-server.mjs`
offers them to the agent as one stdio MCP server per Cupola session, passed in
ACP `session/new`, `session/resume` or `session/load` `mcpServers` (stdio is
ACP's baseline transport; no TCP port is opened). The agent starts
`mcp-server.mjs`, which connects back over an owner-only Unix socket and relays
`tools/list` and `tools/call`; each call reaches the daemon as the same
correlated `item/tool/call` request the Claude bridge sends, so the daemon's
Project scoping, access ceiling and per-turn limits apply unchanged.

- **Credential.** Each server gets a random credential in its environment,
  never on a command line, in the journal or in a frame to the daemon. The
  bridge keeps only its hash in memory. It is bound to one Cupola session and
  one agent process, stays alive while that session replies, expires after 15
  idle minutes, and is revoked when the session is attached again, its agent
  process exits or the bridge closes. A revoked server is told to exit, which
  removes its tools; the next reply attaches the session again with a new one.
- **Which thread is asking.** OpenCode 1.18.27 registers MCP servers per folder
  (`mcp.add` with the session's directory), so every session in a folder lists
  every Cupola session's server (`wonder-<8 hex>`). A call runs only while the
  credential's own session is replying and only after that session reported the
  same tool and arguments (`tool_call` updates); a call through another
  session's server is refused with the caller's own server name. When OpenCode
  asks permission for a Cupola tool, the bridge allows the session's own server
  and rejects another's, without an approval card.
- **Tool list changes** reach the agent through `notifications/tools/list_changed`
  (OpenCode re-lists a server that advertises `tools.listChanged`).

The bridge never tells the agent to call these tools; their descriptions say
what they do, and Cupola's preamble (crates/wonderd/src/preamble.rs) opens a new
session's first prompt with a short note naming them.

## Reuse the contract fixture

Run `node --test services/acp-runtime/test/*.test.mjs`. The existing
`test/fake-agent.mjs` is a scripted ACP peer over real child pipes: it does no
model work and needs no provider account. The transport tests use opaque session
IDs and no OpenCode CLI. The bridge tests cover durable replay, streamed text,
permissions, stopping while setup is held, a delayed cancellation response,
shared peers, unknown outcomes and restart persistence. Command fixtures cover
new/resume/load response ordering, replacement, invalid/empty/omitted catalogs,
interleaved and failed setup, idle expiry and stale-peer fencing without native
provider discovery or a real model turn.

For a new agent, first verify its native initialization capabilities and control
schemas, then adapt only the launch/catalog/options boundary. Run the same
lifecycle contracts and add native capability fixtures for its differences.
A passing synthetic fixture proves Cupola's protocol behavior, not that a real
second agent works. Real-provider acceptance remains a separate, explicitly
authorized test.

Protocol references: [ACP prompt and cancellation lifecycle](https://agentclientprotocol.com/protocol/prompt-turn),
[session configuration](https://agentclientprotocol.com/protocol/session-config-options),
[native command advertising](https://agentclientprotocol.com/protocol/v1/slash-commands),
and [OpenCode 1.18.27 ACP service](https://github.com/anomalyco/opencode/blob/v1.18.27/packages/opencode/src/acp/service.ts).

## Cursor adapter verification and integration boundary

Measured on Cursor CLI `2026.10.16-e3cb207`: `agent status --format json` reports
sign-in separately from installation. ACP initializes with protocol 1 and
`cursor_login`; authentication reads existing credentials and does not run
`agent login` or open a browser. Model discovery starts an ephemeral ACP
session with no MCP servers, reads the native model configuration, then closes
that process group without submitting a prompt. `catalog.mjs cursor-status`
serves the daemon's read-only status probe (signed in: 0, signed out: 42,
not installed: 44), returning only version and the generic account label.
Cursor does keep empty sessions
(`~/.cursor/acp-sessions/<id>/meta.json`; 1,010 accumulated on one Mac from the
five-minute check), offers no `session/close`, and its `worker-server` helper
outlives `agent acp`. After each check Cupola ends the probe's process group and
removes the session folders whose recorded `cwd` is the runtime's own folder,
which only model checks use; Project sessions are never touched.

The bridge owns one catalog per provider and never makes a reply wait for it:
starting an agent needs only the located CLI. Routine reads (the daemon's
five-minute check, model lists) reuse the last check for an hour when it found
models, half an hour when the provider is not set up, and 30 s doubling to
10 minutes after a failed check. A changed CLI (path, size or time) or settings
file (`~/.cursor/cli-config.json`; OpenCode's config and `auth.json`) checks at
once, as does an explicit refresh (Reconnect, opening Providers, an activated
runtime). Concurrent callers share one check.

Cursor's CLI model aliases differ from its ACP model IDs. Cupola retains the
advertised ACP IDs, including parameter brackets, unchanged after the `cursor:`
namespace. The installed agent returned 46 model choices and confirmed exact
model configuration plus switching into and out of Plan mode. No effort choices
or CLI aliases are synthesized. A native model configuration change also
changed the model offered by a later process; the probe restored the original
`default[]` value afterward. The shared bridge attaches with advertised
`session/load`, suppresses its history replay, forwards native permission IDs
and waits for the original prompt response to confirm Stop.

The Cursor adapter uses `acpPermission` for advertised remembered decisions;
OpenCode retains its existing `opencodePermission` wire shape. Daemon validation
and native approval presentation admit the new shape only for the authenticated
Cursor runtime and only when exactly advertised. The store's push approval
parser also needs to recognize that shape before notification actions work.
Cupola's session MCP tools and computer use reach Cursor through the same
per-session server as OpenCode. Cursor builds a separate MCP client map for each
session from its `mcpServers` (layered over `~/.cursor/mcp.json`), so a session
lists only its own Cupola server. It titles an MCP call `<server>: <tool>` and
reports `{providerIdentifier, toolName, args}` as its input (Cursor CLI
2026.10.19 bundle); the bridge matches that shape as it matches OpenCode's.
The adapter refuses unsupported access policies, file resources and
audio; native image support remains subject to attachment integrity validation.
Cursor extension methods such as questions and plan approval are not implemented
by this change and must not be advertised as supported.

Synthetic child-pipe contracts cover authentication ordering, identity, native
model values, modes, loading, durable receipts, permissions and cancellation.
These do not establish a live model turn, app rendering, approvals or Stop with
the real provider. Full acceptance still needs provider registration and a
recorded turn in the authorized run's fixture Project. The duplicate store
Project-family parser and shared HTTP enum/fixture still need Cursor registration;
metadata-only reservation currently refuses it as an unknown family. Native
runtime loading of a session containing a completed turn remains unverified.

Reference: [Cursor ACP documentation](https://cursor.com/docs/cli/acp).

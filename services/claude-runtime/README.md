# Claude runtime

Cupola uses the pinned Claude Agent SDK and retains one Query per owned session
while it is active or warm. Ordinary prompts, native controls and process
lifecycle remain owned by `bridge.mjs`.

## History memory and responsiveness

Session startup discovers saved journal filenames. A conversation loads on demand;
its first load recovers interrupted turns and durable receipts before accepting
input. Concurrent loads share one mutable object. The journal cache has a 16 MiB
byte budget; active turns, reset intent, request leases and unfinished writes keep
their owner until that work ends. Eviction removes only memory, never receipts or
native transcripts. Listing, native attach and child reset/rewind also discover
journals that are cold.

Retained native records have a 24 MiB budget. The transcript and its derived
history share 64 MiB; task projections and supplemental task records each have
4 MiB. Task projections retain a revision instead of the raw transcript. These
are conservative retained-object charges, not RSS ceilings: active requests and
V8's allocator can use more. A transcript charged above the whole 64 MiB budget
replaces the other cached transcripts and is held for 30 s of idle time, so its
history pages reuse one read; other oversized reads return complete history
without entering the cache or evicting unrelated warm entries. Each loaded
journal object carries a per-conversation generation kept across eviction; a
save from an evicted, older object is refused rather than overwriting a newer
journal's turns and receipts.

Native JSONL is read in 64 KiB chunks, retaining only incomplete small records
between appends. Large individual records parse in a temporary worker thread.
The SDK's native read APIs run in one disposable read process per bridge, reused
within its SDK lease for catalogs and helper rosters, then reaped. Only read APIs
are exposed there; ordinary Query ownership stays in the bridge. Responses cross
in bounded, acknowledged batches. Closing the owner cancels queued work and reaps
its process. This adds cold-read latency to keep the bridge responsive and release
the SDK's temporary parse heap, so it is used only where it pays: a transcript
under 8 MiB is read in the bridge (event-loop pauses under 5 ms, measured on
Node 24), and session summaries and agent file listings never start a process.

Projection and JSON fingerprint walks yield between short slices. Fingerprints
preserve the previous JSON bytes, cursor separation and `historyRevision` based
on the complete projected item list, including task-only changes. A native reset
or transcript change during the walk prevents caching the old projection. SDK
activation also invalidates caches and fences reads from the previous SDK lease.

## File and command protection

`permissions.mjs` decides every file tool and configures the command sandbox.

- **Agent configuration asks first.** In Accept edits and Auto, a write to a
  repository's agent configuration asks the owner: Claude Code's
  (`.claude/settings*.json`, hooks, agents, commands, skills, `.mcp.json`) and
  OpenCode's (`opencode.json[c]`, `.opencode/` or `~/.config/opencode/`
  agents, commands, plugins, tools, modes, skills), Cursor's (`.cursor/mcp.json`,
  `hooks.json`, `cli.json`, `environment.json`, commands, agents, skills) and
  Codex's (`.codex/config*.toml`, `hooks.json`, rules, prompts, agents, skills).
  Letter case, `..`, links to them and links named like them are judged by where
  they lead. Instruction files (`CLAUDE.md`, `AGENTS.md`, Cursor rules) stay ordinary
  edits.
- **Glob and Grep are judged by what they search.** Both run outside the command
  sandbox, so they are refused when the folder they search holds, or sits inside,
  a protected folder (Cupola's data, credentials, the keychains). Glob's pattern
  counts as the searched folder when it is absolute, starts with `~`, or climbs
  with `..`; `path` is trimmed and a leading `~` expanded first, as Claude Code
  does before it opens anything. File tools get the same trimming and `~`.
- **The command sandbox's write protection covers Cupola's data folder as a
  whole.** A conversation's own folder lives inside it, and the sandbox cannot
  exempt one folder from a denied one, so every other name in the folders on the way
  down is denied by pattern as well as by listing: entries created later (another
  conversation's folder, a file the host adds mid-session) are refused too.
  Seatbelt matches the patterns case-insensitively and has to end a range on a
  character it can spell; it silently ignores a range ending on a noncharacter.
  A folder name using `_` or characters beyond letters, digits, `.` and `-` is
  still covered by listing alone, and a new entry whose name starts with a control
  character is the one name no pattern can spell. `test/permissions.test.mjs` checks the patterns
  against the real macOS sandbox.

## Project session list

`project/sessions/list` answers from an index of each project folder's session
heads (`session-head-index.mjs`), not from a fresh SDK walk per page. The SDK
reads the head and tail of every transcript to list a folder, so proving the end
of a folder of 300 transcripts with 17 chats cost about 0.6 s on every page that
reached it. The index reads a folder once, then validates every entry against
its file's mtime and size on each request (a `readdir` and a `stat` per file; no
expiry): an added or grown transcript is read again, alone, and a deleted one is
dropped. Whether a transcript is a chat (its first prompt's entrypoint) is kept
per file version too. Folders the index cannot find under the normal name, or
larger than 20,000 files, are listed by the SDK as before. The unchanged cursor
format keeps working across restarts. Measure with `node bench/session-list.mjs`
(`--runtime DIR` for another checkout) or, through `threads.list`, the ignored
`thread_page_limit_sweep_over_a_real_claude_catalog` test in `wonderd`.

## SDK updates and the model catalog

`runtime-updates.mjs` activates a staged SDK patch once no turn holds the old
one. A new SDK can resolve the same aliases (`haiku`, `sonnet`) to newer models,
so on activation the bridge drops its cached inspection and sends
`runtime/activated`; wonderd rediscovers that runtime's catalog within about a
second. Its five-minute discovery poll stays as the fallback for changes the
bridge cannot report, such as a login made outside Cupola.

## Turn context

What Cupola tells Claude about the host for one turn (today the phone signal:
the owner wrote from the Cupola app, away from this computer's screen) arrives
on `turn/start` as `wonderContext`, a short list of lines. The bridge never adds
it to the owner's message: Claude Code receives it as `additionalContext` from a
`UserPromptSubmit` hook for that prompt only, so the transcript keeps exactly
what the owner wrote (the context is a separate `hook_additional_context`
attachment) and a kept process serves the next reply unchanged. Settings that
hold for the whole process, such as the owner's custom instruction, go in the
system prompt append instead.

## Native session command catalog

The bridge-local `thread/commands/list` request takes an existing Cupola
`threadId`. It is not a Claude SDK RPC. The daemon serves it to the phone's
composer `/` list as `GET /api/v1/conversations/{id}/commands`, passing the
availability through and never starting a session to read it.

| `availability` | Meaning |
| --- | --- |
| `ready` | Complete native `commands` array; `[]` is a known-empty list. |
| `unknown` | An owned Query has no complete current catalog, or its bounded read has not settled. |
| `unavailable` | No matching owned Query, including cold/desktop-only sessions, pending initialization, lost ownership or settings not yet applied. |

Responses include `sessionId`, ephemeral `ownerId` and owner-local `revision`.
An unavailable response has null owner/revision/commands; an unknown response
has null commands. `reason: "invalidCatalog"` identifies a rejected native
replacement, while other responses have null reason. Consumers must match
native session and owner identity, not compare revisions across process lives.

`session-commands.mjs` calls only the retained Query's `supportedCommands()`.
Concurrent callers share one operation. Each caller waits at most 250 ms; a
pending SDK operation remains shared until it settles. Discovery never creates
a Query, inspects another session, reinitializes, prompts, extends the idle timer
or writes history. Cold sessions stay unavailable until ordinary Send owns a
Query. SDK failures/timeouts stay unknown and can be retried.

Active and idle `system/commands_changed` events replace the entire list for
the exact native session. Native order, duplicate names, aliases, built-in
markers and other row metadata survive. Limits are 512 rows and 256 KiB per
catalog, with bounded fields/aliases; invalid replacements discard prior rows.
Newer pushes beat older reads. Query loss, native identity/configuration changes
and process replacement fence pending results. A no-op resume preserves them.
Native identity changes suppress the old SDK initialization fallback even across
same-Query settings updates, until a push for the new identity arrives.
Every top-level native `conversation_reset` immediately makes the catalog
unavailable, regardless of trigger or malformed metadata. Its
`new_conversation_id` is not used as a resumable SDK session ID. A subsequent
native init, root SessionStart(clear) hook, or correlated result can identify the
session. A fresh exact-session command push is still required; the old Query's
initialization fallback remains disabled after reset.

The source authority is SDK **0.3.283**: `SlashCommand`,
`SDKCommandsChangedMessage`, and `Query.supportedCommands()`, whose implementation
returns the latest pushed catalog or initialization list. See the
[official command documentation](https://code.claude.com/docs/en/agent-sdk/slash-commands#commands-in-agent-sdk-sessions).

## Edit from here (rewind)

`thread/rewind {threadId, turnId, restoreFiles}` cuts the conversation back to
before the owner's message that opened `turnId` (the prompt's transcript uuid).
Claude Code keeps its transcript: the bridge records the last main-chain entry
before that prompt and the next reply resumes with `resumeSessionAt` at it
(SDK `Options.resumeSessionAt`, sdk.d.ts), so Claude drops the message and
everything after from its context. Until Claude answers on the new branch,
history reads end before the message; once the transcript no longer contains
it the cut is complete. A rewind of the first message starts a new native
session (returned as `sessionId`; the daemon moves the binding) and leaves the
old transcript on disk. Refused while a reply runs, while Claude on the Mac has
the session open or busy, and before the latest compaction.

File restore is opt-in. The owner's Settings switch makes each new Claude
process start with `enableFileCheckpointing` (off by default) and marks the
turns it runs `checkpointed`. `restoreFiles` needs every removed turn to be
checkpointed; it then runs `Query.rewindFiles` in a process that only resumes
the session (no prompt, no settings or hooks, `persistSession: false`): a dry
run first, then the restore. Any refusal changes neither files nor the
conversation. The result reports `filesRestored` only when Claude restored
files. Live behaviour of `resumeSessionAt` and `rewindFiles` is not yet
measured against the installed runtime; tests use the fake SDK.

## Native clear and compaction

`/clear` is executed by Claude Code through ordinary native input. Cupola never
matches command text to emulate it. SDK 0.3.283 declares a top-level
`conversation_reset` (not `system/conversation_reset`); its `new_conversation_id`
is a display identifier, separate from the resumable session UUID. The pinned
2.1.283 native clear implementation generates those identifiers independently,
rotates its native root session, and invokes SessionStart(source=clear). Hooks
can be disabled or deferred, and an init is not guaranteed. The bridge therefore
accepts the owned root hook or subsequent correlated result/init as native
identity authority after a reset. A child hook/init cannot rebind the parent.

The hook first persists pending identity in the existing session journal. It
cannot wait for stream completion: native execution itself waits for the hook.
The old turn flushes before the bridge requests `thread/session/reset`, an
internal daemon acknowledgment with a five-second wait. One guarded SQLite
transaction updates the exact existing Claude binding and canonical history
visibility/revision, invalidates in-flight history hydration, and retires old
questions/approvals. It creates no context handoff. Duplicate acknowledgment
replay is idempotent. The bridge journals completion before releasing Send;
restart, timeout or lost acknowledgment retains pending intent and refuses new
input against the old binding. Delivery receipts, queued messages and an
already accepted Guide remain durable. The original native transcript remains
on disk and discoverable by Claude's native session readers.

Snapshots carry `historyRevision`; paging and client cache merges cannot cross
that reset. Late events from hidden turns are discarded before live publication.
The retained Query's catalog waits for the authoritative new native identity
and its command push without using old initialization metadata.

Only clear's terminal reset boundary is established here. Other or unknown
triggers (including plan_mode_exit, fresh_session and onboarding), malformed
reset metadata, and missing native identity retain the original journal and
native transcript files and keep new input unavailable. In particular Cupola
does not guess how a reset divides a currently streaming reply. Open such a
conversation in Claude on the Mac; no picker should advertise these unsupported
transitions. A triggerless reset is treated as clear only when Claude supplies
its documented clear-specific `user_message_uuid`.

`/compact` keeps the same native session and visible history. The SDK
`compact_boundary` invalidates the context measurement; existing native history
chain readers retain earlier turns. A successful compact command without a
boundary does not imply compaction occurred.

All verification here is synthetic streams/files and native SDK read-only
readers. No real Query, control probe, prompt, model call or physical device
acceptance was performed for this slice. Invocation remains ordinary native
text; a command picker and a new invocation endpoint are separate work.

Run the synthetic runtime contracts with `npm test` from this directory.

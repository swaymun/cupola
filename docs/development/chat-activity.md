# Native message and activity rendering

The parsing layer maps runtime user-message echoes to the canonical submission
using client IDs (or an unambiguous legacy turn match). It preserves submitted
text and timestamps. Matching text is not a deduplication rule: two identical
intentional sends remain two messages. The native cache retains the daemon's
sanitized typed-item payloads for activity rendering.

Adjacent activity items from the same turn and speaker form a collapsible group.
Replies, user messages, and approval items break groups. Each action has its own
expandable details. Commands, searches/page actions, file-change summaries,
MCP/dynamic tool calls, tool outputs, and plans have presentations. Unknown items
have an honest fallback. Reasoning items show status without displaying hidden
reasoning content. Tool output is selectable inert text, not HTML or executable
controls. Existing question and permission controls stay separate.

Failed actions expand automatically; closing the activity group leaves error
text visible. Running actions show progress. Interrupted and unknown states are
not presented as completed. Large output has an explicit full-output disclosure.
The rendering operates on typed Project-thread rows.

## Validation (2026-09-06)

- 30 native tests passed for the parsing PR, including ID reconciliation,
  intentional repeated messages, cache/reload/history merging, and payload retention.
- 36 native tests passed with activity presentation, including command/file
  details, web page actions, failure inference, grouping boundaries, interrupted
  states, unknown types, and inert tool results.
- The iOS simulator build passed for both supported simulator architectures.
- iPhone 17: single canonical user bubble, collapsed/expanded groups, tool-result
  disclosure, and automatic failure expansion checked in the running app.
- iPad (A16): running activity and action list checked in dark mode.

Screenshots in `evidence/chat-activity/` use explicit synthetic fixtures, not a
live model/tool execution. The fixture exercises the real cache-to-row-to-view
path. These checks do not claim physical-device or live-tool streaming acceptance.

Reproduce using the debug simulator build with `-read-preview -activity-preview`.
Add `-activity-failure-preview` or `-activity-running-preview` for those states.
The existing preview mode disables network and production-storage writes.

## Broader transcript checks (2026-09-06)

Inspected the 25 most recently modified September JSONL transcripts under the
local Codex sessions directory. Only shape/count summaries were used; no private
transcript text, images, audio, IDs, or paths were copied into fixtures.
The sample contained 410 assistant commentary messages and 149 final answers;
tool output included 2,426 block arrays, 312 strings, 546 image blocks, and three
audio blocks. These are observations of a local sample, not exhaustive coverage.

Session `response_item` records are not the App Server wire format. The checked-in
0.153.0-alpha.5 experimental schema verifies the translation boundary: assistant
speech is `agentMessage` with optional `phase`; dynamic outputs use camel-case
content tags, while function outputs and MCP content have their own unions.
Fixtures use invented content in those shapes. Developer/system messages and
private reasoning are not reclassified as visible assistant commentary.

Fixes and checks:
- Equal timestamps retain protocol order in the daemon and native client;
  merging older pages retains turn order instead of dictionary iteration order.
- Commentary retains its phase through cache and live-text reconciliation,
  including lifecycle events arriving before or after durable text. Unknown
  phases are no longer invented as final answers. Commentary
  remains visible as compact agent speech, and separates action groups.
- Tool output extracts readable text from content arrays. Image/audio/resource
  results get explicit unavailable-preview placeholders instead of encoded data.
  Structured-only and plain-text results still display; MCP error results fail.
- 33 parsing-layer native tests, 42 combined native tests, and all 73 daemon tests
  passed. The iOS simulator build passed. The new iPhone screenshots exercise
  commentary before actions and a mixed text/image result in the real renderer.

Reproduce the new synthetic screen by adding `-activity-mixed-preview` to the
preview arguments above. This is still not a live-model acceptance run, and media
previews/playback remain unimplemented.

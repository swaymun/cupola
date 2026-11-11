# Native persisted search

`GET /api/v1/search?q=needle&limit=30&conversationId=chat-id&cursor=opaque`

The existing owner-authenticated API boundary applies. `q` is required (1–256
bytes after trimming). `limit` is 1–100, default 30. `conversationId` optionally
narrows results; it never grants access. Omit `cursor` for the first page.

```json
{
  "results": [{
    "kind": "message",
    "id": "matching-message-id",
    "title": "Conversation title",
    "snippet": "Text with a <mark>needle</mark> match",
    "conversationId": "chat-id",
    "updatedAt": "1788932400000",
    "deepLink": "#/chats/chat-id?focus=matching-message-id&kind=message"
  }],
  "nextCursor": null
}
```

Use `(kind, id)` as result identity. Open `conversationId` and focus `id` using
`kind`; native clients need not parse the legacy web `deepLink`. Snippets use
`<mark>` delimiters around matches, not trusted HTML. Retain existing matching
semantics: up to eight alphanumeric prefix terms, all terms required.

## Native history

An unscoped search also covers threads and sessions started on the Mac outside
Cupola, in included Projects' folders (exact folder match), as each provider's
`historySearch` capability says:

- `runtime` (Codex): the App Server's own index, `thread/search`, sorted by
  `updated_at`. One result per thread, kind `thread`, id the native thread.
  Helper (child) and ephemeral threads are skipped. A runtime without the
  method answers unknown method; search then covers Cupola's own messages.
- `transcriptIndex` (Claude Code): a local SQLite FTS5 cache of
  `~/.claude/projects/<folder>/<session>.jsonl` at
  `<data dir>/native-search.sqlite3` (mode 0600, owner routes only, never sent
  elsewhere; delete it any time and it is rebuilt). One result per prompt or
  reply text block, kind `native_message`, with `itemId` the timeline item
  (`<prompt uuid>` or `<turn>:<message id>:<block>`). Only owner prompts and
  reply text are indexed: no tool input or output, thinking, harness notes,
  subagent transcripts or lines over 1 MiB. Bounds: 16 KiB per block, 2 MiB per
  transcript, 128 MiB overall, the newest 2000 transcripts. A refresh reads
  only lines appended since the last one (size and mtime per file; a shrunk or
  older file is read again), at most 20 s per run in the background; a search
  waits up to 1.5 s for it. It runs only while Claude is configured.
- `none` (OpenCode): not searched.

Native results carry `projectId`, `reference` (`codex:<id>`, `claude:<id>`)
and `family`. When the session is already bound to a Cupola conversation,
`conversationId` is set and the result opens that chat; otherwise the client
attaches it with `POST /api/v1/projects/{projectId}/threads/attach
{"reference": ...}` (no model work) and opens the returned conversation.
A bound session whose conversation already has any Cupola match for this
query, is archived in Cupola or is in an excluded Project is not returned,
so a thread never appears both as a Cupola chat and as a native result.

`GET /api/v1/conversations/{id}/search/occurrences?q=` lists matches inside one
Project conversation's native history, oldest first (Codex
`thread/searchOccurrences`, Claude the cache), as `{"supported": bool,
"occurrences": [{"itemId", "turnId", "snippet"}]}`. Clients use it to scroll a
`thread` result to its message after opening; `supported: false` opens the
chat at its usual place.

## Order and cursor

Results merge newest first by timestamp; a tie goes to Cupola, then the
runtime, then the cache. The cursor holds one position per source (version 2);
a version 1 cursor returns 400. A page can be shorter than `limit` (even empty
with a `nextCursor`) when a runtime page held only results outside the
included Projects; continue with the cursor. `partial: true` means a source
could not be read completely this time (a runtime error or timeout, a cache
still being built); results may be missing and a later search can find them.
Within Cupola's own store, order is normalized persisted timestamp descending,
then kind and ID descending. Continue with the same query and conversation
scope; cursor is opaque and bound to both plus the current host epoch.
Invalid/mismatched cursors return 400; restart from the first page. A null next
cursor marks the final page. A conversation-scoped search reads Cupola's store
only.

Each page reads current persisted content in one transaction. This is a live
view, not a frozen multi-page snapshot: edits/deletions and changed visibility
apply to the next request. Refresh from page one after relevant sync changes to
see newly matching or reordered records.

Only threads in included Projects appear. Archived Project threads and provider
child conversations are excluded before pagination. Thread titles, persisted
messages and file names are searchable; runtime events/tool payloads are not
indexed. Explicit conversation scope cannot bypass those filters.

Cupola's own small FTS projection is still rebuilt per query. Pagination bounds
response size, not total index rebuild cost.

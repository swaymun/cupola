# Message rendering audit — RENDER-SANITIZE

Measured read-only on 2026-11-02. All examples and fixtures are synthetic. Source conversations and provider files were not edited, archived, attached, or copied. No model sends, physical-device actions or T3 MCP calls were made.

## Inventory and limits

- Local transcript scan: 852 Codex JSONL files discovered (this worker’s one session excluded) and 2504 Claude JSONL files; 106.13 seconds. Reads are bounded to each file’s size when opened. Fork/sidechain copies remain separate stored records, so these are not unique-message counts.
- Cupola: 26 unique attached chats across 19 Projects via `cupola rpc` (`threads.read`, newest 100 turns requested). 7 chats have earlier pages outside this sample; 40 listed unattached sessions were not attached. Listing repeated the same attached chat across Projects.
- T3: two chats positively identified through Cupola’s routing evidence; their 24 native message records (23 assistant, one user) form the T3 subset below. Their provider is Claude. One read-only Cupola activity request failed; that thread’s holder remains unclassified. No direct T3 database/transcript/MCP access was used.
- Raw Codex `response_item` messages are counted once; mirrored `event_msg` records are excluded. Claude text blocks, metadata-marked messages and tool results are separated. Tool output may itself contain serialized JSON; counts describe stored text, not every nested decoded field.
- Counts are **records containing a pattern**, not regex matches or confirmed bugs. Most tags, IDs, JSON, entities and repeats are legitimate examples. Activity and legacy/outbound mirrors are available separately in `counts-rpc.json` and are not added to message totals.
- The projected Cupola sample has no escaped pipe in an actual table; the raw Claude assistant corpus has 14 such records. The parser defect is independently reproduced by the synthetic fixture. Raw markup occurrences alone are not evidence that all of them were displayed incorrectly.

## Findings table

C = Codex; Cl = Claude. Raw message columns combine user and assistant text; metadata and tool columns remain separate. T3 is a subset of Cupola Cl, not an additional total. Cursor and OpenCode projected message records match none of these patterns.

| Pattern | Raw C messages | Raw Cl messages | Cl metadata | C tools | Cl tools | Cupola C messages | Cupola Cl messages | T3 via Cupola Cl | Synthetic example | Layer / disposition |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- | --- |
| XML-like tags | 272 | 648 | 3883 | 1018 | 2354 | 0 | 1 | 0 | `<system-reminder>sample</system-reminder>` | Preserve literal user content; existing provider projection handles typed metadata. |
| Memory attribution trailer | 187 | 0 | 0 | 0 | 1 | 0 | 0 | 0 | `<oai-mem-citation>…</oai-mem-citation>` | Host: retain human attribution notes, omit envelope and rollout IDs. |
| ANSI CSI | 0 | 0 | 3 | 2 | 333 | 0 | 0 | 0 | `ESC[32mChecks passedESC[0m` | Host: existing CSI handling retained; C1 and other controls extended. |
| ANSI OSC | 0 | 0 | 0 | 0 | 34 | 0 | 0 | 0 | `ESC]0;titleBEL` / `ESC]8;;urlSTlabelESC]8;;ST` | Host: consume complete control strings, preserve printed label. |
| Other ANSI/C1 | 0 | 0 | 0 | 1 | 36 | 0 | 0 | 0 | `ESC(BplainESC7 textESC8` | Host: charset/cursor/string controls removed. |
| Tool-call JSON candidate | 0 | 0 | 0 | 1 | 53 | 0 | 0 | 0 | `{"tool_call_id":"call_synthetic123"}` | Preserve text; typed tool calls already stay in activity. |
| IDs in text | 132 | 329 | 3490 | 1766 | 12097 | 0 | 0 | 0 | `00000000-0000-4000-8000-000000000001` | Only known memory trailer IDs transformed; arbitrary IDs preserved. |
| Adjacent repeated paragraph | 0 | 0 | 1 | 4 | 38 | 0 | 0 | 0 | Two identical paragraphs | Preserve; text equality is not provenance. |
| Markdown table | 378 | 839 | 127 | 200 | 1674 | 1 | 3 | 0 | A two-column header | Native parser: validate separator shape/column count. |
| Escaped pipe in a table | 1 | 15 | 3 | 2 | 128 | 0 | 0 | 0 | A code span containing an escaped pipe | Native parser: keep cell intact, including code spans/trailing pipes. |
| Nested list | 29 | 2143 | 311 | 272 | 2180 | 3 | 8 | 1 | `1. Prepare` then `   - Nested` | Existing hierarchy preserved. |
| Indented code fence | 0 | 5 | 15 | 60 | 405 | 0 | 0 | 0 | Four-space-indented Swift fence under a list | Native parser: recognize list-contained fence and remove container indentation. |
| Unclosed fence candidate | 0 | 2 | 5 | 28 | 201 | 0 | 0 | 0 | An unfinished streaming Swift fence | Existing code fallback preserved; every streaming prefix regression checked. |
| HTML entity | 922 | 12 | 1161 | 37 | 338 | 0 | 0 | 0 | `Fish &amp; chips; &lt;tag&gt;` | Existing inline renderer decodes prose; code/copy preserve source. |
| Line at least 1,000 characters | 213 | 1685 | 91 | 16537 | 3491 | 0 | 1 | 0 | 1,120 synthetic hexadecimal characters | Preserve; wrapping/scrolling/copy covered by the fixture flow. |
| Inline base64 candidate | 0 | 1 | 0 | 104 | 106 | 0 | 0 | 0 | Synthetic `data:image/png;base64,QUJD…` source | Preserve literal source; no image fabricated from a guessed payload. |

## Fixes and preservation boundaries

- **Measured:** host fixture cases cover OSC BEL/ST links and titles, C1 CSI/OSC, DCS/clipboard/image control strings, incomplete controls (an unterminated string control ends at its line, so later lines of a finished message stay visible), charset selection/cursor escapes, UTF-8 limits, and idempotent sanitization. Only escape controls are consumed; their printed labels and textual `\x1b` examples survive.
- **Measured:** complete assistant-only Codex memory attribution trailers become “Memory sources” with their human notes. User messages, fenced examples, malformed trailers and trailers followed by prose retain their original text. Source transcripts are unchanged. Typed live/history items use the host display function. Native row preparation handles raw legacy/cache streams without changing the snapshot text to which future deltas append.
- **Measured:** shared native Markdown fixture tests cover escaped table pipes inside code spans, a pipe at the end of a cell, even-backslash delimiters, tables without outer pipes, separators with one or more dashes per cell (`|-|-|`, `|:-:|`, `|:--|--:|`), malformed separator rows and pipe-only prose, nested fences, unfinished fences and every streamed prefix. Literal XML/JSON, repeated paragraphs, long/base64 strings and entity semantics retain content.
- **Inferred:** these changes address the same shapes in real histories; no edited host was installed against the owner’s live history. The read-only audit used the installed host.
- No blanket XML, JSON, ID, base64 or duplicate-text filter was added. Unmarked T3/Claude text is not reclassified as an internal notification. Existing upstream Claude command/harness handling is outside this change; this work does not claim a complete provenance repair.

## Reproduce

```sh
python3 tests/fixtures/protocol/render-sanitize/audit.py --source transcripts --exclude-cwd "$PWD"
python3 tests/fixtures/protocol/render-sanitize/audit.py --source rpc --threads-per-project 30
cargo test -p wonderd rendering_fixtures_preserve_content_and_remove_transport_markup --lib
swift test --package-path apps/native --filter MarkdownBlocksTests
scripts/wonderctl run apps/ios/control/scenarios/render-sanitize.wctl
```

Audit output contains counts and fixed pattern names only. The two checked-in count files are the observed snapshots, not tests asserting today’s history counts.

## Visual evidence contract

The fixture-only baseline is `6f8fb32ae4216d9db0931bdee43f998e26b32bc5`. Five focused states show one synthetic chat’s Markdown, literal paste, terminal output, memory attribution and long/base64 source. All before/after states use identical raw synthetic input. Terminal/attribution states exercise native legacy/cache presentation; the same cases independently test host typed-item projection in `host-cases.json`. Screenshots do not prove a newly installed live host. `preview.json` waits for the actual message bubble on each native simulator.

## Expanded activity details

The same read-only Cupola sample also examined known display fields (`output`, `aggregatedOutput`, `stdout`, `stderr`, `detail`, and textual tool results). Counts remain separate from chat messages.

| Provider surface | Detail records | XML candidates | IDs in detail | Long lines | ANSI / residual OSC candidates |
| --- | ---: | ---: | ---: | ---: | ---: |
| Cupola Codex | 135 | 0 | 2 | 2 | 0 |
| Cupola Claude | 294 | 3 | 31 | 8 | 0 |
| Cupola Cursor | 1 | 0 | 0 | 0 | 0 |
| T3 via Cupola Claude (subset) | 67 | 0 | 11 | 0 | 0 |

No OSC payload leak was found in this bounded projected sample. Old derived records that have already lost their ESC bytes cannot be distinguished safely from genuine `]0;...` text; their original provider history must be rehydrated to repair them. This branch changes projection and display, not the owner’s stored conversations.

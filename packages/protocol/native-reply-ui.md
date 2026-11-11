# Native reply UI, catalog v1

An agent can include a native interface in an ordinary reply. iPhone and iPad
render a fixed SwiftUI catalog; other clients can show the fenced JSON as text.
No daemon changes, separate tool result, HTML, WebView, scripts, URLs, expressions,
runtime bindings, remote images or automatic actions are part of this contract.

## Transport and authoring

Use a complete Markdown fence named exactly `wonder-ui`, with a single JSON
object matching [wonder-ui-v1.json](schemas/wonder-ui-v1.json). It may sit between
ordinary Markdown paragraphs. The fence opens/closes on its own line, with at
most three leading spaces. Its closing delimiter is exactly three backticks.
Quoted, four-space-indented and outer-fenced examples stay ordinary text.
Incomplete streaming fences stay code until the closing delimiter arrives.

````markdown
Here are the options.

```wonder-ui
{
  "version": 1,
  "fallback": "Riverside takes 45 minutes. Hilltop takes 70 minutes.",
  "components": [
    {
      "id": "routes",
      "type": "table",
      "title": "Walking routes",
      "columns": ["Route", "Time"],
      "rows": [["Riverside", "45 minutes"], ["Hilltop", "70 minutes"]]
    },
    {
      "id": "shorter",
      "type": "button",
      "title": "Draft a shorter route",
      "message": "Suggest a shorter riverside route."
    }
  ]
}
```
````

Supply a useful `fallback`, including the facts someone needs when rich rendering
is unavailable. All catalog strings are literal text, including HTML-looking
text. Use normal Markdown outside the fence for links and long explanations.
Keep component IDs stable when revising a reply; use distinct IDs within each
document. Supported IDs contain only ASCII letters, digits, `_` and `-` (1–64
characters). Version 1 is exact, not a minimum. New component types may join v1 (older apps
show their fallback); any other change needs a new version. Unknown versions, types and properties fail closed to text.

The transport reuses existing authenticated message storage, history, paging,
copy/export and revision behavior across providers. A separate tool result would
require provider-specific translation and protocol/storage migration without
adding authority or safety here. No provider tools are invoked by rendering.
Agents producing v1 should read this contract or receive its catalog in their
task instructions; this change does not modify global provider prompts.

### Visuals

Agents Cupola starts also have the `wonder_show_visual` tool. Its `components`
argument is one v1 document (without `button` or `form`, which act only in a
reply), validated on the Mac against this schema, kept as a verified
`application/vnd.wonder.ui+json` conversation file and drawn by the same
decoder as a reply's block. Its `html` argument is a page the phone converts
natively (literal colours become the nearest theme tone). Either shows inline
with Expand, like T3 Code's `html_render` pages.

## Catalog

Every component requires `id`, `type` and `title` (1–120 characters), plus the
properties below. JSON string limits count Unicode code points, as JSON Schema
does. No extra properties or arbitrary nesting are allowed.

| Type | Required properties | Native behavior and limits |
| --- | --- | --- |
| `card` | `body` | Title and selectable body text, at most 2,000 characters. Uses the existing agent bubble without another decorative card. |
| `table` | `columns`, `rows` | 1–8 column names, 1–20 rows; each row has exactly the column count, each cell at most 256 characters. Empty cells are allowed. Always a grid with a header row, the same as Markdown and page tables: cells wrap to three lines (a longer one opens in full on tap); when the columns can't fit, the table scrolls sideways with its first column pinned. VoiceOver reads each row with its column names. |
| `list` | `items` | 1–20 literal strings, at most 256 characters each; native markers and wrapping. |
| `keyValue` | `entries` | 1–20 `{ "label", "value" }` pairs. Labels at most 120 characters, values at most 512. Values stack below labels when needed. |
| `chart` | `bars` | Horizontal comparison chart with 1–12 `{ "label", "value" }` bars. Labels at most 120 characters. Finite numbers from 0 through 1,000,000,000. Native shapes scaled to the maximum (minimum domain 1); every number remains visible/spoken. Put units in the title. |
| `progress` | `value`, `label` | Finite fraction from 0 through 1; a literal explanation (at most 256 characters), percentage and native bar. No timer or simulated work. |
| `button` | `message` | Title labels an explicit draft action; literal proposed user message at most 2,000 characters. |
| `form` | `fields`, `submit`, `message` | 1–8 fields with unique IDs; submit label at most 120 characters and literal proposed user message at most 2,000. See actions below. |
| `diagram` | `steps` | A vertical flow of 2–12 text nodes connected by native arrows. Each node at most 256 characters. V1 supports ordered flows, not arbitrary graphs or Mermaid. |
| `chart` (series) | `labels`, `series`; optional `kind` | Swift Charts `bar` (default), `line` or `area`: 1–24 distinct category labels (at most 60 characters) and 1–6 `{ "name", "values" }` series with one finite value (±1,000,000,000) per label. Counts labels × series items. A chart has either `bars` or `labels`/`series`. |
| `callout` | `body`; optional `tone` | A note with a tone icon and tint; `tone` defaults to `info`. Body at most 2,000 characters. |
| `stats` | `items` | 1–8 tiles `{ "label", "value", "delta"?, "tone"? }`: value and delta at most 40 characters, label at most 120. |
| `steps` | `steps` | 1–12 `{ "title", "detail"?, "status" }` with status `done`, `current`, `pending` or `failed`; a marked timeline, status also spoken. |
| `checklist` | `items` | 1–20 `{ "text", "done" }` (Boolean); read-only ticks, not controls. |
| `badges` | `items` | 1–12 `{ "text", "tone"? }` labels (at most 60 characters) that wrap. |
| `mockup` | `aspect`, `regions`; optional `direction`, `overlay`, `badge`, `caption` | A layout sketch: a frame of aspect 0.25–4 split `horizontal` (default) or `vertical` into 1–6 regions in proportion to `weight` (0.05–10, default 1): `pane` (`title`/`label`, `tone`), `chat` (`bubbles`: up to 8 of `agent`/`user`, `composer`), `lines` (`count` 1–8) or a thin `divider` (`glyph`, at most 3 characters). `overlay` puts a `sheet` card or `full` cover (a pane or chat, optional `dim`) over it; `badge` pins one label. VoiceOver reads one description. |

`tone` is one of `neutral`, `accent`, `success`, `warning`, `error`, `info` or
`series1`–`series6`: a meaning the reader's theme colours (status colours are
derived per theme at 4.5:1 on the bubble and page), never a colour. Component
types added after the first release keep `version` 1: an older app shows such a
document's fallback, as for any unknown type.

Form field catalog:

| `kind` | Required properties | Behavior |
| --- | --- | --- |
| `text` | `id`, `kind`, `label`, `required` (Boolean) | Native text field, initially empty, at most 512 characters. A required answer must contain non-whitespace text. |
| `toggle` | `id`, `kind`, `label` | Native toggle, initially off. Produces Yes/No in the proposed user message. |
| `select` | `id`, `kind`, `label`, `options` | Native picker with 1–12 distinct nonempty options (at most 120 characters each), initially the first option. |

Field labels have the same 120-character limit and IDs the same syntax as
component IDs. Form values are transient until the user adds them to the draft;
revision changes clear the old values. Do not request passwords or credentials.

## Actions and authority

Button taps and form submits call the existing composer intent owner to fill an
**empty, loaded draft**. They do not send, queue, invoke a tool, open a URL or
execute provider work. The owner reviews/edits the normal message and taps the
ordinary Send control, with its existing persistence and delivery feedback.
An occupied draft, pending message or attachment is preserved and gets inline
recovery feedback. A failed save keeps the proposed text in memory, blocks Send
and explains the storage failure. Repeating the same proposal can retry that
failed save; a different proposal cannot replace the unsaved text. Read-only
conversations disable actions and explain why. Successful insertion says “Added to your draft. Review and send.”

A form produces `message`, a blank line, then one `label: value` line per field
in catalog order. Submitted values are revalidated at this boundary. There is no
template interpolation, action name, hidden argument, automatic submission or
privileged tool-call route. Any later tool call uses the normal provider approval
path after the owner sends the message. Labels should make the draft effect
clear, e.g. “Draft my preferences”.

## Validation, cost and fallback

- A document is at most 32,768 UTF-8 bytes with JSON nesting at most 16.
- A document contains 1–24 components; a message scans at most four documents.
  A fifth block and the remaining text become one bounded text preview with Copy
  recovery. This also bounds malformed fallback stacks and unfinished blocks.
- A message activates at most 24 components and 80 data items in total. A card,
  progress or button counts as one item; tables count cells; lists, key-value,
  charts, forms and diagrams count entries, bars, fields or nodes respectively.
  Documents also enforce the 80-item limit independently. This limits native view
  construction even when many small values fit inside the byte limit.
- Messages over 131,072 UTF-8 bytes with a native fence become a readable size
  explanation and a bounded source preview, with full text retained for Copy.
- Unknown/malformed/oversized input never partially activates controls. A valid,
  bounded fallback string is shown when available; otherwise a readable failure
  explanation is shown. “Show reply text” reveals at most 4,000 source characters.
- The JSON Schema validates structure; the Swift decoder additionally enforces
  UTF-8 byte size, nesting, ID uniqueness and table row width. Both reject unknown
  properties. Schema `examples` contains one document using every component type, including both chart forms.
- A dedicated worker actor scans/decodes/prepares numeric values. Exact message
  UTF-8 text is the revision key (Swift's normalizing string equality is not used).
  The cache retains at most 48 revisions and 1 MiB of
  source keys, clears on memory pressure, and never writes durable history.
  While a new revision is prepared, the last prepared reply remains visible
  with its actions disabled; only the first preparation shows a progress label.
  Stale preparation cannot expose the previous revision's controls. Fence and
  revision metadata are prepared at row input construction. View bodies
  consume validated values; there is no parsing, network, image decode or graph
  layout in a body. Dynamic Type wrapping is native layout work.

## Verification owners

`apps/ios/SnapshotTests/PresentationSnapshotTests.swift` compiles the exact
`NativeReplyUI.swift` parser/renderer in the independent Mac host package. It
owns catalog validation, mixed/streaming fences, fallback/limits, draft generation,
revision caching, retained rendering/height across streamed deltas, stale-task
rejection and four pixel variants per component (light/dark × standard/AX3).
Mac semantic fonts do not scale with Dynamic Type, so those fixtures supply the
same explicit large metrics as the existing presentation tests. Form inputs use
prepared control stand-ins because AppKit controls cannot enter ImageRenderer;
host form snapshots protect field layout, not UIKit input behavior.

The Diagnostics-only `-diagnostics-genui` thread contains the complete catalog.
With `-diagnostics-genui-component <type>` it selects one entry for a bounded
capture. It uses synthetic offline conversation data and starts no model work.
[genui-native.wctl](../../apps/ios/control/scenarios/genui-native.wctl) checks the
actual Testing app, UIKit controls and composer behavior; see
[CONTROL.md](../../apps/ios/CONTROL.md). Native simulator screenshots and clips
are separate from host PNG comparisons. Physical devices and provider-generated
live examples require their own evidence; neither is claimed by host snapshots.
The scenario selects the existing `light-modern` and `dark-modern` themes to
force both appearances; a launch-default `AppleInterfaceStyle` did not change
the iOS trait environment. Text, picker and switch values must reach the draft.

Inspiration: [json-render's fixed catalogs](https://github.com/vercel-labs/json-render)
and [T3 Code](https://github.com/pingdotgg/t3code). No dependency or code is imported
from either project.

---
name: wonder-phone-replies
description: Answer someone reading on their phone through Cupola with readable prose and, when useful, the native wonder-ui component catalog. Use for comparisons, charts, flows and small draft forms in a Cupola reply, not for building app screens or websites.
---

# Replies in Cupola

The user is reading in Cupola on an iPhone or iPad. They cannot see this
computer's screen, editor, Finder or terminal from there.

## Show, don't send them somewhere

- Do not end with "open this file", "check your browser" or "run this
  command" as the way to see a result. Show the result itself: quote the
  lines that matter, summarise the change, or include the output they need.
- For something visual (a UI, a chart, a rendered page), take a screenshot.
  Cupola can open files inside the Project's folders, so save it inside the
  Project (somewhere it will not be committed by accident) and give its path
  relative to the Project.
- When Cupola's `wonder_show_visual` tool is offered, use it for a visual
  explanation, comparison, plan, mockup or dashboard: a wonder-ui document
  (`components`) for simple data, an HTML page for layout. Fix what its result
  reports, and pass `replace` to update a visual instead of adding another.
  The visual does not replace the reply; still answer in text.
- A link must open on the phone. `localhost` and `127.0.0.1` do not; use the
  `wonder-share-to-phone` skill when it is available, or a public address.
- When the user has to act on this computer, say so plainly and say what
  they can still do from the phone.

## Keep it readable on a narrow screen

- Lead with the outcome in a sentence or two, then the detail.
- Short paragraphs and short lists. Keep table values short; use labeled records
  for richer comparisons.
- Keep code blocks to what the user needs, with short lines; long lines wrap
  badly on a phone.
- Name files by their path inside the Project rather than full absolute
  paths.

## Choose the presentation by the reader's task

Use ordinary prose for a direct answer, uncertainty, a short explanation or a
link. Use native components when their structure makes information easier to
compare, follow or answer. A simple fact does not need a card. Do not turn every
reply into a dashboard or show a component just to demonstrate the catalog.

Lead with the answer or recommendation when it adds a conclusion; a component's
title can lead a simple collection of facts. Start with one primary component.
Add another only for a distinct reader need. Put evidence before an optional
next step. Use the same supplied facts whichever presentation you pick;
do not invent measurements, dates, status, recommendations or progress to fill UI.

| Reader needs to… | Choose | Composition judgment |
| --- | --- | --- |
| Read one self-contained notice or conclusion | `card` | Short title and body; use prose if a title adds nothing. |
| Compare several things by the same attributes | `table` | Use 2 columns for short scalar values. For richer comparisons, 3–4 columns become labeled values, as do all tables at accessibility sizes. Keep identity in the first cell of each row. |
| Scan actions, requirements or independent points | `list` | One concrete thought per item. Its markers are bullets, not interactive checkboxes. |
| Inspect named facts about one thing | `keyValue` | Labels carry meaning; values can stack below them. Good for dates, ownership, limitations and mixed units. |
| Compare nonnegative quantities with the same unit | `chart` (`bars`) | Horizontal bars; title names the unit. Keep exact values and explain the takeaway. |
| See a trend or compare a few series over the same categories | `chart` (`labels` + `series`) | `line` for change over ordered categories, `bar` for grouped comparison, `area` for a cumulative amount; 1–6 series, one unit. No pie or dual-axis chart. |
| Notice a warning, success or caveat | `callout` | One short note with a `tone`; prose when the tone adds nothing. |
| See a few headline numbers | `stats` | 2–6 tiles; a `delta` only when the change is real and stated. |
| Follow work with a status per step | `steps` | Each step named with `done`, `current`, `pending` or `failed`; use `diagram` when there is no status. |
| See what is done and what remains | `checklist` | Read-only ticks; not something to tap. |
| Scan short statuses or tags | `badges` | A few short labels with tones; not a sentence. |
| See a screen layout or arrangement | `mockup` | A sketch of areas (panes, a chat, a sidebar, a divider) in proportion, optionally a card or cover over it and one status label; a caption says what it shows. One idea per sketch. |
| See known completion against a known total | `progress` | Supply a fraction and the numerator/denominator in the label. Unknown completion stays prose or named statuses. |
| Follow an ordered process or causal chain | `diagram` | A short vertical sequence with arrows. See diagram rules below. |
| Continue with one useful follow-up | `button` | A concrete “Draft…” label; tapping only fills the composer. Usually one is enough. |
| Supply several related preferences together | `form` | Usually 2–4 fields, with a clear draft submit label. For a single decision, use the native question tool when available. |

Do not make a chart from categories alone, from mixed units, or from missing or
negative values. Use a table for exact lookup, differences, ranges or a time
series when bars would imply the wrong relationship. Zero is valid data; missing
is not zero. Keep uncertainty visible in prose or labeled values.

## Layout, density and accessibility

- Components form one vertical reading order. There are no columns, grids,
  nested cards, custom spacing, colors, font sizes or alignment properties to set.
  The app owns styling and responsive layout. Let its existing bubble do the
  framing; do not draw boxes with Unicode or add decorative separator components.
- Give each component a short, specific title, such as “Travel time (minutes)”.
  Avoid repeating the introduction, title and first row. Use one main comparison
  or explanation per component. Keep qualifications beside the fact they qualify.
  When a flow already explains who acts and when, do not add another card or
  table repeating the same authority warning. Include the actor in the step.
- Start with roughly 3–7 useful rows/items/steps when the task permits; this is a
  density target, not a limit that permits dropping requested facts. Put long
  reasoning in Markdown after the structured overview. For a large dataset,
  explain the selection and provide the rest as text or an actual artifact.
- Use brief, complete labels that still work when values wrap below them. A table
  must remain understandable as consecutive labeled rows, without visual column
  position. Do not align content with spaces or rely on a wide tablet screen.
- Two-column tables can ellipsize long cells on a narrow phone. Keep them to
  short labels and scalar values, such as a route and its time. Do not pack time,
  terrain and a qualification into one cell. Use separate attributes in a 3–4
  column table (which renders as labeled records), or use a wrapping list or
  `keyValue` for sentence-length descriptions. Preserve every important fact;
  the fallback is not a substitute for readable native content. Do not invent
  width or wrapping properties to repair a cell.
- Every chart bar, status and diagram node needs meaningful words. Do not encode
  meaning only in color, an icon, an arrow, emoji or position. Avoid emoji-only
  labels and all-caps emphasis. Native text wraps for Dynamic Type; shortening
  content must preserve its meaning, units and uncertainty.
- Strings inside components are literal text. Markdown, HTML, Mermaid and LaTeX
  there do not become rich content. Put links, code and longer math explanations
  in ordinary Markdown outside the block. Do not add fake links or tap targets.
- Keep button and form-submit labels compact, usually two short words such as
  “Draft walk” or “Draft plan”. Put route names and other qualifiers in `message`
  or the surrounding context. Long action labels can crowd nearby content at
  accessibility text sizes; do not fill the title's 120-character allowance.

## Diagrams that tell the truth

`diagram` is only a vertical ordered flow. Prefer 3–6 short steps, each naming an
actor/action or state. Make transitions explicit in the words: “You review the
draft” → “You tap Send” → “The agent receives the message”. Do not also number
the nodes or embed ASCII arrows; the renderer supplies connectors.

Use it for a recipe, lifecycle or a single path through a workflow. For a branch,
dependency graph, cycle, hierarchy, spatial layout or sequence with parallel
actors, explain the relationships with a table/list or separate clearly named
paths. Do not flatten alternatives into a single flow that falsely implies
sequence or causation. If the requested diagram cannot be represented faithfully,
say so briefly and use readable text or a separately supported artifact. Never
invent `edges`, coordinates, graph syntax or a renderer that Cupola does not have.
For branches with sentence-length instructions, prefer a list of conditional
sentences and name the shared next step explicitly; a two-column sentence table
can hide the condition or action on a phone.

## Exact native catalog v1

Emit a complete, unquoted Markdown fence named exactly `wonder-ui`, containing
one JSON object. Open and close the fence on their own lines, with exactly three
backticks; do not wrap the whole reply in another code fence or indent it as code.

The object has exactly `version` (the number `1`), `fallback` (useful plain text)
and `components` (an array). The fallback must preserve the answer's important
facts, comparison, order and available draft choices for someone without native
rendering; “See the chart” is not a fallback. Do not duplicate the full fallback
as visible prose around a successfully rendered component.

Every component has `id`, `type`, `title` and only the fields listed below. IDs
are distinct within a document, 1–64 ASCII letters/digits/`_`/`-`, and stable when
revising the same component. Titles are 1–120 characters. All text below is a
literal string; all required text must be nonempty unless explicitly allowed.

| Exact `type` | Additional required fields and hard bounds |
| --- | --- |
| `card` | `body`: string, at most 2,000 characters. |
| `table` | `columns`: 1–8 nonempty strings (each at most 120 characters); `rows`: 1–20 arrays of strings, each with exactly the column count; each cell at most 256 characters (empty allowed). |
| `list` | `items`: 1–20 strings, each at most 256 characters. |
| `keyValue` | `entries`: 1–20 objects with exactly `label` (at most 120 characters) and `value` (at most 512). Case matters: `keyValue`. |
| `chart` | `bars`: 1–12 objects with exactly `label` (at most 120 characters) and `value` (a finite number from 0 to 1,000,000,000). Units belong in the title, not in the number. |
| `progress` | `value`: a finite number from 0 to 1, not a percentage integer; `label`: string, at most 256 characters. |
| `diagram` | `steps`: 2–12 strings, each at most 256 characters. |
| `chart` (series form) | `labels`: 1–24 distinct strings (at most 60); `series`: 1–6 objects with exactly `name` (at most 60, distinct) and `values` (one finite number per label, ±1,000,000,000); optional `kind`: `bar`, `line` or `area`. Not combined with `bars`. |
| `callout` | `body`: at most 2,000 characters; optional `tone`. |
| `stats` | `items`: 1–8 objects with `label` (at most 120) and `value` (at most 40), optional `delta` (at most 40) and `tone`. |
| `steps` | `steps`: 1–12 objects with `title` (at most 120), `status` (`done`, `current`, `pending`, `failed`) and optional `detail` (at most 512). |
| `checklist` | `items`: 1–20 objects with exactly `text` (at most 256) and `done` (Boolean). |
| `badges` | `items`: 1–12 objects with `text` (at most 60) and optional `tone`. |
| `mockup` | `aspect`: width/height from 0.25 to 4; `regions`: 1–6 of `{"kind":"pane"}` with `title` and/or `label` (at most 60) and optional `tone`, `{"kind":"chat","bubbles":["agent","user"]}` with optional `composer`, `{"kind":"lines","count":1–8}` or `{"kind":"divider"}` with optional `glyph` (at most 3 characters); non-dividers take optional `weight` (0.05–10). Optional `direction` (`horizontal`/`vertical`), `overlay` `{ "style": "sheet"|"full", "dim"?, "region" }` (a pane or chat), `badge` `{ "text", "tone"? }` and `caption` (at most 500). |

`tone` is one of `neutral`, `accent`, `success`, `warning`, `error`, `info` or
`series1`–`series6`. It names a meaning; the reader's theme picks the colour.
Always say the meaning in words too.
| `button` | `message`: the literal proposed user message, at most 2,000 characters. |
| `form` | `fields`: 1–8 field objects below; `submit`: draft action label, at most 120 characters; `message`: literal proposed user message, at most 2,000 characters. |

Form fields have distinct `id`s (same syntax as component IDs), a `label` (at
most 120 characters), and exactly one of these shapes:

```json
[
  {"id":"notes","kind":"text","label":"Notes","required":true},
  {"id":"pace","kind":"select","label":"Pace","options":["Relaxed","Brisk"]},
  {"id":"lunch","kind":"toggle","label":"Include lunch"}
]
```

`text` requires the Boolean `required`, even when false; it starts empty and
accepts at most 512 characters. `select` requires 1–12 distinct nonempty options,
each at most 120 characters, and starts at the first option. Put a reasonable
default first; do not suggest an unselected placeholder state. `toggle` starts
off and supplies Yes/No. Do not add initial values, placeholders, hidden fields,
validation patterns or field kinds such as date/number/slider. Do not collect
passwords or credentials.

Buttons and form submits fill an **empty, loaded draft** for the user to review
and send. They do not send, run a tool, open a URL, grant approval, save preferences
or perform the labeled real-world task. Name the draft effect honestly (“Draft a
walk”, not “Book now”). Never claim the action happened after rendering.
Describe the control's effect, not a blanket limit on the agent: after the user
sends a request, the agent can work through its normal tools and approval path.
An existing draft/attachment is preserved with recovery feedback. Read-only
threads disable these actions. Form submission appends a blank line and one
`label: value` line per field to `message`; there is no variable interpolation.
Ask only for inputs that will change the next answer, and do not turn ordinary
information into an unnecessary form.

Prefer one block per reply. Hard bounds across a message are four blocks,
24 components and 80 data items; table cells count individually, collections
count their entries/bars/fields/steps, and a card/progress/button counts as one.
Each document is at most 32,768 UTF-8 bytes and 16 JSON nesting levels; fallback
is at most 4,000 characters. These are safety limits, not layout targets.
Unknown types, versions, extra properties and malformed input fall back to text;
do not rely on partial rendering. No HTML, scripts, bindings, URLs, remote images
or automatic effects are supported. Preserve actual artifact links outside the
block instead of inventing an attachment.

## Small worked reply

For supplied facts “Riverside, 45 minutes, mostly level; Hilltop, 70 minutes,
steep climb” and a preference for a gentler walk:

Riverside fits your preference for a gentler walk.

```wonder-ui
{
  "version": 1,
  "fallback": "Riverside: 45 minutes, mostly level. Hilltop: 70 minutes, steep climb. You can draft a request for a shorter riverside route, then review and send it.",
  "components": [
    {
      "id": "routes",
      "type": "table",
      "title": "Walking routes",
      "columns": ["Route", "Time", "Terrain"],
      "rows": [["Riverside", "45 minutes", "Mostly level"], ["Hilltop", "70 minutes", "Steep climb"]]
    },
    {
      "id": "shorter",
      "type": "button",
      "title": "Draft walk",
      "message": "Suggest a shorter riverside route."
    }
  ]
}
```

Before replying, check: does the structure earn its space, does the order tell
the truth, are units/defaults/actions explicit, does the fallback preserve the
answer, and is the JSON within this exact catalog? Do not narrate this check to
the reader.

# Native reply skill evaluation

The fixed requests in [native-replies.json](native-replies.json) exercise the
shipped `wonder-phone-replies` skill. They are evaluation inputs, not evidence
that a model ran or a screenshot was captured. Their review notes are for the
reviewer; do not give those notes or an intended answer to the producing agent.

## Paired run

1. Save the old and new skill bodies, commit IDs, model/reasoning settings and
   the exact request for each case. Use fresh, independent threads with the same
   model/settings and supplied facts. The baseline receives only the old skill;
   the candidate receives only the revised skill. Do not feed the baseline's
   answer to the candidate or ask it to improve a known bad layout.
   Load the skill through discovery or per-session context; keep its full body
   out of the visible user request, which otherwise distorts the app's density.
   Supplying context explicitly does not verify automatic skill selection.
2. In this repository's overnight run, live model requests belong only to the
   run's `Overnight fixtures <date>` Project. Do not create evaluation threads in
   an owner's existing Project. Keep generated responses and all metadata under
   `.local/evidence/genui-skill/`; record actual discovery/loading separately
   from a test that explicitly supplies the skill text.
3. Save the unedited raw reply per case and side as `01-compare-before.md`,
   `01-compare-after.md`, etc. Preserve malformed output as a failure. Run
   `python3.12 skills/tests/validate-native-replies.py` and pass saved replies as
   extra arguments to validate their native blocks against the shipped schema.
   This checks authoring syntax, not visual quality or skill activation.
4. Use `scripts/wonderctl` and its shared lease to render both replies in the
   actual app (Diagnostics build). Capture the same phone size, explicit theme, system font,
   Dynamic Type size and scroll position for each pair. Retain a short recording
   through opening/scrolling; capture both ends of tall replies. Add a focused
   large-text pass for a table, diagram and form. Release the lease between
   bounded capture batches and when finished. An occupied lease is pending
   acceptance, not permission to use a separate simulator.
   Select the reading position explicitly: use the app's Scroll to bottom action
   when present, verify it disappears, and retain an upper capture when a reply
   spans the viewport. A native accessibility label can contain the full value
   while its visible two-column cell is ellipsized; inspect the pixels too.
5. Inspect every pair against the notes below. Report raw pass/fail counts and
   examples, not an invented quality percentage. A syntax-valid response can
   still be misleading, crowded, inappropriate or inaccessible. Host snapshots
   and manually authored examples do not prove live skill improvement.

## Review rubric

For each reply record these independent outcomes with evidence:

- **Faithful:** all supplied facts, units, uncertainty and decision boundaries
  survive; no invented measurements, completed actions or progress.
- **Useful structure:** the component fits the task, including choosing plain
  prose for case 10 and a faithful fallback for branching/unsupported values.
- **Readable:** the first viewport gives the answer; short titles, compact
  vertical rhythm and wrapped labels remain understandable. No clipped content,
  decorative filler or redundant prose/card repetition.
- **Accessible:** large text preserves labels and values; no meaning depends on
  color, an icon or position. Review actual captures, not just a successful tap.
- **Honest actions:** buttons/form submits only draft, preserve occupied drafts,
  and offer a relevant next request. No live send is needed to check insertion.
- **Portable:** fallback includes the facts/choices needed without native UI;
  JSON is valid for catalog v1 and stays within runtime bounds.

Cases 1–7 cover comparisons, numerical bars, ordered flows, named facts, lists,
measured progress and related preferences. Cases 8–10 protect the limits: a
branch must not become a false sequence, a negative/missing value must not become
a positive bar, and a one-sentence answer must not become a dashboard. Not every
component needs to appear in the resulting set: `card` should only appear when
its title and body help the reader.

Case 1 overlaps the skill's worked example; treat it as an authoring positive
control, not evidence of transfer. In case 3, describe what tapping the control
does. A claim that the agent cannot use tools after the user sends is incorrect.

## Browser study

When authorized to study chatgpt.com, use the owner's Chrome, an owned fixture
Project and Instant mode. Keep the model selector, prompt, observed layout,
timestamp and conversation URL with the notes. Never infer a visible behavior
from a model's textual description alone. Ask for diagrams/visual explanations
without generating images. Compare useful reading-order, label-density and
branching decisions with Cupola's fixed catalog; do not copy unsupported web
widgets, styling properties or graph capabilities into native JSON.

Separate measured observations from inferred design lessons. If the browser,
Project or model mode cannot be verified, record the gap; do not claim the study
was performed. The browser study does not replace the paired app captures.

## Study findings, 18 October 2026

Measured: ten visualization prompts were inspected in the owner's Chrome, in
literal Instant mode inside `Overnight fixtures 2026-10-18`. The desktop viewport
was about 1512 × 828; no generated widget was activated. Exact prompts, private
conversation URL, observations and 25 captures are retained under
`.local/evidence/genui-skill/browser/`. These were browser observations, separate
from Cupola's simulator verification.

- Measured: the walk comparison separated attributes in a compact table. The
  distance chart retained exact values but occupied much of a viewport.
  Inferred: compare matching attributes, label units and retain numbers as text;
  use Cupola's horizontal bars without importing unsupported web chart styles.
- Measured: the compact workshop card omitted the supplied year, and a signed
  chart broke “Unknown” across lines at desktop width. Inferred: compression
  must preserve dates and missing-data meaning; labels need room to remain words.
- Measured: the branch diagram showed two paths joining at owner approval, while
  the HTTP diagram needed prose to explain relationships its layout omitted.
  Inferred: preserve topology; Cupola v1 should use explicit conditional text
  when its vertical flow cannot express the graph faithfully.
- Measured: form defaults and draft-only wording were visible; a checklist also
  displayed a publication lock. Those controls were not exercised. Inferred:
  generated labels do not prove effects or enforcement. Keep Cupola actions on
  the real composer path, and distinguish supplied progress from readiness.

Measured in Cupola's separate phone pass: two-column sentence cells ellipsized
important values. The revised skill uses labeled records for richer comparisons
and conditional lists for long branch instructions. Its worked example and the
ten-prompt candidates were then validated against the actual native renderer.

Measured in the separate AX3 dark-theme phone pass: record values stacked and
compact “Draft walk” actions avoided the earlier overlap. A table heading and
the form's built-in review hint still ellipsized. This is a renderer limitation,
not a full accessibility pass; retain those captures and route the sizing issue
to the renderer owner rather than inventing JSON styling properties.

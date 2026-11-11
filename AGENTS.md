# Cupola agent guidance

Cupola is a native messenger for persistent agent conversations and Projects;
keep it understandable without App Server protocol knowledge. This file is for
coding agents and people working in this repository. Build steps are in
[DEVELOPMENT.md](DEVELOPMENT.md); the contribution process is in
[CONTRIBUTING.md](CONTRIBUTING.md).

## Judgment

- **Prove done:** define a pass/fail check and its evidence first; label claims measured, inferred or guessed, and run the checks you can run.
- **Fix causes:** reproduce on the affected surface; when two fixes sharing a premise fail, test that premise before another fix.
- **Exercise judgment:** proceed on reversible work and report it; decline scope or approaches that do not earn their place.
- **Encode lessons:** repeated mistakes belong in architecture, types or a rerunnable check whose error names the fix; keep prose for judgment.
- **Automate repetition:** keep repetitive edits, audits and checks rerunnable with a script or codemod.

## Product judgment

- **One conversation:** compact Chats, clear Project/thread names and previews, provider identity, a compact header and bottom composer; mobile opens the chat with an obvious way back.
- **Native density:** prefer SwiftUI messaging patterns, strong contrast, restrained corners and few borders; use accent color for focus and primary actions.
- **Clear speakers:** left agent/right user bubbles, provider/Project identity in the header and human child-task names; preserve accessible speaker identification.
- **Plain language:** explain meaningful choices, limits, approvals and recovery; omit obvious helper text, redundant labels and oversized empty cards.
- **Honest controls:** visible actions work, persist and show loading/failure, or say unavailable; protocol methods, lifecycle/dispatch labels, IDs, token usage, transport state and raw capability/plugin/MCP inventories belong in explicit developer details.
- **Real artifacts:** preserve previews, downloads, MIME validation and integrity hashes; never fabricate an artifact.
- **Private diagnostics:** collect diagnostics only in Diagnostics builds; Release builds exclude the recorder, fixtures and test driver (`scripts/check-ios-profiles.py` checks an exported app).

The visual design language and per-screen review checklist are in
[apps/ios/DESIGN.md](apps/ios/DESIGN.md); UI work is not done until its captures pass it.

## SwiftUI and concurrency judgment

- **Cheap rendering:** prepare expensive projections when inputs change, narrow dependencies and keep fast state near controls; computed properties alone do not cache.
- **Stable ownership:** stable model IDs, one reference-model owner and one conversation lazy stack; no changing IDs to refresh or nested lazy activity stacks.
- **Responsive interaction:** keep geometry below timeline/composer, publish visibility changes and save a small reading anchor; no per-pixel history rewrites.
- **Correct lifetimes:** MainActor UI, explicit workers for expensive work, idempotent UIKit updates, cancellation plus request/pairing identity after awaits.
- **Bounded work:** deduplicate/downsample images, invalidate and bound caches, coalesce streams and durable intent without losing ordering, terminal results, approvals or reads.
- **Preserved behavior:** respect Reduce Motion, keyboard/IME/selection, internal scrolling, Dynamic Type, paging, search, reads and bottom-arrow behavior.
- **No feedback loops:** end resources with their owner/foreground lifetime; presence updates must not refresh conversations.

## Tests and evidence

- **Evidence over opinion:** a blocking review finding needs a repro (a failing command and its output, a capture, or steps run).
- **Validate, don't pile up tests:** prove changes with builds, simulator runs and screenshots on the affected devices; write a new unit test when it is truly useful (security, protocol, storage, migration, or logic a run cannot exercise), then extend the owning fixture or table.
- **Test behavior:** avoid source-string, flag, inventory and mock-self tests; keep independent security, protocol, storage and migration guards.
- **Match the claim:** capture the actual app for visible changes and label what was exercised (source, simulator, Mac or physical device); a build or a tap is not behavior proof.
- **Preserve privacy:** use synthetic fixtures; never commit real conversations, tokens, pairing links, device identifiers or user databases.

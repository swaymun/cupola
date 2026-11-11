# Cupola Product

<!-- impeccable:product-schema 1 -->

> Concise product record for design tooling. The full product contract is
> [docs/WONDER_PRODUCT_AND_UI.md](docs/WONDER_PRODUCT_AND_UI.md); working and
> release instructions live in [AGENTS.md](AGENTS.md).

## Platform

ios

## Users

Cupola is for someone who wants to continue agent work on their iPhone or iPad
while their own Mac runs it. They return to a Project thread, understand what
has happened, answer requests and keep working. Phone and tablet are equal
product surfaces.

## Product Purpose

Cupola is a native messenger for Project threads. A Project supplies the working
folder; a thread preserves the conversation with its provider. The Mac owns
execution, credentials and local data. The phone makes that work accessible
through readable conversations, files, approvals and remote computer control.

## Positioning

Bring the agent's existing capabilities to mobile. Surface what the connected
harness actually supports; do not emulate a missing harness feature with
Cupola-side model calls or heuristics. Desktop-to-mobile handoff and remote
control of the Mac are deliberate Cupola additions.

## Operating Context

- SwiftUI owns the iPhone and iPad app. The `cupola` command (macOS, Linux and
  Windows) sets up and runs the host and its settings; there is no desktop app.
- The daemon connects Project threads to Codex App Server (the Codex CLI or the
  ChatGPT app's Codex), Claude Agent SDK, and experimental OpenCode and Cursor over
  ACP; threads open in T3 Code go through T3 Code's own MCP endpoint. Their limits
  are listed in [installation](INSTALL.md#opencode--experimental).
  Linux host support requires its own verified release.
- Paired devices reach the Mac directly on the same Wi-Fi, and optionally through
  Tailscale away from home. Push wakes the phone through
  APNs with encrypted previews; a working tunnel alone cannot wake it.
- Provider sign-in and execution happen on the owner's Mac. Pairing grants
  meaningful authority and must explain its consequences.

## Capabilities and Constraints

Preserve Project threads, sending, streaming, stop/retry, history, file previews,
attachments, diffs, approvals, questions, supported agent tasks, pairing and
recovery. Availability follows the installed provider's capabilities. Models,
permissions and provider changes retain their real consequences.

Bots and Group Chats are retired. Remove their routes, storage, fixtures and
copy; move any still-needed Project behavior to its active owner. A remaining
legacy implementation is removal work, not a supported product requirement.

Normal UI uses Project and thread names, provider identity and plain actions.
Keep protocol methods, transport state, internal IDs and raw tool inventories
in explicit developer details. Do not add a Cupola account, default telemetry,
a Chrome extension dependency or cloud conversation storage.

Release and publication permissions are defined by AGENTS.md and the owner's
current request. This document does not override standing upload authorization.

## Brand Commitments

Keep the Cupola sun mark, established native layouts, themes and typography.
Copy is brief: names, values and actions. Explain a consequence or recovery only
when it helps the person act. Avoid decorative metadata and nested status cards.

## Evidence on Hand

- Product decision: [ADR 0008](docs/adr/0008-native-project-threads.md).
- App shell and conversations: `apps/ios/Wonder/`.
- Shared native models: `apps/native/Sources/WonderPairing/`.
- Host commands: `crates/wonderd/src/headless/cli.rs`.
- Theme and typography owners: listed in [DESIGN.md](DESIGN.md).
- Scenario coverage: [apps/ios/CONTROL.md](apps/ios/CONTROL.md).

## Product Principles

1. Keep the conversation and composer easy to reach.
2. Derive status from the latest authoritative provider record.
3. Preserve user intent across navigation, backgrounding and reconnects.
4. Explain meaningful choices; keep internal machinery out of ordinary UI.
5. Every visible action works, persists when appropriate and reports failure.

## Accessibility & Inclusion

Preserve Dynamic Type, VoiceOver speaker and action labels, keyboard navigation,
selection, visible focus, reduced motion and readable contrast. Touch targets
are at least 44 points. A simulator capture does not establish physical-device
or full accessibility acceptance.

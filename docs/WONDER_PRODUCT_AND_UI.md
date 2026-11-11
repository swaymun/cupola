# Cupola product and UI contract

Updated October 21, 2026. This is the current native product direction.
[ADR 0008](adr/0008-native-project-threads.md) replaced the former browser,
Bot and Group Chat model, and that code has been removed. This document is not
a claim that every planned capability has shipped.

## Product and ownership

Cupola lets someone continue agent work from an iPhone or iPad while their own
Mac runs it. A **Project** gives work a folder and a human name. A **thread** is a
persistent provider conversation within that Project. Chats is the compact list
of those conversations. Identify a thread by provider and Project, preserving
its title and history across devices.

The native SwiftUI app is the conversation surface. Phone and tablet share the
same capabilities, with layouts suited to their available space. The Mac
companion handles setup, provider access, pairing, permissions and updates.
Do not rebuild a desktop Chats client or carry browser layout rules into iOS.

Codex App Server and Claude Agent SDK are current provider integrations.
OpenCode ACP and a Linux host are development work until their implementation,
verification and release evidence establish availability. A capability must
be confirmed against the installed harness before the UI offers it.

## Harness parity

Expose a harness feature when the harness supplies it. Preserve its approval,
sandbox, interruption, history and task semantics. Do not fill gaps with extra
Cupola model calls, inferred success or an alternative agent orchestrator.
Desktop-to-mobile handoff of a running thread and remote control of the Mac
are the two deliberate extras.

Project tools keep their ownership, access checks and idempotency. Subagents
are provider tasks associated with a thread, not a new multi-agent chat product.

## Conversations

Use a dense Chats list with readable names, previews and relevant current state.
Opening a thread on a phone gives the conversation the available width, with
an obvious route back to Chats. On iPad, keep the sidebar independently usable
without shrinking the conversation into a phone mockup.

Keep the header compact and the composer at the bottom. User messages align
right; agent messages align left. Identify provider and Project in the header.
Keep accessible speaker identification for messages and child tasks even when
a decorative mark is hidden. Provider marks are identity, not animated status.

Collapse routine tool work into readable activity groups. Keep approvals,
questions, failures, artifacts and actionable file changes discoverable.
Current work and completed summaries must come from typed provider events,
not display-title matching or guessed progress. Large histories page and search
without losing the reading position, selection or the bottom-of-chat control.

Preserve drafts, attachments and send intent through navigation and reconnects.
A request being accepted, delivered, running or completed are different facts;
show only what the authoritative record establishes. Unknown delivery must not
offer a retry that might duplicate the work. An open connection alone does not
prove the provider can accept a turn.

## Files, requests and computer control

Files, diffs and previews belong to the selected Project/thread. Preserve
validated MIME types, integrity hashes, downloads, cancellation and a useful
fallback for unsupported previews. Never fabricate an artifact or treat a
successful request as proof that the file was received or opened.

Approvals and questions state the actual action or decision. Responses use the
paired phone's authority and retain the harness's scope. Keep the choice close
to its consequence, report failure on the affected item and preserve unanswered
requests across navigation. Do not widen permissions merely to simplify UI.

Computer control uses the connected computer-use path and the Mac's permission
boundaries, without a Chrome extension requirement. Keep remote input, active
sessions and meaningful access choices understandable in ordinary language.

## Copy and state

Default to names, values and action labels. Use a status icon with an accessible
label; add text when it explains a meaningful choice, consequence or recovery.
Do not narrate obvious controls, repeat status in several boxes or expose raw
protocol methods, internal IDs, token accounting and transport inventories.
Developer details may expose those records deliberately.

Refresh automatically while a surface is visible. Every action must work,
persist where appropriate and show loading/failure feedback. A label such as
recovered, viewed or unavailable needs evidence. An unsupported action is
unavailable; a requested integration is not automatically a shipped capability.

## Native visual system

[DESIGN.md](../DESIGN.md) records the implemented theme, typography and layout
owners. Preserve the existing composer and native messaging patterns. Prefer
compact spacing, strong contrast, few borders and restrained corners. Reuse
the theme and typography environment rather than defining local palettes.

Respect Dynamic Type, Reduce Motion, Reduce Transparency, keyboard navigation,
text selection and safe areas. Controls need usable touch targets and clear
accessible names. Do not replace native navigation behavior with browser
breakpoints or CSS conventions.

## Local authority and transport

The Mac owns provider credentials and authoritative execution state. Pairing
must explain the authority granted to the phone, support revocation and retain
the signed request boundaries. Tailscale is the supported phone-to-host path.
APNs delivery still needs the push service and its encrypted-preview design.
Removing unused transport code must not remove push or weaken pairing.

Do not add a Cupola account, cloud conversation database, default telemetry or
a second provider-credential store to make a flow easier. Diagnostics and
synthetic fixtures are confined to Diagnostics builds and private evidence;
committed fixtures must not contain real session text or personal data.

## Source and verification

| Concern | Owner |
| --- | --- |
| Chats navigation | `apps/ios/Wonder/AppShell.swift` |
| Project selection and details | `apps/ios/Wonder/ProjectViews.swift`, `ProjectsModel.swift` |
| Conversation presentation | the conversation views under `apps/ios/Wonder/` |
| Native protocol models | `apps/native/Sources/WonderPairing/` |
| Themes and fonts | `WonderTheme.swift`, `WonderTypography.swift`, shared `WonderThemeCatalog.swift` |
| Host commands and settings | `crates/wonderd/src/headless/cli.rs`, `main.rs` |
| Project execution | `crates/wonderd/src/projects.rs`, provider runtimes |
| Scenario coverage | `apps/ios/CONTROL.md`, `apps/ios/control/scenarios/` |

Use [AGENTS.md](../AGENTS.md) for testing, privacy, release authorization and
PR delivery. Record observed behavior separately from intent. Put actual app
Before/After evidence on UI PRs, label the device/build and state what was not
checked. Do not turn a source inspection into a claim of runtime acceptance.

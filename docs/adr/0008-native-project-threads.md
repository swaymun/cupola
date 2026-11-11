# ADR 0008: Native Project threads

**Status:** accepted
**Date:** October 13, 2026
**Supersedes:** [ADR 0007](0007-bot-and-group-chat-product-model.md)

## Context

The product now ships a native SwiftUI iPhone/iPad app and a Mac host companion.
The former browser, Bot and Group Chat model left routes, fixtures and shared
helpers behind. Treating those remnants as product requirements prolongs the
wrong ownership and exposes retired names in the app.

## Decision

Project threads are the conversation model. A Project supplies a working folder;
a thread belongs to its provider and retains that provider's conversation
semantics. Identify it by provider, Project and thread title. Phone and tablet
are equal surfaces; the Mac companion owns setup, Settings and local execution.

Expose capabilities supplied by the connected harness. Do not build substitutes
with Wonder-side model calls, heuristics or coordination. Desktop-to-mobile
handoff and remote computer control remain deliberate Wonder capabilities.
Codex and Claude are current integrations; OpenCode and Linux remain development
work until separately verified and released.

Retire Bot direct chats, Group Chats, their avatars and their orchestration.
Trace callers, move required Project behavior to active owners, then remove
obsolete routes, tables, fixtures, tests and documentation. Do not retain
compatibility handlers just to keep an old test passing. Preserve permissions,
attachment integrity, request idempotency and provider lifecycle boundaries.

The owner permits rewriting or squashing the pre-launch schema and rebuilding
the Wonder database. Before a live rebuild, make a consistent private backup of
the database and its siblings and report which setup or data the owner must
restore. This decision does not authorize deleting unrelated provider sessions.

## Consequences

- Current product and UI guidance follows the native implementation and owner
  direction. ADR 0007 remains readable as historical evidence.
- Fixtures exercise Project threads through the normal provider dialect, with
  synthesized content and privacy checks. Diagnostics stay out of Release.
- Useful approvals, file access, banners and task behavior move with their
  contracts; deleting the old owner must not silently remove the behavior.
- CI must reject retired product copy in visible strings and fixtures, with an
  error pointing to Project/thread/provider terminology. Historical ADRs are
  historical evidence, not app copy.
- This ADR establishes the decision; each removal and capability still requires
  its own implementation, checks and release evidence.

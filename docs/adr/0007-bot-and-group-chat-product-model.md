# ADR 0007: Persistent Bots and Group Chats as the Product Model

**Status:** superseded by [ADR 0008](0008-native-project-threads.md) on October 13, 2026
**Date:** September 2, 2026

This document preserves the historical decision. It is not current product guidance.

## Decision

Wonder is the local container. Bots and Group Chats are the user-facing working
objects. Each object has one persistent user-facing thread. The Rust daemon may
retain separate conversation metadata, Codex threads, and coordinator/member
dispatch records, but those are implementation details rather than additional
inbox objects.

Group Chats reuse the existing `channels` storage and coordinator/worker
runtime. “Group Chat” is the PWA product term; `channel` remains the compatible
Rust/API term until a deliberately versioned API change.

Automations remain Wonder-owned and preserve `continuation` versus `standalone`
semantics. Every automation is scoped to exactly one Bot or Group Chat and is
managed from that object's workspace. There is no global Automations navigation
surface.

Fresh installations do not seed `Default Bot` or preset Bots. Existing legacy
Bots and conversation metadata are preserved. Migration creates a deterministic
canonical Bot workspace mapping, selecting active metadata first, then the most
recently updated metadata, then the stable identifier; historical extra rows
remain recoverable but are not shown as separate Bot workspaces.

## Consequences

- The PWA must show zero-Bot onboarding instead of an empty generic assistant.
- New work creates a Bot or Group Chat, not a disposable conversation.
- App Server protocol claims remain separate from Wonder product choices.
- Context compaction is capability-gated against the installed App Server and
  cannot be represented as supported solely because the UI has a menu item.

Older ADRs retain their original historical decisions; this ADR supersedes only
the active product framing where the two conflict.

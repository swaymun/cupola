# Committed snapshot and replay contract

This is the daemon half of D4, with the step 05 history and retention changes applied. The native client is a separate task; PWA cursor persistence is unchanged. These are private implementation notes.

## State authority and cursor scope

`GET /api/v1/conversations/{id}` returns `hostEpoch` and `lastSequence` with the existing snapshot fields. Project ownership, messages, attachment associations, assistant messages, local thread binding, and retained presentation events are read in one SQLite transaction. Thread projection uses that local data, without calling App Server history in the request. Runtime reconciliation remains responsible for importing missing history. Step 05 bounds each snapshot to 100 canonical entries and provides older-page cursors and a separate runtime refresh endpoint; see [the history contract](../packages/protocol/README.md#step-05-local-history).

The cursor covers the host journal at the snapshot's database view. The projection covers **one conversation**. Installing one conversation snapshot must not advance a shared checkpoint for other cached conversations. A client can keep a separate committed checkpoint per conversation and validate the complete host stream, or invalidate other cached scopes before advancing a shared checkpoint. Non-conversation endpoints do not yet expose an atomic aggregate snapshot.

## Transactional invalidations

Step 09 adds authenticated `GET /api/v1/sync/checkpoint` for invalidation-based native bootstrap, including hosts with no conversations. Read it before fetching independent projections. Atomically persist the returned cursor with invalidations of all cached scopes, then refresh authoritative projections and replay strictly after that cursor. Stale cached scopes remain explicitly stale until replaced. This is an alternative to per-conversation snapshot checkpoints, not an aggregate snapshot. The response binds the epoch/head to `hostInstallationId`.

Migration `0035_committed_sync.sql` installs the durable journal and triggers for messages, assistant messages, attachments, conversation bindings/files, approvals, schedules/runs and devices. Project, folder and thread-setting migrations extend those triggers to their own projections. Each changed row and its journal invalidation commit or roll back together. This also covers dispatch/recovery writes and writes that never reach a later presentation-event publisher. If journaling fails, the projection write fails.

An invalidation uses the existing envelope and activity variant:

```json
{
  "eventId": "projection:epoch:42",
  "hostEpoch": "epoch",
  "sequence": 42,
  "occurredAt": "2026-09-05T20:00:00.000Z",
  "conversationId": "conversation-id",
  "event": {
    "type": "activity",
    "data": {
      "category": "projection_changed",
      "state": "updated",
      "detail": "{\"resource\":\"messages\",\"change\":\"update\"}"
    }
  }
}
```

`detail` is JSON text with `resource` and `change` (`insert`, `update`, `delete`, or workspace `reset`). Global resources may have a null `conversationId`; clients use `resource` to invalidate the relevant lists. Since migration 0141, `approvals` rows name the chat the attention list shows the request under (stored when the request arrives) and `automation_runs` rows name the run's chat, falling back to its automation's; an `approval_resolved` event published without a chat or turn takes its request's owner. Only a request whose owner was not established still journals a null `conversationId`. No row contents or credentials are included in invalidations. Internal receipts, sessions, search indexes and notification queues do not generate product invalidations.

**The journal is an invalidation stream; a few events in it may also be applied.** Most assistant/message/activity events remain presentation hints that can follow the underlying projection change, and a client refreshes authoritative snapshots for them. Two kinds are exact: an `assistant_delta` is journaled in the same transaction as the assistant-text change it describes (the `assistant_messages` invalidation sits immediately below it), and a live `thread_item_upsert` (lifecycle authority above 0) is the very row snapshots build items from. A snapshot read at sequence S therefore contains every such event at or below S and none above it, and a client may apply those above S, in order, to that snapshot (`apps/native/Sources/WonderPairing/LiveEvents.swift`). A live upsert also carries `itemType` and the sanitized display `text` a snapshot would give the item. Everything else (delivery states, approvals, settings, host-wide changes) still needs an authoritative read, which also reconciles a turn when it ends.

A subscription may ask for `"batch": true`. The host then sends each wakeup's committed events as one JSON array frame: the first events after a quiet period at once, later ones no sooner than 50 ms after the previous frame. Only a batching host journals deltas atomically, so a client applies events directly only from array frames. A runtime notification also wakes the projector at once instead of waiting for its 100 ms idle poll, and a streamed delta no longer rewrites its message's delivery state (one `messages` invalidation per token).

New projection tables must have journal triggers in their migration. Migration 0036 replaces the original 1,024-entry journal window with global retention across both replay tables: 7 days, 100,000 stored replay records or 128 MiB payload. Canonical history is retained independently. Workspace reset erases journal contents and leaves a single reset invalidation while keeping the monotonic sequence high-water mark.

## Replay and recovery

The daemon starts a fresh host epoch before accepting work. Sequence numbers are database allocated, contiguous within an epoch and monotonic across epochs; a new epoch need not begin at sequence 1. Rollback does not consume a committed sequence. A snapshot of an empty initial journal can carry sequence 0.

The existing WebSocket subscription sends `deviceId`, `hostEpoch`, and `lastSequence` with its existing authentication proof. The server reads journal bounds, head and rows from one database view, verifies the whole window, then sends at most 256 entries per batch in committed sequence order. It validates initial, interior and trailing gaps, empty missing windows, future cursors and epoch changes. It never sends a valid-looking prefix of an invalid batch.

Broadcast messages only wake readers; they do not supply authoritative order. A 100 ms journal poll covers projection commits without a broadcast and cancellation after commit but before broadcast. A reconnect replays every retained entry after the supplied cursor. A lagging client whose remaining window expires receives `resync_required`.

`resync_required` carries the current committed epoch/head, including head 0. It is a reset instruction, **not a snapshot that a client has applied**. The client must fetch a new authoritative snapshot before replacing its committed checkpoint. A database failure closes the stream if the daemon cannot read an honest head. Approval revocation and the existing session gates remain enforced.

Client durable projection replacement and its checkpoint must share one local transaction. A received-event watermark can be ephemeral, but must not become a durable checkpoint before snapshot installation. Resnapshot must preserve drafts, attachment staging and uncertain-submission receipts. Those client-side guarantees are not implemented by this daemon change.

## Verification

Storage fixtures cover interrupted transactions/reopen, journal insertion failure, concurrent publishers and rollback without sequence holes, a delayed writer blocking later allocation, missing broadcasts, interleaved multi-row snapshots, all gap positions, retention and workspace reset. HTTP verification holds the runtime mutex while fetching a schema-valid snapshot. Real loopback WebSocket tests cover missing/reversed wakeups, reconnect, revocation and reset envelopes, including an empty journal and an interior gap.

These are local daemon fixtures with a fake App Server. They do not establish installed-daemon, remote-network, PWA recovery, native-client or named-device verification.

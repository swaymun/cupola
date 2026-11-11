# Turn lifecycle pilot

This models **Cupola's conversation lifecycle**, across the phone, SQLite-backed
daemon and provider runtime. It does not model the orchestrator. The implementation
is the subject of the model, not a claimed refinement of it.

The desired Stop and strict queue-order contracts have counterexamples. A passing
pilot check means the specification, positive scenarios and recorded
counterexamples reproduced; it does **not** mean Cupola satisfies every invariant.
Findings and reproduction boundaries are in [FINDINGS.md](FINDINGS.md).

## Run

Requires Node/npm, Python 3 and network access on first use. Quint is pinned to
`@informalsystems/quint@0.33.1`; its Rust simulator downloads on first use. No Java,
cloud host, paid model, phone, simulator or live provider is used.

```sh
python3 specs/quint/check.py --output /absolute/path/to/QUINT-TURN-evidence
# Subsequent offline invocation, after the npm package/evaluator are installed:
python3 specs/quint/check.py --offline --output /absolute/path/to/QUINT-TURN-evidence
```

The runner records each command, raw output, exit code, elapsed time and each ITF
trace. It checks 10,000 randomized traces of at most 100 transitions from each of
two initializers, plus 33 directed scenarios and five failing-property traces.
An additional randomized `allInvariants` run independently finds QT-3; its full
trace and specific overtaking postcondition are checked too.
`init` starts empty; `activeInit` starts after a reachable send/receipt/acknowledge
prefix, to exercise Guide and approvals before all three message IDs are used.
`reachableActiveInitTest` checks that prefix's entire state for both providers.

Commands can also be run directly:

```sh
npx --yes @informalsystems/quint@0.33.1 typecheck specs/quint/turn.qnt
npx --yes @informalsystems/quint@0.33.1 test specs/quint/turn_tests.qnt --backend=rust --seed=1
npx --yes @informalsystems/quint@0.33.1 run specs/quint/turn.qnt --backend=rust --n-threads=2 --invariant=allInvariants --seed=42 --max-samples=10000 --max-steps=100 --verbosity=1
# Expected invariant violation, not a green product gate:
npx --yes @informalsystems/quint@0.33.1 run specs/quint/counterexamples.qnt --init=traceInit --step=expiredStopStep --invariant=noContinuationAfterPersistedStop --backend=typescript --seed=1 --max-steps=14 --verbosity=1
```

`quint run` performs randomized bounded simulation, **not exhaustive model
checking or an inductive proof**. The acceptance brief allows `run` or `verify`;
this pilot uses `run`. No fairness or temporal liveness proof is claimed. The
[Quint checking guide](https://quint.sh/docs/checking-properties) distinguishes
simulation witnesses from liveness and symbolic verification.

## Observable contracts

| Invariant | Exact meaning and limits |
| --- | --- |
| `noLostMessage` | Every saved input ID remains in the phone intent or the daemon's durable records. An uncertain, refused or cancelled message remains inspectable. This is conservation, **not eventual delivery** or proof of disk durability under hardware corruption. |
| `durableAcknowledgement` | The phone only retires an intent after the daemon has its record. Repeating the original client ID does not enqueue it again. |
| `noContinuationAfterPersistedStop` | Once Stop is saved, a restart must not issue a continuation for that turn unless the provider **definitively refused** that Stop. The ghost `continuedAfterStop` records violation at the continuation event; a later Stop cannot retroactively invalidate an earlier continuation. An ambiguous transport failure is not a refusal. |
| `atMostOneActiveTurn` | At most one **provider execution** owns this thread. Multiple message receipts can name a Codex turn. Claude may journal a Guide turn before handing execution to it; the handover is abstracted atomically. This is conditional on the provider's busy/refusal contract; it is not proof of Codex internals or exclusion against unrelated desktop writers. |
| `queuedMessagesInOrder` | A queued input cannot be delivered ahead of an earlier, still-unresolved queued input. Explicit cancel removes the obligation; explicit reorder changes the saved order; a refused/stale Guide joins the tail. **Uncertain submission is not an implicit cancellation.** QT-3 violates this stronger user-requested contract. |
| `approvalsRetired` | Once a terminal notification is known, no request for that runtime/turn remains pending or resolving. The provider may have ended before the notification arrives; forwarding then is not proof that it consumed the answer. |
| `approvalConsumedOnce` | A correlated blocking request consumes at most one response. Generation mismatch cannot send an old consent to a new runtime. Signature, nonce and body validation are assumed at this abstraction boundary, not proved. |
| `replayIsSound` | The replay cursor stays within the journal; projected IDs exist durably. Duplicate frames do not create work; a gap forces a snapshot. |

`safety` groups the six nonviolated conservation/execution/approval/replay
properties. **It deliberately does not stand for the two failed contracts**;
`allInvariants` includes queue order and Stop. The runner separately checks each
known violation rather than suppressing it or interpreting an arbitrary nonzero
exit as evidence. A parser/type error, missing ITF trace, changed counterexample
postcondition, or formerly failing invariant becoming green fails the runner and
requires re-triage.

## State and transition mapping to source

All source-derived semantics below are **inferred from code**; executable model
results and offline Rust reproductions are **measured**. Line references are to
the reviewed implementation plus this branch's test-only include/fixture hook.

| Spec state / action | Implementation evidence (`file:line`) |
| --- | --- |
| `issued`, `phone`; `send`, `guide` | `apps/native/Sources/WonderPairing/SendIntent.swift:142` prepares a stable client ID and optional expected turn; `apps/native/Sources/WonderPairing/Outbox.swift:114` appends the saved outbox input. `apps/ios/Wonder/ConnectionModel.swift:4660` indexes/saves pending intent before `deliver`. The spec starts at the successful persistence boundary, not the tap. |
| Phone FIFO and `acknowledge` | `apps/native/Sources/WonderPairing/Outbox.swift:137` selects the oldest nonrefused input; `:153` promotes it atomically. `apps/ios/Wonder/ConnectionModel.swift:4934` retries existing pending input first; `:4709` applies the receipt then saves the intent. `SendIntent.swift:181` checks client/body/conversation correlation. |
| `message=queued/guide`; `receive`, `retryReceipt` | `crates/wonder-store/src/lib.rs:1491` looks up `(device_id, client_message_id)` in the acceptance transaction; `:1597` inserts the message; `:1656` inserts `dispatch_work`; `:1662` inserts `guide_work`; `:1674` commits. The payload is abstracted as immutable for an ID. Conflicting bodies are refused, not a fresh input. |
| `queue`; `claim`, `reorder`, `cancelQueued` | `crates/wonder-store/src/lib.rs:2538` atomically rechecks busy state and the current head; `:2593` lists eligible heads. `crates/wonder-store/src/queue.rs:111` revision-checks the complete reorder under a write boundary; `:67` cancels/edits only pending work. |
| `message=claimed/submitting`, `wire` | `crates/wonder-store/src/lib.rs:2538` writes `dispatching_to_codex` plus attempt phase `claimed`; `:2563` commits phase `submitting` and thread identity. `crates/wonderd/src/projects.rs:10341` persists intent before `turn/start` at `:10407`. There is a crash boundary between these actions. |
| `delivered`, `providerReceipt`, `executing`; `providerAccept`, `bindReceipt` | `crates/wonderd/src/projects.rs:9606` commits `Submission::Started`; `services/claude-runtime/bridge.mjs:1129` checks replay and busy ownership; `:1187` journals acceptance before registering execution. `services/claude-runtime/sessions.mjs:340` deduplicates client IDs and `:351` refuses another ordinary running turn. Codex implementation is external: these are its assumed RPC semantics, not a source audit of Codex. |
| `uncertain`; `lostReceipt`, `reconcile` | `crates/wonderd/src/projects.rs:9647` preserves an uncertain outcome; `:10820` recovers uncertain receipts through native evidence. `crates/wonderd/src/lib.rs:7222` binds a reconciled Guide without checking whether its turn previously settled (QT-4). `crates/wonder-store/src/lib.rs:2606` releases claimed attempts but changes submitting attempts to uncertain after restart. `:2632` permits retry only for safe/failed states. No absent-history inference permits resubmission. |
| `guideTarget`; `submitGuide`, `acceptGuide`, `staleGuide`, `refuseGuide` | `crates/wonderd/src/lib.rs:6439` holds the conversation gate, checks expected turn and ownership; `:6546` persists the uncertain Guide claim before RPC; `:6567` treats a known preacceptance refusal as queueable. `crates/wonder-store/src/queue.rs:164` moves a queued input into Guide ownership; `:193` appends the undeliverable Guide at the tail. `services/claude-runtime/bridge.mjs:1213` journals Guide and reserves admission; `:1237` Stop waits for it. |
| `stops`, `cachedStops`, `stopCall`; `saveStop`, `providerInterrupt`, `stopAcknowledged` | `crates/wonderd/src/lib.rs:6725` takes conversation/dispatch locks; `:6851` persists Stop before interrupt; `:6886` waits for native terminal proof and otherwise returns HTTP 202. `crates/wonderd/src/projects.rs:200` persists and caches; `:223` falls back to the store. `crates/wonder-store/migrations/0150_turn_stop_requests.sql:1` and `turn_stops.rs:10` own durable Stop. |
| `stopRejected`, `stopResponseLost` | `crates/wonderd/src/lib.rs:6864` clears an owned Stop on RPC transport failure; `:6880` separately handles explicit refusal. `crates/wonderd/src/projects.rs:205` distinguishes an earlier Stop. The model preserves an earlier accepted Stop when a duplicate request is refused. |
| `oldStops`; `weekPasses`, `otherThreadStopSweeps` | `crates/wonder-store/src/turn_stops.rs:7` defines seven days; `:18` deletes all old Stop rows without joining active receipts; `:23` refreshes the saved row's timestamp. `crates/wonderd/src/projects.rs:333` prunes hour-old cache entries on new Stop; a host restart also clears them. The environment's other thread is reduced to these sweep effects. Time is coarse: intermediate hourly cache expiry is not separately explored. |
| `host`, `generation`, `acceptedGeneration`; `hostCrash`, `hostRestart` | `crates/wonderd/src/projects.rs:10550` remembers the accepting runtime generation; `:10722` requires the same live generation. Startup claim repair is `crates/wonder-store/src/lib.rs:2606`. `hostCrash` models a daemon restart that also loses its owned runtime, with durable DB/phone contents retained. |
| `recoverUnknown`, `settle`, `continueAfterRestart` | `crates/wonderd/src/projects.rs:10730` defers unreadable Stop; `:10740` chooses interruption recovery; `:10780` preserves unknown outcome as uncertain; `:10807` clears Stop after that pass. `crates/wonderd/src/update_handoff.rs:34` **excludes uncertain receipts**, `:64` saves a continuation, and `:884` claims before `turn/start`. |
| `approval`, `terminalKnown`, `consumed` | `crates/wonderd/src/approval_lifecycle.rs:21` correlates runtime/thread/turn and retires before message mapping. `crates/wonder-store/src/approvals.rs:155` persists terminal identity and closes requests in one transaction. `crates/wonder-store/src/lib.rs:2748` rejects answering an ended turn; `:2803` saves the resolution intent. `crates/wonderd/src/lib.rs:8822` serializes answer/terminal handling; `:9009` checks runtime identity and liveness before forwarding. |
| `journal`, `cursor`, `view`, `needsSnapshot`; replay actions | `apps/native/Sources/WonderPairing/ReadCache.swift:880` and `LiveEvents.swift:262` reject epoch mismatch/gaps and ignore duplicates. `apps/ios/Wonder/ConnectionModel.swift:7605` refreshes the checkpoint; `:7650` applies contiguous frames. This models receipt visibility, not timeline item contents or database replacement with a new epoch. |

## Bounds and abstractions

One existing owned Project thread, one authenticated pairing, three immutable
input IDs and six turn IDs; up to two runtime restarts and one restart continuation
per input. Both Codex and Claude Guide semantics are selected nondeterministically.
The production continuation setting is modeled **on**; the result is
conditional on that setting and frozen settings existing. Failed/completed native
outcomes share the terminal branch; interrupted is separate because it can cause
continuation.

Claude admission and execution handover are collapsed; the interval where Stop
may name either old or Guide turn is not separately interleaved. This pilot does
not prove that bridge race or background-task cancellation.

The locks make acceptance/receipt/Stop decisions atomic where code serializes
them; the provider call and durable submission boundaries remain separate. Stop
does not instantly stop an execution. The continuation's prepare/check/submit path
is collapsed to its eventual successful `turn/start`, with valid frozen settings,
idle native thread and no further fault in that path. The Rust reproductions run
that path rather than assuming it succeeds.

Receipt loss can occur before or after provider acceptance. No resubmission of an
uncertain input is modeled. A missing native receipt can therefore remain uncertain
forever; the safety claim never calls that successful delivery. Explicit reorder,
cancel, stale Guide-to-queue, duplicate phone requests and replay gaps are present.

Approvals model one blocking request per turn and its durable answer intent,
terminal retirement, and generation checks. They do not model cryptography,
per-tool access policy, optional questions, all failure-specific reopen paths,
transport retransmission within a provider, or provider bugs. The singleton
execution claim is conditional on provider busy rejection; it does not cover
independent desktop/T3 writers, background subagents, ACP, switching providers,
rewind, archive/delete, message retention, migrations, multiple phone identities,
attachments, or owner edits to queued bodies. Those need distinct refinements.

## Evidence and maintenance

The final report lives at the run store's `reports/QUINT-TURN.md`, with the exact
rebased branch head, commands/output and live/device limits. The code fixtures
are in [turn_reproductions.rs](turn_reproductions.rs), included only by
`update_handoff::tests`; the existing ingestion fixture has opt-in Stop-response loss and native cwd
metadata for these cases. Normal fixture behavior is unchanged.

When fixing a finding, update its characterization assertion to the desired
behavior, update the corresponding transition, move its counterexample into the
positive scenarios, and re-run the checker. Do not keep a stale expected failure
or remove a failing invariant to claim success. This pilot changes no production
behavior and records no performance improvement.

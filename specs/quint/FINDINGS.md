# Turn lifecycle counterexample triage

The findings below concern the existing implementation, including persisted
Stop commit `6c29b0345`. This branch adds specification and reproduction evidence;
it does **not** fix production. The final run report pins the exact checked head.

**Evidence labels:** measured = executed model or offline implementation fixture;
inferred = source-supported production reachability/impact; guessed = no evidence.
No guessed finding is presented. No live provider or physical-device claim is made.

## QT-1 — P2: retention can erase an unresolved, acknowledged Stop

**Contract:** `noContinuationAfterPersistedStop`. **Triage: real.**

**Cause (inferred from code):**
`crates/wonder-store/src/turn_stops.rs:18` deletes every Stop older than seven days
when any thread records a new Stop. It does not require a terminal message or
native turn. Stop is only checked through this row after memory is lost
(`crates/wonderd/src/projects.rs:223`). A still-active receipt, interrupted native
turn and missing Stop satisfy the restart-continuation branch at
`crates/wonderd/src/projects.rs:10740`; `update_handoff.rs:34` admits that active
receipt and `:890` eventually submits a fresh continuation.

**Counterexample (measured model, 14 transitions):** save input → accept/start →
bind receipt → persist Stop → provider interrupts → HTTP acknowledgement →
restart → advance beyond retention → another thread saves Stop, sweeping the old
row → recovery starts a continuation. `continuedAfterStop={1}` and a new execution
`{4}` violate the invariant. The receipt never became terminal before the sweep.

**Implementation reproduction:**
`quint_repro_expired_stop_continues_after_restart` calls the real Stop handler
(HTTP 202), verifies durable Stop and an active receipt, advances the timestamp
passed to the real `record_turn_stop` retention transaction, clears only the
fixture's in-memory Stop cache, restarts its fake provider and executes both real
recovery owners. It asserts the observed unwanted `turn/start` carries
`CONTINUE_AFTER_RESTART`, not replay of the original input.

**Production scenario (inferred):** a turn's native history/terminal confirmation
remains unavailable beyond retention, a Stop on another thread removes the row,
then a daemon restart and restored native history expose the interrupted turn.
A long downtime plus history-unavailable recovery also supplies this ordering.
Ordinary immediate Stop/restart still passes the existing positive test. The
test's virtual timestamp proves the retention mechanism; it does not claim an
eight-day field experiment or that every startup schedules the same way.

**Suggested fix:** retire Stop only after authoritative terminal settlement, or
retain it while an active/uncertain receipt or continuation can refer to the turn.
Do not age out an unresolved intent solely because another thread saved Stop.

## QT-2 — P2: a lost Stop response is treated as definitive refusal

**Contract:** `noContinuationAfterPersistedStop`. **Triage: real.**

**Cause (inferred from code):**
`crates/wonderd/src/lib.rs:6864` handles an RPC transport error by clearing this
call's durable Stop. This is the same withdrawal used for an explicit provider
refusal at `:6880`, although a missing response does not establish nonacceptance.
`projects.rs:10740` can consequently interpret the stopped native turn as a crash
and schedule continuation after a runtime restart.

**Counterexample (measured model, 12 transitions):** persist Stop → provider takes
the interrupt → lose the response → daemon withdraws its Stop → restart → continue.
The native turn was interrupted before the response disappeared.

**Implementation reproduction:**
`quint_repro_lost_stop_response_continues_after_restart` enables the owning
ingestion fixture's `lose-stop-response` fault. The provider writes the interrupted
native turn and exits without a response. The real handler returns HTTP 503 and
removes Stop; recovery subsequently sends one continuation.

**Qualification:** this is **not** a claim that HTTP 503 tells the owner Stop
succeeded. The UI reports failure. It violates the stronger persisted-intent
contract in this brief: work the provider already stopped is automatically
resumed because acknowledgement was lost. QT-1 independently violates the
acknowledged-Stop case.

**Suggested fix:** distinguish explicit nonacceptance from ambiguous transport
failure. Preserve durable Stop on ambiguity until native reconciliation proves
the turn's outcome; keep its response visibly unconfirmed. Preserve the existing
rule that a refused duplicate cannot withdraw an earlier accepted Stop.

## QT-3 — P2: later queued input overtakes an uncertain submission

**Contract:** `queuedMessagesInOrder`. **Triage: real against the brief's strict FIFO contract.**

**Cause (inferred from code):**
`crates/wonder-store/src/lib.rs:2606` correctly marks a submitting attempt uncertain
after restart, because it may have reached the provider. But eligibility at
`:2593` and the atomic claim at `:2538` exclude `uncertain` both from the busy set
and from the head-of-queue comparison. An earlier input with no native receipt
therefore ceases to hold later queued input. It is not explicitly cancelled and
is not automatically safe to retry (`:2632`).

**Counterexample (measured model, 12 transitions):** queue inputs 1 and 2 → persist
submission intent for 1 → crash before sending its bytes → restart marks 1
uncertain → claim and deliver 2. Input 1 is still retained, so `noLostMessage`
holds, but delivery order is not a prefix of the accepted queue order.

**Implementation reproduction:**
`quint_repro_later_queue_message_overtakes_uncertain_submission` queues two inputs
behind a running fixture turn, completes that turn, claims the first input and
persists submission intent without sending bytes. It calls the real startup
claim recovery, then dispatches the next eligible input through
`projects::dispatch`. The first remains uncertain with no receipt while the second
gets `accepted_by_codex`; the fake runtime records only the second client ID in
`turn/start`.

**Qualification:** the implementation may deliberately prefer progress after an
uncertain send. That is a product-policy choice, not proof of the requested FIFO
invariant. If overtaking is desired, the contract needs an explicit exception and
visible owner control. Neither user reorder nor cancellation occurs in this trace.

**Suggested fix:** preserve a per-conversation ordering barrier for unresolved
submissions; release it only with native outcome evidence or an explicit owner
decision. Do not automatically resend the uncertain request to restore order.

## QT-4 — P2: a late Guide receipt can revive an already settled stopped turn

**Contract:** `noContinuationAfterPersistedStop`. **Triage: real.**

**Cause (inferred from code):** after settling the stopped turn's known receipts,
`crates/wonderd/src/projects.rs:10807` clears Stop. An uncertain Guide with no
bound turn ID was outside that settlement. `crates/wonderd/src/lib.rs:7222`
accepts its later native client-ID proof and marks it accepted without checking
terminal turn identity or retired Stop. The periodic receipt scanner has the
same acceptance at `projects.rs:10903`. The newly bound message has no live
accepting generation, so `projects.rs:10740` can schedule continuation for that
same interrupted native turn. `crates/wonder-store/src/update_handoff.rs:18`
cancels only existing handoffs; Stop does not create a permanent cancellation
record when none exists.

**Counterexample (measured model, 20 transitions):** send/start input 1 → Guide 2
is taken into the same Codex turn but loses its receipt → persist/acknowledge Stop
→ restart → settle input 1 and clear Stop → late Guide proof binds input 2 to the
stopped turn → restart recovery issues a new continuation. Original message 1 is
terminal, `continuedAfterStop={1}`, and the Guide receipt now owns execution `{5}`.

**Implementation reproduction:**
`quint_repro_late_guide_receipt_continues_a_settled_stopped_turn` inserts and claims
an uncertain Guide, calls the real Stop handler, then executes recovery with
native interrupted status but no Guide client-ID proof. It supplies the late
proof through the fake native history, lazily reopens the idle runtime through
its real owner, calls the real `accept_reconciled_guide`, and runs both recovery
owners. This isolates receipt binding from the scanner's 60-second rescan delay;
it does not claim a live provider or a timing experiment. Provider acceptance
and delayed client-ID visibility are controlled fixture premises.

**Production scenario (inferred):** native turn status becomes readable before
the Guide's exact client-ID receipt, or native receipt pagination defers that
proof beyond settlement. The stopped turn's known message can be terminal while
an unbound Guide still waits for reconciliation. Codex Guide shares the native
turn ID, so the late proof can refer to the already stopped work.

**Suggested fix:** bind late receipts against authoritative terminal/Stop
identity and settle them to that outcome; do not make them active continuation
candidates. Preserve Stop/cancellation evidence until all possible receipt and
continuation references are retired. The terminal-approval ledger alone does
not guard restart continuation.

## Spec bugs and rejected hypotheses

| ID | Counterexample or hypothesis | Code check and disposition |
| --- | --- | --- |
| SB-1 | Clearing Stop on unknown native status might cause a later interrupted read to continue it. | Rejected hypothesis, **not a production continuation defect**. `projects.rs:10780` changes the receipt to uncertain, and `update_handoff.rs:34` admits only accepted/streaming. `crates/wonder-store/src/lib.rs:1997` changes every receipt for that thread/turn, including Codex Guide receipts. The offline fixture and `unknownReceiptCannotContinueTest` / `uncertainReceiptBlocksContinuationTest` preserve this distinction. Never weaken that guard in the model. The cleared row can affect display classification; this pilot does not claim that is a continuation. |
| SB-2 | The naive invariant `incorrectInstantStop` fails immediately after persistence because the execution still runs. | **Spec bug**, reproduced in seven model transitions. `lib.rs:6886` explicitly returns 202 while terminal confirmation is pending. Persistence precedes the interrupt; it is not synchronous completion. The actual invariant forbids a later continuation, while permitting the in-flight stop interval. |
| SB-3 | Source review found that a duplicate Stop must refresh its age, not inherit an old expiry marker. | **Abstraction correction**, not a production defect or a retained counterexample. `turn_stops.rs:23` replaces `requested_at`; the model now sweeps old rows and refreshes the saved row together. `refreshedStopDoesNotExpireTest` checks this. The modeled week also exceeds the memory cache's one-hour retention. |

Quint parser/type errors and errors while authoring the test harness are tooling
errors, not system counterexamples. The accepted traces are retained only after
typechecking and with their final postconditions validated by `check.py`.

## Reproduce against the code

Use the shared Rust target and storage lease procedure from `DEVELOPMENT.md`:

```sh
cargo test -p wonderd --lib update_handoff::tests::quint_ -- --nocapture --test-threads=2
# Existing positive controls and the rest of this recovery owner:
cargo test -p wonderd --lib update_handoff::tests:: -- --test-threads=2
```

The five added Rust tests are **characterization tests**: four assert the observed
defects and one disproves SB-1. They pass only when their expected implementation
behavior is reproduced. The desired-contract checks are the separate, genuinely
failing `quint run` invocations in `counterexamples.qnt` (exit 1, complete ITF
trace). No Rust test is ignored, disabled, or wrapped in `should_panic` to make a
product gate green. When fixing production, flip the characterization assertions
to the desired outcome and migrate the corresponding model trace to positive
coverage. The final report includes actual commands/output, test counts, and head.

/// Cupola's owned Project turn lifecycle, not the orchestrator.
/// See README.md for code mapping, abstraction boundaries and counterexamples.
/// Finite pilot: one thread, one pairing, three input IDs, two runtime restarts.
module turn {
  pure val Messages = 1.to(3)
  pure val Turns = 1.to(6)
  type State = {
    family: str, online: bool, host: bool, generation: int,
    issued: Set[int], phone: List[int], acknowledged: Set[int],
    message: int -> str, guideTarget: int -> int, queue: List[int],
    receipt: int -> int, providerReceipt: int -> int, acceptedGeneration: int -> int,
    wire: int, executing: Set[int], native: int -> str,
    delivered: Set[int], deliveryOrder: List[int], badOrder: bool,
    stops: Set[int], cachedStops: Set[int], oldStops: Set[int],
    protectedStops: Set[int], continued: Set[int], continuedAfterStop: Set[int],
    stopCall: int, ownStop: bool, interruptTaken: bool, storeReadable: bool,
    approval: int -> str, approvalGeneration: int -> int,
    terminalKnown: Set[int], consumed: int -> int,
    journal: List[int], cursor: int, view: Set[int], needsSnapshot: bool,
    sawRetry: bool, sawRestart: bool, sawGuide: bool, sawApproval: bool
  }
  var s: State

  pure def empty(family: str): State = {
    family: family, online: true, host: true, generation: 0,
    issued: Set(), phone: [], acknowledged: Set(),
    message: Messages.mapBy(_ => "absent"), guideTarget: Messages.mapBy(_ => 0), queue: [],
    receipt: Messages.mapBy(_ => 0), providerReceipt: Messages.mapBy(_ => 0), acceptedGeneration: Messages.mapBy(_ => -1),
    wire: 0, executing: Set(), native: Turns.mapBy(_ => "absent"),
    delivered: Set(), deliveryOrder: [], badOrder: false,
    stops: Set(), cachedStops: Set(), oldStops: Set(), protectedStops: Set(), continued: Set(), continuedAfterStop: Set(),
    stopCall: 0, ownStop: false, interruptTaken: false, storeReadable: true,
    approval: Turns.mapBy(_ => "absent"), approvalGeneration: Turns.mapBy(_ => -1),
    terminalKnown: Set(), consumed: Turns.mapBy(_ => 0),
    journal: [], cursor: 0, view: Set(), needsSnapshot: false,
    sawRetry: false, sawRestart: false, sawGuide: false, sawApproval: false
  }
  action init = {
    nondet family = Set("codex", "claude").oneOf()
    s' = empty(family)
  }
  // A reachable prefix for deeper exploration of Guide/approval/Stop races.
  // turn_tests checks exact equality against send -> receipt -> acknowledge.
  pure def running(family: str): State = empty(family)
    .with("issued", Set(1)).with("acknowledged", Set(1))
    .with("message", Messages.mapBy(m => if (m == 1) "active" else "absent"))
    .with("receipt", Messages.mapBy(m => if (m == 1) 1 else 0))
    .with("providerReceipt", Messages.mapBy(m => if (m == 1) 1 else 0))
    .with("acceptedGeneration", Messages.mapBy(m => if (m == 1) 0 else -1))
    .with("executing", Set(1)).with("native", Turns.mapBy(t => if (t == 1) "running" else "absent"))
    .with("delivered", Set(1)).with("deliveryOrder", [1]).with("journal", [1, 1])
  action activeInit = {
    nondet family = Set("codex", "claude").oneOf()
    s' = running(family)
  }

  // Save the phone intent before clearing the composer or making a request.
  action send(m: int): bool = all {
    not(s.issued.contains(m)),
    s' = s.with("issued", s.issued.union(Set(m))).with("phone", s.phone.append(m))
  }
  action guide(m: int, t: int): bool = all {
    s.executing.contains(t), not(s.issued.contains(m)),
    s' = s.with("issued", s.issued.union(Set(m))).with("phone", s.phone.append(m))
      .with("guideTarget", s.guideTarget.set(m, t))
  }
  // SQLite commits the ID/body + dispatch_work or guide_work atomically.
  // Only the oldest phone intent travels; a lost answer retains the same ID.
  action receive = {
    val m = s.phone.head()
    all {
      s.host, s.online, s.phone.length() > 0, s.message.get(m) == "absent",
      s' = s.with("message", s.message.set(m, if (s.guideTarget.get(m) == 0) "queued" else "guide"))
        .with("queue", if (s.guideTarget.get(m) == 0) s.queue.append(m) else s.queue)
        .with("journal", s.journal.append(m))
    }
  }
  action retryReceipt = {
    val m = s.phone.head()
    all {
      s.host, s.online, s.phone.length() > 0, s.message.get(m) != "absent",
      // Same (device, clientMessageId): no new work or new queue position.
      s' = s.with("sawRetry", true)
    }
  }
  action acknowledge = {
    val m = s.phone.head()
    all {
      s.host, s.online, s.phone.length() > 0, s.message.get(m) != "absent",
      s' = s.with("phone", s.phone.tail()).with("acknowledged", s.acknowledged.union(Set(m)))
    }
  }
  action disconnect = all { s.online, s' = s.with("online", false) }
  action reconnect = all { not(s.online), s' = s.with("online", true) }

  val queued = s.queue.select(m => s.message.get(m) == "queued")
  val guideInFlight = s.wire > 0 and s.guideTarget.get(s.wire) > 0
  val dispatchBusy = Messages.exists(m => Set("claimed", "submitting", "active").contains(s.message.get(m)))
  // The SQL claim rechecks both the head and active work, under the write boundary.
  action claim = {
    val m = queued.head()
    all {
      s.host, s.stopCall == 0, not(guideInFlight), not(dispatchBusy), queued.length() > 0,
      s' = s.with("message", s.message.set(m, "claimed"))
    }
  }
  action submit(m: int): bool = all {
    s.host, s.stopCall == 0, s.message.get(m) == "claimed", s.wire == 0,
    s' = s.with("message", s.message.set(m, "submitting")).with("wire", m)
  }
  // Explicit owner reorder is legal; FIFO means the current saved queue order.
  action reorder = all {
    s.host, queued.length() == 2,
    s' = s.with("queue", s.queue.select(m => s.message.get(m) != "queued")
      .concat([queued.nth(1), queued.nth(0)]))
  }
  action cancelQueued(m: int): bool = all {
    s.host, s.message.get(m) == "queued",
    s' = s.with("message", s.message.set(m, "cancelled")).with("queue", s.queue.select(x => x != m))
  }
  // Provider contract: turn/start refuses while an execution owns this thread.
  // It returns the existing receipt on duplicate client ID (no second execution).
  action providerAccept = {
    val m = s.wire
    all {
      s.host, m > 0, s.guideTarget.get(m) == 0, (s.executing.size() == 0), not(s.delivered.contains(m)),
      s' = s.with("executing", Set(m)).with("native", s.native.set(m, "running"))
        .with("providerReceipt", s.providerReceipt.set(m, m))
        .with("delivered", s.delivered.union(Set(m))).with("deliveryOrder", s.deliveryOrder.append(m))
        // A still-uncertain earlier send is an ordering obligation too. The
        // actual SQL claim excludes it, so this can find QT-3, not assume FIFO.
        .with("badOrder", s.badOrder or s.queue.select(x => not(s.delivered.contains(x))).head() != m)
    }
  }
  action bindReceipt = {
    val m = s.wire
    all {
      s.host, s.stopCall == 0, m > 0, s.delivered.contains(m),
      s' = s.with("message", s.message.set(m, "active")).with("receipt", s.receipt.set(m, s.providerReceipt.get(m)))
        .with("acceptedGeneration", s.acceptedGeneration.set(m, s.generation))
        .with("wire", 0).with("queue", s.queue.select(x => x != m))
        .with("journal", s.journal.append(m))
    }
  }
  action lostReceipt = {
    val m = s.wire
    all {
      s.host, m > 0,
      s' = s.with("message", s.message.set(m, "uncertain")).with("wire", 0)
    }
  }
  action providerRefuse = {
    val m = s.wire
    all {
      s.host, m > 0, s.guideTarget.get(m) == 0, not((s.executing.size() == 0)), not(s.delivered.contains(m)),
      s' = s.with("message", s.message.set(m, "queued")).with("wire", 0)
    }
  }
  // Only exact native client-ID evidence binds an uncertain receipt. Missing
  // history never authorizes resending it. This is safety, not eventual delivery.
  action reconcile(m: int): bool = all {
    s.host, s.stopCall == 0, s.message.get(m) == "uncertain", s.receipt.get(m) == 0,
    s.delivered.contains(m),
    s' = s.with("message", s.message.set(m, "active")).with("receipt", s.receipt.set(m, s.providerReceipt.get(m)))
      .with("queue", s.queue.select(x => x != m)).with("journal", s.journal.append(m))
  }

  // Guide either reaches its exact target, or moves to the END of the queue.
  // Send now on an existing queued row moves ownership in one transaction.
  action promoteQueuedGuide(m: int, t: int): bool = all {
    s.host, s.stopCall == 0, s.message.get(m) == "queued", s.executing.contains(t),
    s' = s.with("message", s.message.set(m, "guide")).with("guideTarget", s.guideTarget.set(m, t))
      .with("queue", s.queue.select(x => x != m))
  }
  action staleGuide(m: int): bool = all {
    s.host, s.stopCall == 0, s.message.get(m) == "guide", not(s.executing.contains(s.guideTarget.get(m))),
    s' = s.with("message", s.message.set(m, "queued")).with("guideTarget", s.guideTarget.set(m, 0))
      .with("queue", s.queue.append(m))
  }
  action submitGuide(m: int): bool = all {
    s.host, s.stopCall == 0, s.wire == 0, s.message.get(m) == "guide",
    s.executing.contains(s.guideTarget.get(m)),
    // claim_guide saves uncertain before making the RPC.
    s' = s.with("message", s.message.set(m, "uncertain")).with("wire", m)
  }
  action acceptGuide = {
    val m = s.wire
    val old = s.guideTarget.get(m)
    val t = if (s.family == "codex") old else m
    all {
      s.host, m > 0, old > 0, s.executing.contains(old), not(s.delivered.contains(m)),
      // Claude's old-reply finalization/process handover is one abstract step;
      // Codex Guide adds another message receipt to the SAME running turn.
      s' = s.with("delivered", s.delivered.union(Set(m))).with("executing", Set(t))
        .with("native", if (t == old) s.native else s.native.set(old, "completed").set(t, "running"))
        .with("providerReceipt", s.providerReceipt.set(m, t)).with("sawGuide", true)
    }
  }
  action refuseGuide = {
    val m = s.wire
    all {
      s.host, m > 0, s.guideTarget.get(m) > 0, not(s.delivered.contains(m)),
      not(s.executing.contains(s.guideTarget.get(m))),
      s' = s.with("message", s.message.set(m, "queued")).with("guideTarget", s.guideTarget.set(m, 0))
        .with("queue", s.queue.append(m)).with("wire", 0)
    }
  }

  // Stop holds the conversation + dispatch locks across persistence and RPC.
  // Persistence failure sends nothing; unreadable persistence defers recovery.
  action saveStop(t: int): bool = all {
    s.host, s.storeReadable, s.stopCall == 0, not(guideInFlight),
    Messages.exists(m => s.message.get(m) == "active" and s.receipt.get(m) == t),
    // The SQL sweep and this row's refreshed requested_at commit together.
    // A week-old cache entry is also beyond cache_stop's one-hour retention.
    s' = s.with("stops", s.stops.exclude(s.oldStops).union(Set(t)))
      .with("cachedStops", s.cachedStops.exclude(s.oldStops).union(Set(t))).with("oldStops", Set())
      .with("protectedStops", s.protectedStops.union(Set(t))).with("stopCall", t)
      .with("ownStop", not(s.stops.contains(t) or s.cachedStops.contains(t)))
      .with("interruptTaken", false)
  }
  action providerInterrupt = all {
    s.host, s.stopCall > 0, not(s.interruptTaken),
    // Completion can win the race before interrupt reaches the provider.
    s' = s.with("native", if (s.native.get(s.stopCall) == "running")
      s.native.set(s.stopCall, "interrupted") else s.native)
      .with("executing", s.executing.exclude(Set(s.stopCall))).with("interruptTaken", true)
  }
  action stopAcknowledged = all {
    s.host, s.stopCall > 0, s.interruptTaken,
    // The response is 202 while terminal confirmation is outstanding.
    s' = s.with("stopCall", 0)
  }
  action stopRejected = all {
    s.host, s.stopCall > 0, not(s.interruptTaken),
    s' = s.with("stops", if (s.ownStop) s.stops.exclude(Set(s.stopCall)) else s.stops)
      .with("cachedStops", if (s.ownStop) s.cachedStops.exclude(Set(s.stopCall)) else s.cachedStops)
      .with("protectedStops", if (s.ownStop) s.protectedStops.exclude(Set(s.stopCall)) else s.protectedStops)
      .with("stopCall", 0)
  }
  // lib.rs handles a transport error like refusal even if the provider took it.
  action stopResponseLost = all {
    s.host, s.stopCall > 0, s.interruptTaken,
    s' = s.with("stops", if (s.ownStop) s.stops.exclude(Set(s.stopCall)) else s.stops)
      .with("cachedStops", if (s.ownStop) s.cachedStops.exclude(Set(s.stopCall)) else s.cachedStops)
      .with("stopCall", 0)
  }
  action storageUnavailable = all { s.storeReadable, s' = s.with("storeReadable", false) }
  action storageAvailable = all { not(s.storeReadable), s' = s.with("storeReadable", true) }
  // Abstract >7 days offline/history-unavailable, followed by a Stop in another
  // thread. The SQL DELETE has no terminal-turn condition (finding QT-1).
  action weekPasses = all { not((s.stops.size() == 0)), s' = s.with("oldStops", s.stops) }
  action otherThreadStopSweeps = all {
    s.host, s.storeReadable, not((s.oldStops.size() == 0)),
    s' = s.with("stops", s.stops.exclude(s.oldStops))
      .with("cachedStops", s.cachedStops.exclude(s.oldStops)).with("oldStops", Set())
  }

  action hostCrash = all {
    s.host, s.generation < 2,
    s' = s.with("host", false).with("cachedStops", Set()).with("stopCall", 0)
      .with("executing", Set()).with("wire", 0)
      .with("native", Turns.mapBy(t => if (s.native.get(t) == "running") "interrupted" else s.native.get(t)))
  }
  action hostRestart = all {
    not(s.host),
    s' = s.with("host", true).with("generation", s.generation + 1).with("sawRestart", true)
      .with("message", Messages.mapBy(m =>
        if (s.message.get(m) == "claimed") "queued" else if (s.message.get(m) == "submitting") "uncertain" else s.message.get(m)))
  }
  action providerFinishes(t: int): bool = all {
    s.executing.contains(t),
    s' = s.with("executing", s.executing.exclude(Set(t))).with("native", s.native.set(t, "completed"))
  }
  // Missing/unsettled native status + lost runtime => uncertain, not terminal.
  // Code clears Stop here, but uncertain is NOT eligible for continuation.
  action recoverUnknown(m: int): bool = {
    val t = s.receipt.get(m)
    all {
      s.host, s.stopCall == 0, not(guideInFlight), s.storeReadable, s.message.get(m) == "active", t > 0,
      s.acceptedGeneration.get(m) != s.generation,
      // mark_runtime_turn_uncertain updates EVERY receipt for this thread/turn,
      // including multiple Codex Guide messages, not just the loop's message.
      s' = s.with("message", Messages.mapBy(x =>
        if (s.receipt.get(x) == t and Set("active", "uncertain").contains(s.message.get(x))) "uncertain" else s.message.get(x)))
        .with("stops", s.stops.exclude(Set(t))).with("cachedStops", s.cachedStops.exclude(Set(t)))
    }
  }
  action settle(t: int): bool = all {
    s.host, s.stopCall == 0, not(guideInFlight), s.storeReadable, Set("completed", "interrupted").contains(s.native.get(t)),
    Messages.exists(m => s.receipt.get(m) == t and Set("active", "uncertain").contains(s.message.get(m))),
    s' = s.with("message", Messages.mapBy(m => if (s.receipt.get(m) == t) "terminal" else s.message.get(m)))
      .with("stops", s.stops.exclude(Set(t))).with("cachedStops", s.cachedStops.exclude(Set(t)))
      .with("terminalKnown", s.terminalKnown.union(Set(t))).with("approval", s.approval.set(t, "resolved"))
  }
  // The user's Continue-after-restarts option is ON in this pilot. Settings
  // were frozen at dispatch. Only active receipts can schedule a continuation.
  action continueAfterRestart(m: int): bool = {
    val t = s.receipt.get(m)
    val nextTurn = m + 3
    all {
      s.host, s.stopCall == 0, not(guideInFlight), s.storeReadable, s.message.get(m) == "active", t > 0, t <= 3,
      s.acceptedGeneration.get(m) != s.generation, s.native.get(t) == "interrupted",
      not(s.continued.contains(t)), // existing update_handoff is never revived
      not(s.stops.contains(t) or s.cachedStops.contains(t)), (s.executing.size() == 0),
      s' = s.with("continued", s.continued.union(Set(t)))
        .with("continuedAfterStop", if (s.protectedStops.contains(t)) s.continuedAfterStop.union(Set(t)) else s.continuedAfterStop)
        .with("receipt", s.receipt.set(m, nextTurn))
        .with("native", s.native.set(nextTurn, "running")).with("executing", Set(nextTurn))
        .with("acceptedGeneration", s.acceptedGeneration.set(m, s.generation))
    }
  }

  // Correlation includes runtime generation + thread + turn; one blocking
  // request per turn suffices for lifecycle retirement (not payload validation).
  action requestApproval(t: int): bool = all {
    s.host, s.executing.contains(t), not(s.terminalKnown.contains(t)), s.approval.get(t) == "absent",
    s' = s.with("approval", s.approval.set(t, "pending"))
      .with("approvalGeneration", s.approvalGeneration.set(t, s.generation)).with("sawApproval", true)
  }
  action saveApprovalAnswer(t: int): bool = all {
    s.host, s.online, s.approval.get(t) == "pending", not(s.terminalKnown.contains(t)),
    s' = s.with("approval", s.approval.set(t, "resolving"))
  }
  action forwardApproval(t: int): bool = all {
    s.host, s.approval.get(t) == "resolving", not(s.terminalKnown.contains(t)),
    s.approvalGeneration.get(t) == s.generation,
    s' = s.with("approval", s.approval.set(t, "resolved"))
      .with("consumed", s.consumed.set(t, s.consumed.get(t) + (if (s.executing.contains(t)) 1 else 0)))
  }
  action retireOldApproval(t: int): bool = all {
    s.host, s.approvalGeneration.get(t) != s.generation,
    Set("pending", "resolving").contains(s.approval.get(t)),
    s' = s.with("approval", s.approval.set(t, "resolved"))
  }
  action replayNext = all {
    s.host, s.online, not(s.needsSnapshot), s.cursor < s.journal.length(),
    s' = s.with("view", s.view.union(Set(s.journal.nth(s.cursor)))).with("cursor", s.cursor + 1)
  }
  action replayDuplicate = all { s.online, s.cursor > 0, s' = s }
  action replayGap = all {
    s.online, s.cursor + 1 < s.journal.length(), s' = s.with("needsSnapshot", true)
  }
  action snapshot = all {
    s.host, s.online, s.needsSnapshot,
    s' = s.with("view", Messages.filter(m => s.message.get(m) != "absent"))
      .with("cursor", s.journal.length()).with("needsSnapshot", false)
  }

  // Isolate the remaining state space without the already-understood QT-1/2
  // fault actions. The default step below still includes both of them.
  action stepWithoutKnownStopFaults = {
    nondet m = Messages.oneOf()
    nondet t = Turns.oneOf()
    any {
      send(m), guide(m, t), receive, retryReceipt, acknowledge, disconnect, reconnect,
      claim, submit(m), reorder, cancelQueued(m), providerAccept, bindReceipt, lostReceipt, providerRefuse,
      reconcile(m), promoteQueuedGuide(m, t), staleGuide(m), submitGuide(m), acceptGuide, refuseGuide,
      saveStop(t), providerInterrupt, stopAcknowledged, stopRejected,
      storageUnavailable, storageAvailable, weekPasses,
      hostCrash, hostRestart, providerFinishes(t), recoverUnknown(m), settle(t), continueAfterRestart(m),
      requestApproval(t), saveApprovalAnswer(t), forwardApproval(t), retireOldApproval(t),
      replayNext, replayDuplicate, replayGap, snapshot,
      s' = s
    }
  }
  action step = any { stepWithoutKnownStopFaults, stopResponseLost, otherThreadStopSweeps }

  // Conservation, NOT liveness: an uncertain or refused intent is still owned
  // and inspectable. Permanent network/storage failure can prevent delivery.
  val noLostMessage = s.issued.forall(m => s.phone.indices().exists(i => s.phone.nth(i) == m) or s.message.get(m) != "absent")
  val durableAcknowledgement = s.acknowledged.forall(m => s.message.get(m) != "absent")
  val atMostOneActiveTurn = s.executing.size() <= 1
  val queuedMessagesInOrder = not(s.badOrder)
  val noContinuationAfterPersistedStop = (s.continuedAfterStop.size() == 0)
  val approvalsRetired = s.terminalKnown.forall(t => not(Set("pending", "resolving").contains(s.approval.get(t))))
  val approvalConsumedOnce = Turns.forall(t => s.consumed.get(t) <= 1)
  val replayIsSound = s.cursor <= s.journal.length() and s.view.forall(m => s.message.get(m) != "absent")
  val safety = noLostMessage and durableAcknowledgement and atMostOneActiveTurn
    and approvalsRetired and approvalConsumedOnce and replayIsSound
  val guideWitness = s.sawGuide
  val approvalWitness = s.sawApproval
  val restartWitness = s.sawRestart
  val retryWitness = s.sawRetry
  val deliveryWitness = s.delivered.size() > 0
  val stopViolationWitness = not(noContinuationAfterPersistedStop)
  val allInvariants = safety and queuedMessagesInOrder and noContinuationAfterPersistedStop
}

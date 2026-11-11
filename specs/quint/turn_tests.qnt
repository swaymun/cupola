module turn_tests {
  import turn.* from "./turn"

  run startCodex = (s' = empty("codex")).then(send(1)).then(receive)
    .then(claim).then(submit(1)).then(providerAccept).then(bindReceipt)
  run startClaude = (s' = empty("claude")).then(send(1)).then(receive)
    .then(claim).then(submit(1)).then(providerAccept).then(bindReceipt)

  run reachableActiveInitTest = startCodex.then(acknowledge).expect(s == running("codex"))
    .then(startClaude).then(acknowledge).expect(s == running("claude"))

  run offlineRetryTest = init.then(disconnect).then(send(1)).then(send(2))
    .expect(noLostMessage and s.phone == [1, 2]).then(reconnect).then(receive)
    .then(retryReceipt).then(retryReceipt).expect(s.queue == [1])
    .then(acknowledge).then(receive).expect(s.queue == [1, 2] and safety)

  run fifoTest = startCodex.then(acknowledge).then(send(2)).then(receive).then(acknowledge)
    .then(send(3)).then(receive).expect(s.queue == [2, 3])
    .then(providerFinishes(1)).then(settle(1)).then(claim).then(submit(2))
    .then(providerAccept).then(bindReceipt).then(providerFinishes(2)).then(settle(2))
    .then(claim).then(submit(3)).then(providerAccept).then(bindReceipt)
    .expect(s.deliveryOrder == [1, 2, 3] and allInvariants)

  run explicitReorderTest = startCodex.then(acknowledge).then(send(2)).then(receive).then(acknowledge)
    .then(send(3)).then(receive).then(reorder).expect(s.queue == [3, 2])
    .then(providerFinishes(1)).then(settle(1)).then(claim).then(submit(3))
    .then(providerAccept).then(bindReceipt).expect(s.deliveryOrder == [1, 3] and allInvariants)

  run codexGuideSharesTurnTest = startCodex.then(acknowledge).then(guide(2, 1))
    .then(receive).then(submitGuide(2)).then(acceptGuide).then(bindReceipt)
    .expect(s.receipt.get(1) == 1 and s.receipt.get(2) == 1 and atMostOneActiveTurn and s.sawGuide)

  run claudeGuideHandsOverTest = startClaude.then(acknowledge).then(guide(2, 1))
    .then(receive).then(submitGuide(2)).then(acceptGuide).then(bindReceipt)
    .expect(s.receipt.get(2) == 2 and s.native.get(1) == "completed" and s.executing == Set(2) and allInvariants)

  run staleGuideJoinsTailTest = startCodex.then(acknowledge).then(send(2)).then(receive).then(acknowledge)
    .then(guide(3, 1)).then(receive).then(providerFinishes(1)).then(staleGuide(3))
    .expect(s.queue == [2, 3] and noLostMessage)

  run queuedSendNowTest = startCodex.then(acknowledge).then(send(2)).then(receive).then(acknowledge)
    .then(promoteQueuedGuide(2, 1)).then(submitGuide(2)).then(acceptGuide).then(bindReceipt)
    .expect(s.queue == [] and s.receipt.get(2) == 1 and allInvariants)

  run cancelledInputIsRetainedTest = startCodex.then(acknowledge).then(send(2)).then(receive)
    .then(cancelQueued(2)).expect(s.message.get(2) == "cancelled" and s.queue == [] and allInvariants)

  run guideLostReceiptTest = startClaude.then(acknowledge).then(guide(2, 1)).then(receive)
    .then(submitGuide(2)).then(acceptGuide).then(lostReceipt).then(hostCrash).then(hostRestart)
    .then(reconcile(2)).expect(s.receipt.get(2) == 2 and noLostMessage and atMostOneActiveTurn)

  run guideTargetFinishesDuringSubmitTest = startCodex.then(acknowledge).then(guide(2, 1)).then(receive)
    .then(submitGuide(2)).then(providerFinishes(1)).then(refuseGuide)
    .expect(s.queue == [2] and s.message.get(2) == "queued" and allInvariants)

  run claimCrashIsRetryableTest = init.then(send(1)).then(receive).then(claim)
    .then(hostCrash).then(hostRestart).expect(s.message.get(1) == "queued" and allInvariants)

  run submissionCrashIsUncertainTest = init.then(send(1)).then(receive).then(claim).then(submit(1))
    .then(hostCrash).then(hostRestart).expect(s.message.get(1) == "uncertain" and noLostMessage)

  run lostReceiptUsesNativeProofTest = init.then(send(1)).then(receive).then(claim).then(submit(1))
    .then(providerAccept).then(lostReceipt).expect(s.message.get(1) == "uncertain")
    .then(reconcile(1)).expect(s.receipt.get(1) == 1 and s.deliveryOrder == [1] and allInvariants)

  run persistedStopSurvivesRestartTest = startCodex.then(saveStop(1)).then(providerInterrupt)
    .then(stopAcknowledged).then(hostCrash).then(hostRestart)
    .expect(s.stops == Set(1) and s.cachedStops == Set())

    .then(settle(1)).expect(s.continued == Set() and s.message.get(1) == "terminal" and allInvariants)

  run rejectedStopCanContinueTest = startCodex.then(saveStop(1)).then(stopRejected)
    .then(hostCrash).then(hostRestart).then(continueAfterRestart(1))
    .expect(s.continued == Set(1) and allInvariants)

  run completionWinsStopRaceTest = startCodex.then(saveStop(1)).then(providerFinishes(1))
    .then(providerInterrupt).then(stopAcknowledged).then(hostCrash).then(hostRestart)
    .expect(s.native.get(1) == "completed").then(settle(1)).expect(allInvariants)

  run refusedDuplicateKeepsAcceptedStopTest = startCodex.then(saveStop(1)).then(providerInterrupt)
    .then(stopAcknowledged).then(saveStop(1)).then(stopRejected)
    .expect(s.stops == Set(1)).then(hostCrash).then(hostRestart).then(continueAfterRestart(1).fail())

  run refreshedStopDoesNotExpireTest = startCodex.then(saveStop(1)).then(providerInterrupt)
    .then(stopAcknowledged).then(weekPasses).then(saveStop(1)).then(providerInterrupt)
    .then(stopAcknowledged).then(hostCrash).then(hostRestart)
    .expect(s.stops == Set(1) and s.oldStops == Set()).then(otherThreadStopSweeps.fail())

  run unreadableStopDefersTest = startCodex.then(saveStop(1)).then(providerInterrupt)
    .then(stopAcknowledged).then(hostCrash).then(hostRestart).then(storageUnavailable)
    .expect(s.message.get(1) == "active" and s.stops == Set(1) and allInvariants)

    .then(storageAvailable).then(settle(1)).expect(s.message.get(1) == "terminal" and allInvariants)

  run unknownReceiptCannotContinueTest = startCodex.then(saveStop(1)).then(providerInterrupt)
    .then(stopAcknowledged).then(hostCrash).then(hostRestart).then(recoverUnknown(1))
    .expect(s.message.get(1) == "uncertain" and s.stops == Set())

    .then(settle(1)).expect(s.continued == Set() and allInvariants)

  run unknownSharedTurnMarksEveryReceiptTest = startCodex.then(acknowledge).then(guide(2, 1))
    .then(receive).then(submitGuide(2)).then(acceptGuide).then(bindReceipt)
    .then(saveStop(1)).then(providerInterrupt).then(stopAcknowledged).then(hostCrash).then(hostRestart)
    .then(recoverUnknown(1)).expect(s.message.get(1) == "uncertain" and s.message.get(2) == "uncertain")
    .then(continueAfterRestart(2).fail())

  run approvalCompletionRaceTest = startCodex.then(requestApproval(1)).then(saveApprovalAnswer(1))
    .then(providerFinishes(1)).then(settle(1))

    .expect(s.approval.get(1) == "resolved" and s.consumed.get(1) == 0 and allInvariants)

  run approvalAnswerTest = startCodex.then(requestApproval(1)).then(saveApprovalAnswer(1))
    .then(forwardApproval(1)).then(providerFinishes(1)).then(settle(1))
    .expect(s.consumed.get(1) == 1 and allInvariants)

  run approvalRestartTest = startCodex.then(requestApproval(1)).then(saveApprovalAnswer(1))
    .then(hostCrash).then(hostRestart).then(retireOldApproval(1))
    .expect(s.approval.get(1) == "resolved" and s.consumed.get(1) == 0 and allInvariants)

  run reconnectGapSnapshotTest = startCodex.then(disconnect).then(reconnect)
    .then(replayGap).expect(s.needsSnapshot).then(snapshot).then(replayDuplicate)
    .expect(s.cursor == s.journal.length() and s.view == Set(1) and allInvariants)
  // fail() is last: Quint does not carry a next state out of a disabled action.
  run stoppedRestart = startCodex.then(saveStop(1)).then(providerInterrupt)
    .then(stopAcknowledged).then(hostCrash).then(hostRestart)
  run persistedStopBlocksContinuationTest = stoppedRestart.then(continueAfterRestart(1).fail())
  run unavailableStoreBlocksStopTest = startCodex.then(storageUnavailable).then(saveStop(1).fail())
  run unreadableStopBlocksContinuationTest = stoppedRestart.then(storageUnavailable).then(continueAfterRestart(1).fail())
  run unreadableStopBlocksSettlementTest = stoppedRestart.then(storageUnavailable).then(settle(1).fail())
  run uncertainReceiptBlocksContinuationTest = stoppedRestart.then(recoverUnknown(1)).then(continueAfterRestart(1).fail())
  run terminalBlocksApprovalForwardTest = startCodex.then(requestApproval(1)).then(saveApprovalAnswer(1))
    .then(providerFinishes(1)).then(settle(1)).then(forwardApproval(1).fail())
  run restartBlocksOldApprovalForwardTest = startCodex.then(requestApproval(1)).then(saveApprovalAnswer(1))
    .then(hostCrash).then(hostRestart).then(forwardApproval(1).fail())
}

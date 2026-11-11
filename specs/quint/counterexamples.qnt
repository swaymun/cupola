/// Each step uses an action in turn.qnt, starting from its empty state.
/// These desired-contract checks intentionally fail until production is fixed.
module counterexamples {
  import turn.* from "./turn"
  var pc: int
  action traceInit = all { s' = empty("codex"), pc' = 0 }
  action prefix =
    if (pc == 0) send(1)
    else if (pc == 1) receive
    else if (pc == 2) claim
    else if (pc == 3) submit(1)
    else if (pc == 4) providerAccept
    else if (pc == 5) bindReceipt
    else if (pc == 6) saveStop(1)
    else providerInterrupt
  action expiredStopStep = all {
    pc' = pc + 1,
    if (pc < 8) prefix
    else if (pc == 8) stopAcknowledged
    else if (pc == 9) hostCrash
    else if (pc == 10) hostRestart
    else if (pc == 11) weekPasses
    else if (pc == 12) otherThreadStopSweeps
    else if (pc == 13) continueAfterRestart(1)
    else s' = s
  }
  action lostStopResponseStep = all {
    pc' = pc + 1,
    if (pc < 8) prefix
    else if (pc == 8) stopResponseLost
    else if (pc == 9) hostCrash
    else if (pc == 10) hostRestart
    else if (pc == 11) continueAfterRestart(1)
    else s' = s
  }
  action uncertainQueueStep = all {
    pc' = pc + 1,
    if (pc == 0) send(1)
    else if (pc == 1) receive
    else if (pc == 2) acknowledge
    else if (pc == 3) send(2)
    else if (pc == 4) receive
    else if (pc == 5) claim
    else if (pc == 6) submit(1)
    // Crash after submission intent but BEFORE the provider takes the input.
    else if (pc == 7) hostCrash
    else if (pc == 8) hostRestart
    else if (pc == 9) claim
    else if (pc == 10) submit(2)
    else if (pc == 11) providerAccept
    else s' = s
  }
  action lateGuideReceiptStep = all {
    pc' = pc + 1,
    if (pc < 6) prefix
    else if (pc == 6) acknowledge
    else if (pc == 7) guide(2, 1)
    else if (pc == 8) receive
    else if (pc == 9) submitGuide(2)
    else if (pc == 10) acceptGuide
    else if (pc == 11) lostReceipt
    else if (pc == 12) saveStop(1)
    else if (pc == 13) providerInterrupt
    else if (pc == 14) stopAcknowledged
    else if (pc == 15) hostCrash
    else if (pc == 16) hostRestart
    // Native turn status is available before the Guide's client-ID proof.
    else if (pc == 17) settle(1)
    else if (pc == 18) reconcile(2)
    else if (pc == 19) continueAfterRestart(2)
    else s' = s
  }
  // SB-2: an incorrect instantaneous-Stop interpretation. The real endpoint
  // returns 202 while the provider is stopping; persistence is not completion.
  val incorrectInstantStop = s.stops.forall(t => not(s.executing.contains(t)))
}

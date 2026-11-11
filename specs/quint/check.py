#!/usr/bin/env python3
"""Run the pilot and retain honest expected failures, commands, output and traces.

An expected violation is accepted only when Quint exits 1, says invariant
violated, and the ITF trace contains the precise counterexample postcondition.
This is a specification/reproduction gate, NOT a product correctness verdict.
"""
from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import shlex
import subprocess
import time

ROOT = Path(__file__).resolve().parents[2]
QUINT = ["npx", "--yes", "@informalsystems/quint@0.33.1"]


def decode(value):
    if isinstance(value, list):
        return [decode(v) for v in value]
    if isinstance(value, dict):
        if "#bigint" in value:
            return int(value["#bigint"])
        if "#set" in value:
            return {decode(v) for v in value["#set"]}
        if "#map" in value:
            return {decode(k): decode(v) for k, v in value["#map"]}
        return {k: decode(v) for k, v in value.items()}
    return value


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--offline", action="store_true", help="use an already installed npm package")
    parser.add_argument("--samples", type=int, default=10000)
    args = parser.parse_args()
    if args.samples <= 0:
        parser.error("--samples must be positive")
    out = args.output.resolve()
    out.mkdir(parents=True, exist_ok=True)
    command = QUINT[:]
    if args.offline:
        command.insert(1, "--offline")
    head_command = ["git", "-c", "core.fsmonitor=false", "rev-parse", "HEAD"]
    head = subprocess.check_output(head_command, cwd=ROOT, text=True).strip()
    results = []

    def run(name, arguments, *, expected_violation=None):
        cmd = command + arguments
        print("$ " + shlex.join(cmd), flush=True)
        start = time.monotonic()
        result = subprocess.run(cmd, cwd=ROOT, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=600)
        elapsed = round(time.monotonic() - start, 3)
        (out / f"{name}.log").write_text(result.stdout)
        print(result.stdout.strip(), flush=True)
        entry = {"name": name, "command": shlex.join(cmd), "exit": result.returncode, "seconds": elapsed}
        if expected_violation:
            trace_file, contract = expected_violation
            assert result.returncode == 1 and "Invariant violated" in result.stdout, entry
            trace = json.loads(trace_file.read_text())
            state = decode(trace["states"][-1])["s"]
            assert contract(state), f"{name}: violation was not the expected counterexample"
            entry.update(verdict="expected counterexample reproduced", states=len(trace["states"]),
                         trace=trace_file.name, sha256=hashlib.sha256(trace_file.read_bytes()).hexdigest())
        else:
            assert result.returncode == 0, entry
            entry["verdict"] = "passed"
        results.append(entry)
        (out / "results.json").write_text(json.dumps({"head": head, "quint": "0.33.1", "checks": results}, indent=2) + "\n")
        return result.stdout

    for name in ["turn", "turn_tests", "counterexamples"]:
        run(f"typecheck-{name}", ["typecheck", f"specs/quint/{name}.qnt"])
    scenario_output = run("scenarios", ["test", "specs/quint/turn_tests.qnt", "--backend=rust", "--seed=1", "--verbosity=1"])
    assert "33 passing" in scenario_output, "expected all 33 scenarios to run"
    for initializer, seed in [("init", "42"), ("activeInit", "43")]:
        output = run(f"simulation-{initializer}", [
            "run", "specs/quint/turn.qnt", "--backend=rust", "--n-threads=2",
            f"--init={initializer}", "--invariant=safety", f"--seed={seed}",
            f"--max-samples={args.samples}", "--max-steps=100", "--verbosity=1",
            "--witnesses", "deliveryWitness", "guideWitness", "approvalWitness",
            "restartWitness", "retryWitness", "stopViolationWitness",
        ])
        if initializer == "activeInit" and args.samples >= 10000:
            # The reachable active prefix prevents vacuous Guide coverage.
            assert "guideWitness was witnessed in 0 trace" not in output, "no Guide coverage"
    random_trace = out / "random-QT-3.itf.json"
    random_trace.unlink(missing_ok=True)
    run("random-QT-3", [
        "run", "specs/quint/turn.qnt", "--backend=rust", "--n-threads=2",
        "--invariant=allInvariants", "--seed=42", "--max-samples=10000",
        "--max-steps=100", "--verbosity=1", f"--out-itf={random_trace}",
    ], expected_violation=(random_trace, lambda s:
        s["badOrder"] and bool(s["delivered"])
        and any(s["message"][m] == "uncertain" and s["providerReceipt"][m] == 0
                for m in s["queue"])))
    cases = [
        ("QT-1", "expiredStopStep", "noContinuationAfterPersistedStop", 14,
         lambda s: s["continuedAfterStop"] == {1} and s["executing"] == {4}),
        ("QT-2", "lostStopResponseStep", "noContinuationAfterPersistedStop", 12,
         lambda s: s["continuedAfterStop"] == {1} and s["executing"] == {4}),
        ("QT-3", "uncertainQueueStep", "queuedMessagesInOrder", 12,
         lambda s: s["badOrder"] and s["message"][1] == "uncertain" and s["delivered"] == {2}),
        ("QT-4", "lateGuideReceiptStep", "noContinuationAfterPersistedStop", 20,
         lambda s: s["continuedAfterStop"] == {1} and s["executing"] == {5}
         and s["message"][1] == "terminal" and s["receipt"][2] == 5),
        ("SB-2", "expiredStopStep", "incorrectInstantStop", 7,
         lambda s: s["stops"] == {1} and s["executing"] == {1} and s["continuedAfterStop"] == set()),
    ]
    for name, step, invariant, steps, contract in cases:
        trace = out / f"{name}.itf.json"
        # Remove only this invocation's old trace: a failed parser must never
        # accidentally validate an artifact left by an earlier invocation.
        trace.unlink(missing_ok=True)
        run(name, [
            "run", "specs/quint/counterexamples.qnt", "--init=traceInit", f"--step={step}",
            f"--invariant={invariant}", "--backend=typescript", "--seed=1", "--max-samples=1",
            f"--max-steps={steps}", "--verbosity=1", f"--out-itf={trace}",
        ], expected_violation=(trace, contract))
    assert subprocess.check_output(head_command, cwd=ROOT, text=True).strip() == head, "HEAD changed during the checks; rerun"
    print(f"Pilot checks passed at {head}; four desired-contract counterexamples reproduced. See FINDINGS.md for code triage; SB-2 is a spec bug.")


if __name__ == "__main__":
    main()

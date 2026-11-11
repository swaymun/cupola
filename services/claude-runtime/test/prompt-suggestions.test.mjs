import test from "node:test";
import assert from "node:assert/strict";
import { PromptSuggestions } from "../prompt-suggestions.mjs";
import { nativeTurns } from "../project-history.mjs";

test("optional native suggestions reject errors, malformed payloads and newer turns", () => {
  for (const status of ["failed", "interrupted", "completed"]) {
    for (const change of [{ suggestion: "" }, { suggestion: "x".repeat(2001) }, { uuid: null }, { session_id: "other" }, { parent_tool_use_id: "child" }]) {
      const owner = new PromptSuggestions(); owner.complete("turn", "session", { status });
      assert.equal(owner.take({ type: "prompt_suggestion", uuid: "hint", session_id: "session", suggestion: "Next", ...change }, "turn"), null);
    }
    const owner = new PromptSuggestions(); owner.complete("turn", "session", { status });
    assert.equal(owner.take({ uuid: "hint", session_id: "session", suggestion: "Next" }, "newer-turn"), null);
  }
});

test("history keeps the SDK hint only on its original latest completed turn", () => {
  const hint = { id: "hint", type: "promptSuggestion", text: "Next", agentFamily: "claude" };
  const journal = [{ id: "one", status: "completed", items: [hint] }];
  const messages = [{ type: "user", uuid: "one", message: { content: "First" } }];
  assert.equal(nativeTurns(messages, journal)[0].items.at(-1), hint);
  assert.equal(nativeTurns(messages, journal, true)[0].items.some(i => i.type === "promptSuggestion"), false);
  messages.push({ type: "user", uuid: "two", message: { content: "Newer" } });
  assert.equal(nativeTurns(messages, journal).some(t => t.items.some(i => i.type === "promptSuggestion")), false);
});

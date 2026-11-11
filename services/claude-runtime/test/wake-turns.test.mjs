import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync, writeFileSync } from "node:fs";
import { nativeTurns, rewindPoint, turnEndMessageId } from "../project-history.mjs";

// Synthetic fixture: an owner message, then three background-command wakes. Each
// wake is followed by tool calls and a final reply, as Claude Code writes them.
const WAKES = [
  ["alpha", "completed", "Background command \"Build the app\" completed (exit code 0)"],
  ["beta", "failed", "Background command \"Run &quot;slow&quot; tests &amp; lint\" failed (exit code 1)"],
  ["gamma", "killed", "Background command \"Watch the logs\" was stopped"],
];
const at = n => `2026-11-03T10:${String(n).padStart(2, "0")}:00Z`;
const ask = { type: "user", uuid: "owner-1", timestamp: at(0), origin: { kind: "human" }, message: { content: "Run the build" } };
const launch = (id, n) => [
  { type: "assistant", uuid: `la-${id}`, timestamp: at(n), message: { id: `lm-${id}`, content: [
    { type: "tool_use", id: `bg-${id}`, name: "Bash", input: { command: "make", description: `Task ${id}`, run_in_background: true } }] } },
  { type: "user", uuid: `lr-${id}`, timestamp: at(n), message: { content: [{ type: "tool_result", tool_use_id: `bg-${id}`,
    content: `Command running in background with ID: ${id}. Output is being written to: /tmp/${id}.output` }] } }];
const wake = (id, status, summary, n, shape) => {
  const text = `<task-notification>\n<task-id>${id}</task-id>\n<tool-use-id>bg-${id}</tool-use-id>\n<status>${status}</status>\n<summary>${summary}</summary>\n</task-notification>`;
  return shape === "legacy" ? { type: "user", uuid: `wake-${id}`, timestamp: at(n), message: { content: text } }
    : { type: "user", uuid: `wake-${id}`, timestamp: at(n), origin: { kind: "task-notification" }, promptSource: "system", message: { content: text } };
};
const turnWork = (id, n) => [
  { type: "assistant", uuid: `wa-${id}`, timestamp: at(n), message: { id: `wm-${id}`, content: [
    { type: "tool_use", id: `read-${id}`, name: "Read", input: { file_path: `/tmp/${id}.txt` } }] } },
  { type: "user", uuid: `wr-${id}`, timestamp: at(n), message: { content: [{ type: "tool_result", tool_use_id: `read-${id}`, content: "ok" }] } },
  { type: "assistant", uuid: `wf-${id}`, timestamp: at(n + 1), message: { id: `wfm-${id}`, content: [{ type: "text", text: `Reply for ${id}` }] } }];
const transcript = shape => [ask,
  ...WAKES.flatMap(([id], i) => launch(id, 1 + i)),
  { type: "assistant", uuid: "first", timestamp: at(5), message: { id: "first-m", content: [{ type: "text", text: "Started three tasks" }] } },
  ...WAKES.flatMap(([id, status, summary], i) => [wake(id, status, summary, 10 + i * 5, shape), ...turnWork(id, 11 + i * 5)])];

for (const shape of ["origin", "legacy"]) {
  test(`each background wake is its own turn with a row, its work and its reply (${shape})`, () => {
    const turns = nativeTurns(transcript(shape), []);
    assert.deepEqual(turns.map(t => t.id), ["owner-1", "wake-alpha", "wake-beta", "wake-gamma"]);
    assert.deepEqual(turns.map(t => t.items[0].type), ["userMessage", "taskNotification", "taskNotification", "taskNotification"]);
    const labels = turns.slice(1).map(t => [t.items[0].text, t.items[0].taskStatus, t.items[0].kind]);
    assert.deepEqual(labels, [
      ["Command \"Build the app\" finished", "completed", "command"],
      ["Command \"Run \"slow\" tests & lint\" failed", "failed", "command"],
      ["Command \"Watch the logs\" stopped", "interrupted", "command"]]);
    for (const [i, [id]] of WAKES.entries()) {
      const turn = turns[i + 1];
      assert.equal(turn.items.length, 3);
      assert.equal(turn.items[2].type, "agentMessage");
      assert.equal(turn.items.at(-1).text, `Reply for ${id}`);
      assert.notEqual(turn.items.at(-1).phase, "commentary");
      assert.equal(turn.status, "completed");
    }
    // The owner turn keeps its own reply and none of the wakes' replies.
    assert.deepEqual(turns[0].items.filter(i => i.type === "agentMessage").map(i => i.text), ["Started three tasks"]);
    assert.ok(!JSON.stringify(turns).includes("task-notification"));
  });
}

test("a wake with no summary still reads plainly", () => {
  const turns = nativeTurns([ask, { type: "assistant", uuid: "r", message: { id: "rm", content: [{ type: "text", text: "Ok" }] } },
    { type: "user", uuid: "w", origin: { kind: "task-notification" }, message: { content:
    "<task-notification><task-id>x</task-id><status>completed</status></task-notification>" } },
    { type: "assistant", uuid: "a", message: { id: "m", content: [{ type: "text", text: "Noted" }] } }], []);
  assert.equal(turns[1].items[0].text, "Background task finished");
});

// Claude Code also writes a report that lands while a turn is still working (a
// queued command after a tool result). That one belongs to the running turn.
test("a report that lands mid-turn stays inside the running turn", () => {
  const queued = { type: "user", uuid: "queued", origin: { kind: "task-notification" }, message: { content:
    "<task-notification><task-id>a</task-id><status>completed</status><summary>Background command \"Build\" completed</summary></task-notification>" } };
  const turns = nativeTurns([ask, ...launch("a", 1), queued,
    { type: "assistant", uuid: "w1", message: { id: "w1m", content: [{ type: "tool_use", id: "r1", name: "Read", input: { file_path: "/tmp/a" } }] } },
    { type: "user", uuid: "w1r", message: { content: [{ type: "tool_result", tool_use_id: "r1", content: "ok" }] } },
    { type: "assistant", uuid: "fin", message: { id: "finm", stop_reason: "end_turn", content: [{ type: "text", text: "All done" }] } }], []);
  assert.deepEqual(turns.map(t => t.id), ["owner-1"]);
  assert.equal(turns[0].items.at(-1).text, "All done");
  assert.ok(!turns[0].items.some(i => i.type === "taskNotification"));
});

test("a wake turn can be forked at and edited from, and the owner turn ends before it", () => {
  const messages = transcript("origin");
  assert.equal(turnEndMessageId(messages, "wake-beta"), "wf-beta");
  assert.equal(turnEndMessageId(messages, "owner-1"), "first");
  assert.deepEqual(rewindPoint(messages, "wake-beta"), { index: messages.findIndex(m => m.uuid === "wake-beta"), keep: "wf-alpha" });
});

test("a wake run that was stopped reads interrupted even when journaled as completed", () => {
  const messages = transcript("origin");
  messages.splice(messages.findIndex(m => m.uuid === "wake-gamma") + 1, 0, { type: "user", uuid: "stop", message: { content: [{ type: "text", text: "[Request interrupted by user]" }] } });
  const journal = [{ id: "wake-gamma", status: "completed", error: null, items: [] }];
  assert.equal(nativeTurns(messages, journal).at(-1).status, "interrupted");
});

// The host's history test (crates/wonderd/src/history_tests.rs) reads this file,
// so it projects exactly what the runtime writes. Regenerate it with
// `WAKE_FIXTURE_UPDATE=1 node --test test/wake-turns.test.mjs`.
test("the runtime's wake turns match the fixture the host test reads", () => {
  const file = new URL("./fixtures/wake-turns.json", import.meta.url);
  const turns = nativeTurns(transcript("origin"), []);
  if (process.env.WAKE_FIXTURE_UPDATE) writeFileSync(file, `${JSON.stringify(turns, null, 2)}\n`);
  assert.deepEqual(JSON.parse(readFileSync(file, "utf8")), JSON.parse(JSON.stringify(turns)),
    "regenerate: WAKE_FIXTURE_UPDATE=1 node --test test/wake-turns.test.mjs");
});

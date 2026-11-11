// Contract: Stop, cancellation and replacement end only their driver generation.
import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { CuaDriver } from '../cua-driver.mjs';

// The Cua Driver's one lifecycle owner, driven through start, Stop, cancel,
// timeout, failed start and replacement orderings across two generations,
// while a second thread's driver acts throughout. This driver takes INIT ms
// to start (failing with FAIL=error, exiting with FAIL=exit), acts after the
// call's `ms` and records NAME, GEN and the text, and its process ID.
const TABLE_DRIVER = `const e = process.env; require("fs").appendFileSync(e.MARK + ".pids", process.pid + "\\n");
require("readline").createInterface({ input: process.stdin }).on("line", line => {
  const m = JSON.parse(line); if (m.id == null) return;
  const answer = (result, error) => process.stdout.write(JSON.stringify({ jsonrpc: "2.0", id: m.id, ...(error ? { error } : { result }) }) + "\\n");
  if (m.method === "initialize") return setTimeout(() => e.FAIL === "exit" ? process.exit(1) : e.FAIL ? answer(null, { code: -32603, message: "no display" })
    : answer({ protocolVersion: m.params.protocolVersion, capabilities: { tools: {} }, serverInfo: { name: "cua", version: "1" } }), +(e.INIT ?? 0));
  if (m.method === "tools/list") return answer({ tools: [{ name: "type_text", inputSchema: { type: "object", properties: { text: { type: "string" }, ms: { type: "number" } } } }] });
  if (m.method !== "tools/call") return answer({});
  setTimeout(() => { require("fs").appendFileSync(e.MARK, e.NAME + e.GEN + ":" + m.params.arguments.text + "\\n");
    answer({ content: [{ type: "text", text: "typed" }], isError: false }); }, m.params.arguments.ms ?? 0); });`;
// Each step is [action, ...]; `gen` sets how thread A's next driver behaves.
// Cancellation closes the transport and aborts the request concurrently; either
// can reject first. The contract is rejection, no action and no surviving process,
// asserted below, rather than the MCP client's race-dependent error wording.
const DRIVER_ORDERINGS = [
  { name: 'Stop during an action, then a new call', steps: [['gen', 1], ['call', 'a1', 300], ['wait', 100], ['stop'], ['gen', 2], ['call', 'a2'],
    ['rejects', 'a1'], ['resolves', 'a2']] },
  { name: 'a cancelled call, then a call at once', steps: [['gen', 1], ['call', 'a1', 300], ['wait', 100], ['gen', 2], ['cancel', 'a1'], ['call', 'a2'],
    ['rejects', 'a1'], ['resolves', 'a2']] },
  { name: 'a timeout, then Stop, then a new call', timeoutMs: 150, steps: [['gen', 1], ['call', 'a1', 300], ['rejects', 'a1', /timed out/i], ['stop'],
    ['gen', 2], ['call', 'a2'], ['resolves', 'a2']] },
  { name: 'Stop while starting, replaced at once', steps: [['gen', 1, { INIT: '200' }], ['call', 'a1'], ['wait', 50], ['stop'], ['gen', 2], ['call', 'a2'],
    ['rejects', 'a1'], ['resolves', 'a2']] },
  { name: 'a failed start, then a new call', steps: [['gen', 1, { FAIL: 'error' }], ['call', 'a1'], ['rejects', 'a1', /no display/],
    ['gen', 2], ['call', 'a2'], ['resolves', 'a2']] },
  { name: 'a driver that exits while starting, then a new call', steps: [['gen', 1, { FAIL: 'exit' }], ['call', 'a1'], ['rejects', 'a1'],
    ['gen', 2], ['call', 'a2'], ['resolves', 'a2']] },
  { name: 'Stop while starting, the old start failing after its replacement began', steps: [['gen', 1, { INIT: '150', FAIL: 'error' }], ['call', 'a1'],
    ['wait', 50], ['stop'], ['gen', 2, { INIT: '250' }], ['call', 'a2'], ['rejects', 'a1'], ['resolves', 'a2']] },
  { name: 'Stop while starting, then the stopped call cancelled after its replacement began', steps: [['gen', 1, { INIT: '200' }], ['call', 'a1'],
    ['wait', 50], ['stop'], ['gen', 2], ['call', 'a2'], ['cancel', 'a1'], ['rejects', 'a1'], ['resolves', 'a2']] },
];

for (const row of DRIVER_ORDERINGS) test(`computer-use driver lifecycle: ${row.name}`, { timeout: 10000 }, async t => {
  const directory = await mkdtemp(join(tmpdir(), 'wonder-cua-owner-'));
  const mark = join(directory, 'actions');
  const owner = (NAME, timeoutMs) => new CuaDriver({ environment: process.env, timeoutMs,
    server: { command: process.execPath, args: ['-e', TABLE_DRIVER], env: { MARK: mark, NAME, GEN: '1' } } });
  const a = owner('A', row.timeoutMs), b = owner('B');
  t.after(async () => { a.stop(); b.stop(); await rm(directory, { recursive: true, force: true }); });
  const calls = new Map(), wait = ms => new Promise(resolve => setTimeout(resolve, ms));
  const call = (id, ms) => {
    const stop = new AbortController(), result = a.call('type_text', { text: id, ms }, stop.signal);
    result.catch(() => {});
    calls.set(id, { stop, result });
  };
  const typed = [{ type: 'text', text: 'typed' }];
  // Thread B acts throughout thread A's orderings.
  const other = b.call('type_text', { text: 'b', ms: 250 }, new AbortController().signal);
  other.catch(() => {}); // Cleanup after an earlier assertion still owns this call.
  for (const [action, ...args] of row.steps) {
    if (action === 'gen') a.server = { ...a.server, env: { MARK: mark, NAME: 'A', GEN: String(args[0]), ...args[1] } };
    else if (action === 'call') call(args[0], args[1]);
    else if (action === 'cancel') calls.get(args[0]).stop.abort();
    else if (action === 'stop') a.stop();
    else if (action === 'wait') await wait(args[0]);
    else if (action === 'rejects') await assert.rejects(calls.get(args[0]).result, args[1]);
    else if (action === 'resolves') assert.deepEqual((await calls.get(args[0]).result).content, typed);
  }
  assert.deepEqual((await other).content, typed, 'thread B');
  // Stop still reaches whichever generation is current.
  call('tail', 300); await wait(100); a.stop();
  await assert.rejects(calls.get('tail').result);
  await wait(400);
  // Only the second generation's call and thread B's acted, and no driver
  // process outlived Stop.
  assert.deepEqual((await readFile(mark, 'utf8')).split('\n').filter(Boolean).sort(), ['A2:a2', 'B1:b']);
  const alive = pid => { try { process.kill(pid, 0); return true; } catch { return false; } };
  const pids = (await readFile(`${mark}.pids`, 'utf8')).split('\n').filter(Boolean).map(Number);
  b.stop(); await wait(200);
  assert.deepEqual(pids.filter(alive), []);
});

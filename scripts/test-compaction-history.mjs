#!/usr/bin/env node
// Read a synthetic Codex rollout with the installed App Server. No model turn,
// credentials, real chats, or installed Cupola service are used.
import test from 'node:test';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createInterface } from 'node:readline';
import { mkdtemp, mkdir, writeFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

test('Codex rollout compaction has one private completed marker in item order', { timeout: 45_000 }, async () => {
  const root = await mkdtemp(join(tmpdir(), 'wonder-compaction-'));
  const thread = 'b1188871-c86b-4ddd-9fdc-5bd08df17a11', turn = 'b1188872-c86b-4ddd-9fdc-5bd08df17a11';
  let child;
  try {
    const folder = join(root, 'sessions/2026/10/27');
    await mkdir(folder, { recursive: true });
    const completed = (item, at) => ['event_msg', { type: 'item_completed', thread_id: thread, turn_id: turn,
      item, started_at_ms: at, completed_at_ms: at }];
    const rows = [
      ['session_meta', { id: thread, timestamp: '2026-10-27T00:00:00Z', cwd: root,
        originator: 'codex_cli_rs', cli_version: '0.164.0-alpha.11.1', source: 'cli', model_provider: 'openai', history_mode: 'paginated' }],
      ['event_msg', { type: 'task_started', turn_id: turn, root_turn_id: turn, started_at: 1793059201,
        turn_attribution: { turn_id: turn, root_turn_id: turn, turn_trigger: 'exec', parent_turn_id: null, initiating_agent_path: null },
        model_context_window: 200000, collaboration_mode_kind: 'default' }],
      completed({ type: 'UserMessage', id: 'question', content: [{ type: 'text', text: 'Fixture question', text_elements: [] }] }, 1793059202000),
      completed({ type: 'AgentMessage', id: 'before', content: [{ type: 'Text', text: 'Before boundary' }], phase: 'commentary' }, 1793059203000),
      completed({ type: 'ContextCompaction', id: 'compact1' }, 1793059204000),
      ['compacted', { message: 'PRIVATE SYNTHETIC SUMMARY', replacement_history: [] }],
      completed({ type: 'AgentMessage', id: 'after', content: [{ type: 'Text', text: 'After boundary' }], phase: 'final_answer' }, 1793059206000),
      ['event_msg', { type: 'task_complete', turn_id: turn, last_agent_message: 'After boundary' }],
    ];
    await writeFile(join(folder, `rollout-2026-10-27T00-00-00-${thread}.jsonl`), rows.map(([type, payload], ordinal) =>
      JSON.stringify({ timestamp: `2026-10-27T00:00:0${ordinal}Z`, ordinal, type, payload })).join('\n') + '\n');
    child = spawn(process.env.WONDER_CODEX_BIN ?? '/Applications/ChatGPT.app/Contents/Resources/codex-cli/bin/codex',
      ['app-server', '--stdio'], { cwd: root, env: { ...process.env, CODEX_HOME: root }, stdio: ['pipe', 'pipe', 'pipe'] });
    let nextID = 0;
    const pending = new Map();
    createInterface({ input: child.stdout }).on('line', line => {
      try { const message = JSON.parse(line); pending.get(message.id)?.(message); } catch { /* Non-protocol output. */ }
    });
    let stderr = '';
    child.stderr.on('data', bytes => { stderr = (stderr + bytes).slice(-4096); });
    const request = (method, params) => new Promise((resolve, reject) => {
      const id = ++nextID;
      const timeout = setTimeout(() => reject(new Error(`${method} timed out: ${stderr}`)), 15000);
      pending.set(id, message => { clearTimeout(timeout); pending.delete(id);
        message.error ? reject(new Error(JSON.stringify(message.error))) : resolve(message.result); });
      child.once('error', error => { clearTimeout(timeout); reject(error); });
      child.stdin.write(JSON.stringify({ id, method, params }) + '\n');
    });
    await request('initialize', { clientInfo: { name: 'wonder-compaction-fixture', version: '1' }, capabilities: { experimentalApi: true } });
    child.stdin.write('{"method":"initialized"}\n');
    await request('thread/read', { threadId: thread });
    await request('thread/resume', { threadId: thread, excludeTurns: true });
    await request('thread/turns/list', { threadId: thread, itemsView: 'full' });
    for (let read = 0; read < 2; read++) {
      const { data } = await request('thread/items/list', { threadId: thread, sortDirection: 'asc' });
      assert.deepEqual(data.map(row => row.item.id), ['question', 'before', 'compact1', 'after']);
      const markers = data.filter(row => row.item.type === 'contextCompaction');
      assert.equal(markers.length, 1);
      assert.equal(markers[0].turnId, turn);
      assert.equal(markers[0].completedAtMs, 1793059204000);
      assert.equal(JSON.stringify(data).includes('PRIVATE'), false);
    }
  } finally {
    if (child?.pid && child.exitCode === null) {
      const exited = new Promise(resolve => child.once('exit', resolve));
      child.kill('SIGTERM');
      await exited;
    }
    await rm(root, { recursive: true, force: true });
  }
});

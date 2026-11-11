#!/usr/bin/env node
// Offline only: project generated SDK blocks through the real presentation owner.
// Importing these modules does not create a runtime, session store or SDK client.
import { readFileSync } from 'node:fs';
import { TurnProjection } from '../services/claude-runtime/projection.mjs';
import { Sessions } from '../services/claude-runtime/sessions.mjs';

const input = JSON.parse(readFileSync(0, 'utf8'));
if (!Array.isArray(input.turns) || input.turns.length > 1000) throw new Error('Fixture input bound exceeded');
const replay = [], turns = [];
let atMs = 0;
const threadId = 'fixture-claude';
const emit = message => replay.push({ atMs, direction: 'server', message: structuredClone(message) });
for (const [turnIndex, spec] of input.turns.entries()) {
  const projection = new TurnProjection({ threadId, turnId: spec.id, emit });
  projection.start();
  let sourceAt = atMs, latest = atMs;
  for (const item of spec.items) {
    const timing = input.timing[item.id];
    sourceAt += timing.gapMs; atMs = sourceAt;
    if (item.type === 'userMessage') {
      // User inputs are normally admitted by Sessions.accept; the fixture is inert.
      const user = projection.startItem({ ...item, status: 'inProgress' });
      projection.finishItem(user);
    } else if (item.type === 'agentMessage') {
      const size = input.chunkChars || item.text.length;
      const chunks = Math.ceil(item.text.length / size);
      const delay = input.chunkChars ? Math.min(35, Math.floor(timing.gapMs / chunks)) : 0;
      atMs -= chunks * delay;
      projection.accept({ type: 'stream_event', event: { type: 'message_start', message: { id: item.id } } });
      projection.accept({ type: 'stream_event', event: { type: 'content_block_start', index: 0, content_block: { type: 'text', text: '' } } });
      for (let i = 0; i < item.text.length; i += size) {
        atMs += delay;
        projection.accept({ type: 'stream_event', event: { type: 'content_block_delta', index: 0, delta: { type: 'text_delta', text: item.text.slice(i, i + size) } } });
      }
      projection.accept({ type: 'assistant', message: { id: item.id, content: [{ type: 'text', text: item.text }] } });
    } else if (item.type === 'collabAgentToolCall') {
      // Bridge.child owns this lifecycle; no child sessions are created offline.
      const activity = projection.startItem({ type: 'subAgentActivity', id: item.id,
        agentThreadId: 'fixture-child', agentNickname: 'Todo reviewer', agentRole: 'Reviewer', kind: 'started', status: 'running' });
      atMs = Math.max(atMs, sourceAt + timing.durationMs);
      activity.kind = 'completed'; activity.status = 'inProgress'; projection.finishItem(activity);
    } else {
      let name, args;
      if (item.type === 'commandExecution') { name = 'Bash'; args = { command: item.command }; }
      else if (item.type === 'fileChange') {
        name = 'Edit'; args = { file_path: item.changes[0].path, old_string: 'const done = false;', new_string: 'const done = true;\n' + '// Synthetic todo detail\n'.repeat(Math.floor(item.changes[0].diff.length / 25)) };
      } else if (item.type === 'plan') { name = 'ExitPlanMode'; args = { plan: item.text }; }
      else { name = 'mcp__fixture__read'; args = item.arguments ?? { note: 'Read the synthetic todo list.' }; }
      projection.accept({ type: 'assistant', message: { id: item.id, content: [{ type: 'tool_use', id: item.id, name, input: args }] } });
      if (name === 'Bash' && turnIndex % 32 === 0) {
        const id = 'approval-' + item.id;
        emit({ id, method: 'item/commandExecution/requestApproval', params: { threadId, turnId: spec.id, itemId: item.id, agentFamily: 'claude', command: item.command, cwd: '/fixture/todo', reason: 'Allow the synthetic todo check?', availableDecisions: ['accept', 'decline', 'cancel'] } });
        atMs += 250; replay.push({ atMs, direction: 'client', message: { id, result: { decision: 'accept' } } });
      }
      atMs = Math.max(atMs, sourceAt + timing.durationMs);
      if (name !== 'ExitPlanMode') {
        const text = item.aggregatedOutput ?? item.result?.content?.[0]?.text ?? item.contentItems?.[0]?.text ?? 'Synthetic action finished.';
        projection.accept({ type: 'user', message: { content: [{ type: 'tool_result', tool_use_id: item.id, content: text, is_error: item.status === 'failed' }] } });
      }
    }
    latest = Math.max(latest, atMs);
  }
  atMs = latest;
  projection.finish(spec.status, spec.error?.message);
  turns.push(projection.result);
}
const session = { id: threadId, sdkSessionId: 'fixture-session', options: { cwd: '/fixture/todo' }, createdAt: input.epoch, updatedAt: input.epoch + Math.floor(atMs / 1000), turns };
// describe is a pure projection; no Sessions constructor or writes are invoked.
const thread = Sessions.prototype.describe.call(null, session, true);
replay.sort((a, b) => a.atMs - b.atMs);
process.stdout.write(JSON.stringify({ threadRead: { thread }, replay }));

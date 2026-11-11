// Explicit opt-in: node services/acp-runtime/scripts/cursor-live-check.mjs --live
// With no flag, discover models only. No retries and at most two live prompts.
// The real bridge talks to a fixture daemon with zero Project threads. No
// wonderd process, installed service, pairing or ~/.wonder is read or changed.
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { randomUUID } from 'node:crypto';
import { AcpAgent } from '../agent.mjs';
import { AcpBridge } from '../bridge.mjs';
import { Sessions } from '../sessions.mjs';
import { cursorAdapter, cursorModels, locateCursor, removeProbeSessions } from '../catalog.mjs';

assert(process.argv.slice(2).every(arg => arg === '--live'), 'Only --live is supported');
const live = process.argv.includes('--live');
const root = await mkdtemp(join(tmpdir(), 'cupola-cursor-check-'));
const workspace = join(root, 'workspace');
const config = join(root, 'config'), data = join(root, 'data');
const evidence = { live, prompts: [], updates: [], permissions: [], daemonCalls: [], outputs: [], fixtureThreads: 0 };
let bridge, rawSession, phase = 0, accepted = 0;
const frames = [];
try {
  await Promise.all([workspace, config, data].map(path => mkdir(path)));
  const baseline = JSON.parse(await readFile(new URL('../../../crates/wonderd/src/wonder_skills_off_baseline.json', import.meta.url), 'utf8'));
  const fixture = baseline.claude.find(frame => frame.params.dynamicTools)?.params;
  const tools = fixture.dynamicTools;
  const preamble = fixture.wonderPreamble;
  const binary = await locateCursor();
  assert(binary, 'Cursor CLI is missing');
  const sessions = await new Sessions(join(root, 'daemon-state'), cursorAdapter).initialize();
  bridge = new AcpBridge({ sessions, adapter: cursorAdapter, tools: {},
    inspect: async () => ({ installed: true, binary, models: [] }),
    createAgent: handlers => {
      const agent = new AcpAgent({ command: binary, args: ['acp'], label: 'Cursor', cwd: workspace, processGroup: true,
        env: { CURSOR_CONFIG_DIR: config, CURSOR_DATA_DIR: data, WONDER_DATA_DIR: join(root, 'daemon-state') },
        ...handlers,
        onUpdate: params => {
          // Keep actual Cursor call deltas, including rawInput and its timing.
          evidence.updates.push({ phase, at: Date.now(), ...params });
          handlers.onUpdate(params);
        },
        onPermission: async params => {
          const result = await handlers.onPermission(params);
          evidence.permissions.push({ phase, at: Date.now(), params, result });
          return result;
        },
      });
      const request = agent.request.bind(agent);
      agent.request = async (method, params, options) => {
        const result = await request(method, params, options);
        if (method === 'session/new') rawSession = result;
        return result;
      };
      return agent;
    },
    send: async frame => {
      frames.push(frame);
      if (frame.method === 'item/tool/call') {
        evidence.daemonCalls.push({ phase, at: Date.now(), ...frame.params });
        const allowed = phase === 2 && accepted === 0 && frame.params.tool === 'wonder_thread_list';
        if (allowed) accepted++;
        setImmediate(() => bridge.receive({ id: frame.id, result: {
          success: allowed, contentItems: [{ type: 'inputText', text: allowed
            ? JSON.stringify({ threads: [], nextCursor: null }) : 'Refused: only one thread-list call on turn two is authorized.' }],
        } }));
      } else if (frame.id != null && frame.method) {
        setImmediate(() => bridge.receive({ id: frame.id, result: { decision: 'decline' } }));
      }
    },
  });
  const call = async (method, params) => {
    const id = randomUUID();
    await bridge.receive({ id, method, params });
    const reply = frames.find(frame => frame.id === id && !frame.method);
    if (reply?.error) throw new Error(reply.error.message);
    assert(reply, `Missing ${method} response`);
    return reply.result;
  };
  const { thread } = await call('thread/start', { requestId: randomUUID(), cwd: workspace, model: 'cursor:discovery-only',
    wonderPolicy: { mode: 'workspace', approvalMode: 'ask' },
    wonderProject: { cwd: workspace, additionalDirectories: [] }, dynamicTools: tools });
  evidence.models = cursorModels(rawSession).models;
  // Haiku 5.5 is the cheapest listed model (cursor.com/docs/models-and-pricing,
  // checked 2026-10-22). No alias guessing: use this session's ACP value.
  const model = evidence.models.find(row => /^claude-haiku-5-5\[/.test(row.id));
  assert(model, 'Haiku 5.5 is not offered; do not silently use a different model');
  evidence.model = model;
  if (live) {
    for (const prompt of ['List your available tools; do not call any.', 'Call wonder_thread_list once and report how many threads it returned.']) {
      phase++;
      evidence.prompts.push(prompt);
      const input = [{ type: 'text', text: prompt }];
      if (phase === 1) input.unshift({ type: 'text', text: `<wonder-handoff purpose="cupola">\n${preamble}\n</wonder-handoff>\n` });
      const { turn } = await call('turn/start', { threadId: thread.id, clientUserMessageId: randomUUID(), model: `cursor:${model.id}`, input });
      const deadline = Date.now() + 180_000;
      let done;
      while (!(done = frames.find(frame => frame.method === 'turn/completed' && frame.params.turnId === turn.id))) {
        assert(Date.now() < deadline, 'Turn exceeded three minutes; do not retry');
        await new Promise(resolve => setTimeout(resolve, 50));
      }
      evidence.outputs.push(done.params.turn);
      assert.equal(done.params.turn.status, 'completed');
      // Completion is emitted just before the bridge retires the active run.
      while (bridge.active.has(thread.id)) {
        assert(Date.now() < deadline, 'Bridge did not retire the completed turn');
        await new Promise(resolve => setTimeout(resolve, 10));
      }
      if (phase === 1) {
        assert.equal(evidence.daemonCalls.length, 0, 'First turn called a tool');
        const reply = done.params.turn.items.filter(item => item.type === 'agentMessage').map(item => item.text).join('\n');
        assert(!/upgrade your plan/i.test(reply), 'Cursor account gate: Upgrade your plan to continue; no second turn sent');
        for (const tool of tools) assert(reply.includes(tool.name), `Cursor did not list ${tool.name}`);
      }
    }
    assert.equal(evidence.daemonCalls.length, 1);
    assert.equal(accepted, 1);
  }
  evidence.status = 'passed';
} catch (error) {
  evidence.status = 'failed'; evidence.error = error.stack; process.exitCode = 1;
} finally {
  await bridge?.close();
  // Some Cursor versions ignore the data override for ACP metadata. Remove
  // only sessions whose recorded cwd exactly equals this temporary workspace.
  await removeProbeSessions(workspace);
  await rm(root, { recursive: true, force: true });
  evidence.cleaned = true;
  process.stdout.write(`${JSON.stringify(evidence, null, 2)}\n`);
}

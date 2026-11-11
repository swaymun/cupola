#!/usr/bin/env node
// Read provider catalogs and usage. Never submit a prompt or create a thread.
import { spawn } from 'node:child_process';
import { createInterface } from 'node:readline';
import { pathToFileURL } from 'node:url';
import { resolve } from 'node:path';
// SDK_STORE (optional) is the installed app's Claude SDK update store; the catalog then
// comes from the SDK that app actually runs (its activated update), not the bundled one.
const [codex, sdkRoot, cwd, sdkStore] = process.argv.slice(2);
if (!codex || !sdkRoot || !cwd) throw new Error('Expected CODEX_BIN CLAUDE_SDK_ROOT PRIVATE_CWD [SDK_STORE]');
const result = { modelTurns: 0, providers: {} };
async function codexCatalog() {
  const env = Object.fromEntries(['HOME', 'PATH', 'USER', 'LOGNAME', 'SHELL', 'TMPDIR', 'LANG', 'LC_ALL'].filter(k => process.env[k] !== undefined).map(k => [k, process.env[k]]));
  const child = spawn(codex, ['app-server', '--stdio'], { cwd, env, stdio: ['pipe', 'pipe', 'ignore'] });
  const pending = new Map(); let sequence = 0; let failure;
  function failed(error) { failure = error; for (const settle of [...pending.values()]) settle({ error: { message: String(error) } }); }
  child.once('error', failed); child.stdin.on('error', failed);
  child.once('exit', (code, signal) => failed(new Error(`Codex catalog process exited (${signal ?? code})`)));
  const closed = new Promise(resolve => child.once('close', resolve));
  const lines = createInterface({ input: child.stdout });
  lines.on('line', line => { try { const m = JSON.parse(line); pending.get(m.id)?.(m); } catch {} });
  function request(method, params = {}) {
    return new Promise((resolve, reject) => {
      if (failure) { reject(failure); return; }
      const id = ++sequence;
      const timer = setTimeout(() => { pending.delete(id); reject(new Error(`${method} deadline`)); }, 30000);
      pending.set(id, m => { clearTimeout(timer); pending.delete(id); m.error ? reject(new Error(m.error.message)) : resolve(m.result); });
      child.stdin.write(JSON.stringify({ id, method, params }) + '\n');
    });
  }
  try {
    await request('initialize', { clientInfo: { name: 'wonder-live-fixture-preflight', version: '1' }, capabilities: { experimentalApi: true } });
    child.stdin.write('{"method":"initialized"}\n');
    const account = await request('account/read', { refreshToken: false });
    const models = []; let cursor;
    for (let page = 0; page < 10; page++) {
      const response = await request('model/list', { limit: 100, includeHidden: false, ...(cursor ? { cursor } : {}) });
      models.push(...response.data); cursor = response.nextCursor;
      if (!cursor) break;
    }
    if (cursor) throw new Error('Incomplete model catalog');
    const rawUsage = await request('account/rateLimits/read');
    const usage = { ordinaryUsageAllowed: rawUsage.ordinaryUsageAllowed, rateLimits: rawUsage.rateLimits, rateLimitsByLimitId: rawUsage.rateLimitsByLimitId };
    for (const window of [usage.rateLimits, ...Object.values(usage.rateLimitsByLimitId ?? {})]) if (window) delete window.credits;
    return { connected: account.account?.type === 'chatgpt', models, usage };
  } finally {
    lines.close(); child.stdin.end(); child.kill('SIGTERM');
    // An exited provider must not keep the preflight alive for the kill grace.
    let grace;
    try { await Promise.race([closed, new Promise(resolve => { grace = setTimeout(resolve, 1000); })]); }
    finally { clearTimeout(grace); }
    if (child.exitCode === null) child.kill('SIGKILL');
  }
}
const { loadSdk, inspectSdk } = await import(pathToFileURL(resolve(import.meta.dirname, '../services/claude-runtime/sdk-runtime.mjs')));
const { activeRuntime } = await import(pathToFileURL(resolve(import.meta.dirname, '../services/claude-runtime/runtime-updates.mjs')));
async function claudeRuntime() {
  const bundled = await loadSdk(sdkRoot);
  return sdkStore ? activeRuntime({ root: sdkStore, bundled }) : bundled;
}
await Promise.all([
  (async () => { try { result.providers.codex = await codexCatalog(); } catch (e) { result.providers.codex = { error: String(e) }; } })(),
  (async () => { try { const runtime = await claudeRuntime(); result.providers.claude = { ...await inspectSdk(runtime, { cwd, includeUsage: true }), runtimeVersion: runtime.version }; } catch (e) { result.providers.claude = { error: String(e) }; } })(),
]);
console.log(JSON.stringify(result));

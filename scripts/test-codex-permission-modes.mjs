// Exercises built-in Codex command sandbox modes against disposable local files.
// No model turn, API usage, or existing file mutation.
import { spawn } from 'node:child_process';
import { createInterface } from 'node:readline';
import { mkdtempSync, mkdirSync, rmSync } from 'node:fs';
import { homedir } from 'node:os';

// Keep fixtures outside /tmp, which the native workspace preset also allows.
const root = mkdtempSync(`${homedir()}/.wonder-mode-probe-`);
const workspace = `${root}/workspace`;
const outside = `${root}/outside`;
mkdirSync(workspace);
mkdirSync(outside);
const child = spawn(
  process.env.WONDER_CODEX_BIN ?? '/Applications/ChatGPT.app/Contents/Resources/codex',
  ['app-server', '--stdio'],
  { cwd: workspace, stdio: ['pipe', 'pipe', 'ignore'] },
);
const pending = new Map();
let nextId = 0;
createInterface({ input: child.stdout }).on('line', (line) => {
  try {
    const message = JSON.parse(line);
    pending.get(message.id)?.(message);
  } catch { /* Ignore non-protocol output. */ }
});
function request(method, params) {
  return new Promise((resolve, reject) => {
    const id = ++nextId;
    const timer = setTimeout(() => {
      pending.delete(id);
      reject(new Error(`Timed out waiting for ${method}`));
    }, 20_000);
    pending.set(id, (message) => {
      clearTimeout(timer);
      pending.delete(id);
      resolve(message);
    });
    child.stdin.write(`${JSON.stringify({ id, method, params })}\n`);
  });
}
try {
  const initialize = await request('initialize', {
    clientInfo: { name: 'wonder-mode-check', version: '1' },
    capabilities: { experimentalApi: true },
  });
  if (initialize.error) throw new Error(JSON.stringify(initialize.error));
  child.stdin.write('{"method":"initialized"}\n');
  const results = [];
  for (const [mode, target, expectedWrite] of [
    [':read-only', `${workspace}/read-only.txt`, false],
    [':workspace', `${workspace}/workspace.txt`, true],
    [':workspace', `${outside}/blocked.txt`, false],
    [':danger-full-access', `${outside}/full.txt`, true],
  ]) {
    const response = await request('command/exec', {
      permissionProfile: mode,
      cwd: workspace,
      command: ['/bin/sh', '-c', 'printf fixture > "$1"', 'probe', target],
      timeoutMs: 5_000,
    });
    results.push({
      mode,
      target: target.replace(root, 'FIXTURE'),
      expectedWrite,
      exitCode: response.result?.exitCode,
      error: response.error,
      passed: !response.error && (response.result?.exitCode === 0) === expectedWrite,
    });
  }
  console.log(JSON.stringify({ results }, null, 2));
  if (results.some((result) => !result.passed)) process.exitCode = 1;
} finally {
  child.stdin.end();
  child.kill('SIGTERM');
  rmSync(root, { recursive: true, force: true });
}

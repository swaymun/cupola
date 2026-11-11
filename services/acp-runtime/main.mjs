// Wonder's ACP runtime: App Server JSON-RPC lines to wonderd, native ACP
// processes for the explicitly selected provider. Providers never share journals.
import { resolve, join } from "node:path";
import { Worker } from "node:worker_threads";
import { homedir } from "node:os";
import { AcpAgent } from "./agent.mjs";
import { AcpBridge } from "./bridge.mjs";
import { Sessions } from "./sessions.mjs";
import { ownerArguments, inspectOpenCode, locateOpenCode, openCodeCommand, openCodeAdapter, cursorAdapter, inspectCursor, locateCursor, settingsFiles, setupFingerprint } from "./catalog.mjs";

const args = process.argv.slice(2);
const options = {};
for (let i = 0; i < args.length; i += 2) {
  if (!["--state-dir", "--npm-cli", "--provider"].includes(args[i]) || !args[i + 1]) throw new Error("Invalid ACP runtime launch arguments");
  options[args[i]] = args[i + 1];
}
const provider = options["--provider"] ?? "opencode";
if (!["opencode", "cursor"].includes(provider)) throw new Error("Unsupported ACP provider");
const adapter = provider === "cursor" ? cursorAdapter : openCodeAdapter;
const label = adapter.label;
const root = resolve(options["--state-dir"] ?? join(homedir(), ".wonder", `${provider}-runtime`));
const sessions = await new Sessions(join(root, "sessions"), adapter).initialize();
async function within(operation, ms, message) {
  let timer;
  try {
    return await Promise.race([operation, new Promise((_, reject) => {
      timer = setTimeout(() => reject(new Error(message)), ms);
    })]);
  } finally { clearTimeout(timer); }
}
// Windows writes to a stdout pipe synchronously: a daemon that stops reading
// would block the event loop, so the output bound, the write deadline and
// shutdown could never run (measured). There a worker owns fd 1 and the event
// loop only waits for its acknowledgements. Elsewhere stdout is nonblocking.
function stdoutWriter() {
  if (process.platform !== "win32") {
    process.stdout.on("error", error => bridge.output.fail(error));
    return { write: line => new Promise((resolve, reject) => {
      process.stdout.write(line, error => error ? reject(error) : resolve());
    }), blocked: () => false };
  }
  const worker = new Worker(`const { parentPort } = require("node:worker_threads");
const { writeSync } = require("node:fs");
parentPort.on("message", line => {
  try {
    const bytes = Buffer.from(line);
    for (let offset = 0; offset < bytes.length;) offset += writeSync(1, bytes, offset);
    parentPort.postMessage(null);
  } catch (error) { parentPort.postMessage(error.message ?? String(error)); }
});`, { eval: true, stdout: false, stderr: false });
  const pending = [];
  worker.on("message", error => {
    const { resolve, reject } = pending.shift();
    if (error == null) resolve(); else reject(new Error(error));
  });
  worker.on("error", error => { for (const write of pending.splice(0)) write.reject(error); });
  // After the listeners, which would otherwise keep the process alive.
  worker.unref();
  return {
    write: line => new Promise((resolve, reject) => { pending.push({ resolve, reject }); worker.postMessage(line); }),
    // A write still inside writeSync keeps the process from exiting normally.
    blocked: () => pending.length > 0,
  };
}
let stdout;
const locate = () => provider === "cursor" ? locateCursor() : locateOpenCode();
const discoveryAbort = new AbortController();
const bridge = new AcpBridge({
  sessions, adapter,
  // Cupola's thread tools and Cua Driver reach OpenCode and Cursor through a
  // per-session server in ACP `mcpServers`; Cursor keeps a separate MCP
  // client per session, so each server serves only its own thread.
  tools: {},
  // The bridge owns admission and serialization, including streaming output.
  // Retain the shipped ten-second deadline, including time waiting in queue.
  write: (line, admittedAt) => {
    const message = `Cupola stopped reading ${label}'s response`;
    const remaining = 10_000 - (performance.now() - admittedAt);
    if (remaining <= 0) return Promise.reject(new Error(message));
    return within(stdout.write(line), remaining, message);
  },
  onOutputFailure: fatal,
  inspect: async ({ signal: yielded } = {}) => {
    const binary = await locate();
    const signal = yielded ? AbortSignal.any([discoveryAbort.signal, yielded]) : discoveryAbort.signal;
    return provider === "cursor" ? inspectCursor(binary, root, { signal }) : inspectOpenCode(binary, { signal });
  },
  locate,
  fingerprint: async () => setupFingerprint(await locate(), settingsFiles(provider)),
  // The owner's launch arguments (Settings > provider > Arguments) are the program's
  // options, so they come before the `acp` subcommand.
  createAgent: (handlers, { binary }) => new AcpAgent({ ...(provider === "cursor"
    ? { command: binary, args: [...ownerArguments(provider), "acp"] } : openCodeCommand(binary, [...ownerArguments(provider), "acp"])), label, cwd: root,
    ...handlers }),
  onFatal: error => process.stderr.write(`${label} turn failed: ${error?.message ?? error}\n`),
});
let closing;
async function close() {
  if (closing) return closing;
  closing = (async () => {
    discoveryAbort.abort();
    const exited = await bridge.close();
    // Catalog probes also own native processes; let cancellation finish their
    // exact-child shutdown before the main process can exit.
    if (bridge.inspecting) {
      try { await within(bridge.inspecting.catch(() => {}), 4500, `${label} discovery did not finish closing`); }
      catch { return false; }
    }
    try { await within(bridge.output.flush(), 2000, `${label} output did not finish closing`); }
    catch { return false; }
    return exited;
  })();
  return closing;
}
stdout = stdoutWriter();
// process.exit waits for a worker blocked writing to a stalled pipe; end the
// process instead (Windows reports exit code 1 for it, as on a normal failure).
function exit(code) {
  if (code !== 0 && stdout.blocked()) process.kill(process.pid, "SIGKILL");
  process.exit(code);
}
function fatal() { close().then(() => exit(1), () => exit(1)); }
for (const signal of ["SIGTERM", "SIGINT"]) {
  process.once(signal, () => { close().then(exited => exit(exited ? 0 : 1), () => exit(1)); });
}
process.stdin.setEncoding("utf8");
let buffered = "";
const requests = new Set();
try {
  for await (const chunk of process.stdin) {
    buffered += chunk;
    if (Buffer.byteLength(buffered) > 64 * 1024 * 1024) throw new Error("Runtime frame exceeded the allowed size");
    let newline;
    while ((newline = buffered.indexOf("\n")) !== -1) {
      const line = buffered.slice(0, newline); buffered = buffered.slice(newline + 1);
      if (!line.trim()) continue;
      if (requests.size >= 128) throw new Error("Too many pending runtime requests");
      const request = bridge.receive(JSON.parse(line));
      requests.add(request);
      request.finally(() => requests.delete(request)).catch(() => {});
    }
  }
} finally { if (!(await close())) exit(1); }

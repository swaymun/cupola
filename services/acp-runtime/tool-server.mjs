// Wonder's Project thread tools for ACP agents. Each Wonder session gets its
// own stdio MCP server entry (`mcp-server.mjs`) in ACP `session/new`,
// `session/resume` or `session/load`; that process connects back here over a
// private Unix socket (a named pipe on Windows) and presents a credential scoped to that one session.
//
// Credentials live only in memory, as hashes. One expires when its session
// has shown no sign of life (a running reply, a tool call) for `ttlMs`, and is
// revoked when its session is attached again, its agent process exits, or the
// bridge closes. A revoked server is told to exit, which removes its tools
// from the agent.
import { spawn } from "node:child_process";
import { createHash, randomBytes } from "node:crypto";
import { mkdtemp, rm } from "node:fs/promises";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createInterface } from "node:readline";
import { fileURLToPath } from "node:url";
import { isProjectTool } from "./thread-tools.mjs";

// A visual carries its page (up to 6 MB, more once JSON-escaped).
const MAX_LINE = 16 * 1024 * 1024;
const SCRIPT = fileURLToPath(new URL("./mcp-server.mjs", import.meta.url));
const hash = token => createHash("sha256").update(token).digest("hex");

// The server name is per session: OpenCode registers MCP servers per folder,
// not per session, so two threads in one folder must not share a name.
export const serverName = sessionId => `wonder-${createHash("sha256").update(sessionId).digest("hex").slice(0, 8)}`;
// OpenCode names an MCP tool `<server>_<tool>`, other characters become `_`.
// Cursor titles its call `<server>: <tool>` (Cursor CLI 2026.10.19).
const safe = value => value.replace(/[^a-zA-Z0-9_-]/g, "_");
export const qualifiedName = (server, tool) => `${safe(server)}_${safe(tool)}`;
const QUALIFIED = /^(wonder-[0-9a-f]{8})(?:_|: )(wonder_[a-z_]+)$/;
export function wonderTool(title) {
  const match = typeof title === "string" ? QUALIFIED.exec(title) : null;
  return match && isProjectTool(match[2]) ? { server: match[1], tool: match[2] } : null;
}
// Any tool of a session's server: Wonder's thread tools or computer use.
const SESSION_TOOL = /^(wonder-[0-9a-f]{8})(?:_|: )(.+)$/;
export function sessionTool(title) {
  const match = typeof title === "string" ? SESSION_TOOL.exec(title) : null;
  return match ? { server: match[1], tool: match[2] } : null;
}

// The arguments an agent reported for a call. OpenCode reports them as the
// call's input; Cursor wraps them with the server and tool they went to.
export function callArguments(call) {
  const input = call?.rawInput ?? {};
  const named = sessionTool(call?.name ?? call?.title);
  return named && input.providerIdentifier === named.server && input.toolName === named.tool
    && input.args && typeof input.args === "object" ? input.args : input;
}

// Key-order independent, so the agent's echo of the arguments matches.
export function canonical(value) {
  if (Array.isArray(value)) return `[${value.map(canonical).join(",")}]`;
  if (value && typeof value === "object") return `{${Object.keys(value).sort().map(key => `${JSON.stringify(key)}:${canonical(value[key])}`).join(",")}}`;
  return JSON.stringify(value ?? null);
}

export function mcpTools(specs) {
  return (Array.isArray(specs) ? specs : []).filter(spec => isProjectTool(spec?.name)).map(spec => ({
    name: spec.name,
    description: `${spec.description} This server acts for one Cupola thread; if another thread in this folder lists the same tools, only this thread's own server accepts its calls.`,
    inputSchema: spec.inputSchema,
  }));
}

// A session's Cua Driver (trycua/cua), a stdio MCP server the bridge itself
// runs: its tools reach a session only through that session's own server,
// which asks the bridge whether the session may use them right now.
//
// This is the one owner of the driver's lifecycle. Each process it starts is
// a generation; start, Stop, a timeout, a failed start and a replacement all
// end a generation through `end(run)`, which ends only that generation's
// process and requests, and clears `current` only if that generation is still
// current. Cua Driver 0.36 ignores `notifications/cancelled` and finishes the
// action (a `run_script` keeps acting), so cancelling a call ends its
// generation; the next call starts a fresh one.
export class CuaDriver {
  constructor({ command, args = [], env = {}, timeoutMs = 30_000 }) {
    Object.assign(this, { command, args, env, timeoutMs });
    this.generation = 0; this.current = null;
  }
  // The current generation, started if there is none.
  acquire() {
    if (this.current) return this.current;
    const child = spawn(this.command, this.args, { env: { ...process.env, ...this.env }, stdio: ["pipe", "pipe", "ignore"] });
    const run = this.current = { generation: ++this.generation, child, pending: new Map(), nextId: 1, listing: null, ended: false };
    child.on("exit", () => this.end(run));
    child.on("error", () => this.end(run));
    child.stdin.on("error", () => {});
    createInterface({ input: child.stdout }).on("line", line => {
      let frame;
      try { frame = JSON.parse(line); } catch { return; }
      if (frame.method === "notifications/tools/list_changed") { run.listing = null; return; }
      const waiter = run.pending.get(frame.id);
      if (!waiter) return;
      run.pending.delete(frame.id);
      if (frame.error) waiter.reject(new Error(frame.error.message ?? "The computer-use driver refused the request."));
      else waiter.resolve(frame.result);
    });
    run.ready = this.rpc(run, "initialize", { protocolVersion: "2025-06-18", capabilities: {}, clientInfo: { name: "Cupola", version: "1" } })
      .then(() => this.write(run, { method: "notifications/initialized" }));
    run.ready.catch(() => this.end(run));
    return run;
  }
  // Ends generation `run` (the current one by default). An older generation's
  // cleanup never reaches the current one.
  end(run = this.current) {
    if (!run || run.ended) return;
    run.ended = true;
    if (this.current?.generation === run.generation) this.current = null;
    for (const { reject } of [...run.pending.values()]) reject(new Error("The computer-use driver stopped. Try again."));
    run.child.kill();
  }
  stop() { this.end(); }
  write(run, frame) { if (!run.ended) run.child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", ...frame })}\n`); }
  // A request that is cancelled or times out ends its generation, or its
  // action would outlive Stop.
  rpc(run, method, params, signal) {
    if (signal?.aborted) return Promise.reject(new Error("Computer use cancelled."));
    if (run.ended) return Promise.reject(new Error("The computer-use driver stopped. Try again."));
    const id = run.nextId++;
    return new Promise((resolve, reject) => {
      const done = () => { clearTimeout(timer); signal?.removeEventListener("abort", abort); run.pending.delete(id); };
      const abort = () => {
        done(); reject(new Error("Computer use cancelled."));
        this.write(run, { method: "notifications/cancelled", params: { requestId: id, reason: "Stopped in Cupola." } });
        this.end(run);
      };
      const timer = setTimeout(() => { done(); reject(new Error("The computer-use driver did not answer in time.")); this.end(run); }, this.timeoutMs);
      signal?.addEventListener("abort", abort, { once: true });
      run.pending.set(id, { resolve: value => { done(); resolve(value); }, reject: error => { done(); reject(error); } });
      this.write(run, { id, method, params });
    });
  }
  // Every use of the driver goes through here with the reply's signal: Stop
  // (or a cancelled call) ends the generation it acquired, also while that
  // generation is still starting or listing its tools.
  async using(signal, work) {
    if (signal?.aborted) throw new Error("Computer use cancelled.");
    const run = this.acquire(), end = () => this.end(run);
    signal?.addEventListener("abort", end, { once: true });
    try {
      await run.ready;
      return await work(run);
    } catch (error) {
      throw signal?.aborted ? new Error("Computer use cancelled.") : error;
    } finally { signal?.removeEventListener("abort", end); }
  }
  tools(signal) {
    return this.using(signal, run => {
      run.listing ??= this.rpc(run, "tools/list", {}).then(result => Array.isArray(result?.tools) ? result.tools : []);
      run.listing.catch(() => { run.listing = null; });
      return run.listing;
    });
  }
  call(name, args, signal) { return this.using(signal, run => this.rpc(run, "tools/call", { name, arguments: args }, signal)); }
}

export class ToolTokens {
  constructor({ ttlMs = 15 * 60_000, now = () => Date.now(), alive = () => false, onRevoke = () => {} } = {}) {
    Object.assign(this, { ttlMs, now, alive, onRevoke });
    this.records = new Map();
  }
  issue(sessionId, owner) {
    this.revoke(sessionId);
    const token = randomBytes(32).toString("base64url");
    this.records.set(hash(token), { sessionId, owner, lastAlive: this.now() });
    return token;
  }
  // The session the credential belongs to, or null. A running reply counts as
  // a sign of life, so a long response never loses its tools.
  resolve(tokenHash) {
    const record = this.records.get(tokenHash);
    if (!record) return null;
    const now = this.now();
    if (this.alive(record.sessionId)) record.lastAlive = now;
    else if (now - record.lastAlive > this.ttlMs) { this.drop(tokenHash, record); return null; }
    return record;
  }
  live(sessionId) {
    for (const [tokenHash, record] of this.records) if (record.sessionId === sessionId) return Boolean(this.resolve(tokenHash));
    return false;
  }
  touch(sessionId) {
    for (const record of this.records.values()) if (record.sessionId === sessionId) record.lastAlive = this.now();
  }
  sweep() { for (const tokenHash of [...this.records.keys()]) this.resolve(tokenHash); }
  drop(tokenHash, record) { this.records.delete(tokenHash); this.onRevoke(tokenHash, record); }
  revoke(sessionId) { for (const [tokenHash, record] of [...this.records]) if (record.sessionId === sessionId) this.drop(tokenHash, record); }
  revokeOwner(owner) { for (const [tokenHash, record] of [...this.records]) if (record.owner === owner) this.drop(tokenHash, record); }
  revokeAll() { for (const [tokenHash, record] of [...this.records]) this.drop(tokenHash, record); }
}

// The socket end of every `mcp-server.mjs`. `handler` answers `list` and
// `call` for an authenticated session.
export class ToolServer {
  constructor({ handler, ttlMs, now, alive, node = process.execPath, script = SCRIPT, sweepMs = 30_000 }) {
    Object.assign(this, { handler, node, script });
    this.connections = new Map(); this.sockets = new Set();
    this.tokens = new ToolTokens({ ttlMs, now, alive, onRevoke: tokenHash => this.disconnect(tokenHash) });
    this.sweeper = setInterval(() => this.tokens.sweep(), sweepMs);
    this.sweeper.unref?.();
  }
  async listen() {
    if (this.closed) throw new Error("Cupola's chat tools are closing.");
    this.listening ??= (async () => {
      if (process.platform === "win32") {
        // Windows has no socket files here: a named pipe with an unguessable
        // name. Its default ACL lets other local users open it read-only, so
        // the per-session credential (sent before any reply) is the gate.
        this.path = `\\\\.\\pipe\\wonder-tools-${randomBytes(16).toString("hex")}`;
      } else {
        // mkdtemp creates the folder owner-only (0700): no other user can connect.
        this.dir = await mkdtemp(join(tmpdir(), "wonder-tools-"));
        this.path = join(this.dir, "tools.sock");
      }
      this.server = createServer(socket => this.accept(socket));
      await new Promise((resolve, reject) => { this.server.once("error", reject); this.server.listen(this.path, resolve); });
    })();
    await this.listening;
  }
  // The ACP `mcpServers` entry for a session; the credential travels in the
  // environment, never on a command line or in Wonder's journal.
  async entry(sessionId, owner) {
    await this.listen();
    const token = this.tokens.issue(sessionId, owner);
    return { name: serverName(sessionId), command: this.node, args: [this.script],
      env: [{ name: "WONDER_TOOLS_SOCKET", value: this.path }, { name: "WONDER_TOOLS_TOKEN", value: token }] };
  }
  accept(socket) {
    let buffered = "", tokenHash = null;
    const reply = frame => { if (!socket.destroyed && socket.writable) socket.write(`${JSON.stringify(frame)}\n`); };
    const refuse = () => { reply({ type: "revoked" }); socket.end(); };
    this.sockets.add(socket);
    socket.setEncoding("utf8");
    socket.on("error", () => {});
    socket.on("close", () => {
      this.sockets.delete(socket);
      if (tokenHash && this.connections.get(tokenHash)?.has(socket)) this.connections.get(tokenHash).delete(socket);
    });
    socket.on("data", chunk => {
      buffered += chunk;
      if (buffered.length > MAX_LINE) { socket.destroy(); return; }
      let newline;
      while ((newline = buffered.indexOf("\n")) !== -1) {
        const line = buffered.slice(0, newline); buffered = buffered.slice(newline + 1);
        let frame;
        try { frame = JSON.parse(line); } catch { socket.destroy(); return; }
        if (!tokenHash) {
          const presented = frame?.type === "hello" && typeof frame.token === "string" ? hash(frame.token) : null;
          if (!presented || !this.tokens.resolve(presented)) return refuse();
          tokenHash = presented;
          if (!this.connections.has(tokenHash)) this.connections.set(tokenHash, new Set());
          this.connections.get(tokenHash).add(socket);
          reply({ type: "ready" });
          continue;
        }
        const record = this.tokens.resolve(tokenHash);
        if (!record) return refuse();
        Promise.resolve()
          .then(() => this.handler(record.sessionId, frame))
          .then(result => reply({ id: frame.id, result }), error => reply({ id: frame.id, error: error?.message ?? "Cupola could not run this tool." }));
      }
    });
  }
  // The session's tool list changed; its server asks the agent to list again.
  // Returns how many connected servers were told.
  changed(sessionId) {
    let told = 0;
    for (const [tokenHash, record] of this.tokens.records) {
      if (record.sessionId !== sessionId) continue;
      for (const socket of this.connections.get(tokenHash) ?? []) if (socket.writable) { socket.write(`${JSON.stringify({ type: "changed" })}\n`); told++; }
    }
    return told;
  }
  disconnect(tokenHash) {
    for (const socket of this.connections.get(tokenHash) ?? []) { if (!socket.destroyed) socket.end(`${JSON.stringify({ type: "revoked" })}\n`); }
    this.connections.delete(tokenHash);
  }
  async close() {
    if (this.closed) return;
    this.closed = true;
    clearInterval(this.sweeper);
    this.tokens.revokeAll();
    if (this.listening) {
      await this.listening.catch(() => {});
      for (const socket of this.sockets) socket.destroy();
      await new Promise(resolve => this.server ? this.server.close(() => resolve()) : resolve());
      if (this.dir) await rm(this.dir, { recursive: true, force: true });
    }
  }
}

// One Agent Client Protocol (ACP) connection: JSON-RPC 2.0 over the agent's
// stdio. The agent's requests for permission reach `onPermission`; file system
// and terminal access are not offered, so the agent uses its own tools.
import { spawn } from "node:child_process";

export const ACP_PROTOCOL = 1;
const MAX_LINE = 32 * 1024 * 1024;
const deadline = ms => {
  if (!Number.isSafeInteger(ms) || ms <= 0 || ms > 2 ** 31 - 1) throw new Error("Invalid ACP timeout.");
  return ms;
};

// Only local control flow can prove that no request bytes were offered to the
// child. Provider errors, pipe failures and missing replies cannot prove it.
export class AcpNotSubmittedError extends Error {
  constructor(cause) { super(cause.message, { cause }); this.name = "AcpNotSubmittedError"; }
}

export class AcpAgent {
  // Prompts can spend hours working or waiting for a person. Control requests
  // use a shorter deadline; callers can override either without killing peers.
  constructor({ command, args = [], label = "ACP agent", env = {}, cwd = undefined, onPermission, onUpdate, onExit = () => {},
    requestTimeoutMs = 60_000, promptTimeoutMs = 24 * 60 * 60_000, writeTimeoutMs = 10_000, processGroup = false }) {
    // processGroup: the agent runs as the leader of its own process group, and
    // close also ends helpers it left behind (Cursor's `worker-server` outlives
    // `agent acp` otherwise). POSIX only.
    processGroup &&= process.platform !== "win32";
    Object.assign(this, { command, args, label, env, cwd, onPermission, onUpdate, onExit, requestTimeoutMs, promptTimeoutMs, writeTimeoutMs, processGroup });
    this.pending = new Map(); this.writes = new Set(); this.nextId = 1; this.alive = false; this.info = null; this.attached = new Set();
    this.prompting = new Map(); this.todos = new Map(); this.toolSessions = new Map(); this.unrouted = new Map();
  }
  async start({ timeoutMs = 60_000 } = {}) {
    if (this.child) throw new Error("An ACP connection can only be started once.");
    deadline(timeoutMs);
    this.child = spawn(this.command, this.args, { cwd: this.cwd, env: { ...process.env, ...this.env }, stdio: ["pipe", "pipe", "pipe"], detached: this.processGroup });
    this.alive = true;
    this.exitPromise = new Promise(resolve => { this.resolveExit = resolve; });
    // The agent's own diagnostics are kept short; nothing on stderr is a reply.
    this.child.stderr.on("data", chunk => { this.stderrTail = `${this.stderrTail ?? ""}${chunk}`.slice(-4000); });
    this.child.once("exit", (code, signal) => this.exited(code, signal));
    this.child.once("error", error => this.exited(null, null, error));
    this.child.stdin.on("error", () => this.fail(new Error(`${this.label} stopped receiving requests. Reconnect to ${this.label} on your computer.`)));
    this.child.stdout.on("error", () => this.fail(new Error(`${this.label} stopped sending replies. Reconnect to ${this.label} on your computer.`)));
    this.readOutput();
    try {
      this.info = await this.request("initialize", { protocolVersion: ACP_PROTOCOL,
        clientCapabilities: { fs: { readTextFile: false, writeTextFile: false }, terminal: false },
        clientInfo: { name: "Cupola", version: "1" } }, { timeoutMs });
      if (this.info?.protocolVersion !== ACP_PROTOCOL) throw new Error(`${this.label} speaks an unsupported version of the Agent Client Protocol. Update ${this.label} on your computer.`);
      return this;
    } catch (error) { await this.close(); throw error; }
  }
  readOutput() {
    let parts = [], bytes = 0;
    const decoder = new TextDecoder("utf-8", { fatal: true });
    this.child.stdout.on("data", chunk => {
      if (!this.alive) return;
      let offset = 0;
      while (offset < chunk.length) {
        const newline = chunk.indexOf(10, offset);
        const part = chunk.subarray(offset, newline === -1 ? chunk.length : newline);
        bytes += part.length;
        if (bytes > MAX_LINE) {
          parts = []; bytes = 0;
          this.fail(new Error(`${this.label} sent a reply that is too large.`));
          return;
        }
        parts.push(part);
        if (newline === -1) return;
        try {
          const line = decoder.decode(Buffer.concat(parts, bytes));
          parts = []; bytes = 0;
          if (line.trim()) this.receive(JSON.parse(line));
        } catch {
          parts = []; bytes = 0;
          this.fail(new Error(`${this.label} sent an invalid reply. Reconnect to ${this.label} on your computer.`));
          return;
        }
        offset = newline + 1;
      }
    });
  }
  rejectPending(reason) {
    for (const pending of this.pending.values()) pending.reject(reason);
    for (const reject of this.writes) reject(reason);
  }
  exited(code, signal, error) {
    if (this.exitNotified) return;
    this.exitNotified = true;
    this.alive = false;
    const reason = new Error(error?.code === "ENOENT" ? `${this.label} is not installed on this computer.` : `${this.label} stopped unexpectedly. Send again to restart it.`);
    this.rejectPending(reason);
    this.resolveExit();
    this.onExit({ code, signal, error });
  }
  fail(reason) {
    if (!this.alive) return;
    this.rejectPending(reason);
    void this.close();
  }
  receive(frame) {
    if (!frame || typeof frame !== "object" || Array.isArray(frame) || frame.jsonrpc !== "2.0") throw new Error("Invalid ACP frame");
    if (frame.id != null && typeof frame.id !== "string" && (typeof frame.id !== "number" || !Number.isFinite(frame.id))) throw new Error("Invalid ACP id");
    if (frame.id != null && frame.method == null) {
      if (("result" in frame) === ("error" in frame)) throw new Error("Invalid ACP response");
      if ("error" in frame && (!frame.error || !Number.isInteger(frame.error.code) || typeof frame.error.message !== "string")) throw new Error("Invalid ACP error");
      const pending = this.pending.get(frame.id);
      if (!pending) return;
      if (frame.error) pending.reject(Object.assign(new Error(frame.error.message ?? `${this.label} refused the request.`), { code: frame.error.code, data: frame.error.data }));
      else pending.resolve(frame.result ?? {});
      return;
    }
    if (typeof frame.method !== "string") throw new Error("Invalid ACP method");
    if (frame.method === "session/update") {
      const params = frame.params ?? {};
      this.onUpdate?.(params);
      this.toolSession(params.update?.toolCallId, params.sessionId);
      return;
    }
    if (frame.method === "cursor/update_todos") {
      this.todoUpdate(frame.params ?? {});
      if (frame.id != null) this.write({ jsonrpc: "2.0", id: frame.id, result: {} }).catch(error => this.fail(error));
      return;
    }
    if (frame.id == null) return;
    if (frame.method === "session/request_permission") {
      Promise.resolve()
        .then(() => this.onPermission(frame.params ?? {}))
        .then(outcome => this.write({ jsonrpc: "2.0", id: frame.id, result: { outcome } }),
          () => this.write({ jsonrpc: "2.0", id: frame.id, result: { outcome: { outcome: "cancelled" } } }))
        .catch(error => this.fail(error));
      return;
    }
    this.write({ jsonrpc: "2.0", id: frame.id, error: { code: -32601, message: `${frame.method} is not offered by Cupola.` } }).catch(error => this.fail(error));
  }
  // Cursor's to-do list arrives outside session/update, naming the tool call
  // that changed it rather than always its session. It goes to the session
  // it names, else the session that made that tool call, else the one session
  // with a reply in progress. Otherwise it waits for its tool call: guessing
  // could show one chat's list in another, or a merge as a whole list.
  todoUpdate(params) {
    const { sessionId, toolCallId } = params;
    const prompting = [...this.prompting.keys()];
    const session = typeof sessionId === "string" ? sessionId
      : this.toolSessions.get(toolCallId) ?? (prompting.length === 1 ? prompting[0] : null);
    if (session) {
      this.toolSession(toolCallId, session);
      const update = this.todoPlan(session, params);
      if (update) this.onUpdate?.(update);
      return;
    }
    if (typeof toolCallId !== "string" || !toolCallId) return;
    const waiting = this.unrouted.get(toolCallId) ?? [];
    if (waiting.length >= 16) return;
    waiting.push(params);
    this.unrouted.delete(toolCallId); this.unrouted.set(toolCallId, waiting);
    while (this.unrouted.size > 64) this.unrouted.delete(this.unrouted.keys().next().value);
  }
  // Which session made a tool call; delivers its waiting to-do updates in order.
  toolSession(toolCallId, sessionId) {
    if (typeof toolCallId !== "string" || !toolCallId || typeof sessionId !== "string") return;
    this.toolSessions.delete(toolCallId); this.toolSessions.set(toolCallId, sessionId);
    while (this.toolSessions.size > 1024) this.toolSessions.delete(this.toolSessions.keys().next().value);
    const waiting = this.unrouted.get(toolCallId);
    if (!waiting) return;
    this.unrouted.delete(toolCallId);
    for (const params of waiting) this.todoUpdate({ ...params, sessionId });
  }
  todoPlan(session, { todos, merge }) {
    if (!Array.isArray(todos)) return null;
    const list = merge === true ? new Map(this.todos.get(session)) : new Map();
    for (const [index, todo] of todos.entries()) {
      if (!todo || typeof todo !== "object" || typeof todo.content !== "string") continue;
      const id = typeof todo.id === "string" || Number.isFinite(todo.id) ? `id:${todo.id}` : `index:${index}`;
      if (list.size >= 256 && !list.has(id)) continue;
      list.set(id, { content: todo.content.slice(0, 1024), status: typeof todo.status === "string" ? todo.status.slice(0, 64) : "unknown" });
    }
    this.todos.delete(session); this.todos.set(session, list);
    while (this.todos.size > 64) this.todos.delete(this.todos.keys().next().value);
    return { sessionId: session, update: { sessionUpdate: "plan", entries: [...list.values()] } };
  }
  async write(frame, { timeoutMs = this.writeTimeoutMs, onSubmit } = {}) {
    if (!this.alive) throw new Error(`${this.label} is not running.`);
    const line = `${JSON.stringify(frame)}\n`;
    if (Buffer.byteLength(line) > MAX_LINE + 1) throw new Error(`This ${this.label} request is too large.`);
    deadline(timeoutMs);
    return new Promise((resolve, reject) => {
      let settled = false;
      const finish = error => {
        if (settled) return;
        settled = true; clearTimeout(timer); this.writes.delete(finish);
        if (error) reject(error); else resolve();
      };
      const timer = setTimeout(() => {
        this.fail(new Error(`${this.label} stopped receiving requests. Reconnect to ${this.label} on your computer.`));
      }, timeoutMs);
      this.writes.add(finish);
      const failed = cause => {
        const error = new Error(`${this.label} stopped receiving requests. Reconnect to ${this.label} on your computer.`, { cause });
        this.fail(error); finish(error);
      };
      try {
        onSubmit?.();
        this.child.stdin.write(line, error => { if (error) failed(error); else finish(); });
      }
      catch (error) { failed(error); }
    });
  }
  // For an admitted prompt, onTimeout persists uncertainty while its original
  // receipt stays pending until the agent responds or the exact process exits.
  request(method, params, { timeoutMs = method === "session/prompt" ? this.promptTimeoutMs : this.requestTimeoutMs, onTimeout } = {}) {
    if (!this.alive) return Promise.reject(new AcpNotSubmittedError(new Error(`${this.label} is not running.`)));
    const id = this.nextId++;
    let submitted = false;
    const prompt = method === "session/prompt" && typeof params?.sessionId === "string" ? params.sessionId : null;
    if (prompt) this.prompting.set(prompt, (this.prompting.get(prompt) ?? 0) + 1);
    const settled = () => {
      if (!prompt) return;
      const count = (this.prompting.get(prompt) ?? 1) - 1;
      if (count > 0) this.prompting.set(prompt, count); else this.prompting.delete(prompt);
    };
    return new Promise((resolve, reject) => {
      deadline(timeoutMs);
      let written = false;
      const retire = () => { if (!this.pending.delete(id)) return false; clearTimeout(timer); return true; };
      const settle = (callback, value) => { if (retire()) callback(value); };
      const timer = setTimeout(() => {
        if (!written) {
          // A partial frame cannot be recalled. Close this broken pipe before
          // it can later deliver work that Wonder has already rejected.
          this.fail(new Error(`${this.label} stopped receiving requests. Reconnect to ${this.label} on your computer.`));
          return;
        }
        const error = Object.assign(new Error(`${this.label} did not confirm this request. It may still be running; check ${this.label} on your computer before trying again.`),
          { code: "ACP_REQUEST_TIMEOUT", method, requestId: id, outcomeUnknown: true });
        // A response timeout must not kill the shared process or another turn.
        if (method === "session/prompt" && typeof params?.sessionId === "string") {
          // The bridge keeps the accepted turn blocked while ACP finishes
          // cancelling it. Retain its original response receipt so a late
          // native completion can release that turn without killing peers.
          if (onTimeout) {
            Promise.all([Promise.resolve().then(() => onTimeout(error)), this.notify("session/cancel", { sessionId: params.sessionId })])
              .catch(cause => { error.cause = cause; settle(reject, error); });
          } else {
            if (!retire()) return;
            this.notify("session/cancel", { sessionId: params.sessionId }).then(() => reject(error), cause => {
              error.cause = cause; reject(error);
            });
          }
        } else settle(reject, error);
      }, timeoutMs);
      this.pending.set(id, { resolve: value => settle(resolve, value), reject: error => settle(reject, error) });
      this.write({ jsonrpc: "2.0", id, method, params }, { timeoutMs: Math.min(timeoutMs, this.writeTimeoutMs), onSubmit: () => { submitted = true; } })
        .then(() => { written = true; }, error => settle(reject, error));
    }).finally(settled).catch(error => { throw submitted ? error : new AcpNotSubmittedError(error); });
  }
  notify(method, params) { return this.write({ jsonrpc: "2.0", method, params }); }
  supports(capability) { return this.info?.agentCapabilities?.sessionCapabilities?.[capability] != null; }
  // Signal the group only after its leader has exited, so the agent itself
  // gets its grace period first; ESRCH means nothing was left behind.
  async reapGroup(killMs) {
    if (!this.processGroup || !this.child?.pid) return;
    const signal = name => { try { process.kill(-this.child.pid, name); return true; } catch { return false; } };
    if (!signal("SIGTERM")) return;
    await new Promise(resolve => setTimeout(resolve, Math.min(killMs, 500)));
    signal("SIGKILL");
  }
  close({ graceMs = 2000, killMs = 1000 } = {}) {
    if (this.closing) return this.closing;
    if (!this.child) return Promise.resolve(true);
    deadline(graceMs); deadline(killMs);
    if (this.exitNotified) return (this.closing = this.reapGroup(killMs).then(() => true));
    const child = this.child;
    this.alive = false;
    this.rejectPending(new Error(`The ${this.label} connection was closed.`));
    // Drop queued bytes as well as signaling EOF; they must not execute later.
    child.stdin.destroy();
    const wait = async ms => {
      let timer;
      try { return await Promise.race([this.exitPromise.then(() => true), new Promise(resolve => { timer = setTimeout(() => resolve(false), ms); })]); }
      finally { clearTimeout(timer); }
    };
    const reap = exited => this.reapGroup(killMs).then(() => exited);
    this.closing = (async () => {
      if (await wait(graceMs)) return reap(true);
      // This object never replaces its child. Check that exact process before
      // each signal, including signalCode (exitCode stays null after a signal).
      if (child.exitCode == null && child.signalCode == null) child.kill("SIGTERM");
      if (await wait(killMs)) return reap(true);
      if (child.exitCode == null && child.signalCode == null) child.kill("SIGKILL");
      const exited = await wait(killMs);
      child.stdout.destroy(); child.stderr.destroy();
      return reap(exited);
    })();
    return this.closing;
  }
}

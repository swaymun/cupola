import { fork } from "node:child_process";
import { setImmediate as yieldLoop } from "node:timers/promises";

const methods = ["getSessionMessages", "getSubagentMessages", "listSessions", "listSubagents", "getSessionInfo"];

// Starting a reader costs about 0.1 s of CPU plus an SDK import, so one
// process serves every read in turn. Parsing garbage stays in its heap, not
// the bridge's: it exits after this much idle time, this many requests, or
// once it reports this much memory.
const IDLE_MS = 30_000;
const MAX_USES = 256;
const MAX_RSS = 512 * 1024 * 1024;

function nativeProcess() {
  const child = fork(new URL("./read-worker.mjs", import.meta.url), [], { execArgv: [], stdio: ["ignore", "ignore", "ignore", "ipc"] });
  child.on("error", () => {}); // A late IPC close still has an owner after an exchange is cancelled.
  const reader = { pid: child.pid, uses: 0, rss: 0, entry: null, exited: false,
    on: (...args) => child.on(...args), off: (...args) => child.off(...args), postMessage: message => child.send(message),
    // An idle reader never keeps the runtime (or a test) alive on its own.
    hold: busy => { if (busy) { child.ref(); child.channel?.ref(); } else { child.unref(); child.channel?.unref(); } },
    terminate: () => new Promise(resolve => {
      if (child.exitCode !== null || child.signalCode !== null) return resolve();
      child.ref(); child.once("exit", resolve); child.kill("SIGKILL");
    }) };
  child.once("exit", () => { reader.exited = true; });
  reader.hold(false);
  return reader;
}

// Only one bounded clone is in flight. ACK after yielding so other requests
// run even when a native transcript contains many thousands of records.
async function exchange(worker, input, signal) {
  signal.throwIfAborted();
  const rows = [];
  let stop, failed, exited, receive;
  try {
    return await new Promise((resolve, reject) => {
      stop = () => reject(signal.reason); failed = reject;
      exited = code => reject(new Error(`Claude history reader exited (${code})`));
      receive = async message => {
        try {
          if (Number.isFinite(message.rss)) worker.rss = message.rss;
          if (message.type === "rows") {
            rows.push(...message.rows);
            await yieldLoop();
            if (!signal.aborted) worker.postMessage("next");
          } else if (message.type === "done") resolve(message.array ? rows : message.value);
          else if (message.type === "error") {
            const error = message.syntax ? new SyntaxError(message.message) : new Error(message.message);
            if (message.code) error.code = message.code;
            reject(error);
          }
        } catch (error) { reject(error); }
      };
      signal.addEventListener("abort", stop, { once: true });
      worker.on("error", failed); worker.on("exit", exited); worker.on("message", receive);
      worker.postMessage(input);
    });
  } finally {
    signal.removeEventListener("abort", stop);
    worker.off("error", failed); worker.off("exit", exited); worker.off("message", receive);
  }
}

// The one parsing owner for its caller: SDK history reads and JSON records
// too large for the event loop run in a single reused process, one request at
// a time. Parsing never uses a worker thread: terminating one left its
// allocator's large arenas in bridge RSS. Close cancels queued work.
export class ReadWorkers {
  constructor({ idleMs = IDLE_MS, maxUses = MAX_USES, maxRss = MAX_RSS } = {}) {
    Object.assign(this, { idleMs, maxUses, maxRss, tail: Promise.resolve(), abort: new AbortController(), reader: null, idle: null, starts: 0 });
  }
  enqueue(action) {
    const task = this.tail.then(() => { clearTimeout(this.idle); return action(); }).finally(() => this.rest());
    this.tail = task.then(() => {}, () => {}); return task;
  }
  // Called after each queued request; the next one clears the timer first.
  rest() {
    clearTimeout(this.idle);
    if (!this.reader) return;
    this.idle = setTimeout(() => { void this.retire(); }, this.idleMs);
    this.idle.unref?.();
  }
  async retire() {
    clearTimeout(this.idle);
    const reader = this.reader; this.reader = null;
    if (reader) await (this.retiring = reader.terminate());
  }
  // Inside the queue only. A different SDK build gets a fresh process.
  async process(entry) {
    const old = this.reader;
    if (old && (old.exited || old.uses >= this.maxUses || old.rss > this.maxRss || (entry && old.entry && old.entry !== entry))) await this.retire();
    if (!this.reader) { this.reader = nativeProcess(); this.starts++; }
    if (entry) this.reader.entry = entry;
    return this.reader;
  }
  // A cancelled exchange may still be streaming rows, so its process is not reused.
  async run(entry, input, signal) {
    signal.throwIfAborted();
    const reader = await this.process(entry);
    reader.uses++; reader.hold(true);
    try { return await exchange(reader, input, signal); }
    catch (error) { if (signal.aborted || reader.exited) await this.retireReader(reader); throw error; }
    finally { reader.hold(false); }
  }
  async retireReader(reader) { if (this.reader === reader) await this.retire(); else await reader.terminate(); }
  sdk(entry, method, args) {
    return this.usingSdk(entry, sdk => {
      if (!methods.includes(method)) throw new Error("Unavailable native history reader");
      return sdk[method](...args);
    });
  }
  // A lease (e.g. a roster's 200 helper reads) keeps the queue until it ends.
  usingSdk(entry, action) {
    return this.enqueue(async () => {
      const owner = new AbortController(), signal = AbortSignal.any([this.abort.signal, owner.signal]);
      signal.throwIfAborted();
      let pending = Promise.resolve();
      const sdk = Object.fromEntries(methods.map(method => [method, (...args) => {
        const call = pending.then(() => this.run(entry, { type: "sdk", entry, method, args }, signal));
        pending = call.then(() => {}, () => {}); return call;
      }]));
      try { return await action(sdk); }
      finally { owner.abort(new Error("Claude native read lease closed")); await pending; }
    });
  }
  // One JSON value from a file range. `summary` returns only the transcript
  // index entry for the record, so a huge record never crosses into the bridge.
  json(path, position, length, { summary = false } = {}) {
    return this.enqueue(() => this.run(null, { type: "json", path, position, length, summary }, this.abort.signal));
  }
  async close() { this.abort.abort(new Error("Claude history reader closed")); await this.tail; await this.retire(); }
}

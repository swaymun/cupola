// Wonder's App Server dialect on one side, a selected ACP agent on the
// other. The daemon talks to this bridge exactly as it talks to the Claude
// bridge; the agent sees an ordinary ACP client that offers no file system or
// terminal, so it uses its own tools and configured permission policy.
import { createHash, randomUUID } from "node:crypto";
import { open, stat } from "node:fs/promises";
import { constants } from "node:fs";
import { isAbsolute } from "node:path";
import { BRIDGE_PROTOCOL, TurnProjection, toolItem } from "./projection.mjs";
import { databaseLocked, openCodeAdapter } from "./catalog.mjs";
import { AcpNotSubmittedError } from "./agent.mjs";
import { SessionCommands } from "./session-commands.mjs";
import { isProjectTool } from "./thread-tools.mjs";
import { CuaDriver, ToolServer, callArguments, canonical, mcpTools, qualifiedName, serverName, sessionTool, wonderTool } from "./tool-server.mjs";

async function settleWithin(promise, ms) {
  let timer;
  try { return await Promise.race([promise.then(() => true), new Promise(resolve => { timer = setTimeout(() => resolve(false), ms); })]); }
  finally { clearTimeout(timer); }
}

// One admission owner for every daemon-facing frame, including projections,
// receipts and server calls across all turns. Account before retaining a line;
// there are no per-turn promise closures waiting to reach this queue.
class OutputQueue {
  constructor(write, onFailure, { bytes = 32 * 1024 * 1024, items = 4096 } = {}) {
    Object.assign(this, { write, onFailure, maxBytes: bytes, maxItems: items });
    this.queue = []; this.bytes = 0; this.items = 0; this.writing = null; this.failure = null;
  }
  rejected(error) {
    const rejected = Promise.reject(error);
    // Projection emits synchronously. Its caller may await only the final send,
    // so every rejection must be observed immediately, not at turn completion.
    rejected.catch(() => {});
    return rejected;
  }
  send(frame) {
    if (this.failure) return this.rejected(this.failure);
    let line;
    try { line = `${JSON.stringify(frame)}\n`; }
    catch (error) { this.fail(error); return this.rejected(error); }
    const bytes = Buffer.byteLength(line);
    if (bytes > this.maxBytes - this.bytes || this.items >= this.maxItems) {
      const error = Object.assign(new Error(`ACP output exceeded the allowed size (${this.bytes} bytes / ${this.items} frames queued; limits ${this.maxBytes} bytes / ${this.maxItems} frames)`),
        { queuedBytes: this.bytes, queuedItems: this.items, rejectedBytes: bytes });
      this.fail(error);
      return this.rejected(error);
    }
    let resolve, reject;
    const sent = new Promise((yes, no) => { resolve = yes; reject = no; });
    sent.catch(() => {});
    this.bytes += bytes; this.items++;
    this.queue.push({ line, bytes, admittedAt: performance.now(), resolve, reject });
    if (!this.writing) {
      this.writing = this.drain();
    }
    return sent;
  }
  async drain() {
    while (this.queue.length && !this.failure) {
      const entry = this.queue.shift();
      this.current = entry;
      try {
        const written = this.write(entry.line, entry.admittedAt);
        entry.line = null; // only the in-flight writer retains these bytes
        await written;
        if (!this.failure) entry.resolve();
      } catch (error) { this.fail(error); }
      finally {
        entry.line = null;
        this.bytes -= entry.bytes; this.items--;
        this.current = null;
      }
    }
    // Clear before resolving the drain promise: a send continuation can admit
    // another frame immediately when the last write callback fires.
    this.writing = null;
  }
  fail(error) {
    if (this.failure) return;
    this.failure = error;
    this.current?.reject(error);
    for (const entry of this.queue) {
      entry.reject(error);
      this.bytes -= entry.bytes; this.items--;
    }
    this.queue = [];
    this.onFailure(error);
  }
  async flush() {
    await this.writing;
    if (this.failure) throw this.failure;
  }
}

// How long a check answers routine reads. A working provider is checked
// again hourly for changes nothing local shows (a new model, an account
// change on the provider's side); one that is not set up every half hour.
// A changed CLI or settings file, or an explicit refresh, checks at once.
export const CATALOG_FRESH_MS = 60 * 60_000;
export const SETUP_FRESH_MS = 30 * 60_000;
const CHECK_RETRY_MS = 30_000, CHECK_RETRY_MAX_MS = 10 * 60_000;

// What a check found, in the terms the daemon acts on: a provider that is not
// set up waits for its owner, a failed check is tried again, and neither is a
// broken runtime.
export function availability(catalog) {
  if (catalog.missing) return "notInstalled";
  if (!catalog.installed) return "unavailable";
  if (catalog.connected === false) return catalog.statusFailed ? "unavailable" : "signedOut";
  if (catalog.models.length) return "ready";
  return catalog.modelsError ? "unavailable" : "noModels";
}

// A failed read keeps what the last good check found; a confirmed answer
// (not installed, signed out, an empty list) replaces it.
function retainLastGood(value, previous) {
  if (!previous) return value;
  const next = { ...value };
  if (value.installed && value.modelsError && previous.models?.length) Object.assign(next, { models: previous.models, stale: true });
  if (value.configError) next.defaultModel = previous.defaultModel ?? null;
  return next;
}

async function projectContext(value, cwd) {
  const project = value && typeof value === "object" ? value : { cwd, additionalDirectories: [] };
  const dirs = [project.cwd, ...(project.additionalDirectories ?? [])];
  if (!isAbsolute(project.cwd ?? "") || dirs.length > 16 || dirs.some(d => typeof d !== "string" || !isAbsolute(d)))
    throw new Error("The project folders are invalid. Edit the project in Cupola.");
  for (const dir of dirs) if (!(await stat(dir).catch(() => null))?.isDirectory()) throw new Error("A project folder is missing on this computer. Edit the project in Cupola.");
  return { cwd: project.cwd, additionalDirectories: [...new Set(project.additionalDirectories ?? [])].filter(d => d !== project.cwd) };
}

function page(data, params, label = "OpenCode") {
  const fingerprint = createHash("sha256").update(JSON.stringify(data)).digest("hex").slice(0, 16);
  const [expected, offsetText] = String(params.cursor ?? `${fingerprint}:0`).split(":");
  const offset = Number(offsetText);
  if (expected !== fingerprint || !Number.isSafeInteger(offset) || offset < 0 || offset > data.length) throw new Error(`${label} history changed. Refresh the conversation.`);
  const limit = Number.isSafeInteger(params.limit) ? Math.max(1, Math.min(params.limit, 200)) : 50;
  return { data: data.slice(offset, offset + limit), nextCursor: offset + limit < data.length ? `${fingerprint}:${offset + limit}` : null };
}

const sniff = bytes => bytes.subarray(0, 8).equals(Buffer.from([137, 80, 78, 71, 13, 10, 26, 10])) ? "image/png"
  : bytes[0] === 255 && bytes[1] === 216 && bytes[2] === 255 ? "image/jpeg"
  : ["GIF87a", "GIF89a"].includes(bytes.subarray(0, 6).toString()) ? "image/gif"
  : bytes.subarray(0, 4).toString() === "RIFF" && bytes.subarray(8, 12).toString() === "WEBP" ? "image/webp" : null;

export async function promptBlocks(input, capabilities, adapter = openCodeAdapter) {
  const label = adapter.label;
  if (!Array.isArray(input) || !input.length) throw new Error(`${label} requires a message.`);
  const blocks = [];
  let attachmentBytes = 0;
  const videoReferences = new Set();
  for (const block of input) {
    if (block.type === "text" && typeof block.text === "string") blocks.push({ type: "text", text: block.text });
    else if (["localImage", "localFile"].includes(block.type) && typeof block.path === "string" && isAbsolute(block.path)) {
      const image = block.type === "localImage";
      if (typeof block.mimeType !== "string" || !/^[\w!#$&^.+-]+\/[\w!#$&^.+-]+$/.test(block.mimeType))
        throw new Error("The attached file's format is missing. Attach it again.");
      const video = !image && block.mimeType.toLowerCase().startsWith("video/");
      if (image ? !capabilities?.image : !video && !capabilities?.embeddedContext)
        throw new Error(image ? `This ${label} setup cannot read images.` : `This ${label} setup cannot read files.`);
      if (block.mimeType.startsWith("audio/")) throw new Error(`${label} cannot read audio attachments. Dictation can send its transcript as text.`);
      if (!Number.isSafeInteger(block.byteSize) || block.byteSize <= 0 || block.byteSize > (video ? 1024 * 1024 * 1024 : 8 * 1024 * 1024))
        throw new Error(`The attachment size is invalid. ${label} accepts videos up to 1 GiB and other files up to 8 MB each.`);
      if (!video) attachmentBytes += block.byteSize;
      if (attachmentBytes > 16 * 1024 * 1024) throw new Error("These attachments exceed 16 MB. Send them in separate messages.");
      if (typeof block.sha256 !== "string" || !/^[a-f0-9]{64}$/.test(block.sha256))
        throw new Error("The attachment integrity record is missing. Attach it again.");
      const file = await open(block.path, constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK);
      try {
        const info = await file.stat();
        if (!info.isFile() || info.size !== block.byteSize) throw new Error("An attached file changed. Attach it again before retrying.");
        if (video) {
          const hash = createHash("sha256"), buffer = Buffer.alloc(256 * 1024);
          let length = 0;
          while (true) {
            const { bytesRead } = await file.read(buffer, 0, buffer.length, null);
            if (!bytesRead) break;
            length += bytesRead;
            if (length > block.byteSize) throw new Error("An attached file changed. Attach it again before retrying.");
            hash.update(buffer.subarray(0, bytesRead));
          }
          if (length !== block.byteSize || hash.digest("hex") !== block.sha256)
            throw new Error("An attached file changed. Attach it again before retrying.");
          // The daemon already copied verified bytes to this private sent
          // path. Let the agent's own tools open it, as with Codex/Claude.
          videoReferences.add(blocks.length);
          blocks.push({ type: "text", text: `Attached file ${JSON.stringify(block.name ?? "video")} is available at ${JSON.stringify(block.path)}.` });
          continue;
        }
        // Read at most the declared bytes plus one; a changing file cannot
        // grow memory or replace the content verified at upload time.
        const buffer = Buffer.alloc(block.byteSize + 1);
        let length = 0;
        while (length < buffer.length) {
          const { bytesRead } = await file.read(buffer, length, buffer.length - length, null);
          if (!bytesRead) break;
          length += bytesRead;
        }
        const bytes = buffer.subarray(0, length);
        if (length !== block.byteSize || createHash("sha256").update(bytes).digest("hex") !== block.sha256)
          throw new Error("An attached file changed. Attach it again before retrying.");
        if (image) {
          const mimeType = sniff(bytes);
          if (!mimeType) throw new Error(`This image format is not supported by ${label}.`);
          blocks.push({ type: "image", mimeType, data: bytes.toString("base64") });
        } else {
          const resource = { uri: `wonder-attachment://${block.sha256}/${encodeURIComponent(block.name ?? "file")}`, mimeType: block.mimeType };
          if (block.mimeType.startsWith("text/") || block.mimeType === "application/json") {
            try { resource.text = new TextDecoder("utf-8", { fatal: true }).decode(bytes); }
            catch { throw new Error("This text attachment is not UTF-8. Save it as UTF-8 and attach it again."); }
          } else resource.blob = bytes.toString("base64");
          blocks.push({ type: "resource", resource });
        }
      } finally { await file.close(); }
    } else throw new Error(`This attachment type is not supported by ${label}. Dictation can send its transcript as text.`);
  }
  if (Buffer.byteLength(JSON.stringify(blocks)) > 30 * 1024 * 1024) throw new Error(`This message is too large for ${label}. Send it in smaller parts.`);
  // Keep the adapter's slash-command attachment rule even though video file
  // references become text on the provider wire.
  adapter.validatePromptComposition(blocks.map((block, index) => videoReferences.has(index) ? { type: "resource_link" } : block));
  return blocks;
}

// Wonder's tool result as an MCP `tools/call` result.
function mcpResult(result) {
  const content = (result?.contentItems ?? []).flatMap(block => {
    if (block.type === "inputText") return [{ type: "text", text: block.text }];
    const image = block.type === "inputImage" && /^data:(image\/(?:png|jpeg|webp|gif));base64,([A-Za-z0-9+/=]+)$/.exec(block.imageUrl ?? "");
    return image ? [{ type: "image", mimeType: image[1], data: image[2] }] : [];
  });
  return { content: content.length ? content : [{ type: "text", text: "The action returned no displayable result." }], isError: result?.success !== true };
}

// While the owner controls the computer from a phone, computer use waits
// (see the daemon's wonder_computer_control_check).
export const OWNER_HAS_CONTROL = "The owner has control; wait.";
export const CONTROL_UNKNOWN = "Cupola couldn't check whether the owner is using this computer, so this action did not run. Try again shortly.";
const CONTROL_CHECK = "wonder_computer_control_check";

// Windows and Linux hosts: the Cua Driver (trycua/cua) the daemon named for
// this thread's current turn, or null when computer use is off for it (or in
// Plan mode).
function cuaDriver(options) {
  const driver = options?.config?.["mcp_servers.cua_driver"];
  if (!driver || typeof driver !== "object" || driver.enabled !== true || typeof driver.command !== "string") return null;
  if (options.offerComputer === false) return null;
  return { command: driver.command, args: Array.isArray(driver.args) ? driver.args.filter(arg => typeof arg === "string") : [],
    env: Object.fromEntries(Object.entries(driver.env ?? {}).filter(([, value]) => typeof value === "string")) };
}

// Providers that refuse a request listing more tools than this. The Cua Driver
// adds 60+ tools to every OpenCode thread, so on these models its tools are
// offered only to a turn that asks for computer use.
const TOOL_LIMITS = [[/(^|\/)beam(-|$)/i, 128]];
// Only explicit requests to drive the computer: words like screenshot, desktop or
// simulator appear in ordinary coding chats and would re-attach the tools (and, held,
// keep them) on nearly every thread, hitting the limit again.
const COMPUTER_REQUEST = /\b(computer use|use (the|my) computer|control (the|my) (computer|mac)|cua( driver)?)\b/i;
// A limited model gets only the Cua tools a turn needs to look at and drive the screen:
// the full set (60+) plus Cupola's and the owner's own servers still overflows the limit.
const COMPACT_COMPUTER_TOOLS = new Set(["check_permissions", "list_apps", "launch_app", "bring_to_front", "list_windows", "get_window_state",
  "get_desktop_state", "get_screen_size", "click", "double_click", "right_click", "drag", "scroll", "type_text", "press_key", "hotkey", "run_steps"]);
export function limitedComputerTools(model, tools) {
  return TOOL_LIMITS.some(([pattern]) => pattern.test(String(model ?? ""))) ? tools.filter(tool => COMPACT_COMPUTER_TOOLS.has(tool?.name)) : tools;
}
const TOOL_LIMIT_ERROR = /maximum number of items is \d+|too many tools/i;
// Once offered, a session keeps the tools (`held`), so its driver key changes
// at most once and does not restart the agent on every turn.
export function offersComputerUse(model, input, held = false) {
  if (!TOOL_LIMITS.some(([pattern]) => pattern.test(String(model ?? "")))) return true;
  if (held) return true;
  const text = (Array.isArray(input) ? input : []).map(block => (typeof block?.text === "string" ? block.text : "")).join(" ");
  return COMPUTER_REQUEST.test(text);
}

export class AcpBridge {
  // `createAgent(handlers, { binary })` starts ACP with the owner's native configuration;
  // `inspect()` reports the installed version and models. `tools` configures
  // the thread-tool server (`false` offers no tools); `matchMs` bounds how long
  // a tool call waits for the agent's report of the same call.
  // Stop gets one 10 s budget for cancel delivery and native confirmation:
  // below the phone's 15 s HTTP timeout and the daemon's 30 s RPC timeout,
  // leaving 5 s for transport/handler overhead. Writing cancel is not success.
  // `locate()` finds the CLI an agent starts from, without a model check;
  // `fingerprint()` cheaply identifies what a check would read (the CLI and
  // its settings files), so a change is noticed before the check goes stale.
  constructor({ sessions, send, write, outputLimits, createAgent, inspect, locate = null, fingerprint = async () => null, now = Date.now, onFatal = () => {}, onOutputFailure = () => {}, idleMs = 10 * 60_000, stopTimeoutMs = 10_000, tools = {}, matchMs = 5000, relistMs = 3000, adapter = openCodeAdapter }) {
    Object.assign(this, { adapter, family: adapter.family, label: adapter.label, sessions, createAgent, inspect, locate, fingerprint, now, onFatal, onOutputFailure, idleMs, stopTimeoutMs, matchMs, relistMs });
    if (sessions.family !== this.family) throw new Error("ACP session storage belongs to another provider.");
    this.output = new OutputQueue(write ?? (line => send(JSON.parse(line))), error => {
      this.onFatal(error);
      for (const pending of this.pending.values()) { pending.cleanup(); pending.reject(error); }
      this.pending.clear();
      // A lost shared daemon transport cannot deliver any turn's events or
      // approvals. Stop all owned work; only native outcomes/exits settle it.
      this.close().catch(this.onFatal);
      this.onOutputFailure(error);
    }, outputLimits);
    this.send = frame => this.output.send(frame);
    this.agentEntry = null; this.ownedAgents = new Set(); this.drivers = new Map(); this.driverHeld = new Map(); this.active = new Map(); this.pending = new Map();
    this.inspection = null; this.checkAbort = null; this.inspectedFingerprint = null; this.nextCheckAt = 0; this.retryMs = CHECK_RETRY_MS;
    // One provider process at a time does its setup: a model check and an
    // agent's startup never overlap (OpenCode's database refuses a second
    // opener). Held until the agent has initialized, never for its sessions.
    this.launches = Promise.resolve();
    // Sessions waiting for their server to read its tool list again after a change.
    this.relist = new Map();
    this.tools = tools === false ? null : new ToolServer({ ...tools, handler: (sessionId, frame) => this.toolRequest(sessionId, frame),
      alive: sessionId => this.active.has(sessionId) });
  }
  // Computer use reaches a session through the session's own server, never as
  // a server of its own: OpenCode registers MCP servers per folder, so a
  // shared one would give every thread in the folder the same access. The
  // bridge runs the driver and serves its tools to the sessions whose current
  // turn allows them. Each session has a driver of its own: Stop ends the
  // driver process, and one thread's Stop must not end another's action.
  driver(session) {
    const wanted = this.tools ? cuaDriver(session.options) : null;
    const key = wanted ? canonical(wanted) : null, own = this.drivers.get(session.id);
    if (own && own.key !== key) { own.client.stop(); this.drivers.delete(session.id); }
    if (!wanted) return null;
    if (!this.drivers.has(session.id)) this.drivers.set(session.id, { key, client: new CuaDriver(wanted) });
    return this.drivers.get(session.id).client;
  }
  driverKey(session) { const wanted = this.tools ? cuaDriver(session.options) : null; return wanted ? canonical(wanted) : null; }
  // The `mcpServers` for a session: its own server, with a fresh credential
  // bound to `agent`, when it has Wonder's thread tools or computer use.
  async mcpServers(session, agent) {
    if (!this.wantsTools(session)) return [];
    // The tools are an addition: a session that cannot get them still runs.
    try { return [await this.tools.entry(session.id, agent)]; } catch { return []; }
  }
  get replyOnlyTools() { return this.family === "opencode"; }
  // Wait until the session's server has read its tool list again (OpenCode caches a server's
  // tools and refreshes them asynchronously), or give up: the tools are an addition.
  awaitRelist(sessionId, notify) {
    const waiters = this.relist.get(sessionId) ?? new Set();
    this.relist.set(sessionId, waiters);
    return new Promise(resolve => {
      // The list reaches OpenCode on another pipe than the prompt: leave a moment for it to be read.
      const done = () => { clearTimeout(timer); waiters.delete(done); if (!waiters.size) this.relist.delete(sessionId); setTimeout(resolve, 50); };
      const timer = setTimeout(done, this.relistMs);
      waiters.add(done);
      if (notify() === 0) done();
    });
  }
  wantsTools(session) { return Boolean(this.tools) && (mcpTools(session.options?.dynamicTools).length > 0 || this.driverKey(session) !== null); }
  // A thread tool call from the session's own MCP server. It runs only during
  // that session's reply, and only once the agent has reported the same call
  // (name and arguments) in that session: OpenCode lists every session's
  // server in a shared folder, so the report decides which thread is asking.
  // Computer-use tools follow the same rule, and the session's current turn
  // must allow computer use.
  async toolRequest(sessionId, frame) {
    const session = this.sessions.get(sessionId);
    const tools = mcpTools(session.options?.dynamicTools);
    const driver = this.driver(session);
    // A session with no active reply lists no computer-use tools: OpenCode shares one tool
    // list across a folder, so an idle neighbour's 60+ tools would push a limited model over.
    const activeRun = this.active.get(sessionId);
    // Stop ends a driver this reply is still starting or listing.
    const driverTools = driver && activeRun ? limitedComputerTools(activeRun.options.model, await driver.tools(activeRun.abort.signal).catch(() => [])) : [];
    // OpenCode counts every session's server in a folder toward one model's tool limit (Beam: 128),
    // so only the replying thread offers anything; an idle neighbour lists none. Cursor keeps a
    // separate client per session and has no shared limit, so it always lists.
    if (frame.type === "list") {
      const offered = activeRun || !this.replyOnlyTools ? [...tools, ...driverTools.filter(tool => !isProjectTool(tool?.name))] : [];
      for (const done of this.relist.get(sessionId) ?? []) done();
      return offered;
    }
    if (frame.type !== "call") throw new Error("Unknown Cupola tool request.");
    const name = frame.name;
    const computer = !isProjectTool(name) && driverTools.some(tool => tool.name === name);
    if (!computer && (!isProjectTool(name) || !tools.some(tool => tool.name === name))) return mcpResult({ success: false, contentItems: [{ type: "inputText", text: "That tool is not offered to this thread." }] });
    const server = serverName(sessionId), args = canonical(frame.arguments ?? {});
    const matches = (run, owner) => [...run.projection.calls.values()].find(call => {
      const named = sessionTool(call.name ?? call.title);
      return named?.server === owner && named.tool === name && !run.wonderCalls.has(call.toolCallId) && canonical(callArguments(call)) === args;
    });
    const deadline = Date.now() + this.matchMs;
    let run, call;
    for (;;) {
      run = this.active.get(sessionId);
      call = run && run.promptIssued && !run.finalizing ? matches(run, server) : null;
      if (call || Date.now() >= deadline) break;
      await new Promise(resolve => setTimeout(resolve, 25));
    }
    if (!call) {
      const other = [...this.active.values()].find(peer => peer.session.id !== sessionId && peer.promptIssued && matches(peer, server));
      const text = other ? (computer ? "This computer-use tool belongs to another thread in this folder, so it did not run."
        : `These Cupola tools belong to another thread in this folder. Use ${qualifiedName(serverName(other.session.id), name)} instead.`)
        : run ? "Cupola could not match this call to this thread's running reply, so it did not run."
          : "Cupola's thread tools work only while this thread is replying.";
      return mcpResult({ success: false, contentItems: [{ type: "inputText", text }] });
    }
    run.wonderCalls.add(call.toolCallId);
    if (computer) {
      // While the owner controls the computer from a phone, the agent waits.
      const refusal = await this.computerControl(session, run, call.toolCallId);
      if (refusal) return mcpResult({ success: false, contentItems: [{ type: "inputText", text: refusal }] });
      // The driver's own result (text and screenshots) goes back as is.
      try { return await driver.call(name, frame.arguments ?? {}, run.abort.signal); }
      catch (error) { return mcpResult({ success: false, contentItems: [{ type: "inputText", text: error.message }] }); }
    }
    const result = await this.serverCall("item/tool/call", { threadId: session.id, turnId: run.turn.id, callId: call.toolCallId,
      tool: name, arguments: frame.arguments ?? {}, agentFamily: this.family }, run.abort.signal);
    return mcpResult(result);
  }
  // Only the daemon's explicit, successful "no" lets a computer-use action
  // run; an error, a failed check or an unreadable answer refuses it rather
  // than risk acting while the owner is using the computer.
  async computerControl(session, run, callId) {
    try {
      const result = await this.serverCall("item/tool/call", { threadId: session.id, turnId: run.turn.id, callId,
        tool: CONTROL_CHECK, arguments: {}, agentFamily: this.family }, run.abort.signal);
      if (result?.success !== true) return CONTROL_UNKNOWN;
      const held = JSON.parse(result.contentItems?.[0]?.text ?? "").ownerHasControl;
      return held === false ? null : held === true ? OWNER_HAS_CONTROL : CONTROL_UNKNOWN;
    } catch { return CONTROL_UNKNOWN; }
  }
  exclusive(task) {
    const turn = this.launches.then(task);
    this.launches = turn.then(() => {}, () => {});
    return turn;
  }
  // The one owner of what this provider offers. A routine read (the daemon's
  // periodic check, a model list) answers from the last check while it is
  // fresh and the CLI and its settings files are unchanged; `refresh`
  // (Reconnect, opening Providers, an activated runtime) checks now. One check
  // runs at a time and every caller shares it. No reply waits for a check.
  async catalog(refresh = false) {
    if (this.inspecting) return this.inspecting;
    const fingerprint = await this.fingerprint().catch(() => null);
    if (this.inspecting) return this.inspecting;
    if (!refresh && this.inspection && fingerprint === this.inspectedFingerprint && this.now() < this.nextCheckAt) return this.inspection;
    const previous = this.inspection;
    this.inspecting = this.yieldingCheck().then(async value => {
      // Read after the check, so a file the CLI itself rewrites while being
      // checked does not count as a change at the next read.
      this.inspectedFingerprint = await this.fingerprint().catch(() => null);
      this.inspection = retainLastGood(value, previous);
      const found = availability(value);
      const failed = found === "unavailable" || value.modelsError || value.configError;
      this.nextCheckAt = this.now() + (failed ? this.retryAfter() : found === "ready" ? CATALOG_FRESH_MS : SETUP_FRESH_MS);
      if (!failed) this.retryMs = CHECK_RETRY_MS;
      return this.inspection;
    }, error => {
      // A check that could not finish says nothing new; the last good one stands.
      if (this.closing || !previous?.models?.length) throw error;
      this.inspection = { ...previous, stale: true };
      this.nextCheckAt = this.now() + this.retryAfter();
      return this.inspection;
    });
    try { return await this.inspecting; } finally { this.inspecting = null; }
  }
  // A check shares the setup queue with agent starts (OpenCode's database
  // takes one opener), but it never holds a reply up: a start cancels the
  // running check, which ends its commands, and the check runs again after
  // the start, from the same queue.
  yieldingCheck() {
    const yielded = new AbortController();
    // Publish cancellation at admission, before the queue gets a chance to
    // run. A reply can arrive while this check is still waiting in the queue.
    this.checkAbort = yielded;
    return this.exclusive(() => {
      yielded.signal.throwIfAborted();
      return this.inspect({ signal: yielded.signal });
    }).finally(() => { if (this.checkAbort === yielded) this.checkAbort = null; })
      .catch(error => {
        if (!yielded.signal.aborted || this.closing) throw error;
        return this.yieldingCheck();
      });
  }
  // A failed check is tried again after 30 s, doubling to 10 minutes, spread
  // so several runtimes do not retry together.
  retryAfter() {
    const after = this.retryMs * (0.8 + Math.random() * 0.4);
    this.retryMs = Math.min(this.retryMs * 2, CHECK_RETRY_MAX_MS);
    return after;
  }
  // Sessions share one owned process for this provider.
  async agent() {
    if (this.closing) throw new Error(`${this.label} is closing. Try again after Cupola restarts.`);
    // A live agent is used as is: a reply never waits for a model check.
    let entry = this.agentEntry;
    if (entry && (entry.starting || entry.agent.alive)) {
      if (entry.starting) await entry.starting;
      if (entry.agent.alive) { this.touch(entry); return entry.agent; }
    }
    entry?.commands.clear();
    // Starting one needs only where the CLI is, not what it offers.
    const binary = this.locate ? await this.locate() : undefined;
    if (this.closing) throw new Error(`${this.label} is closing. Try again after Cupola restarts.`);
    // Another reply may have started one meanwhile; share it.
    if (this.agentEntry && this.agentEntry !== entry) return this.agent();
    if (this.locate && !binary) throw new Error(`Install ${this.label}’s command-line tool on your computer, then reconnect ${this.label}.`);
    for (let attempt = 0; ; attempt++) {
      try { return await this.startAgent(binary); } catch (error) {
        // Nothing has been sent to the agent yet, so a start refused by a
        // locked database is safe to try once more.
        if (attempt > 0 || this.closing || !error.lockedStart) throw error;
      }
    }
  }
  async startAgent(binary) {
    if (this.closing) throw new Error(`${this.label} is closing. Try again after Cupola restarts.`);
    let entry;
    const agent = this.createAgent({
      onUpdate: params => this.update(params, agent),
      onPermission: params => this.permission(params, agent),
      onExit: () => {
        this.tools?.tokens.revokeOwner(agent);
        entry?.commands.clear();
        if (this.agentEntry?.agent === agent) { clearTimeout(this.agentEntry.timer); this.agentEntry = null; }
        clearTimeout(entry?.timer);
        this.ownedAgents.delete(entry);
        this.agentExited(agent);
      },
    }, { binary });
    entry = { agent, commands: new SessionCommands(agent.attached), timer: null,
      starting: this.exclusive(() => agent.start({ timeoutMs: 5000 }).then(() => this.adapter.authenticate?.(agent))) };
    // Queued first, so the check that yields runs again after this start.
    this.checkAbort?.abort(new Error(`${this.label} check yielded to a reply.`));
    this.agentEntry = entry;
    this.ownedAgents.add(entry);
    try { await entry.starting; } catch (error) {
      if (this.agentEntry === entry) this.agentEntry = null;
      await agent.close();
      // The agent's last words can arrive just after it exits.
      const stderr = agent.child?.stderr;
      if (stderr && !stderr.closed) await settleWithin(new Promise(resolve => stderr.once("close", resolve)), 250);
      if (databaseLocked(agent.stderrTail)) error.lockedStart = true;
      throw error;
    } finally { entry.starting = null; }
    if (this.closing) { await agent.close(); throw new Error(`${this.label} is closing.`); }
    this.touch(entry);
    return agent;
  }
  touch(entry) {
    clearTimeout(entry.timer);
    entry.timer = setTimeout(() => {
      if ([...this.active.values()].some(run => run.agent === entry.agent)) return this.touch(entry);
      entry.commands.clear();
      entry.agent.close();
    }, this.idleMs);
    entry.timer.unref?.();
  }
  agentExited(agent) {
    for (const run of this.active.values()) if (run.agent === agent) {
      run.exited = true;
      run.abort.abort();
      if (run.uncertain) this.finish(run, { error: new Error(`${this.label} stopped before confirming this response. You can send again.`) }).catch(error => { if (error !== this.output.failure) this.onFatal(error); });
    }
  }
  serverCall(method, params, signal) {
    if (signal?.aborted) return Promise.reject(new Error(`${this.label} action cancelled.`));
    const id = `${this.family}-request-${randomUUID()}`;
    return new Promise((resolve, reject) => {
      const abort = () => { this.pending.delete(id); cleanup(); reject(new Error(`${this.label} action cancelled.`)); };
      const cleanup = () => signal?.removeEventListener("abort", abort);
      signal?.addEventListener("abort", abort, { once: true });
      this.pending.set(id, { resolve, reject, cleanup });
      Promise.resolve(this.send({ id, method, params })).catch(error => { this.pending.delete(id); cleanup(); reject(error); });
    });
  }
  async receive(frame) {
    if (!frame || typeof frame !== "object") throw new Error("Invalid runtime frame");
    if (!frame.method && frame.id != null) {
      const pending = this.pending.get(frame.id);
      if (!pending) return;
      this.pending.delete(frame.id); pending.cleanup();
      if (frame.error) pending.reject(new Error("The action was cancelled in Cupola.")); else pending.resolve(frame.result);
      return;
    }
    if (frame.id == null) return;
    try { await this.send({ id: frame.id, result: await this.request(frame.method, frame.params ?? {}) }); }
    catch (error) {
      // A refusal before the turn was recorded is typed like Claude's, so the daemon stores it safe to retry.
      const data = error.inputRefusal && frame.method === "turn/start" ? { turnStart: { accepted: false, action: error.inputRefusal } } : undefined;
      await this.send({ id: frame.id, error: { code: -32000, message: error.message, ...(data ? { data } : {}) } });
    }
  }
  runFor(acpSessionId) {
    for (const run of this.active.values()) if (run.session.acpSessionId === acpSessionId) return run;
    return null;
  }
  update({ sessionId, update }, agent) {
    if (this.output.failure) return;
    // Capability state is not a turn item, including during setup/replay and
    // after a turn ends. A replaced idle peer must never mutate its successor.
    if (update?.sessionUpdate === "available_commands_update") {
      if (!this.closing && this.agentEntry?.agent === agent && agent.alive) this.agentEntry.commands.update(sessionId, update.availableCommands);
      return;
    }
    const run = this.runFor(sessionId);
    if (run && run.agent !== agent) return;
    if (["config_option_update", "current_mode_update"].includes(update?.sessionUpdate)) {
      const session = run?.session ?? [...this.sessions.values.values()].find(value => value.acpSessionId === sessionId && agent.attached.has(sessionId));
      if (session) {
        if (update.sessionUpdate === "config_option_update") session.configuration = { ...session.configuration, configOptions: update.configOptions };
        else if (session.configuration?.modes) session.configuration.modes.currentModeId = update.currentModeId;
        this.sessions.save(session).catch(this.onFatal);
      }
    }
    // Replayed history and updates after a confirmed reply ended are not part
    // of a running turn. Configuration still matters for the next prompt.
    if (run && !run.replaying) run.projection.update(update);
  }
  async permission({ sessionId, toolCall = {}, options = [] }, agent) {
    const run = this.runFor(sessionId);
    const ownsRequest = () => run && this.active.get(run.session.id) === run
      && run.agent === agent && agent.alive && run.promptIssued && !run.exited && !run.finalizing
      && !run.stopped && !run.uncertain && !run.abort.signal.aborted;
    if (!ownsRequest()) return { outcome: "cancelled" };
    // IDs are opaque provider authority. Ambiguous IDs cannot be safely
    // selected, even if only one of the duplicate rows has a known kind.
    if (!Array.isArray(options) || options.some(o => !o || typeof o.optionId !== "string" || !o.optionId.trim())
      || new Set(options.map(o => o.optionId)).size !== options.length) return { outcome: "cancelled" };
    const allow = options.find(o => o.kind === "allow_once"), reject = options.find(o => o.kind === "reject_once");
    // Wonder's own thread tools check their caller's authority themselves.
    // Another thread's copy of them is never this session's to use.
    const wonder = wonderTool(toolCall.title) ?? wonderTool(run.projection.calls.get(toolCall.toolCallId)?.name);
    const owned = sessionTool(toolCall.title) ?? sessionTool(run.projection.calls.get(toolCall.toolCallId)?.name);
    if (wonder || (owned && owned.server !== serverName(run.session.id))) {
      const option = owned.server === serverName(run.session.id) ? allow : reject;
      return option ? { outcome: "selected", optionId: option.optionId } : { outcome: "cancelled" };
    }
    const choices = options.flatMap(option => {
      if (option === allow) return [{ decision: "accept", optionId: option.optionId }];
      if (option === reject) return [{ decision: "decline", optionId: option.optionId }];
      if (["allow_always", "reject_always"].includes(option.kind)) return [{
        decision: { [this.adapter.permissionKey]: { optionId: option.optionId, kind: option.kind } }, optionId: option.optionId,
      }];
      return [];
    });
    if (!choices.length) return { outcome: "cancelled" };
    // The card refers to the item it approves; show it first.
    run.projection.update({ ...toolCall, sessionUpdate: "tool_call_update", status: toolCall.status ?? "pending" });
    const call = { ...(run.projection.calls.get(toolCall.toolCallId) ?? {}), ...toolCall };
    const item = toolItem(call, this.label);
    const base = { threadId: run.session.id, turnId: run.turn.id, itemId: toolCall.toolCallId, agentFamily: this.family };
    const command = item.type === "commandExecution";
    const exactCommand = command && typeof call.rawInput?.command === "string" && call.rawInput.command.trim();
    // A tool's input goes whole, as Claude's does: the phone words every
    // argument and shows this exact JSON as Full details, so shortening any
    // value could hide where the tool writes or sends data.
    const response = await this.serverCall(command || item.type !== "fileChange" ? "item/commandExecution/requestApproval" : "item/fileChange/requestApproval", {
      ...base, kind: exactCommand ? "command" : "tool", ...(item.type === "fileChange" ? { reason: `Allow ${this.label} to change ${item.changes.map(c => c.path.split("/").at(-1)).join(", ") || "a file"}?` }
        : { command: command ? item.command : `${item.tool}: ${JSON.stringify(item.arguments ?? {})}`, cwd: command ? item.cwd : run.cwd,
          reason: command ? `Allow ${this.label} to run this command?` : `Allow ${this.label} to perform this action?` }),
      availableDecisions: [...choices.map(choice => choice.decision), "cancel"],
    }, run.abort.signal).catch(() => null);
    if (!ownsRequest()) return { outcome: "cancelled" };
    const selected = choices.find(choice => canonical(choice.decision) === canonical(response?.decision ?? null));
    if (selected) return { outcome: "selected", optionId: selected.optionId };
    return { outcome: "cancelled" };
  }
  async request(method, params) {
    if (method === "initialize") return { userAgent: `Cupola ${this.label} bridge`, wonderBridge: { protocolVersion: BRIDGE_PROTOCOL, family: this.family }, capabilities: { experimentalApi: true } };
    if (method === "account/read") {
      const catalog = await this.catalog(params.refresh);
      // `availability` separates a provider that is not set up from a check
      // that failed; `diagnostic` is a short reason for the host log only.
      const diagnostic = catalog.modelsError ?? catalog.configError ?? null;
      return { account: (catalog.connected ?? catalog.installed) ? { type: this.family, planType: null } : null, requiresOpenaiAuth: false, agentFamily: this.family,
        connected: (catalog.connected ?? catalog.installed) && catalog.models.length > 0, availability: availability(catalog), stale: catalog.stale === true,
        ...(diagnostic ? { diagnostic } : {}), runtime: { version: catalog.version } };
    }
    if (method === "model/list") {
      const { models, defaultModel } = await this.catalog();
      const preferred = models.find(m => m.id === defaultModel);
      // The daemon uses catalog order when Wonder has no saved preference.
      // Keep OpenCode's configured model first, without inventing a free-model preference.
      const ordered = preferred ? [preferred, ...models.filter(m => m !== preferred)] : models;
      return { data: ordered.map(m => ({ id: `${this.family}:${m.id}`, model: `${this.family}:${m.id}`, displayName: m.name,
        description: `${this.adapter.providerName(m.providerId)}${m.free && m.providerId === "opencode" ? " · Free" : ""}`, hidden: false, agentFamily: this.family,
        nativeModelIds: [m.id], isDefault: m === preferred, supportedReasoningEfforts: [], defaultReasoningEffort: null })), nextCursor: null };
    }
    if (method === "account/rateLimits/read") throw new Error(`${this.label} does not report usage limits.`);
    if (["app/installed", "app/read"].includes(method)) return { apps: [], nextCursor: null };
    if (method === "mcpServerStatus/list") throw new Error(`${this.label} does not report connected tool status through ACP. Check ${this.label} on your computer.`);
    if (method === "thread/loaded/list") return { data: [...this.sessions.values.values()].filter(session => session.turns.some(turn => turn.status === "inProgress")).map(session => session.id), nextCursor: null };
    if (method === "thread/list") return page([...this.sessions.values.values()].filter(s => s.acpSessionId).map(s => this.sessions.describe(s)), params, this.label);
    if (method === "thread/start") {
      this.adapter.nativeModel(params.model);
      this.adapter.validatePolicy(params.wonderPolicy);
      const project = await projectContext(params.wonderProject, params.cwd);
      const options = { ...params, wonderProject: project, cwd: project.cwd, offerComputer: offersComputerUse(params.model, null) };
      const { session, duplicate } = await this.sessions.create(options, async session => {
        let entry, token, sessionId, requestIssued = false;
        try {
          const agent = await this.agent();
          entry = this.agentEntry;
          if (entry?.agent !== agent || !agent.alive) throw new Error("The agent connection changed. Send again to reconnect.");
          const driver = this.driverKey(session);
          const mcpServers = await this.mcpServers(session, agent);
          token = entry.commands.beginCreate();
          requestIssued = true;
          const created = await agent.request("session/new", { cwd: project.cwd, mcpServers }, { timeoutMs: 10_000 });
          if (!this.closing && this.agentEntry === entry && agent.alive && typeof created?.sessionId === "string" && created.sessionId) {
            sessionId = created.sessionId;
            agent.attached.add(sessionId);
            this.driverHeld.set(session.id, driver);
          }
          return created;
        } catch (error) {
          this.tools?.tokens.revoke(session.id);
          throw requestIssued ? error : new AcpNotSubmittedError(error);
        } finally { if (token) entry.commands.endCreate(token, sessionId); }
      });
      if (!duplicate) await this.send({ method: "thread/started", params: { thread: this.sessions.describe(session), agentFamily: this.family } });
      return { thread: this.sessions.describe(session), model: params.model };
    }
    const session = this.sessions.get(params.threadId);
    // Wonder bridge-local read, not a standard ACP method. Do not call agent(),
    // touch(), inspect(), create or attach to discover a cold session's commands.
    if (method === "thread/commands/list") {
      const entry = this.agentEntry;
      if (!this.closing && entry?.agent.alive && entry.agent.attached.has(session.acpSessionId)) return entry.commands.read(session.acpSessionId);
      return { availability: "unavailable", peerId: null, sessionId: session.acpSessionId, revision: null, reason: null, commands: null };
    }
    if (method === "thread/read") return { thread: this.sessions.describe(session, params.includeTurns === true) };
    if (method === "thread/turns/list") return page([...session.turns].reverse().map(t => params.itemsView === "notLoaded" ? { ...t, items: [], itemCount: t.items.length } : t), params, this.label);
    if (method === "thread/items/list") {
      const turns = params.turnId ? session.turns.filter(t => t.id === params.turnId) : params.sortDirection === "asc" ? session.turns : [...session.turns].reverse();
      return page(turns.flatMap(t => t.items.map(item => ({ turnId: t.id, item }))), params, this.label);
    }
    if (method === "thread/nativeSettings") return { settings: null };
    if (["thread/resume", "thread/settings/update"].includes(method)) {
      this.adapter.validatePolicy(params.wonderPolicy);
      if (this.active.has(session.id) || session.unresolvedTurnId) {
        if (method === "thread/resume") return { thread: this.sessions.describe(session, params.excludeTurns !== true), model: session.options.model };
        throw new Error(`Wait for ${this.label} to finish before changing this conversation.`);
      }
      if (params.model) this.adapter.nativeModel(params.model);
      const next = { ...session.options, ...params, wonderProject: session.options.wonderProject, cwd: session.options.cwd };
      session.options = next; await this.sessions.save(session);
      return { thread: this.sessions.describe(session, true), model: next.model };
    }
    if (method === "turn/start") {
      // Everything before the durable receipt is a refusal: nothing was accepted, so a resend is safe.
      let options, model;
      try {
        if (this.closing) throw new Error(`${this.label} is closing. Try again after Cupola restarts.`);
        options = { ...session.options, ...params, wonderProject: session.options.wonderProject, cwd: session.options.cwd };
        if (params.wonderProject?.cwd && params.wonderProject.cwd !== session.options.cwd) throw new Error("A project conversation keeps its working folder.");
        this.adapter.validatePolicy(options.wonderPolicy);
        model = this.adapter.nativeModel(options.model);
        // The daemon sends the thread tools with every turn; a change reaches
        // the session's MCP server, which tells the agent to list them again.
        if (Array.isArray(params.dynamicTools) && canonical(params.dynamicTools) !== canonical(session.options.dynamicTools ?? null)) {
          session.options = { ...session.options, dynamicTools: params.dynamicTools };
          this.tools?.changed(session.id);
        }
        // So is computer use (off in Plan mode): it applies to this session only.
        if (params.config && typeof params.config === "object") session.options = { ...session.options, config: params.config };
        // Only a limited model's explicit request is held; a thread that merely had the tools on a
        // roomier model must not carry all of them over when switched to a limited one.
        const offerComputer = offersComputerUse(options.model, params.input, session.options.computerHeld === true);
        session.options = { ...session.options, offerComputer, computerHeld: offerComputer && !offersComputerUse(options.model, null) };
      } catch (error) { throw Object.assign(error, { inputRefusal: "retry" }); }
      const { turn, duplicate } = await this.sessions.accept(session, params);
      if (duplicate) return { turn };
      const run = { session, turn, model, options, cwd: session.options.cwd, abort: new AbortController(), stopped: false, agent: null, wonderCalls: new Set() };
      run.projection = new TurnProjection({ family: this.family, label: this.label, threadId: session.id, turnId: turn.id, emit: frame => { run.output = this.send(frame); } });
      run.output = Promise.resolve();
      this.active.set(session.id, run);
      // The tool listing depends on whether this thread has an active reply. The list-changed
      // notice and the prompt travel separate pipes: OpenCode must have read the new list before
      // the prompt, or the first step misses this thread's tools. Cursor lists the same either way.
      const hold = this.replyOnlyTools && this.tools && this.wantsTools(session)
        ? this.awaitRelist(session.id, () => this.tools.changed(session.id))
        : (this.tools?.changed(session.id), new Promise(resolve => setTimeout(resolve, this.driverKey(session) !== null ? 150 : 0)));
      // Return the durable receipt before the agent can produce an event.
      run.finished = hold.then(() => this.execute(run)).then(undefined, error => { if (error !== this.output.failure) this.onFatal(error); });
      return { turn };
    }
    if (method === "turn/interrupt") {
      const run = this.active.get(session.id);
      const turn = session.turns.find(value => value.id === params.turnId);
      if (!turn) throw new Error("This response is no longer available to stop.");
      if (run?.turn === turn) {
        // One deadline covers the cancel write and native acknowledgement.
        // An already unresolved run may have finished its local task without
        // a native outcome, so task settlement alone cannot confirm Stop.
        await settleWithin((async () => {
          if (!run.stopped) {
            run.stopped = true;
            run.abort.abort();
            if (run.promptIssued && run.agent?.alive) await run.agent.notify("session/cancel", { sessionId: session.acpSessionId });
          }
          await run.finished;
        })(), this.stopTimeoutMs);
        if (turn.status === "inProgress") {
          // Acceptance is already durable and reloads as unresolved. Persist
          // this more specific error without extending the Stop deadline.
          run.uncertainUpdate ??= this.markUncertain(run);
          run.uncertainUpdate.catch(this.onFatal);
        }
      }
      if (!["completed", "interrupted", "failed"].includes(turn.status)) throw new Error(this.sessions.uncertainResponse);
      return { turn: { id: turn.id, status: turn.status } };
    }
    if (method === "turn/steer") throw new Error(`Guide is not available for ${this.label}. Queue your message instead.`);
    throw new Error(`This action is not available for ${this.label} yet.`);
  }
  // Attach the session to `agent`: a process that did not create it resumes
  // it (no replay) or, failing that, loads it with its replay ignored.
  async attach(agent, run) {
    const { session } = run;
    // An attached session whose tool credential expired is attached again
    // with a new one; the agent replaces the server of the same name. So is
    // one whose computer use changed, so OpenCode lists its tools again.
    const driver = this.driverKey(session);
    const held = this.driverHeld.get(session.id) ?? null;
    if (agent.attached.has(session.acpSessionId) && driver === held && (!this.wantsTools(session) || this.tools.tokens.live(session.id))) return;
    // A session that no longer needs its server loses it: OpenCode keeps a
    // server a later attach omits, so it is told to exit.
    if (!this.wantsTools(session)) this.tools?.tokens.revoke(session.id);
    const entry = this.agentEntry;
    if (entry?.agent !== agent || !agent.alive) throw new Error("The agent connection changed. Send again to reconnect.");
    const params = { sessionId: session.acpSessionId, cwd: session.options.cwd, mcpServers: await this.mcpServers(session, agent) };
    entry.commands.beginAttach(session.acpSessionId);
    let succeeded = false;
    try {
      let state;
      if (agent.supports("resume")) state = await agent.request("session/resume", params, { timeoutMs: 10_000 });
      else if (agent.info?.agentCapabilities?.loadSession) {
        run.replaying = true;
        try { state = await agent.request("session/load", params, { timeoutMs: 10_000 }); } finally { run.replaying = false; }
      } else throw new Error(`${this.label} cannot reopen this conversation. Start a new one.`);
      if (this.closing || this.agentEntry !== entry || !agent.alive) throw new Error("The agent connection changed. Send again to reconnect.");
      session.configuration = state;
      agent.attached.add(session.acpSessionId);
      this.driverHeld.set(session.id, driver);
      succeeded = true;
    } finally { entry.commands.endAttach(session.acpSessionId, succeeded); }
  }
  async configure(agent, run) {
    const { session, model, options } = run;
    const set = async (category, value) => {
      const state = session.configuration;
      const config = state?.configOptions?.find(option => option.category === category)
        ?? state?.configOptions?.find(option => option.id === category);
      const message = category === "model" ? `${this.label} could not switch to this model. Check the model in ${this.label} on your computer.`
        : value === "plan" ? `${this.label} could not enter Plan mode.` : `${this.label} could not leave Plan mode. Change the mode in ${this.label} on your computer.`;
      const request = async (method, params) => {
        try { return await agent.request(method, params, { timeoutMs: 10_000 }); }
        catch (error) {
          if (error.outcomeUnknown || !agent.alive) session.configuration = null;
          throw Object.assign(new Error(message, { cause: error }), { code: error.code, outcomeUnknown: error.outcomeUnknown });
        }
      };
      if (config?.type === "select") {
        const values = config.options?.flatMap(option => option.options ?? [option]) ?? [];
        if (!values.some(option => option.value === value)) throw new Error(message);
        if (config.currentValue === value) return;
        const changed = await request("session/set_config_option", { sessionId: session.acpSessionId, configId: config.id, value });
        session.configuration = changed;
        if (!changed?.configOptions?.some(option => option.id === config.id && option.currentValue === value)) throw new Error(message);
        return;
      }
      // Older agents expose modes separately. Only use that API when the
      // session has no configOptions; do not guess a mode or config ID.
      if (category === "mode" && !state?.configOptions && state?.modes?.availableModes?.some(mode => mode.id === value)) {
        if (state.modes.currentModeId === value) return;
        await request("session/set_mode", { sessionId: session.acpSessionId, modeId: value });
        session.configuration.modes.currentModeId = value;
        return;
      }
      if (category === "model" && !state?.configOptions && state?.models?.currentModelId === value) return;
      throw new Error(message);
    };
    for (const [category, value] of this.adapter.sessionSelections(model, options.wonderPolicy)) {
      if (run.stopped) break;
      await set(category, value);
    }
  }
  async execute(run) {
    const { session, projection } = run;
    projection.start();
    let stopReason = null, failure = null;
    try {
      if (run.stopped) return await this.finish(run, { stopReason: "cancelled" });
      const agent = await this.agent();
      run.agent = agent;
      if (run.stopped) return await this.finish(run, { stopReason: "cancelled" });
      // A restarted process must attach before accepting session options.
      await this.attach(agent, run);
      if (run.stopped) return await this.finish(run, { stopReason: "cancelled" });
      await this.configure(agent, run);
      if (run.stopped) return await this.finish(run, { stopReason: "cancelled" });
      const prompt = await promptBlocks(run.options.input, agent.info?.agentCapabilities?.promptCapabilities, this.adapter);
      if (run.stopped) stopReason = "cancelled";
      else {
        run.promptIssued = true;
        const response = await agent.request("session/prompt", { sessionId: session.acpSessionId, prompt }, {
          onTimeout: () => { run.uncertainUpdate = this.markUncertain(run); return run.uncertainUpdate; },
        });
        await run.uncertainUpdate;
        if (!["end_turn", "cancelled", "max_tokens", "max_turn_requests", "refusal"].includes(response?.stopReason))
          throw Object.assign(new Error(`${this.label} did not confirm the response outcome.`), { outcomeUnknown: true });
        stopReason = response.stopReason;
      }
    } catch (error) {
      // A timeout/cancel notification is not a completion. Losing the pipe is
      // equally uncertain until this exact child exits; keep peers running.
      if (!run.exited && (error.outcomeUnknown || (run.promptIssued && !Number.isInteger(error.code)) || (run.agent && !run.agent.alive))) {
        await this.markUncertain(run);
        if (run.exited) await this.finish(run, { error: new Error(`${this.label} stopped before confirming this response. You can send again.`) });
        return;
      }
      // Before submission, Stop owns cancellation of local setup. Once the
      // prompt was sent, a native error remains a failure even if Stop raced it.
      if (run.stopped && !run.promptIssued) stopReason = "cancelled";
      else failure = error;
      // A tool-limit refusal must not leave the thread failing every later turn: drop the held tools.
      if (TOOL_LIMIT_ERROR.test(String(error?.message ?? error?.data?.message ?? ""))) run.session.options = { ...run.session.options, offerComputer: false, computerHeld: false };
    }
    await this.finish(run, { stopReason, error: failure });
  }
  async markUncertain(run) {
    const { session, turn, projection } = run;
    run.uncertain = true;
    run.abort.abort();
    session.unresolvedTurnId = turn.id;
    turn.error = { message: this.sessions.uncertainResponse };
    turn.items = [turn.items[0], ...projection.items.values()];
    await this.sessions.save(session);
  }
  async finish(run, outcome) {
    if (run.finalizing) return run.finalizing;
    run.finalizing = (async () => {
      const { session, turn, projection } = run;
      run.abort.abort();
      const result = projection.finish(outcome);
      turn.status = result.status; turn.items = [turn.items[0], ...result.items]; turn.error = result.error;
      turn.firstTokenMs = projection.firstTokenAt && projection.startedAt ? projection.firstTokenAt - projection.startedAt : null;
      if (session.unresolvedTurnId === turn.id) delete session.unresolvedTurnId;
      session.updatedAt = Math.floor(Date.now() / 1000);
      // Persist before waiting on the consumer's output pipe, including close.
      await this.sessions.save(session);
      if (this.active.get(session.id) === run) { this.active.delete(session.id); this.tools?.changed(session.id); }
      // The credential's idle lifetime starts when the reply ends.
      this.tools?.tokens.touch(session.id);
      await run.output;
    })();
    return run.finalizing;
  }
  close() {
    if (this.closing) return this.closing;
    // Prevent new admission before awaiting any owned process or persistence.
    this.closing = (async () => {
      const runs = [...this.active.values()];
      for (const run of runs) { run.stopped = true; run.abort.abort(); }
      const entries = [...this.ownedAgents];
      for (const { timer, commands } of entries) { clearTimeout(timer); commands.clear(); }
      const exits = await Promise.all(entries.map(async ({ agent }) => {
        const exited = await agent.close();
        if (exited) this.agentExited(agent);
        return exited;
      }));
      // A blocked client output, catalog or storage must not hold shutdown
      // forever. Returning false keeps an incomplete close visible to main.
      const finished = await settleWithin(Promise.allSettled(runs.map(run => run.finished)), 1000);
      await settleWithin(this.tools?.close() ?? Promise.resolve(), 1000);
      for (const { client } of this.drivers.values()) client.stop();
      const flushed = await settleWithin(this.sessions.flush(), 1000);
      return exits.every(Boolean) && finished && flushed;
    })();
    return this.closing;
  }
}

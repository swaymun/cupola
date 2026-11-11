import { randomUUID } from "node:crypto";
import { isDeepStrictEqual } from "node:util";

// Ignore request-only fields such as excludeTurns on a no-op thread/resume.
const commandOptions = ({ cwd, model, effort, wonderProject, wonderPolicy, outputSchema, dynamicTools, config }) =>
  ({ cwd, model, effort, wonderProject, wonderPolicy, outputSchema, dynamicTools, config });

const text = (value, limit) => typeof value === "string" && value.length <= limit;
function snapshot(commands) {
  // Keep the whole native replacement or none of it. Names, aliases and builtin
  // precedence belong to Claude; a name-keyed map would change their meaning.
  if (!Array.isArray(commands) || commands.length > 512 || commands.some(row =>
    !row || typeof row !== "object" || Array.isArray(row)
    || !text(row.name, 256) || !row.name.length || !text(row.description, 16_384) || !text(row.argumentHint, 4096)
    || (row.builtin !== undefined && typeof row.builtin !== "boolean")
    || (row.aliases !== undefined && (!Array.isArray(row.aliases) || row.aliases.length > 128
      || row.aliases.some(alias => !text(alias, 256) || !alias.length))))) return null;
  try {
    const json = JSON.stringify(commands);
    if (Buffer.byteLength(json) <= 256 * 1024) return JSON.parse(json);
  } catch { /* Malformed native metadata cannot preserve an old catalog. */ }
  return null;
}

// One transient owner per retained Query. Never stored with a session, and never
// creates a Query or touches its idle clock. The bridge supplies exact ownership.
export class SessionCommands {
  constructor(session, handle, owns) {
    Object.assign(this, { session, handle, owns, ownerId: randomUUID(), revision: 0, generation: 0, pending: null });
    this.catalogSessionId = session.sdkSessionId;
    this.bind();
  }
  bind() {
    this.generation++;
    this.sessionId = this.session.sdkSessionId;
    this.options = structuredClone(commandOptions(this.session.options));
    this.shape = this.handle.shape;
    this.query = this.handle.query;
    this.commands = null; this.reason = null; this.readAllowed = this.catalogSessionId === this.sessionId;
    this.revision++;
  }
  optionsMatch() { return isDeepStrictEqual(this.options, commandOptions(this.session.options)); }
  owned() {
    return this.owns() && !this.handle.abort.signal.aborted && this.optionsMatch()
      && this.handle.shape === this.shape && this.handle.query === this.query;
  }
  current() {
    return this.owned() && !this.awaitingInit && this.session.sdkSessionId === this.sessionId;
  }
  // An owned SessionStart/reset result identifies native identity even when
  // Claude emits no init. The bridge keeps the durable old binding until ACK,
  // so current() still fences reads while a new native catalog is buffered.
  identifyResetSession(id) {
    if (!this.owned()) return;
    if (this.awaitingInit || this.sessionId !== id) {
      this.generation++; this.revision++; this.sessionId = id;
      this.commands = null; this.reason = null;
    }
    this.awaitingInit = false; this.readAllowed = false;
    this.catalogSessionId = this.commands === null ? null : id;
  }
  observe(message) {
    if (!this.owned() || message.parent_tool_use_id) return;
    if (message.type === "conversation_reset") {
      // new_conversation_id names a UI conversation, not the resumable SDK
      // session. Every native reset invalidates capability until a fresh init.
      this.awaitingInit = true; this.catalogSessionId = null;
      this.commands = null; this.reason = null; this.readAllowed = false;
      this.generation++; this.revision++;
      return;
    }
    if (message.type !== "system") return;
    if (message.subtype === "init" && text(message.session_id, 1024) && message.session_id.length
      && (this.awaitingInit || message.session_id !== this.sessionId)) {
      this.generation++;
      this.sessionId = message.session_id; this.commands = null; this.reason = null; this.revision++;
      this.awaitingInit = false;
      // /clear can change native identity on this Query. Its initialization
      // fallback may describe the old session; wait for an exact native push.
      this.readAllowed = false;
    } else if (!this.awaitingInit && message.subtype === "commands_changed" && message.session_id === this.sessionId) {
      this.catalogSessionId = message.session_id;
      this.replace(message.commands);
    }
  }
  replace(commands) {
    this.commands = snapshot(commands);
    this.reason = this.commands === null ? "invalidCatalog" : null;
    this.revision++;
    this.readAllowed = false;
  }
  value() {
    if (!this.current()) return unavailableCommands(this.session.sdkSessionId);
    return { availability: this.commands === null ? "unknown" : "ready", sessionId: this.sessionId,
      ownerId: this.ownerId, revision: this.revision, reason: this.reason, commands: structuredClone(this.commands) };
  }
  async read() {
    if (!this.current() || !this.readAllowed || typeof this.query?.supportedCommands !== "function") return this.value();
    if (!this.pending) {
      const revision = this.revision;
      const current = () => this.current() && this.revision === revision;
      const pending = Promise.resolve().then(() => current() ? this.query.supportedCommands() : undefined)
        .then(commands => { if (current()) this.replace(commands); }, () => {});
      this.pending = pending;
      // A timed-out caller must not multiply reads that the SDK has not settled.
      pending.finally(() => { if (this.pending === pending) this.pending = null; });
    }
    const generation = this.generation;
    let timer;
    try {
      await Promise.race([this.pending, new Promise(resolve => { timer = setTimeout(resolve, 250); })]);
      return generation === this.generation ? this.value() : unavailableCommands(this.session.sdkSessionId);
    } finally { clearTimeout(timer); }
  }
}

export const unavailableCommands = sessionId => ({ availability: "unavailable", sessionId,
  ownerId: null, revision: null, reason: null, commands: null });

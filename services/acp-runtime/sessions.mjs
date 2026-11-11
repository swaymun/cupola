import { mkdir, readFile, readdir, rename, stat, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { randomUUID } from "node:crypto";
import { isDeepStrictEqual } from "node:util";
import { FAMILY } from "./projection.mjs";
import { AcpNotSubmittedError } from "./agent.mjs";

export const UNCERTAIN_RESPONSE = "OpenCode has not confirmed that this response stopped. Check OpenCode on your computer; start a new conversation if it cannot be recovered.";

// Receipts outlive the 200-turn window so a late retry of an evicted turn is
// still recognized, but they must not grow with the conversation: every save
// rewrites them. Receipts of turns still in the journal are never dropped; of
// the rest the oldest go first. Turns past this window run again if resent.
export const RECEIPT_LIMIT = 1000;
export function trimReceipts(session) {
  const keys = Object.keys(session.receipts);
  let excess = keys.length - RECEIPT_LIMIT;
  if (excess <= 0) return;
  const live = new Set(session.turns.map(turn => turn.clientUserMessageId));
  for (const key of keys) {
    if (excess <= 0) break;
    if (live.has(key)) continue;
    delete session.receipts[key];
    excess -= 1;
  }
}

// Windows refuses to rename onto a file another handle has open (EPERM,
// measured), such as a reader or a virus scanner looking at the last save.
// Retry for a bounded time; a directory in the way still fails at once.
const RENAME_BUSY = new Set(["EPERM", "EBUSY", "EACCES"]);
async function replace(temporary, path) {
  for (let delay = 5, waited = 0; ; waited += delay, delay = Math.min(delay * 2, 100)) {
    try { return await rename(temporary, path); }
    catch (error) {
      if (process.platform !== "win32" || !RENAME_BUSY.has(error.code) || waited >= 2000
        || (await stat(path).catch(() => null))?.isDirectory()) throw error;
      await new Promise(resolve => setTimeout(resolve, delay));
    }
  }
}

async function writeJson(path, value) {
  const temporary = `${path}.${randomUUID()}.tmp`;
  await writeFile(temporary, `${JSON.stringify(value)}\n`, { mode: 0o600 });
  await replace(temporary, path);
}

// Wonder's daemon owns the visible transcript. This private journal
// binds each Wonder thread to its ACP session and keeps accepted-turn
// receipts so a resent message is never run twice.
export class Sessions {
  constructor(root, { family = FAMILY, label = "OpenCode" } = {}) {
    if (!/^[a-z]+$/.test(family)) throw new Error("Invalid ACP provider namespace.");
    Object.assign(this, { root, family, label, prefix: `${family}-` });
    this.uncertainResponse = `${label} has not confirmed that this response stopped. Check ${label} on your computer; start a new conversation if it cannot be recovered.`;
    this.uncertainCreate = `${label} has not confirmed whether this conversation was created. Check ${label} on your computer before starting a new one.`;
    this.values = new Map(); this.writes = new Map(); this.creations = new Map(); this.creating = new Map(); this.accepting = new Map();
  }
  validId(id) { return typeof id === "string" && id.startsWith(this.prefix) && /^[0-9a-f-]{36}$/.test(id.slice(this.prefix.length)); }
  async initialize() {
    await mkdir(this.root, { recursive: true, mode: 0o700 });
    for (const file of await readdir(this.root)) {
      if (!file.endsWith(".json") || !this.validId(file.slice(0, -5))) continue;
      const session = JSON.parse(await readFile(join(this.root, file), "utf8"));
      if (session.schemaVersion !== 1 || !this.validId(session.id) || !Array.isArray(session.turns)) throw new Error(`${this.label} session storage is incompatible`);
      session.receipts ??= {};
      let interrupted = false;
      for (const turn of session.turns) {
        if (turn.status === "inProgress") {
          session.unresolvedTurnId = turn.id;
          turn.error = { message: this.uncertainResponse };
          interrupted = true;
        }
      }
      trimReceipts(session);
      this.values.set(session.id, session);
      if (session.creation) this.creations.set(session.creation.requestId, session);
      if (interrupted) await this.save(session);
    }
    return this;
  }
  get(id) {
    const session = this.values.get(id);
    if (!session) throw new Error(`This ${this.label} conversation is unavailable. Its saved messages are still in Cupola.`);
    return session;
  }
  async create(options, createRemote) {
    const requestId = options.requestId;
    if (typeof requestId !== "string" || !requestId || requestId.length > 128) throw new Error(`A saved conversation request is required before starting ${this.label}.`);
    const prior = this.creations.get(requestId);
    if (prior) {
      if (!isDeepStrictEqual(prior.creation.options, options)) throw new Error("This conversation was already requested with different settings.");
      if (this.creating.has(requestId)) await this.creating.get(requestId);
      if (prior.acpSessionId) return { session: prior, duplicate: true };
      if (prior.creation.retryable !== true) throw new Error(this.uncertainCreate);
    }
    const now = Math.floor(Date.now() / 1000);
    const session = prior ?? { schemaVersion: 1, id: `${this.prefix}${randomUUID()}`, acpSessionId: null, createdAt: now, updatedAt: now,
      options, creation: { requestId, options: structuredClone(options) }, turns: [], receipts: {} };
    // Reserve synchronously and persist uncertainty before every external
    // create, including a retry: ACP has no idempotency key for session/new.
    this.creations.set(requestId, session);
    this.values.set(session.id, session);
    const creating = (async () => {
      delete session.creation.retryable;
      try { await this.save(session); }
      catch (error) { session.creation.retryable = true; throw error; }
      let created;
      try { created = await createRemote(session); }
      catch (error) {
        if (error instanceof AcpNotSubmittedError) {
          session.creation.retryable = true;
          await this.save(session);
        }
        throw error;
      }
      if (typeof created?.sessionId !== "string" || !created.sessionId) throw new Error(this.uncertainCreate);
      session.acpSessionId = created.sessionId;
      session.configuration = created;
      try { await this.save(session); } catch (error) { session.acpSessionId = null; throw error; }
    })();
    this.creating.set(requestId, creating);
    try { await creating; } finally { this.creating.delete(requestId); }
    return { session, duplicate: false };
  }
  async save(session, admission = null) {
    if (!this.validId(session.id)) throw new Error(`Invalid ${this.label} conversation ID`);
    for (const turn of session.turns) if (turn.clientUserMessageId) session.receipts[turn.clientUserMessageId] = { id: turn.id, status: turn.status };
    trimReceipts(session);
    const snapshot = structuredClone(session);
    const admissions = [...this.accepting.values()].filter(pending => pending.session === session);
    if (admission) snapshot.updatedAt = Math.max(snapshot.updatedAt ?? session.createdAt, admission.updatedAt);
    const previous = this.writes.get(session.id) ?? Promise.resolve();
    const next = previous.catch(() => {}).then(async () => {
      // A queued snapshot must not resurrect an admission whose own write
      // failed. Keep these objects even after accepting releases the promise.
      for (const pending of admissions) if (pending.failed) {
        snapshot.turns = snapshot.turns.filter(turn => turn.id !== pending.turn.id);
        const clientId = pending.turn.clientUserMessageId;
        if (clientId && snapshot.receipts[clientId]?.id === pending.turn.id) delete snapshot.receipts[clientId];
      }
      if (admissions.length) {
        snapshot.turns = snapshot.turns.slice(-200);
        snapshot.updatedAt = Math.max(snapshot.updatedAt ?? session.createdAt, session.updatedAt ?? session.createdAt);
      }
      try { await writeJson(join(this.root, `${session.id}.json`), snapshot); }
      catch (error) {
        // Undo only this admission before the next queued writer proceeds.
        admission?.rollback();
        throw error;
      }
    });
    this.writes.set(session.id, next);
    try { await next; } finally { if (this.writes.get(session.id) === next) this.writes.delete(session.id); }
  }
  async flush() { await Promise.all([...this.writes.values()]); }
  describe(session, includeTurns = false) {
    const active = Boolean(session.unresolvedTurnId) || session.turns.at(-1)?.status === "inProgress";
    return { id: session.id, sessionId: session.acpSessionId, agentFamily: this.family, cwd: session.options.cwd,
      createdAt: session.createdAt, updatedAt: session.updatedAt ?? session.createdAt,
      status: { type: active ? "active" : "idle", activeFlags: [] }, canAcceptDirectInput: Boolean(session.acpSessionId) && !active,
      modelProvider: this.family, source: "sdk", turns: includeTurns ? session.turns : [] };
  }
  async accept(session, params) {
    const previous = params.clientUserMessageId && session.turns.find(t => t.clientUserMessageId === params.clientUserMessageId);
    if (previous) {
      await this.accepting.get(previous.id)?.promise;
      return { turn: previous, duplicate: true };
    }
    const receipt = params.clientUserMessageId && session.receipts[params.clientUserMessageId];
    if (receipt) return { turn: { ...receipt, items: [] }, duplicate: true };
    if (session.unresolvedTurnId) throw new Error(this.uncertainResponse);
    if (!session.acpSessionId) throw new Error(this.uncertainCreate);
    if (session.turns.some(t => t.status === "inProgress")) throw new Error(`${this.label} is already working in this conversation.`);
    const turn = { id: randomUUID(), clientUserMessageId: params.clientUserMessageId ?? null, status: "inProgress", items: [{
      type: "userMessage", id: randomUUID(), clientId: params.clientUserMessageId ?? null, content: params.input ?? [] }], error: null };
    session.turns.push(turn);
    const admission = { session, turn, updatedAt: Math.floor(Date.now() / 1000), failed: false,
      rollback: () => {
        admission.failed = true;
        const index = session.turns.indexOf(turn);
        if (index !== -1) session.turns.splice(index, 1);
        if (turn.clientUserMessageId && session.receipts[turn.clientUserMessageId]?.id === turn.id)
          delete session.receipts[turn.clientUserMessageId];
      } };
    this.accepting.set(turn.id, admission);
    try {
      admission.promise = this.save(session, admission);
      await admission.promise;
      session.updatedAt = Math.max(session.updatedAt ?? session.createdAt, admission.updatedAt);
      return { turn, duplicate: false };
    } catch (error) {
      admission.rollback();
      throw error;
    } finally {
      this.accepting.delete(turn.id);
      // Defer eviction so a failed admission cannot discard valid history.
      if (session.turns.length > 200) session.turns.splice(0, session.turns.length - 200);
    }
  }
}

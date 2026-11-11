import { mkdir, open, readFile, readdir, stat } from "node:fs/promises";
import { join } from "node:path";
import { randomUUID } from "node:crypto";
import { retainedBytes } from "./cooperative.mjs";
import { ReadWorkers } from "./read-workers.mjs";
import { writeJson } from "./runtime-updates.mjs";

// Rejections are typed at admission; display wording never decides retry safety.
export const RUNNING_REFUSAL = "This conversation already has an active turn. Your message waits until Claude finishes.";
export const STALE_TURN = "Claude stopped before this response completed. Continue in the conversation.";

// Construct only before accept() changes the journal or submits native input.
export class InputRefusal extends Error {
  constructor(message, { action = "retry", method = "turn/start" } = {}) {
    super(message); this.action = action; this.method = method;
  }
}

export function resetRefusal(session, method = "turn/start") {
  const unsupported = session.pendingReset?.unsupported || session.pendingReset?.invalid || session.pendingReset?.unverified;
  return new InputRefusal(unsupported
    ? "Claude reset this conversation in a way Cupola cannot continue yet. Open the new conversation in Claude on your computer."
    : "Claude is resetting this conversation. Your message waits until the reset is saved.",
  { action: unsupported ? "retry" : "wait", method });
}

const validId = id => typeof id === "string" && /^claude-[0-9a-f-]{36}$/.test(id);

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

// The top-level `parent` value at the head of journal text: null or an object
// when the text settles it, undefined when more text is needed or `turns`
// comes first. Strings are skipped whole, so a quoted key inside options or a
// prompt cannot match.
export function headParent(text) {
  const stringEnd = start => {
    for (let i = start + 1; i < text.length; i++) {
      if (text[i] === "\\") i++;
      else if (text[i] === '"') return i;
    }
    return -1;
  };
  let depth = 0, expectKey = false, key = null;
  for (let i = 0; i < text.length; i++) {
    const c = text[i];
    if (c === '"') {
      const end = stringEnd(i);
      if (end < 0) return undefined;
      if (depth === 1 && expectKey) { key = JSON.parse(text.slice(i, end + 1)); expectKey = false; }
      i = end;
    } else if (c === ":" && depth === 1 && key !== null) {
      if (key === "turns") return undefined;
      if (key !== "parent") { key = null; continue; }
      let start = i + 1;
      while (/\s/.test(text[start] ?? "")) start++;
      if (text.startsWith("null", start)) return null;
      if (text[start] !== "{") return undefined;
      let level = 0;
      for (let j = start; j < text.length; j++) {
        if (text[j] === '"') { j = stringEnd(j); if (j < 0) return undefined; }
        else if (text[j] === "{" || text[j] === "[") level++;
        else if ((text[j] === "}" || text[j] === "]") && --level === 0) {
          const parent = JSON.parse(text.slice(start, j + 1));
          return typeof parent?.threadId === "string" ? parent : undefined;
        }
      }
      return undefined;
    } else if (c === "{" || c === "[") { depth++; if (depth === 1 && c === "{") expectKey = true; }
    else if (c === "}" || c === "]") depth--;
    else if (c === "," && depth === 1) expectKey = true;
  }
  return undefined;
}

// Reads a growing head, up to 1 MiB: options (and their prompt) come first.
async function readHeadParent(path) {
  const file = await open(path, "r");
  try {
    for (let size = 16 * 1024; size <= 1024 * 1024; size *= 4) {
      const head = Buffer.alloc(size);
      const { bytesRead } = await file.read(head, 0, size, 0);
      const parent = headParent(head.toString("utf8", 0, bytesRead));
      if (parent !== undefined || bytesRead < size) return parent;
    }
    return undefined;
  } finally { await file.close(); }
}

// The daemon owns the visible transcript. This bounded private journal owns
// SDK session bindings and accepted-turn receipts for safe crash recovery.
export class Sessions {
  constructor(root, { maxBytes = 16 * 1024 * 1024 } = {}) {
    Object.assign(this, { root, maxBytes, cacheBytes: 0, values: new Map(), ids: new Set(), loading: new Map(),
      charges: new Map(), retained: new Map(), writes: new Map(), accepting: new Map(), readers: new ReadWorkers(), pinned: () => false,
      // Each loaded object of an id gets the next generation, kept across
      // eviction: an object a caller held past its eviction cannot save over
      // the journal a newer object of the same id wrote meanwhile.
      generations: new Map(), owners: new WeakMap(),
      // Native session ID -> journal IDs. A Mac can keep hundreds of journals,
      // far more than the cache, so a read of a few native sessions must not
      // load every one of them (270 journals took 0.4 s, past the 250 ms read).
      byNative: new Map(), nativeOf: new Map(), nativeIndex: null,
      // Parent thread ID -> child journal IDs, so a chat's agents are found
      // without loading every journal (269 journals took 1.5 s per listing).
      // A child's parent never changes after create.
      children: new Map(), parentOf: new Map(), parentIndex: null });
  }
  noteParent(id, parentThreadId) {
    if (this.parentOf.has(id)) return;
    this.parentOf.set(id, parentThreadId);
    if (parentThreadId == null) return;
    if (!this.children.has(parentThreadId)) this.children.set(parentThreadId, new Set());
    this.children.get(parentThreadId).add(id);
  }
  note(session) {
    this.noteParent(session.id, session.parent?.threadId ?? null);
    const previous = this.nativeOf.get(session.id);
    if (this.nativeOf.has(session.id) && previous === session.sdkSessionId) return;
    this.byNative.get(previous)?.delete(session.id);
    this.nativeOf.set(session.id, session.sdkSessionId);
    if (!this.byNative.has(session.sdkSessionId)) this.byNative.set(session.sdkSessionId, new Set());
    this.byNative.get(session.sdkSessionId).add(session.id);
  }
  // Every journal starts with `{"schemaVersion":1,"id":…,"sdkSessionId":…`
  // (create's key order, kept by every save), so its head names it. Another
  // head is left unindexed and stays a candidate of every native read.
  indexNative() {
    return this.nativeIndex ??= (async () => {
      const head = Buffer.alloc(160);
      for (const id of [...this.ids]) {
        if (this.nativeOf.has(id)) continue;
        let text = "";
        try {
          const file = await open(join(this.root, `${id}.json`), "r");
          try { const { bytesRead } = await file.read(head, 0, head.length, 0); text = head.toString("utf8", 0, bytesRead); }
          finally { await file.close(); }
        } catch { continue; }
        const match = /^\{"schemaVersion":1,"id":"([^"]+)","sdkSessionId":"([^"]+)"/.exec(text);
        if (match?.[1] === id && !this.nativeOf.has(id)) this.note({ id, sdkSessionId: match[2] });
      }
    })();
  }
  // create writes `parent` before `turns` and every save keeps that key order,
  // so a journal's head names its parent without reading its turns. A head
  // that does not settle it is left unindexed and stays a candidate of every
  // child read.
  indexParents() {
    return this.parentIndex ??= (async () => {
      for (const id of [...this.ids]) {
        if (this.parentOf.has(id)) continue;
        let parent;
        try { parent = await readHeadParent(join(this.root, `${id}.json`)); } catch { continue; }
        if (parent !== undefined && !this.parentOf.has(id)) this.noteParent(id, parent?.threadId ?? null);
      }
    })();
  }
  // The journals that may be children of this thread, in `all()` order.
  // Callers still check each session's parent.
  async *childrenOf(threadId) {
    await this.indexParents();
    for (const id of this.ids) {
      if (this.parentOf.has(id) && this.parentOf.get(id) !== threadId) continue;
      const release = this.retain(id);
      try { yield await this.load(id); } finally { release(); }
    }
  }
  // The journals that may belong to these native sessions, in `all()` order:
  // the indexed ones, unindexed ones and every loaded object (its native ID
  // can change before its next save). Callers still check each session.
  async *withNative(nativeIds) {
    await this.indexNative();
    const candidates = new Set(this.values.keys());
    for (const native of nativeIds) for (const id of this.byNative.get(native) ?? []) candidates.add(id);
    for (const id of this.ids) {
      if (!candidates.has(id) && this.nativeOf.has(id)) continue;
      const release = this.retain(id);
      try { yield await this.load(id); } finally { release(); }
    }
  }
  adopt(session) {
    const generation = (this.generations.get(session.id) ?? 0) + 1;
    this.generations.set(session.id, generation); this.owners.set(session, generation);
  }
  async initialize() {
    await mkdir(this.root, { recursive: true, mode: 0o700 });
    // Discovery needs filenames, not every turn and tool result ever saved.
    for (const file of await readdir(this.root))
      if (file.endsWith(".json") && validId(file.slice(0, -5))) this.ids.add(file.slice(0, -5));
    return this;
  }
  retain(id) {
    this.retained.set(id, (this.retained.get(id) ?? 0) + 1);
    return () => {
      const count = this.retained.get(id) - 1;
      if (count) this.retained.set(id, count); else this.retained.delete(id);
      this.evict();
    };
  }
  async using(id, action) {
    const release = this.retain(id);
    try { return await action(await this.load(id)); } finally { release(); }
  }
  async *all() {
    for (const id of this.ids) {
      const release = this.retain(id);
      try { yield await this.load(id); } finally { release(); }
    }
  }
  async findId(predicate, nativeIds = null) {
    for await (const session of nativeIds ? this.withNative(nativeIds) : this.all()) if (predicate(session)) return session.id;
    return null;
  }
  evict(keep = null) {
    for (const [id, session] of this.values) {
      if (this.cacheBytes <= this.maxBytes) break;
      if (id === keep || this.retained.has(id) || this.loading.has(id) || this.writes.has(id)
        || session.pendingReset || session.turns.some(t => t.status === "inProgress")
        || [...this.accepting.values()].some(p => p.session === session) || this.pinned(session)) continue;
      this.values.delete(id); this.cacheBytes -= this.charges.get(id) ?? 0; this.charges.delete(id);
    }
  }
  async charge(session) {
    const bytes = await retainedBytes(session);
    this.cacheBytes += bytes - (this.charges.get(session.id) ?? 0); this.charges.set(session.id, bytes);
  }
  async load(id) {
    if (!validId(id) || !this.ids.has(id)) throw new Error("This Claude conversation is unavailable. Its saved messages are still in Cupola.");
    if (this.loading.has(id)) return this.loading.get(id);
    if (this.values.has(id)) {
      const session = this.values.get(id); this.values.delete(id); this.values.set(id, session); return session;
    }
    const read = this.readSession(id); this.loading.set(id, read);
    try { return await read; }
    finally { if (this.loading.get(id) === read) this.loading.delete(id); this.evict(id); }
  }
  async readSession(id) {
    const path = join(this.root, `${id}.json`), info = await stat(path);
    const session = info.size > 1024 * 1024 ? await this.readers.json(path) : JSON.parse(await readFile(path, "utf8"));
    if (session.schemaVersion !== 1 || session.id !== id || !Array.isArray(session.turns)) throw new Error("Claude session storage is incompatible");
    session.receipts ??= {};
    let interrupted = false;
    if (session.pendingReset && (!session.pendingReset.observed || !session.pendingReset.newSessionId)) {
      session.pendingReset.unverified = true; interrupted = true;
    }
    for (const turn of session.turns) {
      if (turn.status === "inProgress") {
        turn.status = "interrupted"; turn.error = { message: STALE_TURN }; interrupted = true;
      }
      if (turn.clientUserMessageId) session.receipts[turn.clientUserMessageId] = { id: turn.id, status: turn.status };
    }
    trimReceipts(session);
    this.adopt(session); this.note(session); this.values.set(id, session); await this.charge(session);
    try { if (interrupted) await this.save(session); }
    catch (error) { this.values.delete(id); this.cacheBytes -= this.charges.get(id) ?? 0; this.charges.delete(id); throw error; }
    return session;
  }
  get(id) {
    const session = this.values.get(id);
    if (!session) throw new Error("This Claude conversation is unavailable. Its saved messages are still in Cupola.");
    return session;
  }
  async create(options, parent = null, native = null) {
    // Attaching an existing native session keeps its exact provider UUID.
    const session = { schemaVersion: 1, id: `claude-${randomUUID()}`, sdkSessionId: native?.sdkSessionId ?? randomUUID(),
      sdkStarted: native?.sdkStarted === true, createdAt: Math.floor(Date.now() / 1000), options, parent, turns: [], receipts: {} };
    this.adopt(session);
    await this.save(session);
    this.ids.add(session.id); this.values.set(session.id, session); await this.charge(session); this.evict(session.id);
    return session;
  }
  async save(session, admission = null, owner = session) {
    if (!validId(session.id)) throw new Error("Invalid Claude conversation ID");
    const generation = this.owners.get(owner);
    if ((this.values.has(session.id) && this.values.get(session.id) !== owner)
      || (generation !== undefined && generation !== this.generations.get(session.id)))
      throw new Error("Claude session ownership changed. Reopen the conversation.");
    for (const turn of session.turns) if (turn.clientUserMessageId) session.receipts[turn.clientUserMessageId] = { id: turn.id, status: turn.status };
    trimReceipts(session);
    this.note(session);
    const snapshot = structuredClone(session);
    const admissions = [...this.accepting.values()].filter(pending => pending.session === owner);
    if (admission) snapshot.updatedAt = Math.max(snapshot.updatedAt ?? session.createdAt, admission.updatedAt);
    const previous = this.writes.get(session.id) ?? Promise.resolve();
    const next = previous.catch(() => {}).then(async () => {
      // A snapshot queued during admission must not resurrect a turn whose
      // own write failed. Keep the captured admission objects after map cleanup.
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
        // Roll back before the next queued writer can use its snapshot.
        admission?.rollback();
        throw error;
      }
    });
    this.writes.set(session.id, next);
    // An evicted owner still saves its own generation, but holds no charge.
    try { await next; if (this.values.get(session.id) === owner) await this.charge(session); }
    finally { if (this.writes.get(session.id) === next) this.writes.delete(session.id); this.evict(session.id); }
  }
  async saveSnapshot(owner, snapshot) { return this.save(snapshot, null, owner); }
  describe(session, includeTurns = false) {
    const active = session.turns.at(-1)?.status === "inProgress";
    return { id: session.id, sessionId: session.sdkSessionId, agentFamily: "claude", cwd: session.options.cwd,
      createdAt: session.createdAt, updatedAt: session.updatedAt ?? session.createdAt,
      status: { type: active ? "active" : "idle", activeFlags: [] },
      canAcceptDirectInput: !session.parent, modelProvider: "anthropic",
      ...(session.parent ? { parentThreadId: session.parent.threadId, source: { subAgent: { thread_spawn: {
        parent_thread_id: session.parent.threadId, depth: session.parent.depth,
        agent_nickname: session.parent.name, agent_role: session.parent.role } } } } : { source: "sdk" }),
      turns: includeTurns ? session.turns : [] };
  }
  // `steering`: a Guide message becomes its own turn while the reply it cuts
  // short is still finishing.
  // `checkpointed`: the Claude process taking it keeps file backups, so Edit
  // from here can restore the files this turn and later ones edited.
  async accept(session, params, { steering = false, checkpointed = false } = {}) {
    if (session.parent) throw new Error("Continue child-agent work in its parent conversation.");
    const previous = params.clientUserMessageId && session.turns.find(t => t.clientUserMessageId === params.clientUserMessageId);
    if (previous) {
      await this.accepting.get(previous.id)?.promise;
      return { turn: previous, duplicate: true };
    }
    const receipt = params.clientUserMessageId && session.receipts[params.clientUserMessageId];
    if (receipt) return { turn: { ...receipt, items: [] }, duplicate: true };
    if (session.pendingReset) throw resetRefusal(session);
    // Refused before anything is written; the daemon keeps the message queued.
    if (!steering && session.turns.some(t => t.status === "inProgress")) throw new InputRefusal(RUNNING_REFUSAL, { action: "wait" });
    const turn = { id: randomUUID(), clientUserMessageId: params.clientUserMessageId ?? null, status: "inProgress", items: [{
      type: "userMessage", id: randomUUID(), clientId: params.clientUserMessageId ?? null, content: params.input ?? [] }], error: null,
      ...(checkpointed ? { checkpointed: true } : {}) };
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
      // Keep valid history until all admissions settle; a failed Guide must
      // not evict an older turn. Every persisted snapshot remains bounded.
      if (![...this.accepting.values()].some(pending => pending.session === session) && session.turns.length > 200)
        session.turns.splice(0, session.turns.length - 200);
    }
  }
}

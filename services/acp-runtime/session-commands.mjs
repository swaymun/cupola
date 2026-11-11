// Ephemeral ACP capability state for one owned peer. No provider launch,
// model, mode, invocation or durable-session policy belongs here.
import { randomUUID } from "node:crypto";

const MAX_CATALOG_BYTES = 256 * 1024;
const MAX_CATALOGS = 64;
const MAX_PENDING = 32;
const validSessionId = id => typeof id === "string" && id.length > 0 && id.length <= 1024;
const text = (value, limit) => typeof value === "string" && value.length <= limit;
const object = value => value !== null && typeof value === "object" && !Array.isArray(value);

function snapshot(commands, revision) {
  // Reject the entire replacement, including a formerly valid catalog. A
  // truncated list or an old row left behind would advertise false capability.
  if (!Array.isArray(commands) || commands.length > 512 || commands.some(row =>
    !object(row) || !text(row.name, 256) || !row.name.length || !text(row.description, 16_384)
    || (row.input != null && (!object(row.input) || !text(row.input.hint, 4096))))) return { revision, commands: null };
  try {
    const json = JSON.stringify(commands);
    if (Buffer.byteLength(json) <= MAX_CATALOG_BYTES) return { revision, commands: JSON.parse(json) };
  } catch { /* An invalid native value cannot retain the previous catalog. */ }
  return { revision, commands: null };
}

function replace(map, key, value, limit) {
  map.delete(key);
  map.set(key, value);
  if (map.size > limit) map.delete(map.keys().next().value);
}

export class SessionCommands {
  constructor(attached) {
    this.attached = attached;
    this.peerId = randomUUID();
    this.revision = 0;
    this.catalogs = new Map();
    this.creating = new Set();
    this.pending = new Map();
    this.attaching = new Map();
  }
  beginCreate() {
    if (this.creating.size >= MAX_PENDING) throw new Error("Too many sessions are being opened. Wait for them to finish.");
    const token = Symbol();
    this.creating.add(token);
    return token;
  }
  endCreate(token, sessionId) {
    if (!this.creating.delete(token)) return;
    const pending = this.pending.get(sessionId);
    if (validSessionId(sessionId) && pending?.owners.has(token)) {
      replace(this.catalogs, sessionId, pending.snapshot, MAX_CATALOGS);
      this.pending.delete(sessionId);
    }
    // An unknown notification is eligible only for creates already outstanding
    // when it arrived. Concurrent receipts bind by exact native ID, never order.
    for (const [id, value] of this.pending) {
      value.owners.delete(token);
      if (!value.owners.size) this.pending.delete(id);
    }
  }
  beginAttach(sessionId) {
    if (this.attaching.size >= MAX_PENDING) throw new Error("Too many sessions are being opened. Wait for them to finish.");
    this.catalogs.delete(sessionId);
    this.attaching.set(sessionId, null);
  }
  endAttach(sessionId, succeeded) {
    const value = this.attaching.get(sessionId);
    this.attaching.delete(sessionId);
    if (succeeded && value) replace(this.catalogs, sessionId, value, MAX_CATALOGS);
  }
  update(sessionId, commands) {
    if (!validSessionId(sessionId)) return;
    if (this.attaching.has(sessionId)) this.attaching.set(sessionId, snapshot(commands, ++this.revision));
    else if (this.attached.has(sessionId)) replace(this.catalogs, sessionId, snapshot(commands, ++this.revision), MAX_CATALOGS);
    else if (this.creating.size) replace(this.pending, sessionId, {
      snapshot: snapshot(commands, ++this.revision), owners: new Set(this.creating),
    }, MAX_PENDING);
  }
  read(sessionId) {
    const value = this.catalogs.get(sessionId);
    return { availability: value?.commands ? "ready" : "unknown", peerId: this.peerId, sessionId,
      revision: value?.revision ?? this.revision, reason: value?.commands === null ? "invalidCatalog" : null,
      commands: value?.commands ? structuredClone(value.commands) : null };
  }
  clear() {
    this.catalogs.clear(); this.creating.clear(); this.pending.clear(); this.attaching.clear();
  }
}

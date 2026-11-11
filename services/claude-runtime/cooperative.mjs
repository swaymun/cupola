import { createHash } from "node:crypto";
import { setImmediate as yieldLoop } from "node:timers/promises";

// These are retained-object budgets, not a promise about V8's RSS or active
// request working sets. Callers charge shared strings conservatively twice.
// `oversizeMs`: an entry larger than the whole budget is normally refused. With
// a hold, it replaces every other entry and expires after that many idle
// milliseconds, so one large read serves its follow-up pages without the cache
// retaining it indefinitely.
export class ByteCache extends Map {
  constructor(maxBytes, maxEntries = Infinity, { oversizeMs = 0 } = {}) {
    super(); Object.assign(this, { maxBytes, maxEntries, oversizeMs, bytes: 0, charges: new Map(), holds: new Map() });
  }
  set(key, value, bytes) {
    if (!Number.isFinite(bytes) || bytes < 0) throw new Error("A cache entry needs a byte charge");
    this.delete(key);
    if (bytes > this.maxBytes) {
      if (!(this.oversizeMs > 0)) return this;
      this.clear();
      const hold = setTimeout(() => { if (this.holds.get(key) === hold) this.delete(key); }, this.oversizeMs);
      hold.unref?.(); this.holds.set(key, hold);
    }
    super.set(key, value); this.charges.set(key, bytes); this.bytes += bytes;
    while ((this.bytes > this.maxBytes && this.size > 1) || this.size > this.maxEntries) this.delete(this.keys().next().value);
    return this;
  }
  delete(key) {
    this.bytes -= this.charges.get(key) ?? 0; this.charges.delete(key);
    clearTimeout(this.holds.get(key)); this.holds.delete(key);
    return super.delete(key);
  }
  clear() { for (const hold of this.holds.values()) clearTimeout(hold); this.holds.clear(); super.clear(); this.charges.clear(); this.bytes = 0; }
}

export async function drainSteps(steps) {
  let slice = performance.now(), next;
  while (!(next = steps.next()).done) {
    if (performance.now() - slice >= 4) { await yieldLoop(); slice = performance.now(); }
  }
  return next.value;
}

function* byteSteps(value) {
  const pending = [value], seen = new Set(); let bytes = 0;
  while (pending.length) {
    const item = pending.pop();
    if (typeof item === "string") bytes += item.length * 2 + 24;
    else if (item && typeof item === "object" && !seen.has(item)) {
      seen.add(item); bytes += 64;
      if (item instanceof Map) for (const [key, value] of item) { pending.push(key, value); bytes += 32; }
      else for (const key of Object.keys(item)) { bytes += key.length * 2 + 16; pending.push(item[key]); }
    } else bytes += 8;
    yield;
  }
  return bytes;
}
export const retainedBytes = value => drainSteps(byteSteps(value));

// Hash the exact JSON representation without constructing a second transcript
// string. Chunk strings without splitting a surrogate pair; cursor identities
// stay byte-for-byte compatible with JSON.stringify(data).
function* jsonTokens(value, key = "", ancestors = new Set()) {
  if (value && typeof value.toJSON === "function") value = value.toJSON(key);
  if (typeof value === "string") {
    yield '"';
    for (let at = 0; at < value.length;) {
      let end = Math.min(at + 16 * 1024, value.length);
      const last = value.charCodeAt(end - 1), next = value.charCodeAt(end);
      if (last >= 0xd800 && last <= 0xdbff && next >= 0xdc00 && next <= 0xdfff) end--;
      yield JSON.stringify(value.slice(at, end)).slice(1, -1); at = end;
    }
    yield '"'; return;
  }
  if (!value || typeof value !== "object") { yield JSON.stringify(value) ?? "null"; return; }
  if (ancestors.has(value)) throw new TypeError("Cannot fingerprint circular history");
  ancestors.add(value);
  if (Array.isArray(value)) {
    yield "[";
    for (let i = 0; i < value.length; i++) { if (i) yield ","; yield* jsonTokens(value[i], String(i), ancestors); }
    yield "]";
  } else {
    yield "{"; let first = true;
    for (const name of Object.keys(value)) {
      if (["undefined", "function", "symbol"].includes(typeof value[name])) continue;
      if (!first) yield ","; first = false;
      yield JSON.stringify(name); yield ":"; yield* jsonTokens(value[name], name, ancestors);
    }
    yield "}";
  }
  ancestors.delete(value);
}
export async function historyFingerprint(value) {
  const hash = createHash("sha256"); let slice = performance.now();
  for (const token of jsonTokens(value)) {
    hash.update(token);
    if (performance.now() - slice >= 4) { await yieldLoop(); slice = performance.now(); }
  }
  return hash.digest("hex").slice(0, 16);
}

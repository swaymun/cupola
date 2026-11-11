// Claude Code's own session files, read-only. The SDK's message reader starts
// at the latest compaction, so turns before it (and the agent tasks launched
// there) would vanish from Wonder. This reader keeps the earlier chain and the
// per-task metadata Claude Code stores beside the transcript.
import { lstat, open, readdir, readFile } from "node:fs/promises";
import { homedir } from "node:os";
import { isAbsolute, join } from "node:path";
import { createHash, randomUUID } from "node:crypto";
import { setImmediate as yieldLoop } from "node:timers/promises";
import { ByteCache, retainedBytes } from "./cooperative.mjs";
import { ReadWorkers } from "./read-workers.mjs";
import { fileEditResult, claudeToolItem } from "./projection.mjs";
import { claudeSessionRecords, metaMessage, openNoFollow, sessionSummary } from "./project-history.mjs";
import { SessionHeadIndex } from "./session-head-index.mjs";

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
const MAX_SESSION_BYTES = 512 * 1024 * 1024;
const CACHED_FILES = 4;
const MAX_TASK_FILES = 200;
const NO_TASKS = Object.freeze([]);
const TASK_READ_MS = 1000;
const CATALOG_READ_MS = 1000;
// A request that waited its whole budget for a read slot still gives a read
// that starts at the deadline this long to answer (reads take milliseconds).
const CATALOG_LATE_START_MS = 100;
// Match the daemon's native catalog cursor lifetime. Retain bounded first
// pages too: refreshes can show completed rows while native metadata revalidates.
const CATALOG_RESUME_MS = 15 * 60 * 1000;
const CATALOG_PAGE_SIZE = 100;
// Completed first pages (at most 100 small rows each), one per Project and
// page size: enough that a refresh of every Project does not evict its own.
const CATALOG_CACHED_PAGES = 32;
const CATALOG_MAX_READS = 64;
// Configured roots plus linked worktrees; the time/read budgets above still
// bound each page. Match the daemon's bounded native-cursor envelope.
const CATALOG_MAX_FOLDERS = 512;
const CATALOG_MAX_CURSOR = 256 * 1024;
// A transcript record up to this size parses on the event loop (a few ms per
// MiB). A larger one, near the limits, is indexed by the reader process.
const INLINE_RECORD_BYTES = 4 * 1024 * 1024;
// The newest settings and message are near the end of a transcript; look
// back this far before indexing the whole file.
const TAIL_SCAN_BYTES = 4 * 1024 * 1024;
// A transcript index costs about this much per row besides its strings.
const ENTRY_BYTES = 160;

// Claude Code names a project folder after its path with every other character replaced.
export const projectFolderName = cwd => cwd.replace(/[^a-zA-Z0-9]/g, "-");

// Interactive entrypoints started on the computer; undefined is unknown.
const startedOnComputer = entrypoint => ["cli", "claude-desktop"].includes(entrypoint) ? true : entrypoint === undefined ? undefined : false;

export class ClaudeSessionFiles {
  // An added account's sessions live in its own Claude Code folder.
  constructor(root = join(process.env.CLAUDE_CONFIG_DIR || join(homedir(), ".claude"), "projects"), withSdk = null) {
    Object.assign(this, { root, withSdk, paths: new ByteCache(1024 * 1024, 2000), files: new ByteCache(24 * 1024 * 1024, CACHED_FILES, { oversizeMs: 30_000 }), tails: new ByteCache(256 * 1024, 64), helperFiles: new ByteCache(4 * 1024 * 1024, MAX_TASK_FILES), taskCaches: new ByteCache(4 * 1024 * 1024, CACHED_FILES), reading: new Map(), taskReads: new Map(), catalogReads: new Map(), catalogPages: new ByteCache(4 * 1024 * 1024, CATALOG_CACHED_PAGES), readers: new ReadWorkers(), catalogWaiters: new Set(), heads: new SessionHeadIndex(root, projectFolderName) });
  }

  // Keep the native offset of each folder, not an offset into a truncated
  // merged prefix. The daemon keeps these immutable cursors and deduplicates
  // native IDs across pages. Only unfinished reads retain their native heads,
  // briefly, so a slow folder can finish without restarting earlier folders.
  async listSessions(params, budget = performance.now() + CATALOG_READ_MS) {
    const dirs = Array.isArray(params.dirs) ? [...new Set(params.dirs)].sort() : [];
    if (!dirs.length || dirs.length > CATALOG_MAX_FOLDERS || dirs.some(d => typeof d !== "string" || !isAbsolute(d)))
      throw new Error("Choose project folders to list.");
    if (params.offset != null) throw new Error("Use the Claude session cursor to continue the list.");
    const limit = Number.isSafeInteger(params.limit) ? Math.max(1, Math.min(params.limit, 100)) : 20;
    const excluded = new Set(Array.isArray(params.excludeSessionIds) ? params.excludeSessionIds : []);
    if (excluded.size > 10000 || [...excluded].some(id => typeof id !== "string" || id.length > 200))
      throw new Error("Choose fewer Claude sessions to exclude.");
    const signature = createHash("sha256").update(JSON.stringify([dirs, [...excluded].sort()])).digest("hex");
    let cursor;
    try {
      if (params.cursor != null && (typeof params.cursor !== "string" || params.cursor.length > CATALOG_MAX_CURSOR)) throw 0;
      cursor = params.cursor == null ? { signature, roots: dirs.map(() => ({ offset: 0, anchor: null, done: false })) }
        : JSON.parse(Buffer.from(params.cursor, "base64url").toString());
      if ((cursor.pending != null && (typeof cursor.pending !== "string" || !UUID.test(cursor.pending)))
        || cursor.signature !== signature || !Array.isArray(cursor.roots) || cursor.roots.length !== dirs.length
        || cursor.roots.some(r => !Number.isSafeInteger(r.offset) || r.offset < 0 || typeof r.done !== "boolean"
          || (r.offset === 0 ? r.anchor !== null : !Array.isArray(r.anchor) || r.anchor.length !== 2
            || typeof r.anchor[0] !== "string" || !Number.isFinite(r.anchor[1])))) throw 0;
    } catch { throw new Error("Claude sessions changed. Refresh the list."); }
    const continuation = cursor.pending;
    delete cursor.pending;
    const initial = Buffer.from(JSON.stringify(cursor)).toString("base64url");
    const incomplete = () => ({ data: [], nextCursor: initial, partial: true });
    const key = `${initial}:${limit}:${params.sessionId ?? ""}`;
    for (const [id, pending] of this.catalogReads) {
      if (!pending.promise && pending.expires <= performance.now()) this.catalogReads.delete(id);
    }
    // Directory changes invalidate a warm first page (including newly created
    // transcripts). Appends do not change directory mtime, so every warm request
    // also starts or joins a native refresh. Never cache exact lookups or later
    // pages: their offset anchors still validate against native metadata.
    const fresh = params.cursor == null && params.sessionId == null;
    const observation = fresh ? JSON.stringify(await Promise.all([this.root,
      ...dirs.map(dir => join(this.root, projectFolderName(dir)))].map(path => revision(path, true)))) : null;
    const cacheKey = fresh ? `${key}:${observation}` : null;
    for (const [id, page] of this.catalogPages) {
      if (page.expires <= performance.now()) this.catalogPages.delete(id);
    }
    const cached = this.catalogPages.get(cacheKey)?.result;
    let read = this.catalogReads.get(continuation);
    // Changing page size or exact lookup starts a new read at the same native
    // positions; it must not consume another request's retained result.
    if (!read || (read.request !== key && read.resume !== key)) read = this.catalogReads.get(key);
    if (fresh && read && read.observation !== observation) {
      // Preserve an uncancellable lease/explicit cursor, but free the request
      // key for this new directory observation.
      this.catalogReads.delete(read.key);
      read.key = read.ticket;
      this.catalogReads.set(read.key, read);
      read = null;
    }
    if (!read && fresh) read = [...this.catalogReads.values()]
      .find(pending => pending.request === key && pending.observation === observation && !pending.abandoned);
    if (cached && read?.result && !read.promise) {
      this.catalogReads.delete(read.key);
      read = null;
    }
    if (!read) {
      if (this.catalogReads.size >= CACHED_FILES) {
        const finished = [...this.catalogReads].find(([, pending]) => !pending.promise);
        if (finished) this.catalogReads.delete(finished[0]);
        // The phone refreshes every Project at once and these reads take
        // milliseconds: wait for one to settle within this request's budget
        // instead of answering "still reading". Rejecting here left whichever
        // Projects came fifth and later loading on every refresh, even empty
        // ones. A warm page needs no new read.
        else if (cached) return cached;
        else if (performance.now() >= budget) return incomplete();
        else { await this.catalogSlot(budget); return this.listSessions(params, budget); }
      }
      let progress, exactIndex = 0, exactDirs;
      const snapshot = partial => {
        if (!progress) return incomplete();
        const next = { signature, roots: progress.roots.map(({ offset, anchor, done }) => ({ offset, anchor, done })) };
        return { data: [...progress.data], nextCursor: progress.roots.every(r => r.done) ? null
          : Buffer.from(JSON.stringify(next)).toString("base64url"), partial };
      };
      const run = () => {
        const deadline = performance.now() + CATALOG_READ_MS;
        return this.withSdk(async sdk => {
          // One bounded, birth-time-validated scan for the whole catalog page.
          // Unknown or programmatic entrypoints never establish desktop ownership.
          let owners;
          const summary = async (info, entrypoint) => {
            owners ??= new Set((await claudeSessionRecords(join(this.root, "..", "sessions")) ?? [])
              .filter(record => record.isDesktopOwned).map(record => record.sessionId));
            return sessionSummary(info, owners.has(info.sessionId) ? true : startedOnComputer(entrypoint));
          };
          // Exact lookup opens a chat from any page without scanning newer ones.
          if (params.sessionId != null) {
            if (!UUID.test(params.sessionId)) throw new Error("Invalid Claude session.");
            // The transcript is <root>/<folder>/<id>.jsonl, so a stat per folder
            // finds the few that hold it; the SDK (slow per folder, 54+ worktrees)
            // asks those first. Folders without the file are still asked after,
            // in case Claude Code named one differently (very long paths).
            exactDirs ??= await (async () => {
              const has = await Promise.all(dirs.map(dir => lstat(join(this.root, projectFolderName(dir), `${params.sessionId}.jsonl`)).then(() => true, () => false)));
              return [...dirs.filter((_, i) => has[i]), ...dirs.filter((_, i) => !has[i])];
            })();
            const start = exactIndex;
            for (; exactIndex < exactDirs.length; exactIndex++) {
              if (exactIndex > start && performance.now() >= deadline) return incomplete();
              const dir = exactDirs[exactIndex];
              const info = await sdk.getSessionInfo(params.sessionId, { dir });
              if (info !== undefined && (!info || info.sessionId !== params.sessionId
                || typeof info.cwd !== "string" || !Number.isFinite(info.lastModified))) return incomplete();
              if (info?.sessionId === params.sessionId && info.cwd === dir)
                return { data: [await summary(info, await this.producerEntrypoint(info.sessionId, info.cwd))], nextCursor: null, partial: false };
            }
            return { data: [], nextCursor: null, partial: false };
          }
          progress ??= { roots: cursor.roots.map((r, i) => ({ ...r, dir: dirs[i], rows: [], ended: false })), data: [] };
          const { roots, data } = progress, seen = new Set(data.map(row => row.sessionId));
          // Each folder's rows are read once (see SessionHeadIndex) and held
          // for this read, so every page of it, including the one that proves
          // its end, is answered from the same list.
          const heads = new Map();
          const headRows = async (dir, offset, count) => {
            if (!heads.has(dir)) heads.set(dir, await this.heads.snapshot(sdk, dir));
            const snapshot = heads.get(dir);
            return snapshot ? snapshot.rows.slice(offset, offset + count)
              : sdk.listSessions({ dir, includeWorktrees: false, includeProgrammatic: true, offset, limit: count });
          };
          // A linked worktree without a Claude transcript adds nothing to merge
          // (ACSRecomp has 136); only the time budget bounds those reads.
          let calls = 0, reads = 0, partial = false;
          const anchor = row => [row.sessionId, row.lastModified];
          const newer = (a, b) => b.lastModified - a.lastModified || (a.sessionId > b.sessionId ? -1 : a.sessionId < b.sessionId ? 1 : 0);
          while (data.length < limit) {
            for (const root of roots) {
              if (root.done || root.rows.length) continue;
              if (reads >= CATALOG_MAX_READS || (calls > 0 && performance.now() >= deadline)) { partial = true; break; }
              calls++;
              const previous = root.offset > 0 ? 1 : 0;
              let rows;
              try { rows = await headRows(root.dir, root.offset - previous, CATALOG_PAGE_SIZE + previous); }
              catch { partial = true; break; }
              if (!Array.isArray(rows) || rows.length > CATALOG_PAGE_SIZE + previous
                || rows.some(row => !row || typeof row.sessionId !== "string" || row.sessionId.length > 200 || !Number.isFinite(row.lastModified))) {
                partial = true; break;
              }
              // The SDK offers offsets, not snapshot cursors. A changed boundary
              // must refresh instead of skipping a chat after an insertion/move.
              if (previous && JSON.stringify(anchor(rows[0] ?? {})) !== JSON.stringify(root.anchor))
                throw new Error("Claude sessions changed. Refresh the list.");
              root.ended = rows.length < CATALOG_PAGE_SIZE + previous;
              root.rows = rows.slice(previous);
              if (root.rows.length) reads++;
              else root.done = true;
            }
            if (partial) break; // Without every head, the merged order is unknown.
            const next = roots.filter(r => r.rows.length).sort((a, b) => newer(a.rows[0], b.rows[0]))[0];
            if (!next) break;
            const row = next.rows.shift();
            next.offset++; next.anchor = anchor(row);
            if (!next.rows.length && next.ended) next.done = true;
            if (row.cwd !== next.dir || excluded.has(row.sessionId) || seen.has(row.sessionId)) continue;
            // One-shot `claude -p` runs are not chats: they never enter the list.
            const probe = () => this.producerEntrypoint(row.sessionId, row.cwd);
            const entrypoint = heads.get(next.dir) ? await heads.get(next.dir).entrypoint(row, probe) : await probe();
            if (entrypoint === "sdk-cli") continue;
            seen.add(row.sessionId); data.push(await summary(row, entrypoint));
          }
          return snapshot(partial);
        });
      };
      read = { key, request: key, observation, cacheKey, ticket: randomUUID(), run, snapshot, promise: null,
        result: null, expires: Infinity, abandoned: false };
      this.catalogReads.set(key, read);
    }
    // Nothing reached the caller, so the retained heads stay with this read,
    // even when empty folders already settled some positions.
    const unfinished = result => result.partial && result.data.length === 0;
    // A completed native read may arrive after the caller timed out. Deliver it
    // once; a partial read with no rows resumes its retained heads.
    if (read.result && unfinished(read.result)) read.result = null;
    if (!read.result && !read.promise) {
      read.promise = Promise.resolve().then(read.run).then(async result => {
        read.promise = null; read.result = result; read.expires = performance.now() + CATALOG_RESUME_MS;
        if (read.cacheKey && !result.partial) {
          this.catalogPages.delete(read.cacheKey);
          this.catalogPages.set(read.cacheKey, { result, expires: read.expires }, await retainedBytes(result));
          while (this.catalogPages.size > CATALOG_CACHED_PAGES) this.catalogPages.delete(this.catalogPages.keys().next().value);
        }
        if (read.abandoned && this.catalogReads.get(read.key) === read) this.catalogReads.delete(read.key);
        this.wakeCatalog();
        return result;
      }, error => {
        if (this.catalogReads.get(read.key) === read) this.catalogReads.delete(read.key);
        this.wakeCatalog();
        throw error;
      });
    }
    let timer;
    try {
      // The read above owns the SDK lease even when a warm page returns now.
      // Observe failures here too; the next fresh request can try again.
      if (cached) {
        read.promise?.catch(() => {});
        return cached;
      }
      const result = await Promise.race([read.result ?? read.promise,
        new Promise(resolve => { timer = setTimeout(() => resolve(read.snapshot(true)),
          Math.max(budget - performance.now(), CATALOG_LATE_START_MS)); })]);
      if (!unfinished(result)) {
        // A returned cursor already advances the reader. Keep only the live SDK
        // lease until it settles; the next page cannot use these old heads.
        read.abandoned = true;
        if (!read.promise && this.catalogReads.get(read.key) === read) this.catalogReads.delete(read.key);
      } else if (this.catalogReads.get(read.key) === read) {
        // An explicit cursor or an identical fresh request can adopt this
        // observation. Directory changes start a separate bounded native read.
        if (read.key !== read.ticket) {
          this.catalogReads.delete(read.key);
          read.key = read.ticket;
          this.catalogReads.set(read.key, read);
        }
        // Settled positions survive in the cursor if the retained read expires.
        const positions = result.nextCursor ?? initial;
        read.resume = `${positions}:${limit}:${params.sessionId ?? ""}`;
        const resumed = { ...JSON.parse(Buffer.from(positions, "base64url").toString()), pending: read.ticket };
        return { ...result, nextCursor: Buffer.from(JSON.stringify(resumed)).toString("base64url") };
      }
      return result;
    } finally { clearTimeout(timer); }
  }

  // Resolves when a native read settles (its slot can be reused) or at the deadline.
  catalogSlot(deadline) {
    return new Promise(resolve => {
      const wake = () => { clearTimeout(timer); this.catalogWaiters.delete(wake); resolve(); };
      const timer = setTimeout(wake, Math.max(0, deadline - performance.now()));
      this.catalogWaiters.add(wake);
    });
  }

  wakeCatalog() {
    for (const wake of [...this.catalogWaiters]) wake();
  }

  // Read only the first 64 KiB, never a full history projection per sidebar row.
  // A later desktop reply cannot rewrite origin. SDK forks retain entrypoints
  // but stamp forkedFrom on the copied messages; those roots remain unknown.
  async startedOnComputer(sessionId, cwd) {
    return startedOnComputer(await this.producerEntrypoint(sessionId, cwd));
  }

  // The entrypoint of the first real prompt: "cli"/"claude-desktop" are
  // interactive, "sdk-ts" is an SDK host (Cupola, T3 Code) and "sdk-cli" is a
  // one-shot `claude -p` run. Undefined when the evidence is missing.
  async producerEntrypoint(sessionId, cwd) {
    if (!UUID.test(sessionId) || typeof cwd !== "string" || !isAbsolute(cwd)) return undefined;
    const path = join(this.root, projectFolderName(cwd), `${sessionId}.jsonl`);
    let handle;
    try {
      if (!(await lstat(join(this.root, projectFolderName(cwd)))).isDirectory()) return undefined;
      handle = await openNoFollow(path);
      if (!(await handle.stat()).isFile()) return undefined;
      const buffer = Buffer.alloc(64 * 1024);
      const { bytesRead } = await handle.read(buffer, 0, buffer.length, 0);
      const lines = buffer.subarray(0, bytesRead).toString("utf8").split("\n");
      lines.pop(); // A partial line is never evidence.
      for (const line of lines) {
        if (!line.trim()) continue;
        const row = JSON.parse(line);
        if (!["user", "assistant"].includes(row?.type) || row.isSidechain) continue;
        if (row.type !== "user" || row.parentUuid !== null || row.sessionId !== sessionId
          || row.forkedFrom != null || row.isCompactSummary || row.isMeta
          || ["daemon", "daemon-worker"].includes(row.sessionKind)) return undefined;
        return ["cli", "claude-desktop", "sdk-ts", "sdk-cli", "sdk-py"].includes(row.entrypoint) ? row.entrypoint : undefined;
      }
    } catch { /* Missing, malformed or unsupported producer evidence is unknown. */ }
    finally { await handle?.close(); }
    return undefined;
  }

  async locate(sessionId, cwd) {
    if (!UUID.test(sessionId)) return null;
    const known = this.paths.get(sessionId);
    if (known && await regularFile(known)) return known;
    const direct = typeof cwd === "string" ? join(this.root, projectFolderName(cwd), `${sessionId}.jsonl`) : null;
    if (direct && await regularFile(direct)) { this.paths.set(sessionId, direct, (sessionId.length + direct.length) * 2); return direct; }
    // Long paths are shortened by Claude Code; find the session by its UUID instead.
    let folders = [];
    try { folders = (await readdir(this.root, { withFileTypes: true })).filter(d => d.isDirectory()).slice(0, 2000); } catch { return null; }
    for (const folder of folders) {
      const path = join(this.root, folder.name, `${sessionId}.jsonl`);
      if (await regularFile(path)) { this.paths.set(sessionId, path, (sessionId.length + path.length) * 2); return path; }
    }
    return null;
  }

  // A compact index of the transcript, read incrementally: an append reads
  // only the new bytes. Rows keep their byte range and chain fields, not their
  // content, so a screenshot-heavy transcript stays cached; the few full rows
  // a caller needs are read back by range. Reads of one file run one at a
  // time, since each advances its shared cache.
  async entries(path, cache = this.files) {
    const read = (this.reading.get(path) ?? Promise.resolve()).catch(() => {}).then(() => this.readEntries(path, cache));
    this.reading.set(path, read);
    try { return await read; }
    finally { if (this.reading.get(path) === read) this.reading.delete(path); }
  }

  async readEntries(path, cache = this.files) {
    let info;
    try { info = await lstat(path); } catch { return null; }
    if (!info.isFile() || info.size > MAX_SESSION_BYTES) return null;
    let cached = cache.get(path);
    if (!cached || cached.ino !== info.ino || info.size < cached.offset
      || (info.size === cached.offset && cached.stamp !== fileRevision(info))) {
      cached = { ino: info.ino, offset: 0, lineStart: 0, pending: [], pendingBytes: 0, rows: [], byId: new Map(), edits: new Map(),
        latestMessage: null, settings: null, bytes: 256, stamp: "" };
    }
    if (info.size > cached.offset) {
      const handle = await openNoFollow(path);
      try {
        if ((await handle.stat()).ino !== info.ino) throw new Error("Claude history changed during the read");
        let slice = performance.now();
        while (cached.offset < info.size) {
          const buffer = Buffer.alloc(Math.min(64 * 1024, info.size - cached.offset));
          const { bytesRead } = await handle.read(buffer, 0, buffer.length, cached.offset);
          if (!bytesRead) throw new Error("Claude history changed during the read");
          let at = 0;
          while (at < bytesRead) {
            const newline = buffer.indexOf(10, at);
            if (newline < 0 || newline >= bytesRead) {
              cached.pendingBytes += bytesRead - at;
              if (cached.pendingBytes <= INLINE_RECORD_BYTES) cached.pending.push(Buffer.from(buffer.subarray(at, bytesRead)));
              else cached.pending = []; // A huge record stays on disk until complete.
              break;
            }
            const end = cached.offset + newline;
            const length = end - cached.lineStart;
            let entry = null;
            try {
              if (length > INLINE_RECORD_BYTES) entry = await this.readers.json(path, cached.lineStart, length, { summary: true });
              else {
                const chunks = [...cached.pending, buffer.subarray(at, newline)];
                entry = transcriptEntry(JSON.parse(Buffer.concat(chunks, length).toString("utf8")), cached.lineStart, length);
              }
            } catch (error) { if (!(error instanceof SyntaxError)) throw error; }
            cached.pending = []; cached.pendingBytes = 0; cached.lineStart = end + 1; at = newline + 1;
            if (entry) addEntry(cached, entry);
            if (performance.now() - slice >= 4) { await yieldLoop(); slice = performance.now(); }
          }
          cached.offset += bytesRead;
        }
      } finally { await handle.close(); }
    }
    cached.stamp = fileRevision(info);
    cache.set(path, cached, cached.bytes + cached.pendingBytes);
    return cached;
  }

  // Full rows for index entries, read back by byte range from the indexed file.
  async fullRows(path, file, entries) {
    if (!entries.length) return [];
    const handle = await openNoFollow(path), rows = [];
    try {
      if ((await handle.stat()).ino !== file.ino) throw new Error("Claude history changed during the read");
      let slice = performance.now();
      for (const entry of entries) {
        let row;
        if (entry.length > INLINE_RECORD_BYTES) row = await this.readers.json(path, entry.at, entry.length);
        else {
          const buffer = Buffer.alloc(entry.length);
          const { bytesRead } = await handle.read(buffer, 0, entry.length, entry.at);
          if (bytesRead !== entry.length) throw new Error("Claude history changed during the read");
          row = JSON.parse(buffer.toString("utf8"));
        }
        if (row?.uuid !== entry.uuid) throw new Error("Claude history changed during the read");
        rows.push(row);
        if (performance.now() - slice >= 4) { await yieldLoop(); slice = performance.now(); }
      }
    } finally { await handle.close(); }
    return rows;
  }

  // Task badges share the incremental offset index with settings and history
  // context. Select its current chain, including pre-compaction ancestors and
  // parallel tool siblings, using only compact task records. No SDK history
  // array or full transcript content is materialized for this read.
  async taskMessages(sessionId, cwd) {
    const path = await this.locate(sessionId, cwd), file = path ? await this.entries(path) : null;
    if (!file) throw new Error("Claude's task status could not be read. Try again in a moment.");
    return this.taskRows(file);
  }

  async taskRows(file, includeSidechain = false) {
    // Claude Code can reattach preserved messages at a compact boundary.
    // Resolve those links without mutating the original history-context index.
    const parents = new Map();
    const parent = row => parents.get(row.uuid) ?? row.parent ?? row.logicalParent;
    let slice = performance.now();
    for (const boundary of file.rows) {
      const compact = boundary.compact;
      if (!compact) continue;
      const ids = compact.uuids ?? [compact.head];
      if (!ids.length || ids.some(id => !file.byId.has(id))) continue;
      let previous = compact.anchor;
      for (const id of ids) { parents.set(id, previous); previous = id; }
      const tail = compact.tail ?? previous;
      for (const row of file.rows) {
        if (parent(row) === compact.anchor && row.uuid !== ids[0]) parents.set(row.uuid, tail);
        if (performance.now() - slice >= 4) { await yieldLoop(); slice = performance.now(); }
      }
    }
    const keep = new Set();
    let current = file.rows.findLast(row => (!row.sidechain || includeSidechain) && !row.team
      && (["user", "assistant"].includes(row.type) || row.task?.origin?.kind === "task-notification"));
    while (current && !keep.has(current.uuid)) {
      keep.add(current.uuid);
      current = file.byId.get(parent(current));
    }
    const messageIds = new Set(), uses = new Set();
    for (const id of keep) {
      const row = file.byId.get(id);
      if (row?.type === "assistant" && row.messageId) messageIds.add(row.messageId);
    }
    const messages = [];
    slice = performance.now();
    for (const row of file.rows) {
      if ((row.sidechain && !includeSidechain) || row.team) continue;
      const sibling = row.type === "assistant" && row.messageId && messageIds.has(row.messageId);
      if (keep.has(row.uuid) || sibling || (row.type === "user" && row.task?.message?.content?.some?.(
        block => block.type === "tool_result" && uses.has(block.tool_use_id)))) {
        if (row.task) messages.push(row.task);
        for (const block of row.task?.message?.content ?? []) if (block?.type === "tool_use") uses.add(block.id);
      }
      if (performance.now() - slice >= 4) { await yieldLoop(); slice = performance.now(); }
    }
    return messages;
  }

  async messageContext(sessionId, cwd, firstUuid) {
    const path = await this.locate(sessionId, cwd), file = path ? await this.entries(path) : null;
    return { earlier: await this.earlierRows(path, file, sessionId, firstUuid), edits: new Map(file?.edits) };
  }

  async close() { await this.readers.close(); }

  // The newest records decide these, so read the end of the transcript
  // backwards; an unchanged file answers from memory. Only a tail without the
  // answer falls back to the incremental index.
  async latest(sessionId, cwd, field) {
    const path = await this.locate(sessionId, cwd);
    if (!path) return null;
    let info;
    try { info = await lstat(path); } catch { return null; }
    if (!info.isFile() || info.size > MAX_SESSION_BYTES) return null;
    const stamp = fileRevision(info), indexed = this.files.get(path);
    if (indexed?.ino === info.ino && indexed.offset === info.size && indexed.stamp === stamp) return indexed[field];
    let known = this.tails.get(path);
    if (known?.stamp !== stamp) known = { stamp };
    if (known[field] === undefined) {
      Object.assign(known, await this.scanTail(path, info, field));
      this.tails.set(path, known, 1024);
    }
    return known[field] !== undefined ? known[field] : (await this.entries(path))?.[field] ?? null;
  }

  // Complete lines in the last TAIL_SCAN_BYTES, newest first. A line that
  // starts before the window is unknown, not absent.
  async scanTail(path, info, field) {
    const found = {};
    let handle;
    try {
      handle = await openNoFollow(path);
      if ((await handle.stat()).ino !== info.ino) return found;
      const start = Math.max(0, info.size - TAIL_SCAN_BYTES), buffer = Buffer.alloc(info.size - start);
      const { bytesRead } = await handle.read(buffer, 0, buffer.length, start);
      if (bytesRead !== buffer.length) return found;
      let end = buffer.lastIndexOf(10), slice = performance.now();
      while (end >= 0) {
        const newline = end > 0 ? buffer.lastIndexOf(10, end - 1) : -1;
        if (newline < 0 && start > 0) return found;
        try {
          const entry = transcriptEntry(JSON.parse(buffer.toString("utf8", newline + 1, end)), start + newline + 1, end - newline - 1);
          if (entry?.message && found.latestMessage === undefined) found.latestMessage = entry.uuid;
          if (entry?.settings && found.settings === undefined) found.settings = entry.settings;
        } catch (error) { if (!(error instanceof SyntaxError)) throw error; }
        if (found[field] !== undefined) return found;
        end = newline;
        if (performance.now() - slice >= 4) { await yieldLoop(); slice = performance.now(); }
      }
      // Only when the whole file was read is whatever was not found absent.
      return start > 0 ? found : { latestMessage: null, settings: null, ...found };
    } catch { return found; } finally { await handle?.close(); }
  }

  // The newest top-level prompt or reply in the transcript: who wrote last.
  latestMessageUuid(sessionId, cwd) { return this.latest(sessionId, cwd, "latestMessage"); }

  // The SDK's reader drops Claude Code's `toolUseResult`; file edits keep
  // theirs here, by message uuid, so history counts the lines really changed.
  async fileEdits(sessionId, cwd) {
    const path = await this.locate(sessionId, cwd);
    return new Map((path ? await this.entries(path) : null)?.edits);
  }

  // Messages before the transcript point where the SDK's reader begins.
  async earlierMessages(sessionId, cwd, firstUuid) {
    const path = typeof firstUuid === "string" ? await this.locate(sessionId, cwd) : null;
    return this.earlierRows(path, path ? await this.entries(path) : null, sessionId, firstUuid);
  }

  async earlierRows(path, file, sessionId, firstUuid) {
    const start = file?.byId.get(firstUuid);
    if (!start) return [];
    const keep = new Set(), seen = new Set([start.uuid]);
    let current = start;
    for (;;) {
      const next = current.parent ?? current.logicalParent;
      current = typeof next === "string" ? file.byId.get(next) : null;
      if (!current || seen.has(current.uuid)) break;
      seen.add(current.uuid);
      keep.add(current.uuid);
    }
    if (!keep.size) return [];
    // Parallel tool calls branch: keep sibling blocks of the same assistant
    // message and the tool results answering them.
    const messageIds = new Set();
    for (const id of keep) { const row = file.byId.get(id); if (row.type === "assistant" && row.messageId) messageIds.add(row.messageId); }
    for (const row of file.rows) {
      if (row.index >= start.index) break;
      if (row.sidechain || typeof row.uuid !== "string" || keep.has(row.uuid)) continue;
      if (row.type === "assistant" && messageIds.has(row.messageId)) keep.add(row.uuid);
      else if (row.type === "user" && keep.has(row.parent) && row.toolResult) keep.add(row.uuid);
    }
    const kept = [];
    for (const row of file.rows) {
      if (row.index >= start.index) break;
      if (keep.has(row.uuid) && !row.sidechain && (["user", "assistant", "attachment"].includes(row.type) || row.compactionBoundary)) kept.push(row);
    }
    const messages = [];
    for (const row of await this.fullRows(path, file, kept)) {
      const message = sdkShape(row, sessionId);
      if (message) messages.push(message);
    }
    return messages;
  }

  // What the newest assistant message ran with, as Claude Code recorded it.
  // `native` is false for turns Wonder's own SDK session wrote ("sdk-ts"), so
  // only a turn from the desktop app or terminal changes a thread's settings.
  latestSettings(sessionId, cwd) { return this.latest(sessionId, cwd, "settings"); }

  // The SDK owns subagent discovery; the shared offset index supplies compact
  // activity records. Task linkage and labels come from native metadata files.
  // A timed-out read keeps its SDK lease and single-flight slot until it ends,
  // so repeated phone polls cannot pile up uncancelable filesystem reads.
  async agentTasks(sessionId, cwd, { requireComplete = false } = {}) {
    if (!this.withSdk || !UUID.test(sessionId)) return NO_TASKS;
    let read = this.taskReads.get(sessionId);
    if (!read) {
      if (this.taskReads.size >= CACHED_FILES) {
        if (requireComplete) throw new Error("Claude is still reading task status. Try again in a moment.");
        return NO_TASKS;
      }
      const deadline = performance.now() + TASK_READ_MS;
      read = this.withSdk(sdk => this.readAgentTasks(sessionId, cwd, sdk, deadline)).catch(() => null);
      this.taskReads.set(sessionId, read);
      void read.finally(() => { if (this.taskReads.get(sessionId) === read) this.taskReads.delete(sessionId); });
    }
    let timer;
    try {
      const tasks = await Promise.race([read, new Promise(resolve => { timer = setTimeout(() => resolve(null), TASK_READ_MS); })]);
      // A list may retain an older verified answer; partial evidence is never
      // a verified empty roster. The native read still owns its single-flight slot.
      if (tasks == null && requireComplete) throw new Error("Claude is still reading task status. Try again in a moment.");
      return tasks ?? NO_TASKS;
    } finally { clearTimeout(timer); }
  }

  async readAgentTasks(sessionId, cwd, sdk, deadline) {
    const path = await this.locate(sessionId, cwd);
    if (!path || typeof sdk.listSubagents !== "function") return NO_TASKS;
    const dir = join(path.slice(0, -".jsonl".length), "subagents");
    const listing = `${await revision(path)}|${await revision(dir, true)}`;
    let cache = this.taskCaches.get(path);
    if (!cache) cache = { listing: null, agentIds: [], helpers: new Map(), tasks: [], dirty: true };
    this.taskCaches.delete(path);
    this.taskCaches.set(path, cache, await retainedBytes(cache));
    while (this.taskCaches.size > CACHED_FILES) this.taskCaches.delete(this.taskCaches.keys().next().value);
    try {
      if (listing !== cache.listing) {
        const agentIds = (await sdk.listSubagents(sessionId, { dir: cwd })).filter(id => /^[A-Za-z0-9_-]{1,64}$/.test(id)).slice(0, MAX_TASK_FILES);
        cache.dirty ||= agentIds.length !== cache.agentIds.length || agentIds.some((id, index) => id !== cache.agentIds[index]);
        cache.listing = listing;
        cache.agentIds = agentIds;
      }
      const agentIds = cache.agentIds;
      const helpers = new Map();
      let changed = cache.dirty || agentIds.length !== cache.helpers.size;
      for (const agentId of agentIds) {
        if (performance.now() >= deadline) return null; // Partial evidence is not a completed roster.
        const known = cache.helpers.get(agentId);
        const metaPath = join(dir, `agent-${agentId}.meta.json`), transcriptPath = join(dir, `agent-${agentId}.jsonl`);
        const key = `${await revision(metaPath)}|${await revision(transcriptPath)}`;
        if (known?.key === key) { helpers.set(agentId, known); continue; }
        changed = true;
        const meta = await smallJson(metaPath);
        let task = null;
        if (meta && typeof meta.toolUseId === "string") {
          let transcript = { startedAt: null, finished: false, currentActivity: null };
          if (await completeTranscriptRevision(transcriptPath)) {
            const file = await this.entries(transcriptPath, this.helperFiles);
            const messages = file ? await this.taskRows(file, true) : [];
            const after = `${await revision(metaPath)}|${await revision(transcriptPath)}`;
            if (after !== key) return null; // A native read racing an append is not a stable observation.
            transcript = subagentActivity(messages);
          }
          task = { agentId, toolUseId: meta.toolUseId, agentType: typeof meta.agentType === "string" ? meta.agentType : null,
            description: typeof meta.description === "string" ? meta.description : null,
            background: meta.requestShape === "background", ...transcript };
        }
        const helper = { key, task };
        helpers.set(agentId, helper);
        // Keep verified work across a slow poll, but publish only a complete roster.
        cache.helpers.set(agentId, helper);
        cache.dirty = true;
      }
      if (!changed && [...cache.helpers.keys()].some(id => !helpers.has(id))) changed = true;
      cache.helpers = helpers;
      if (changed) cache.tasks = [...helpers.values()].map(h => h.task).filter(Boolean);
      cache.dirty = false;
      return cache.tasks;
    } finally {
      this.taskCaches.set(path, cache, await retainedBytes(cache));
    }
  }

  async stamp(sessionId, cwd) {
    const path = await this.locate(sessionId, cwd);
    if (!path) return null;
    const stamp = await completeTranscriptRevision(path);
    // An unfinished last record is a write in progress: worth reading again.
    if (!stamp) throw Object.assign(new Error("Claude is updating this conversation. Try again in a moment."), { appended: true });
    return `${path}:${stamp}`;
  }
}

// What the index keeps of one transcript record: its byte range, the fields
// that link the chain, and what latest-message, settings and edit lookups
// need. Runs in the reader process for records too large for the event loop.
export function transcriptEntry(row, at, length) {
  if (!row || typeof row !== "object" || Array.isArray(row)) return null;
  const string = value => typeof value === "string" ? value : null;
  const uuid = string(row.uuid), type = string(row.type), sidechain = Boolean(row.isSidechain);
  const entry = { at, length, uuid, type, parent: string(row.parentUuid), logicalParent: string(row.logicalParentUuid),
    sidechain, messageId: string(row.message?.id),
    team: Boolean(row.teamName), task: taskMessage(row),
    toolResult: type === "user" && Array.isArray(row.message?.content) && row.message.content.some(block => block?.type === "tool_result") };
  if (type === "system" && row.subtype === "compact_boundary") {
    entry.compactionBoundary = true;
    const { preservedMessages: kept, preservedSegment: segment } = row.compactMetadata ?? {};
    if (typeof kept?.anchorUuid === "string" && Array.isArray(kept.uuids) && kept.uuids.every(id => typeof id === "string"))
      entry.compact = { anchor: kept.anchorUuid, uuids: kept.uuids };
    else if ([segment?.anchorUuid, segment?.headUuid, segment?.tailUuid].every(id => typeof id === "string"))
      entry.compact = { anchor: segment.anchorUuid, head: segment.headUuid, tail: segment.tailUuid };
  }
  if (["user", "assistant"].includes(type) && !sidechain && uuid) entry.message = true;
  const model = row.message?.model;
  if (type === "assistant" && !sidechain && typeof model === "string" && model && !model.startsWith("<")) {
    const text = value => typeof value === "string" && value ? value : null;
    entry.settings = { turnId: row.uuid, native: row.entrypoint !== "sdk-ts", model, effort: text(row.effort), speed: text(row.message?.usage?.speed) };
  }
  const edit = type === "user" && uuid ? fileEditResult(row.toolUseResult) : null;
  if (edit) entry.edit = edit;
  return entry;
}

const TASK_TOOLS = new Set(["Agent", "Task", "Bash", "Monitor", "TaskStop", "KillShell", "TaskOutput", "BashOutput"]);

function addEntry(file, { message, settings, edit, ...entry }) {
  // Keep the exact tool/result linkage for helper activity, but retain text
  // only for tools the task state projection understands. A large Read/Web
  // result must not grow the metadata cache with unrelated history payload.
  file.taskUses ??= new Set();
  const content = entry.task?.message?.content;
  for (const block of Array.isArray(content) ? content : []) {
    if (block.type === "tool_use" && TASK_TOOLS.has(block.name) && !file.taskUses.has(block.id)) {
      file.taskUses.add(block.id); file.bytes += 32 + String(block.id).length * 2;
    } else if (block.type === "tool_result" && !file.taskUses.has(block.tool_use_id)) block.content = "";
  }
  entry.index = file.rows.length; file.rows.push(entry);
  if (entry.uuid) file.byId.set(entry.uuid, entry);
  if (message) file.latestMessage = entry.uuid;
  if (settings) file.settings = settings;
  // An edit is charged by its whole record: its patch is at most that large.
  if (edit) { file.edits.set(entry.uuid, edit); file.bytes += entry.length * 2; }
  if (entry.task) file.bytes += JSON.stringify(entry.task).length * 2;
  if (entry.compact) file.bytes += JSON.stringify(entry.compact).length * 2;
  file.bytes += ENTRY_BYTES + 2 * ((entry.uuid?.length ?? 0) + (entry.type?.length ?? 0) + (entry.parent?.length ?? 0)
    + (entry.logicalParent?.length ?? 0) + (entry.messageId?.length ?? 0)) + (settings ? 256 : 0);
}

// Bounds match the task projection's displayed/detail fields. Images, thinking,
// ordinary reply text and tool payloads outside task/activity fields are omitted.
function taskMessage(row) {
  if (row.teamName) return null;
  const message = sdkShape(row, row.sessionId, false);
  if (!message) return null;
  const text = (value, length = 32_000) => typeof value === "string" ? value.slice(0, length) : null;
  const contentText = content => typeof content === "string" ? text(content)
    : Array.isArray(content) ? content.filter(b => b?.type === "text").map(b => text(b.text) ?? "").join("\n").slice(0, 32_000) : "";
  let content = message.message?.content;
  if (Array.isArray(content)) content = content.flatMap(block => {
    if (block?.type === "tool_use") {
      const input = block.input ?? {}, bounded = {};
      for (const key of ["description", "subagent_type", "prompt", "command", "bash_id", "shell_id", "task_id", "file_path", "notebook_path", "path", "pattern", "query", "url", "content", "old_string", "new_string"])
        if (typeof input[key] === "string") bounded[key] = text(input[key], 8000);
      if (input.run_in_background === true) bounded.run_in_background = true;
      return [{ type: "tool_use", id: block.id, name: block.name, input: bounded }];
    }
    if (block?.type === "tool_result") return [{ type: "tool_result", tool_use_id: block.tool_use_id,
      is_error: block.is_error === true, content: contentText(block.content) }];
    if (message.type === "user" && block?.type === "text") return [{ type: "text", text: text(block.text, block.text?.trimStart().startsWith("<task-notification>") ? 32_000 : 256) }];
    return [];
  });
  else content = message.type === "user" ? text(content, content?.trimStart?.().startsWith("<task-notification>") ? 32_000 : 256) : [];
  const result = message.tool_use_result ?? row.toolUseResult;
  const state = source => source && typeof source === "object" ? Object.fromEntries(
    ["status", "exitCode", "exit_code", "backgroundTaskId"].filter(key => ["string", "number"].includes(typeof source[key]))
      .map(key => [key, typeof source[key] === "string" ? text(source[key], 256) : source[key]])) : null;
  const toolResult = state(result);
  if (result?.task && toolResult) toolResult.task = state(result.task);
  return { type: message.type, uuid: message.uuid, timestamp: message.timestamp, parent_tool_use_id: null,
    ...(message.is_meta ? { is_meta: true } : {}), ...(message.origin ? { origin: message.origin } : {}),
    message: { content, stop_reason: message.message?.stop_reason ?? null },
    ...(toolResult ? { tool_use_result: toolResult } : {}) };
}

function sdkShape(row, sessionId, includeEdits = true) {
  const base = { uuid: row.uuid, session_id: sessionId, parent_tool_use_id: null, timestamp: row.timestamp ?? null };
  if (row.type === "system" && row.subtype === "compact_boundary") return { ...base, type: "system", subtype: "compact_boundary" };
  if (row.type === "assistant" && row.message) return { ...base, type: "assistant", message: row.message };
  if (row.type === "user" && row.message) {
    if (metaMessage(row) && !(Array.isArray(row.message.content) && row.message.content.some(b => b?.type === "tool_result"))) return null;
    const edit = includeEdits ? fileEditResult(row.toolUseResult) : null;
    return { ...base, type: "user", message: row.message, ...(metaMessage(row) ? { is_meta: true } : {}), ...(row.origin ? { origin: row.origin } : {}), ...(edit ? { tool_use_result: edit } : {}) };
  }
  // Task completions are stored as queued-command attachments; the SDK presents them as user notices.
  const attachment = row.attachment;
  if (row.type === "attachment" && attachment?.type === "queued_command" && attachment.commandMode === "task-notification"
    && typeof attachment.prompt === "string") {
    return { ...base, type: "user", message: { role: "user", content: attachment.prompt }, origin: { kind: "task-notification" } };
  }
  return null;
}

async function regularFile(path) {
  try { return (await lstat(path)).isFile(); } catch { return false; }
}

// A file's revision: changes when it is replaced, grows or is rewritten.
const fileRevision = info => `${info.dev}:${info.ino}:${info.size}:${info.mtimeMs}:${info.ctimeMs}`;
// Whether a transcript stamp moved only by growing the same file, as an
// append does (a rewrite or replacement is not an append).
export function appendedStamp(before, after) {
  if (typeof before !== "string" || typeof after !== "string") return false;
  const parse = stamp => { const parts = stamp.split(":"); return { file: parts.slice(0, -5).join(":"), dev: parts.at(-5), ino: parts.at(-4), size: Number(parts.at(-3)) }; };
  const a = parse(before), b = parse(after);
  return a.file === b.file && a.dev === b.dev && a.ino === b.ino && b.size > a.size;
}
async function revision(path, directory = false) {
  try { const info = await lstat(path); return (directory ? info.isDirectory() : info.isFile()) ? fileRevision(info) : null; } catch { return null; }
}

async function smallJson(path) {
  try {
    const info = await lstat(path);
    if (!info.isFile() || info.size > 64 * 1024) return null;
    return JSON.parse(await readFile(path, "utf8"));
  } catch { return null; }
}

// The SDK omits incomplete JSONL records without marking the result partial.
// Check only framing/size here; message content and chain selection stay native.
async function completeTranscriptRevision(path) {
  let handle;
  try {
    handle = await openNoFollow(path);
    const info = await handle.stat();
    if (!info.isFile() || !info.size || info.size > MAX_SESSION_BYTES) return null;
    const last = Buffer.alloc(1);
    const { bytesRead } = await handle.read(last, 0, 1, info.size - 1);
    const stamp = fileRevision(info);
    return bytesRead === 1 && last[0] === 10 && await revision(path) === stamp ? stamp : null;
  } catch { return null; } finally { await handle?.close(); }
}

function subagentActivity(messages) {
  const result = { startedAt: messages.find(m => typeof m.timestamp === "string")?.timestamp ?? null,
    finished: false, currentActivity: null };
  const latest = messages.at(-1);
  result.finished = latest?.type === "assistant" && latest.message?.stop_reason === "end_turn";
  if (result.finished) return result;
  const completed = new Set();
  for (let index = messages.length - 1; index >= 0; index--) {
    const row = messages[index], content = row.message?.content;
    for (const block of Array.isArray(content) ? [...content].reverse() : []) {
      if (block.type === "tool_result") completed.add(block.tool_use_id);
      if (block.type === "tool_use" && !completed.has(block.id)) {
        result.currentActivity = claudeToolItem(block, "inProgress");
        if (result.currentActivity) return result;
      }
    }
    if (row.type === "user" && !row.is_meta && !row.isCompactSummary
      && (typeof content === "string" || content?.some?.(block => block.type === "text"))) break;
  }
  return result;
}

// The heads of one project folder's Claude Code sessions, newest first, so a
// catalog page past the last chat does not read the folder again.
//
// The SDK lists a folder by reading the head and tail of every transcript
// until it has the rows asked for. A folder of 300 transcripts that holds 17
// chats therefore costs about 0.6 s to prove its end (measured live), on every
// page that reaches it. Here each folder is read once; afterwards a request
// stats the folder's files and reads again only the ones that were added or
// changed. This is an index of what is on disk, not a result cache: every
// request validates each entry against its file's mtime and size, there is no
// expiry, and a deleted, added or grown transcript shows up on the next request.
import { readdir, realpath, stat } from "node:fs/promises";
import { join } from "node:path";

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
// Beyond these a folder (or the set of folders) is read by the SDK as before,
// so the index stays small next to the transcripts it describes.
const MAX_FOLDERS = 128;
const MAX_FILES = 20000;
const STAT_BATCH = 64;
// Claude Code shortens a longer folder name and adds a hash; the SDK finds
// those by prefix, which this index does not repeat.
const MAX_FOLDER_NAME = 150;

const newer = (a, b) => b.lastModified - a.lastModified
  || (a.sessionId > b.sessionId ? -1 : a.sessionId < b.sessionId ? 1 : 0);

function checkRows(rows, limit) {
  if (!Array.isArray(rows) || rows.length > limit
    || rows.some(row => !row || typeof row.sessionId !== "string" || row.sessionId.length > 200 || !Number.isFinite(row.lastModified)))
    throw new Error("Claude sessions could not be read.");
}

export class SessionHeadIndex {
  constructor(root, folderName) {
    Object.assign(this, { root, folderName, folders: new Map() });
  }

  // The folder's rows as of this call, or null when the SDK must list it
  // (its folder is not where Claude Code normally keeps it, or it is too big).
  // `rows` is never changed afterwards; `entrypoint` memoizes a probe per
  // transcript version.
  async snapshot(sdk, dir) {
    if (this.folderName(dir).length > MAX_FOLDER_NAME) return null;
    let folder = this.folders.get(dir);
    if (!folder) {
      folder = { entries: new Map(), rows: [], queue: Promise.resolve(), ready: false };
      this.folders.set(dir, folder);
      while (this.folders.size > MAX_FOLDERS) this.folders.delete(this.folders.keys().next().value);
    } else { this.folders.delete(dir); this.folders.set(dir, folder); }
    // Requests queue, so one that begins after a file changed always sees it.
    const run = folder.queue.catch(() => {}).then(() => this.refresh(sdk, dir, folder));
    folder.queue = run;
    const state = await run;
    if (!state) return null;
    const { entries, rows } = state;
    return { rows, entrypoint: (row, probe) => {
      const entry = entries.get(row.sessionId.toLowerCase());
      if (entry?.row !== row) return probe();
      return entry.entrypoint ??= probe();
    } };
  }

  async refresh(sdk, dir, folder) {
    // Claude Code names the folder after the real path of the directory.
    let names, path;
    for (const candidate of [dir, await realpath(dir).catch(() => null)]) {
      if (!candidate || candidate.length > MAX_FOLDER_NAME) continue;
      path = join(this.root, this.folderName(candidate));
      try { names = await readdir(path); break; } catch { /* Try the real path. */ }
    }
    if (!names) return false;
    names = names.filter(name => name.endsWith(".jsonl") && UUID.test(name.slice(0, -6)));
    if (names.length > MAX_FILES) return false;
    const stats = new Map();
    for (let i = 0; i < names.length; i += STAT_BATCH) {
      await Promise.all(names.slice(i, i + STAT_BATCH).map(async name => {
        try {
          const info = await stat(join(path, name));
          if (info.isFile()) stats.set(name.slice(0, -6).toLowerCase(), { mtime: info.mtime.getTime(), size: info.size });
        } catch { /* Gone since the listing. */ }
      }));
    }
    const current = new Map(), changed = [];
    for (const [id, now] of stats) {
      const old = folder.entries.get(id);
      if (old?.settled && old.mtime === now.mtime && old.size === now.size) current.set(id, old);
      else { current.set(id, { ...now, row: null, settled: false, entrypoint: undefined }); changed.push(id); }
    }
    if (!changed.length && folder.ready && current.size === folder.entries.size) return { entries: folder.entries, rows: folder.rows };
    if (changed.length) {
      // The SDK lists newest first and stops at `limit` rows, so asking for as
      // many rows as there are files up to the oldest changed one reads exactly
      // the files that changed (new or appended files are the newest) and
      // those before them. A first read changes every file.
      const order = [...current].sort(([ai, a], [bi, b]) => b.mtime - a.mtime || (ai > bi ? -1 : ai < bi ? 1 : 0));
      const position = new Map(order.map(([id], index) => [id, index]));
      const limit = Math.max(...changed.map(id => position.get(id))) + 1;
      const rows = await sdk.listSessions({ dir, includeWorktrees: false, includeProgrammatic: true, limit });
      checkRows(rows, limit);
      let last = -1;
      for (const row of rows) {
        const id = row.sessionId.toLowerCase(), entry = current.get(id);
        if (!entry) continue; // Created since the files were listed: the next request reads it.
        last = Math.max(last, position.get(id));
        // A file that changed again while it was read is read again next time.
        entry.row = row; entry.settled = row.lastModified === entry.mtime;
      }
      // A full page ends at its last row; a short one reached the end of the
      // folder. Files in between that gave no row are not chats (the SDK drops
      // them), which is as settled as a row.
      const through = rows.length >= limit ? last : order.length - 1;
      for (const [index, [id, entry]] of order.entries()) {
        if (entry.row || entry.settled) continue;
        if (index <= through) entry.settled = true;
        else current.delete(id); // Not reached: the next request reads it.
      }
    }
    folder.entries = current;
    folder.rows = [...current.values()].filter(entry => entry.row).map(entry => entry.row).sort(newer);
    folder.ready = true;
    return { entries: current, rows: folder.rows };
  }
}

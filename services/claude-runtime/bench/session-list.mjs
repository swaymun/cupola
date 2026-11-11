// Times project/sessions/list on a synthetic folder of 300 large transcripts,
// 17 of them chats (what the live Projects look like), before and after the
// session head index. Usage:
//   node bench/session-list.mjs [--runtime DIR] [--files 300] [--runs 20]
// --runtime points at another checkout's services/claude-runtime (for the
// "before" numbers); the fixture helper always comes from this checkout.
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { writeSessionFixture } from "../test/session-fixture.mjs";

const arg = (name, fallback) => { const i = process.argv.indexOf(`--${name}`); return i < 0 ? fallback : process.argv[i + 1]; };
const runtime = resolve(arg("runtime", new URL("..", import.meta.url).pathname));
const files = Number(arg("files", 300)), runs = Number(arg("runs", 20));
const { ClaudeSessionFiles } = await import(pathToFileURL(join(runtime, "session-file.mjs")));
const root = await mkdtemp(join(tmpdir(), "session-list-bench-")), cwd = "/work/Repo";
process.env.CLAUDE_CONFIG_DIR = root;
const { chats } = await writeSessionFixture(root, cwd, { files });
const sdk = await import(pathToFileURL(join(runtime, "node_modules/@anthropic-ai/claude-agent-sdk/sdk.mjs")));
const make = () => new ClaudeSessionFiles(join(root, "projects"), action => action(sdk));
const ids = page => page.data.map(row => row.sessionId);

// What the daemon asks for: a first page, then (for a list that fills its page
// but may have more) the page after the last row, which must prove the end.
const patterns = {
  "limit 17 (stops at 17 rows)": async f => ids(await f.listSessions({ dirs: [cwd], limit: 17 })),
  "limit 30 (17 rows, proves end)": async f => ids(await f.listSessions({ dirs: [cwd], limit: 30 })),
  "limit 17, then past the last row": async f => {
    const first = await f.listSessions({ dirs: [cwd], limit: 17 });
    const rest = first.nextCursor ? await f.listSessions({ dirs: [cwd], limit: 17, cursor: first.nextCursor }) : { data: [] };
    return [...ids(first), ...ids(rest)];
  },
  "paging by 5 to the end": async f => {
    const all = []; let cursor = null;
    do { const page = await f.listSessions({ dirs: [cwd], limit: 5, cursor }); all.push(...ids(page)); cursor = page.nextCursor; } while (cursor);
    return all;
  },
};
const ms = async action => { const t = performance.now(); const value = await action(); return [performance.now() - t, value]; };
const q = (values, p) => [...values].sort((a, b) => a - b)[Math.min(values.length - 1, Math.floor(values.length * p))];
console.log(`${runtime}\n${files} transcripts, ${chats.length} chats\n`);
console.log("pattern".padEnd(36), "cold ms".padStart(9), "warm p50".padStart(9), "warm p90".padStart(9), " rows");
for (const [name, run] of Object.entries(patterns)) {
  const cold = [], warm = []; let rows = [];
  for (let i = 0; i < 3; i++) { const [t, value] = await ms(() => run(make())); cold.push(t); rows = value; }
  const f = make(); await run(f);
  for (let i = 0; i < runs; i++) warm.push((await ms(() => run(f)))[0]);
  const ok = JSON.stringify(rows) === JSON.stringify(chats) ? "ok" : `MISMATCH ${rows.length}`;
  console.log(name.padEnd(36), q(cold, 0.5).toFixed(0).padStart(9), q(warm, 0.5).toFixed(1).padStart(9), q(warm, 0.9).toFixed(1).padStart(9), ` ${rows.length} ${ok}`);
}
await rm(root, { recursive: true, force: true });

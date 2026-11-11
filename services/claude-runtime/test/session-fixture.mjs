// A synthetic Claude Code project folder shaped like a busy repository's: many
// large transcripts, of which only a few are chats. The files are sparse, so
// their size costs no disk; only the head and tail (what the SDK reads) are real.
import { mkdir, open, utimes } from "node:fs/promises";
import { join } from "node:path";
import { projectFolderName } from "../session-file.mjs";

export const fixtureId = (n, prefix = "00000000") => `${prefix}-0000-4000-8000-${String(n).padStart(12, "0")}`;

// Writes `files` transcripts for `cwd` under `<config>/projects`. Every
// `every`th one is a chat ("cli" entrypoint); the others are one-shot
// `claude -p` runs ("sdk-cli"), which the catalog skips. Returns the chat IDs,
// newest first, and the folder.
export async function writeSessionFixture(config, cwd, { files = 300, every = 18, bytes = 8 * 1024 * 1024, newest = 1_700_000_000 } = {}) {
  const folder = join(config, "projects", projectFolderName(cwd));
  await mkdir(folder, { recursive: true });
  const chats = [];
  for (let n = 0; n < files; n++) {
    const chat = n % every === 0, sessionId = fixtureId(n);
    await writeTranscript(folder, cwd, sessionId, { entrypoint: chat ? "cli" : "sdk-cli", bytes, at: newest - n * 60 });
    if (chat) chats.push(sessionId);
  }
  return { folder, chats };
}

export async function writeTranscript(folder, cwd, sessionId, { entrypoint = "cli", bytes = 8 * 1024 * 1024, at, prompt = `Prompt of ${sessionId}` } = {}) {
  const line = value => JSON.stringify(value) + "\n";
  const head = line({ type: "user", sessionId, uuid: `u-${sessionId}`, parentUuid: null, cwd, entrypoint, version: "2.1.0",
    timestamp: new Date((at ?? Date.now() / 1000) * 1000).toISOString(), message: { role: "user", content: prompt } });
  const tail = line({ type: "assistant", sessionId, uuid: `a-${sessionId}`, parentUuid: `u-${sessionId}`, cwd,
    message: { role: "assistant", content: [{ type: "text", text: "Done." }] } });
  const path = join(folder, `${sessionId}.jsonl`), file = await open(path, "w");
  try {
    await file.write(head, 0, "utf8");
    await file.truncate(bytes);
    await file.write(tail, bytes - tail.length, "utf8");
  } finally { await file.close(); }
  if (at !== undefined) await utimes(path, at, at);
  return path;
}

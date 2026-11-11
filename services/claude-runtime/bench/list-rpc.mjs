// Answers `project/sessions/list` over stdin/stdout lines (JSON-RPC shaped), as
// the runtime does, using only the catalog reader: no model, no daemon.
//   CLAUDE_CONFIG_DIR=… node bench/list-rpc.mjs [RUNTIME_DIR]
// RUNTIME_DIR picks another checkout's services/claude-runtime (for "before" numbers).
import { createInterface } from "node:readline";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";

const runtime = resolve(process.argv[2] ?? new URL("..", import.meta.url).pathname);
const { ClaudeSessionFiles } = await import(pathToFileURL(join(runtime, "session-file.mjs")));
const sdk = await import(pathToFileURL(join(runtime, "node_modules/@anthropic-ai/claude-agent-sdk/sdk.mjs")));
const files = new ClaudeSessionFiles(join(process.env.CLAUDE_CONFIG_DIR, "projects"), action => action(sdk));
for await (const line of createInterface({ input: process.stdin })) {
  const { id, method, params } = JSON.parse(line);
  try {
    if (method !== "project/sessions/list") throw new Error(`Unsupported ${method}`);
    console.log(JSON.stringify({ id, result: await files.listSessions(params) }));
  } catch (error) {
    console.log(JSON.stringify({ id, error: { code: -32000, message: String(error?.message ?? error) } }));
  }
}

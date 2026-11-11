// Live check (not part of `node --test`): a conversation whose folder lives inside
// Cupola's data folder (a scratch data folder here). Real Claude Code runs five
// Bash commands with Haiku under ToolPolicy.sandbox(); creating a new file or
// conversation folder in the data folder, or writing a sibling's folder, must be
// refused, while the conversation's own folder and the rest of the disk stay writable.
//
//   node test/live-data-folder.mjs
//
// Needs a Claude login and must run outside another sandbox. Haiku only.
import { mkdirSync, mkdtempSync, realpathSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { dirname, join as joinPath } from "node:path";
import { fileURLToPath } from "node:url";
import { ToolPolicy } from "../permissions.mjs";
import { baseOptions, loadSdk } from "../sdk-runtime.mjs";
import { HAIKU_MODEL } from "../projection.mjs";
const R = joinPath(dirname(fileURLToPath(import.meta.url)), "..");
const root = realpathSync(mkdtempSync(join(tmpdir(), "hcp-live-")));
const home = join(root, "home"), data = join(home, ".cupola"), ws = join(data, "project-media", "t1-abc");
for (const d of [ws, join(data, "project-media", "t0-old"), join(home, "elsewhere")]) mkdirSync(d, { recursive: true });
const policy = new ToolPolicy({ cwd: ws, workspace: ws, mode: "workspace", approvalMode: "full_access", readRoots: ["/"], writeRoots: [ws], deniedRoots: [data], home });
const sandbox = policy.sandbox();
const { sdk, executable } = await loadSdk(R);
const probes = { "new file in data root": `touch ${data}/new-secret`, "new sibling conversation folder": `mkdir ${data}/project-media/t2-new`,
  "write into sibling t0-old": `touch ${data}/project-media/t0-old/x`, "own folder": `touch ${ws}/ok`, "outside data": `touch ${home}/elsewhere/ok` };
for (const [label, command] of Object.entries(probes)) {
  const outputs = [];
  const options = { ...baseOptions({ executable }), cwd: ws, model: HAIKU_MODEL, maxTurns: 4, persistSession: false, settingSources: [], sandbox, tools: ["Bash"], permissionMode: "default",
    systemPrompt: "You run one shell command with the Bash tool exactly as given, then reply with the single word DONE. Never retry, never modify the command, never set dangerouslyDisableSandbox.",
    canUseTool: async (tool, input) => input.dangerouslyDisableSandbox ? { behavior: "deny", message: "refused" } : { behavior: "allow", updatedInput: input } };
  for await (const m of sdk.query({ prompt: `Run exactly this Bash command: ${command}`, options }))
    if (m.type === "user" && Array.isArray(m.message?.content)) for (const p of m.message.content) if (p.type === "tool_result") outputs.push(typeof p.content === "string" ? p.content : JSON.stringify(p.content));
  const created = { "new file in data root": join(data, "new-secret"), "new sibling conversation folder": join(data, "project-media", "t2-new"), "write into sibling t0-old": join(data, "project-media", "t0-old", "x"), "own folder": join(ws, "ok"), "outside data": join(home, "elsewhere", "ok") }[label];
  console.log(`${label}: ${existsSync(created) ? "WRITTEN" : "REFUSED"}  | ${outputs.join(" / ").slice(0, 160)}`);
}

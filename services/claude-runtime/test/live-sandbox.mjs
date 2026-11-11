// Live check (not part of `node --test`): drives the real Claude runtime pieces
// (loadSdk, baseOptions, ToolPolicy.sandbox, the policy's Bash decision) with
// Haiku against a scratch git repo, and prints what each developer command
// returned inside Cupola's command sandbox.
//
//   node test/live-sandbox.mjs --mode full_access|read_only [--sdk <dir with node_modules>] [--only a,b]
//
// Needs a Claude login and must run outside another sandbox (macOS refuses a
// nested one). Haiku only; no secrets are read; `git push` is --dry-run.
import { execFileSync } from "node:child_process";
import { mkdtempSync, realpathSync, writeFileSync } from "node:fs";
import { homedir, tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { ToolPolicy } from "../permissions.mjs";
import { baseOptions, loadSdk } from "../sdk-runtime.mjs";
import { HAIKU_MODEL } from "../projection.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const args = Object.fromEntries(process.argv.slice(2).reduce((pairs, arg, index, all) => arg.startsWith("--") ? [...pairs, [arg.slice(2), all[index + 1]]] : pairs, []));
const mode = args.mode ?? "full_access";
const home = homedir();
const scratch = realpathSync(mkdtempSync(join(tmpdir(), "b3-sandbox-")));
const git = (...a) => execFileSync("git", a, { cwd: scratch, stdio: "pipe" }).toString();
git("init", "-q", "-b", "main"); writeFileSync(join(scratch, "README"), "scratch\n");
git("add", "."); git("-c", "user.name=b3", "-c", "user.email=b3@example.com", "commit", "-q", "-m", "scratch");
const remote = args.remote ?? "https://github.com/octocat/Hello-World";

const COMMANDS = {
  xcodebuild: "xcodebuild -version",
  simctl: "xcrun simctl list devices | head",
  open: "open -g -a TextEdit",
  quit: "osascript -e 'quit app \"TextEdit\"'",
  osascript: "osascript -e 'return 1'",
  lsremote: `git ls-remote ${remote} HEAD`,
  gh: "gh auth status",
  pushdry: `git push --dry-run ${args.pushRemote ?? remote} HEAD:refs/heads/b3-sandbox-scratch`,
  killall: "killall TextEdit",
  // Whether `open` hands the sandbox a way out: Terminal runs the script unsandboxed.
  escape: "printf '#!/bin/sh\\nls %s/Library/Keychains > " + scratch + "/escape.out 2>&1\\n' " + home + " > probe.command && chmod +x probe.command && open -g -a Terminal probe.command; sleep 6; cat escape.out",
  // Boundaries that must hold in every mode.
  keychains: `ls ${home}/Library/Keychains`,
  writeHome: `touch ${home}/b3-sandbox-probe && echo WROTE && rm ${home}/b3-sandbox-probe`,
  writeCwd: "touch probe && echo WROTE",
};
const only = args.only ? args.only.split(",") : Object.keys(COMMANDS);

const policy = new ToolPolicy({ cwd: scratch, mode, approvalMode: mode === "read_only" ? "ask" : "full_access",
  readRoots: [scratch], writeRoots: mode === "read_only" ? [] : [scratch], projectRoots: [scratch],
  deniedRoots: [join(home, ".cupola"), join(home, "Library/Application Support/Wonder"), join(home, ".wonder")], tools: [] });

const { sdk, executable } = await loadSdk(args.sdk ?? join(here, ".."));
const results = {};
for (const name of only) {
  const command = COMMANDS[name];
  const outputs = [];
  const options = { ...baseOptions({ executable }), cwd: scratch, model: HAIKU_MODEL, maxTurns: 4, persistSession: false,
    settingSources: [], sandbox: policy.sandbox(), tools: ["Bash"], permissionMode: "default",
    systemPrompt: "You run one shell command with the Bash tool exactly as given, then reply with the single word DONE. Never retry, never modify the command, never set dangerouslyDisableSandbox.",
    // The runtime's own decision: Bash runs without asking in Full access, and
    // Read only's in-sandbox runs are approved here; a sandbox escape is refused.
    canUseTool: async (tool, input) => input.dangerouslyDisableSandbox
      ? { behavior: "deny", message: "sandbox escape refused" }
      : (await policy.decision(tool, input)) === "deny" ? { behavior: "deny", message: "denied by policy" } : { behavior: "allow", updatedInput: input } };
  for await (const message of sdk.query({ prompt: `Run exactly this Bash command: ${command}`, options })) {
    if (message.type === "user" && Array.isArray(message.message?.content))
      for (const part of message.message.content) if (part.type === "tool_result")
        outputs.push({ error: part.is_error === true, text: typeof part.content === "string" ? part.content : JSON.stringify(part.content) });
  }
  results[name] = { command, outputs };
  console.log(`## ${name}: ${command}\n${outputs.map(o => `${o.error ? "[error] " : ""}${o.text}`).join("\n---\n") || "(no tool result)"}\n`);
}
console.log(JSON.stringify({ mode, scratch, sandbox: policy.sandbox() }, null, 1));

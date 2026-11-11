import test from "node:test";
import assert from "node:assert/strict";
import { execFile, execFileSync } from "node:child_process";
import { mkdtemp, mkdir, readFile, writeFile, symlink, rm, stat, realpath } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, sep } from "node:path";
const within = (path, root) => path === root || path.startsWith(`${root}${sep}`);
import { MACH_SERVICES, NESTED_SANDBOX_GUIDANCE, ToolPolicy } from "../permissions.mjs";

// Contract: model-supplied paths cannot turn an approved folder into access to
// a denied or unrelated folder, including when creating a file through a link.
test("Project folders enforce lexical and canonical scope for reads and new writes", async t => {
  const root = await mkdtemp(join(tmpdir(), "wonder-policy-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const cwd = join(root, "allowed"), other = join(root, "other"), denied = join(cwd, "secrets");
  await Promise.all([mkdir(denied, { recursive: true }), mkdir(other)]);
  await writeFile(join(other, "secret"), "private");
  await symlink(other, join(cwd, "escape"));
  const policy = new ToolPolicy({ cwd, mode: "workspace", approvalMode: "ask", readRoots: [cwd], writeRoots: [cwd], deniedRoots: [denied], commandIsolation: "seatbelt" });
  assert.equal(await policy.permits("new/file.txt", true), true);
  assert.equal(await policy.permits("escape/secret"), false);
  assert.equal(await policy.permits("escape/new.txt", true), false);
  assert.equal(await policy.permits("../other/secret"), false);
  assert.equal(await policy.permits("secrets/new.txt", true), false);
  assert.equal(await policy.decision("Write", { file_path: "new.txt" }), "ask");
  assert.equal(await policy.decision("FuturePowerTool", {}), "deny");
  assert.equal(await policy.decision("StructuredOutput", {}), "deny");
  // Claude in Chrome asks like any other MCP tool.
  assert.equal(await policy.decision("mcp__claude-in-chrome__navigate", {}), "ask");
  // Existing installations keep a legacy alias to the protected data root.
  // Its canonical scope must allow the same owned workspace, but no sibling.
  const legacy = join(root, "legacy");
  await symlink(root, legacy);
  const privateWorkspace = new ToolPolicy({ cwd, workspace: cwd, mode: "workspace", approvalMode: "ask", readRoots: [root], writeRoots: [cwd], deniedRoots: [root, legacy, denied] });
  assert.equal(await privateWorkspace.permits(cwd), true);
  assert.equal(await privateWorkspace.permits("new.txt", true), true);
  assert.equal(await privateWorkspace.permits("secrets/new.txt", true), false);
  assert.equal(await privateWorkspace.permits("../other/secret"), false);
  assert.equal(await privateWorkspace.permits(join(legacy, "other/secret")), false);
  assert.equal(await privateWorkspace.permits("escape/secret"), false);
  assert.equal(await policy.decision("Bash", { dangerouslyDisableSandbox: true }), "deny");
});
test("read-only mode cannot be widened by full-access approval", async () => {
  const policy = new ToolPolicy({ cwd: "/tmp", mode: "read_only", approvalMode: "full_access", readRoots: ["/tmp"], writeRoots: ["/tmp"], deniedRoots: [] });
  assert.equal(await policy.permits("new.txt", true), false);

});
// Contract: Projects run commands without a sandbox, as Claude Code does,
// except Read only, whose sandbox confines $HOME to the project and toolchains,
// reaches common developer hosts and keeps protected folders closed. Live behavior is proven by
// scripts/claude-sdk-smoke/access-acceptance.mjs.
test("Projects run commands without a sandbox; Read only retains it", async t => {
  const root = await mkdtemp(join(tmpdir(), "wonder-sandbox-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const home = join(root, "home"), app = join(home, "app"), data = join(home, ".wonder"), ssh = join(home, ".ssh");
  const control = join(data, "wonder.sqlite3");
  await Promise.all([app, data, ssh].map(path => mkdir(path, { recursive: true })));
  await writeFile(control, "");
  const projectPolicy = modes => new ToolPolicy({ cwd: app, readRoots: ["/"], writeRoots: modes.mode === "read_only" ? [] : [app],
    deniedRoots: [data, ssh], projectRoots: [app], home, ...modes });
  for (const modes of [{ mode: "workspace", approvalMode: "ask" }, { mode: "workspace", approvalMode: "auto" },
    { mode: "workspace", approvalMode: "auto", planMode: true }, { mode: "full_access", approvalMode: "full_access" }])
    { const box = projectPolicy(modes).sandbox();
      assert.deepEqual([box.enabled, box.failIfUnavailable, box.allowUnsandboxedCommands], [true, true, false], JSON.stringify(modes)); }
  // Read only is enforced by the sandbox, with the developer-friendly Project layout.
  const readOnly = projectPolicy({ mode: "read_only", approvalMode: "ask" }).sandbox();
  assert.equal(readOnly.enabled, true);
  assert.equal(readOnly.allowUnsandboxedCommands, true);
  assert.ok(readOnly.network.allowedDomains.includes("github.com"));
  assert.equal(readOnly.network.allowLocalBinding, true);
  for (const denied of [home, data, ssh]) assert.ok(readOnly.filesystem.denyRead.includes(denied));
  for (const readable of [app, join(home, ".cargo"), join(home, ".gitconfig")]) assert.ok(readOnly.filesystem.allowRead.includes(readable));
  assert.ok(!readOnly.filesystem.allowRead.some(path => path === home || path.startsWith(ssh) || path.startsWith(data)));
  assert.ok(readOnly.filesystem.allowWrite.includes("/tmp"));
  assert.ok(!readOnly.filesystem.allowWrite.includes(app));
  assert.ok(readOnly.filesystem.denyWrite.includes(app));

});
test("delegated work inherits the selected model", async () => {
  const policy = new ToolPolicy({ cwd: "/tmp", mode: "read_only", approvalMode: "ask", readRoots: [], writeRoots: [], deniedRoots: [] });
  const result = await policy.beforeTool({ tool_name: "Agent", tool_input: { prompt: "Check", model: "opus" } });
  assert.deepEqual(result.hookSpecificOutput.updatedInput, { prompt: "Check" });
});
test("a helper agent can hand its result back in every access mode", async () => {
  for (const [mode, approvalMode] of [["read_only", "ask"], ["workspace", "ask"], ["full_access", "full_access"]]) {
    const policy = new ToolPolicy({ cwd: "/tmp", mode, approvalMode, readRoots: [], writeRoots: [], deniedRoots: [] });
    assert.equal(await policy.decision("SubagentHandback", { result: "Done" }), "allow", mode);
  }
});

// Contract: Cupola's approval modes are decided in one table. Auto and Full
// access still send Bash through canUseTool (the hook only ever answers "ask"),
// leaving the sandbox always asks the owner, and plan mode forbids every write
// even when the owner's approval mode would allow it.
test("approval modes and plan mode decide each tool once", async t => {
  const root = await mkdtemp(join(tmpdir(), "wonder-modes-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const cwd = join(root, "app"), outside = join(root, "other");
  await Promise.all([mkdir(cwd), mkdir(outside)]);
  const policy = (approvalMode, planMode = false, mode = approvalMode === "full_access" ? "full_access" : "workspace") =>
    new ToolPolicy({ cwd, mode, approvalMode, planMode, readRoots: ["/"], writeRoots: [cwd], deniedRoots: [], project: true });
  const write = { file_path: join(cwd, "a.txt") }, notebook = { notebook_path: join(cwd, "n.ipynb") };
  const decisions = async p => ({ write: await p.decision("Write", write), edit: await p.decision("Edit", write), notebook: await p.decision("NotebookEdit", notebook),
    outside: await p.decision("Write", { file_path: join(outside, "a.txt") }), bash: await p.decision("Bash", { command: "ls" }),
    powershell: await p.decision("PowerShell", { command: "Get-ChildItem" }),
    unsandboxed: await p.decision("Bash", { command: "ls", dangerouslyDisableSandbox: true }),
    web: await p.decision("WebFetch", {}), mcp: await p.decision("mcp__claude_ai_Gmail__list_labels", {}), exit: await p.decision("ExitPlanMode", { plan: "x" }) });
  const asks = { write: "ask", edit: "ask", notebook: "ask", outside: "deny", bash: "ask", powershell: "ask", unsandboxed: "deny", web: "ask", mcp: "ask", exit: "ask" };
  const edits = { write: "allow", edit: "allow", notebook: "allow" };
  assert.deepEqual(await decisions(policy("ask")), asks);
  assert.deepEqual(await decisions(policy("accept_edits")), { ...asks, ...edits });
  // Auto asks only when Claude Code's classifier escalates; see the hook below.
  assert.deepEqual(await decisions(policy("auto")), { ...asks, ...edits });
  // Full access: no file scope, no sandbox and no prompts.
  assert.deepEqual(await decisions(policy("full_access")), { ...asks, ...edits, outside: "allow", bash: "allow", powershell: "allow", unsandboxed: "deny", web: "allow" });
  // Planning: no write in any approval mode, no automatic command or web access.
  const planning = { ...asks, write: "deny", edit: "deny", notebook: "deny", exit: "deny" };
  for (const approvalMode of ["ask", "accept_edits", "auto", "full_access"])
    assert.deepEqual(await decisions(policy(approvalMode, true)), planning, approvalMode);
  // A planning thread cannot ask for or run with a secret, whatever its access.
  for (const approvalMode of ["ask", "accept_edits", "auto", "full_access"])
    for (const tool of ["wonder_request_secret", "wonder_run_with_secret"]) {
      assert.equal(await policy(approvalMode, true).decision(`mcp__wonder__${tool}`, {}), "deny", `${approvalMode} ${tool}`);
      assert.equal(await policy(approvalMode).decision(`mcp__wonder__${tool}`, {}), "ask", `${approvalMode} ${tool} outside plan`);
    }
  assert.match(policy("full_access", true).denial("mcp__wonder__wonder_run_with_secret"), /secret/);
  assert.equal(policy("auto").autoRunsCommands, false);
  assert.equal(policy("full_access").autoRunsCommands, true);
  assert.equal(policy("auto", true).autoRunsCommands, false);
  // Claude Code's own mode runs underneath: Auto uses its classifier.
  assert.deepEqual(["ask", "accept_edits", "auto", "full_access"].map(mode => policy(mode).permissionMode), ["default", "default", "auto", "default"]);
  assert.equal(policy("auto", true).permissionMode, "plan");
  // The hook never allows Bash itself: Claude Code decides commands and web
  // access unless nothing asks, and never lets a write through in plan mode.
  for (const approvalMode of ["ask", "accept_edits", "auto", "full_access"]) {
    for (const tool of [["Bash", { command: "ls" }], ["PowerShell", { command: "Get-ChildItem" }], ["WebFetch", { url: "https://example.com" }]]) {
      const hook = await policy(approvalMode).beforeTool({ tool_name: tool[0], tool_input: tool[1] });
      assert.deepEqual(hook, approvalMode === "full_access"
        ? { hookSpecificOutput: { hookEventName: "PreToolUse", permissionDecision: tool[0] === "WebFetch" ? "allow" : "ask" } } : {}, `${approvalMode} ${tool[0]}`);
      assert.deepEqual(await policy(approvalMode, true).beforeTool({ tool_name: tool[0], tool_input: tool[1] }), {}, approvalMode);
    }
    const planned = await policy(approvalMode, true).beforeTool({ tool_name: "Write", tool_input: write });
    assert.equal(planned.hookSpecificOutput.permissionDecision, "deny", approvalMode);
    assert.match(planned.hookSpecificOutput.permissionDecisionReason, /Plan mode: describe the change instead of editing/);
  }
  assert.equal((await policy("accept_edits").beforeTool({ tool_name: "Edit", tool_input: write })).hookSpecificOutput.permissionDecision, "allow");
  const review = await policy("ask", true).beforeTool({ tool_name: "ExitPlanMode", tool_input: { plan: "x" } });
  assert.equal(review.hookSpecificOutput.permissionDecision, "deny");
  assert.match(review.hookSpecificOutput.permissionDecisionReason, /owner reviews the plan in Cupola/);
  assert.match(policy("ask").denial("Write"), /outside this conversation’s allowed tools/);
  // Windows changes isolation only. The same approval/plan table above still
  // owns every tool, and unsupported isolation cannot prevent SDK startup.
  for (const mode of ["ask", "accept_edits", "auto", "full_access"]) {
    for (const planning of [false, true]) {
      const box = policy(mode, planning).sandbox("win32");
      assert.equal(box.enabled, false);
      assert.equal(box.failIfUnavailable, false);
      assert.equal(box.allowUnsandboxedCommands, true);
    }
  }
  assert.equal(policy("ask", false, "read_only").sandbox("win32").failIfUnavailable, false);
});
test("unsupported approval modes fail closed instead of widening access", () => {
  const scope = { cwd: "/tmp", readRoots: [], writeRoots: [], deniedRoots: [] };
  assert.doesNotThrow(() => new ToolPolicy({ ...scope, mode: "workspace", approvalMode: "auto", planMode: true }));
  for (const bad of [{ mode: "workspace", approvalMode: "yolo" }, { mode: "workspace", approvalMode: "ask", planMode: "yes" },
    // Accepting edits or running commands automatically refines Workspace only.
    { mode: "read_only", approvalMode: "auto" }, { mode: "full_access", approvalMode: "accept_edits" }])
    assert.throws(() => new ToolPolicy({ ...scope, ...bad }), /unsupported/);
});

// Contract owner: the command policy. Leaving the sandbox or reaching a new
// host always asks the owner in a sandboxed Project and is fenced by plan mode;
// Full access has no sandbox. Claude Code settings files cannot be edited
// silently, and Claude's file tools share the sandbox's view of $HOME.
test("Projects ask through Claude Code instead of a sandbox and settings edits still ask", async t => {
  const root = await mkdtemp(join(tmpdir(), "wonder-escape-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const home = join(root, "home"), app = join(home, "app");
  for (const path of [join(app, ".claude"), join(home, "Documents"), join(home, ".cargo")]) await mkdir(path, { recursive: true });
  await writeFile(join(home, "Documents", "private.txt"), "private");
  const base = { cwd: app, mode: "workspace", approvalMode: "auto", projectRoots: [app], home,
    readRoots: ["/"], writeRoots: [app], deniedRoots: [] };
  const outside = { command: "git push", dangerouslyDisableSandbox: true };
  const decide = async (change, name, input) => new ToolPolicy({ ...base, ...change }).decision(name, input);
  // What reaches canUseTool from Claude Code asks the owner; Full access never asks.
  assert.equal(await decide({}, "Bash", { command: "ls" }), "ask");
  // Commands never retry outside the sandbox in a Project.
  assert.equal(await decide({}, "Bash", outside), "deny");
  assert.equal(await decide({ planMode: true }, "Bash", outside), "deny");
  assert.equal(await decide({ mode: "full_access", approvalMode: "full_access" }, "Bash", outside), "deny");
  // Read only keeps its sandbox and may leave it only with approval.
  assert.equal(await decide({ mode: "read_only", approvalMode: "ask", writeRoots: [] }, "Bash", outside), "ask");
  for (const change of [{ mode: "read_only", approvalMode: "ask", writeRoots: [], planMode: true }])
    assert.equal(await decide(change, "Bash", outside), "deny");
  assert.equal(await decide({}, "SandboxNetworkAccess", { host: "example.com" }), "ask");
  const settings = { file_path: join(app, ".claude", "settings.local.json"), content: "{}" };
  assert.equal(await decide({}, "Write", settings), "ask");
  assert.equal(await decide({}, "Write", { file_path: join(app, "notes.md"), content: "" }), "allow");
  assert.equal(await decide({ mode: "full_access", approvalMode: "full_access" }, "Write", settings), "allow");
  // As in Claude Code, Read covers the disk except protected folders; only
  // a sandboxed Read only Project confines $HOME.
  const documents = { file_path: join(home, "Documents", "private.txt") };
  assert.equal(await decide({}, "Read", documents), "allow");
  const readOnly = { mode: "read_only", approvalMode: "ask", writeRoots: [] };
  assert.equal(await decide(readOnly, "Read", documents), "deny");
  assert.equal(await decide(readOnly, "Read", { file_path: join(home, ".cargo") }), "allow");
  assert.equal(await decide({ mode: "full_access", approvalMode: "full_access" }, "Read", documents), "allow");
});

// Contract: no spelling of a Claude Code configuration file edits it without
// the owner: letter case, `.`/`..` segments, a link to it, a link named like
// it, the other files that carry hooks or commands, and project MCP servers.
// A target whose location cannot be read is refused.
test("Claude Code configuration edits ask under every spelling", async t => {
  const root = await mkdtemp(join(tmpdir(), "wonder-config-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const app = join(root, "app"), config = join(app, ".claude");
  await mkdir(join(app, "docs"), { recursive: true }); await mkdir(config);
  await writeFile(join(config, "settings.json"), "{}"); await writeFile(join(app, "notes.json"), "{}");
  await symlink(config, join(app, "alias"));
  await symlink(join(config, "settings.json"), join(app, "innocent.json"));
  await mkdir(join(app, "other", ".claude"), { recursive: true });
  await symlink(join(app, "notes.json"), join(app, "other", ".claude", "settings.json"));
  await symlink(join(app, "loop"), join(app, "loop"));
  // Measured bypass: a link to a settings file that does not exist yet was
  // read as an ordinary new file and allowed; writing through it creates the
  // settings file. Same for a link to nothing inside a protected folder.
  await symlink(join(config, "settings.later.json"), join(app, "dangling.json"));
  await symlink(join(root, "missing", ".claude"), join(app, "ghost"));
  await mkdir(join(root, "protected"));
  await symlink(join(root, "protected", "planted.sh"), join(app, "drop.sh"));
  const configuration = [".claude/settings.json", "./.claude/./settings.json", "docs/../.claude/settings.local.json",
    ".CLAUDE/Settings.JSON", join(config, "settings.local.json"), ".claude/settings.team.json", ".claude/hooks/check.sh",
    ".claude/agents/reviewer.md", ".claude/Commands/ship.md", ".claude/skills/deploy/SKILL.md", ".mcp.json", "docs/.MCP.json",
    // Through a link to the folder, a link to the file, and a settings path that is itself a link.
    "alias/settings.json", "innocent.json", "other/.claude/settings.json"];
  for (const approvalMode of ["accept_edits", "auto"]) {
    const policy = new ToolPolicy({ cwd: app, mode: "workspace", approvalMode, readRoots: ["/"], writeRoots: [app], deniedRoots: [] });
    for (const path of configuration) {
      assert.equal(await policy.decision("Write", { file_path: path, content: "{}" }), "ask", `${approvalMode} Write ${path}`);
      assert.equal(await policy.decision("Edit", { file_path: path, old_string: "{", new_string: "{" }), "ask", `${approvalMode} Edit ${path}`);
    }
    assert.equal(await policy.decision("NotebookEdit", { notebook_path: ".claude/hooks/run.ipynb" }), "ask", approvalMode);
    for (const path of ["notes.json", "docs/settings.json", ".claude/CLAUDE.md", ".claude/worktrees/x/src/main.rs"])
      assert.equal(await policy.decision("Write", { file_path: path, content: "" }), "allow", `${approvalMode} ${path}`);
    assert.equal(await policy.decision("Write", { file_path: "loop/settings.json", content: "{}" }), "deny", approvalMode);
    assert.equal(await policy.decision("Write", { file_path: "loop/.claude/settings.json", content: "{}" }), "deny", approvalMode);
    // Whatever cannot be fully resolved is never allowed.
    for (const path of ["dangling.json", "ghost/settings.json", "drop.sh"])
      for (const tool of ["Write", "Edit"]) assert.notEqual(await policy.decision(tool, { file_path: path, content: "{}" }), "allow", `${approvalMode} ${tool} ${path}`);
    const guarded = new ToolPolicy({ cwd: app, mode: "workspace", approvalMode, readRoots: ["/"], writeRoots: [app], deniedRoots: [join(root, "protected")] });
    assert.equal(await guarded.decision("Write", { file_path: "drop.sh", content: "" }), "deny", approvalMode);
  }
  // Nothing asks in Full access.
  const full = new ToolPolicy({ cwd: app, mode: "full_access", approvalMode: "full_access", readRoots: ["/"], writeRoots: [app], deniedRoots: [] });
  assert.equal(await full.decision("Write", { file_path: "alias/settings.json", content: "{}" }), "allow");
});

// Contract (security): Claude Code itself runs unconfined, and only its shell
// commands are sandboxed. In a Project the sandbox settings close every
// protected folder and the account's keychains to commands for reading and
// writing, forbid retrying a command outside the sandbox, and fail rather than
// run unsandboxed; Claude's file tools are refused the same folders.
test("Project command sandbox closes Cupola's folders and the keychains", async t => {
  const root = await mkdtemp(join(tmpdir(), "wonder-commands-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const home = join(root, "home"), data = join(home, ".cupola"), alias = join(home, ".wonder"), keychains = join(home, "Library", "Keychains");
  const workspace = join(data, "threads", "t1"), app = join(root, "app");
  for (const path of [workspace, app, keychains]) await mkdir(path, { recursive: true });
  await symlink(data, alias);
  await Promise.all([writeFile(join(data, "host-key"), "secret"), writeFile(join(keychains, "login.keychain-db"), "secret")]);
  const policy = (change = {}) => new ToolPolicy({ cwd: workspace, workspace, mode: "workspace", approvalMode: "auto", readRoots: ["/"], writeRoots: [workspace],
    deniedRoots: [data, alias], home, ...change });
  for (const change of [{}, { approvalMode: "ask" }, { mode: "full_access", approvalMode: "full_access" }]) {
    const sandbox = policy(change).sandbox();
    assert.equal(sandbox.enabled, true, JSON.stringify(change));
    assert.equal(sandbox.failIfUnavailable, true);
    assert.equal(sandbox.allowUnsandboxedCommands, false);
    assert.equal(sandbox.autoAllowBashIfSandboxed, false);
    for (const path of [data, alias, keychains]) assert.ok(sandbox.filesystem.denyRead.includes(path), `read ${path}`);
    // This conversation's own folder inside the data folder stays readable and writable.
    assert.ok(sandbox.filesystem.allowRead.includes(workspace));
    for (const path of [data, alias, keychains]) assert.ok(sandbox.filesystem.denyWrite.includes(path) || sandbox.filesystem.denyWrite.some(entry => within(entry, path)), `write ${path}`);
    assert.ok(!sandbox.filesystem.denyWrite.includes(workspace));
    assert.ok(sandbox.filesystem.denyWrite.includes(join(data, "host-key")), "entries around the owned folder");
    // Developer commands (simctl, open, FSEvents) get their Mach services; the keychain's stay closed.
    assert.deepEqual(sandbox.network.allowMachLookup, MACH_SERVICES);
    for (const name of ["com.apple.FSEvents", "com.apple.CoreSimulator.CoreSimulatorService", "com.apple.runningboard"]) assert.ok(MACH_SERVICES.includes(name), name);
    assert.ok(!MACH_SERVICES.some(name => /SecurityServer|securityd|secd|trustd|keychain/i.test(name)), "no keychain service");
  }
  // Read only keeps the narrower list: it never launches apps or simulators.
  assert.deepEqual(policy({ mode: "read_only", approvalMode: "ask", writeRoots: [] }).sandbox().network.allowMachLookup, ["com.apple.FSEvents"]);
  // The agent is told why gh and git credentials fail, so it does not retry blindly.
  assert.match(NESTED_SANDBOX_GUIDANCE, /keychains are closed/);
  assert.match(NESTED_SANDBOX_GUIDANCE, /could not read Username/);
  // A link to the data folder or keychains does not open them.
  const link = join(root, "link"); await symlink(keychains, link);
  assert.ok(policy({ deniedRoots: [data, link] }).sandbox().filesystem.denyRead.includes(keychains));
  // Retrying outside the sandbox is refused, not asked, in a Project.
  assert.equal(await policy({ mode: "full_access", approvalMode: "full_access" }).decision("Bash", { command: "ls", dangerouslyDisableSandbox: true }), "deny");
  // File tools are checked too, since the sandbox does not cover them.
  for (const target of [join(keychains, "login.keychain-db"), join(data, "host-key"), join(alias, "host-key")]) {
    assert.equal(await policy({ mode: "full_access", approvalMode: "full_access" }).decision("Read", { file_path: target }), "deny", target);
    assert.equal(await policy({ mode: "full_access", approvalMode: "full_access" }).decision("Write", { file_path: target, content: "x" }), "deny", target);
  }
  assert.equal(await policy({ mode: "full_access", approvalMode: "full_access" }).decision("Grep", { path: join(home, "Library") }), "deny");
  // Claude Code reads its own configuration; no setting is refused here.
  assert.deepEqual(policy().settingSources, ["user", "project", "local"]);
  assert.deepEqual(policy({ mode: "read_only", approvalMode: "ask", writeRoots: [] }).settingSources, []);
});

// macOS reaches every path on the user's volume a second way, through the
// /System/Volumes/Data firmlink. File tools must refuse Cupola's data, the
// credential folders and the keychains under that spelling too, and an alias
// of the home folder must not slip past Read only's home confinement.
test("File tools refuse protected folders under the macOS firmlink spelling", { skip: process.platform !== "darwin" }, async t => {
  const FIRMLINK = "/System/Volumes/Data";
  const root = await realpath(await mkdtemp(join(tmpdir(), "wonder-firmlink-")));
  t.after(() => rm(root, { recursive: true, force: true }));
  const home = join(root, "home"), data = join(home, ".cupola"), keychains = join(home, "Library", "Keychains");
  const workspace = join(data, "threads", "t1"), app = join(root, "app");
  for (const path of [workspace, app, keychains]) await mkdir(path, { recursive: true });
  await Promise.all([writeFile(join(data, "host-key"), "secret"), writeFile(join(keychains, "login.keychain-db"), "secret"), writeFile(join(app, "ok.txt"), "ok")]);
  const alias = path => FIRMLINK + path;
  await stat(alias(join(data, "host-key"))); // the spelling really opens the file
  const policy = (change = {}) => new ToolPolicy({ cwd: app, workspace: app, mode: "full_access", approvalMode: "full_access", readRoots: ["/"], writeRoots: [app],
    deniedRoots: [data], home, ...change });
  for (const target of [join(data, "host-key"), join(keychains, "login.keychain-db")]) {
    for (const spelling of [target, alias(target)]) {
      assert.equal(await policy().decision("Read", { file_path: spelling }), "deny", `Read ${spelling}`);
      assert.equal(await policy().decision("Write", { file_path: spelling, content: "x" }), "deny", `Write ${spelling}`);
    }
  }
  for (const folder of [data, keychains, join(home, "Library"), home]) {
    for (const tool of ["Grep", "Glob"]) assert.equal(await policy().decision(tool, { path: alias(folder) }), "deny", `${tool} ${alias(folder)}`);
  }
  // Workspace and Read only reach the same answers; a grant under the alias still works.
  for (const change of [{ mode: "workspace", approvalMode: "auto" }, { mode: "read_only", approvalMode: "ask", writeRoots: [] }]) {
    assert.equal(await policy(change).decision("Read", { file_path: alias(join(data, "host-key")) }), "deny", JSON.stringify(change));
  }
  assert.equal(await policy().decision("Read", { file_path: alias(join(app, "ok.txt")) }), "allow");
  // Read only confines $HOME to the project and toolchains under either spelling.
  const confined = policy({ mode: "read_only", approvalMode: "ask", writeRoots: [], projectRoots: [app], readRoots: ["/"] });
  await mkdir(join(home, "notes"), { recursive: true }); await writeFile(join(home, "notes", "n.txt"), "n");
  assert.equal(await confined.decision("Read", { file_path: join(home, "notes", "n.txt") }), "deny");
  assert.equal(await confined.decision("Read", { file_path: alias(join(home, "notes", "n.txt")) }), "deny");
  // The command sandbox is given both spellings so Seatbelt matches either.
  const sandbox = policy().sandbox();
  for (const path of [data, keychains]) assert.ok(sandbox.filesystem.denyRead.includes(alias(path)), `denyRead ${alias(path)}`);
});

// Contract: other agents' repository configuration is treated like Claude
// Code's own. OpenCode, Cursor and Codex load plugins, hooks, MCP servers,
// custom agents/commands and permission settings from these files, so in Accept
// edits and Auto a write to them asks first, under every spelling. Notes and
// instruction files (AGENTS.md, rules) stay ordinary edits.
test("Other agents' configuration edits ask like Claude Code's own", async t => {
  const root = await mkdtemp(join(tmpdir(), "wonder-agent-config-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const app = join(root, "app");
  for (const folder of [".opencode/plugin", ".opencode/node_modules/x", ".cursor/rules", ".cursor/commands", ".codex/rules", ".codex/prompts", ".config/opencode/plugin", "docs"])
    await mkdir(join(app, folder), { recursive: true });
  await writeFile(join(app, ".cursor", "mcp.json"), "{}"); await writeFile(join(app, "notes.json"), "{}");
  await symlink(join(app, ".cursor"), join(app, "cursor-alias"));
  await symlink(join(app, ".cursor", "mcp.json"), join(app, "innocent.json"));
  await symlink(join(app, ".codex", "config.toml"), join(app, "dangling.toml"));
  const configuration = ["opencode.json", "OpenCode.jsonc", "docs/opencode.json", ".opencode/opencode.json", ".opencode/plugin/hook.ts",
    ".opencode/agent/reviewer.md", ".opencode/agents/reviewer.md", ".opencode/command/ship.md", ".opencode/tools/run.ts", ".OPENCODE/Plugins/x.js",
    ".config/opencode/opencode.json", ".config/opencode/plugin/x.js",
    ".cursor/mcp.json", ".cursor/hooks.json", ".cursor/cli.json", ".cursor/environment.json", ".cursor/commands/ship.md", ".cursor/agents/a.md", ".CURSOR/MCP.JSON",
    ".codex/config.toml", ".codex/hooks.json", ".codex/rules/default.rules", ".codex/prompts/p.md", ".codex/agents/a.toml", ".codex/skills/s/SKILL.md",
    "docs/../.cursor/mcp.json", "cursor-alias/mcp.json", "innocent.json"];
  for (const approvalMode of ["accept_edits", "auto"]) {
    const policy = new ToolPolicy({ cwd: app, mode: "workspace", approvalMode, readRoots: ["/"], writeRoots: [app], deniedRoots: [] });
    for (const path of configuration) {
      assert.equal(await policy.decision("Write", { file_path: path, content: "{}" }), "ask", `${approvalMode} Write ${path}`);
      assert.equal(await policy.decision("Edit", { file_path: path, old_string: "{", new_string: "{" }), "ask", `${approvalMode} Edit ${path}`);
    }
    assert.equal(await policy.decision("NotebookEdit", { notebook_path: ".codex/prompts/run.ipynb" }), "ask", approvalMode);
    // A link to a config file that does not exist yet is refused: writing through it would create the file.
    assert.notEqual(await policy.decision("Write", { file_path: "dangling.toml", content: "" }), "allow", approvalMode);
    for (const path of ["notes.json", "AGENTS.md", "docs/opencode.json.md", ".cursor/rules/style.mdc", ".cursorrules", ".codex/notes.md",
      ".opencode/node_modules/x/index.js", "src/cursor/mcp.json", ".config/other/opencode-notes.md"])
      assert.equal(await policy.decision("Write", { file_path: path, content: "" }), "allow", `${approvalMode} ${path}`);
  }
  // Nothing asks in Full access, and Ask mode already asks for every edit.
  const full = new ToolPolicy({ cwd: app, mode: "full_access", approvalMode: "full_access", readRoots: ["/"], writeRoots: [app], deniedRoots: [] });
  assert.equal(await full.decision("Write", { file_path: ".cursor/mcp.json", content: "{}" }), "allow");
  const ask = new ToolPolicy({ cwd: app, mode: "workspace", approvalMode: "ask", readRoots: ["/"], writeRoots: [app], deniedRoots: [] });
  assert.equal(await ask.decision("Write", { file_path: "notes.json", content: "" }), "ask");
});

// Contract: Glob and Grep run outside the command sandbox, so they are judged
// by the folder they really search. An absolute (or `~`, or `..`) Glob pattern
// is its own search root whatever `path` says, and `path` is judged after the
// trimming and `~` expansion Claude Code applies before it opens anything.
test("Glob and Grep are judged by the directory they actually search", async t => {
  const root = await realpath(await mkdtemp(join(tmpdir(), "wonder-search-")));
  t.after(() => rm(root, { recursive: true, force: true }));
  const home = join(root, "home"), data = join(home, ".cupola"), keychains = join(home, "Library", "Keychains"), app = join(home, "app");
  for (const path of [data, keychains, join(app, "src")]) await mkdir(path, { recursive: true });
  const policy = new ToolPolicy({ cwd: app, workspace: app, mode: "full_access", approvalMode: "full_access", readRoots: ["/"], writeRoots: [app],
    deniedRoots: [data], home });
  const refused = [
    // The pattern is the searched directory.
    { tool: "Glob", input: { pattern: `${data}/**/*` } },
    { tool: "Glob", input: { pattern: `${data}/*.json`, path: app } },
    { tool: "Glob", input: { pattern: join(keychains, "*.keychain-db") } },
    { tool: "Glob", input: { pattern: `${home}/**/host-key` } },
    { tool: "Glob", input: { pattern: "/**/host-key" } },
    { tool: "Glob", input: { pattern: "~/.cupola/**" } },
    { tool: "Glob", input: { pattern: "~/Library/Keychains/*" } },
    { tool: "Glob", input: { pattern: "../.cupola/**/*", path: app } },
    { tool: "Glob", input: { pattern: "src/../../.cupola/*" } },
    { tool: "Glob", input: { pattern: `  ${data}/**  ` } },
    // The path is judged as Claude Code will open it.
    { tool: "Grep", input: { pattern: "key", path: ` ${data} ` } },
    { tool: "Grep", input: { pattern: "key", path: `${data}\n` } },
    { tool: "Grep", input: { pattern: "key", path: "~/.cupola" } },
    { tool: "Grep", input: { pattern: "key", path: "~" } },
    { tool: "Grep", input: { pattern: "key", path: "~/Library" } },
    { tool: "Glob", input: { pattern: "*", path: "~/.cupola/" } },
    { tool: "Grep", input: { pattern: "key", path: "" } },
  ];
  for (const { tool, input } of refused) assert.equal(await policy.decision(tool, input), "deny", `${tool} ${JSON.stringify(input)}`);
  const allowed = [
    { tool: "Glob", input: { pattern: "**/*.js" } },
    { tool: "Glob", input: { pattern: "src/**/*.ts" } },
    { tool: "Glob", input: { pattern: `${app}/src/**` } },
    { tool: "Glob", input: { pattern: "~/app/**/*.md" } },
    { tool: "Glob", input: { pattern: "*.md", path: " src " } },
    { tool: "Grep", input: { pattern: "key", path: "~/app" } },
    { tool: "Grep", input: { pattern: "key", path: " src\n", glob: "*.ts" } },
    { tool: "Grep", input: { pattern: "key" } },
  ];
  for (const { tool, input } of allowed) assert.equal(await policy.decision(tool, input), "allow", `${tool} ${JSON.stringify(input)}`);
  // File tools expand the same way.
  for (const file_path of ["~/.cupola/host-key", ` ${join(data, "host-key")} `])
    assert.equal(await policy.decision("Read", { file_path }), "deny", file_path);
});

// What Claude Code's sandbox does with a denied entry on macOS: an entry with
// `*`, `?` or `[` is a Seatbelt pattern (its translation is mirrored here), any
// other entry is a folder and everything below it.
const sandboxRegex = glob => "^" + glob.replace(/[.^$+{}()|\\]/g, "\\$&").replace(/\[([^\]]*?)$/g, "\\[$1")
  .replace(/\*\*\//g, "__GLOBSTAR_SLASH__").replace(/\*\*/g, "__GLOBSTAR__").replace(/\*/g, "[^/]*").replace(/\?/g, "[^/]")
  .replace(/__GLOBSTAR_SLASH__/g, "(.*/)?").replace(/__GLOBSTAR__/g, ".*") + "(/.*)?$";
const sandboxPattern = glob => new RegExp(sandboxRegex(glob), "u");
const sandboxDenies = (entries, path) => entries.some(entry => /[*?[]/.test(entry) ? sandboxPattern(entry).test(path) : path === entry || path.startsWith(`${entry}/`));

// Contract (security): the command sandbox keeps writes out of Cupola's data
// folder as a whole. A conversation's own folder lives inside it and stays
// writable, but a name nothing listed at session start (another conversation's
// folder, a file wonderd adds later) is refused too, not only the entries that
// existed then.
test("Command sandbox write protection covers Cupola's data folder entries created later", async t => {
  const root = await realpath(await mkdtemp(join(tmpdir(), "wonder-later-")));
  t.after(() => rm(root, { recursive: true, force: true }));
  const home = join(root, "home"), data = join(home, ".cupola"), workspace = join(data, "project-media", "t1-abc");
  for (const path of [workspace, join(data, "project-media", "t0-old"), join(home, "elsewhere")]) await mkdir(path, { recursive: true });
  await writeFile(join(data, "host-key"), "secret");
  for (const change of [{ mode: "workspace", approvalMode: "auto" }, { mode: "full_access", approvalMode: "full_access" }, { mode: "read_only", approvalMode: "ask", writeRoots: [] }]) {
    const policy = new ToolPolicy({ cwd: workspace, workspace, readRoots: ["/"], writeRoots: [workspace], deniedRoots: [data], home, ...change });
    const { denyWrite } = policy.sandbox().filesystem;
    // Claude Code mangles a control character in a path, which would silently loosen a pattern.
    assert.ok(denyWrite.every(entry => !/[\u0000-\u001f\u007f-\u009f]/.test(entry)), "no control characters in denied entries");
    const spellings = process.platform === "darwin" ? [data, `/System/Volumes/Data${data}`] : [data];
    for (const base of spellings) {
      const at = (...parts) => join(base, ...parts);
      for (const created of [at("host-key"), at("new-secret"), at("new-dir", "x"), at("Zeta"), at("-dash"), at("_under"), at("é"), at("p"), at("project-media-extra"),
        at("project-medi"), at("Project-Media", "x"), at("project-media", "t2-new"), at("project-media", "t0-old", "file"), at("project-media", "t1-abcd"),
        at("project-media", "t1-ab"), at("project-media", "t1-abc.bak"), at("project-media", "t1-ab", "x"), at("project-media", "T1-abc")])
        assert.ok(sandboxDenies(denyWrite, created), `${JSON.stringify(change)} refuses ${created}`);
      // The conversation's own folder, and everything it creates, stays writable (Read only denies its working folder on purpose).
      for (const owned of [at("project-media", "t1-abc"), at("project-media", "t1-abc", "notes.txt"), at("project-media", "t1-abc", "a", "b", "c")])
        if (base === data && change.mode !== "read_only") assert.ok(!sandboxDenies(denyWrite, owned), `${JSON.stringify(change)} allows ${owned}`);
    }
    assert.ok(!sandboxDenies(denyWrite, join(home, "elsewhere", "file")));
  }
});

// The same entries, handed to the real macOS sandbox: every name that differs
// from the conversation's own folder at any character is refused, and the
// folder itself stays writable. Skipped where `sandbox-exec` cannot run (macOS
// refuses a sandbox inside another).
const canSandbox = (() => {
  if (process.platform !== "darwin") return false;
  try { execFileSync("sandbox-exec", ["-p", "(version 1)(allow default)", "/usr/bin/true"], { stdio: "ignore" }); return true; } catch { return false; }
})();
test("Seatbelt refuses names that differ from the conversation's folder at any character", { skip: !canSandbox && "needs sandbox-exec", timeout: 120_000 }, async t => {
  const root = await realpath(await mkdtemp(join(tmpdir(), "wonder-seatbelt-")));
  t.after(() => rm(root, { recursive: true, force: true }));
  const probeChars = [...Array.from({ length: 0x5f }, (_, index) => String.fromCharCode(0x20 + index)).filter(char => char !== "/"), "é", "日", "😀"];
  for (const [index, name] of ["0123456789", "abcdefghijklmnopqrstuvwxyz", "ABCDEFGHIJKLMNOPQRSTUVWXYZ", "t1-abc.d-9", "a-z-0-Y.z."].entries()) {
    // A folder of its own each: names that differ only in case are one folder on a macOS volume.
    const home = join(root, `home${index}`), data = join(home, ".cupola");
    const workspace = join(data, name);
    await mkdir(workspace, { recursive: true });
    const policy = new ToolPolicy({ cwd: workspace, workspace, mode: "workspace", approvalMode: "auto", readRoots: ["/"], writeRoots: [workspace], deniedRoots: [data], home });
    const rules = policy.sandbox().filesystem.denyWrite.map(entry => /[*?[]/.test(entry) ? `(regex #"${sandboxRegex(entry)}")` : `(subpath "${entry}")`);
    const chars = Array.from(name);
    const probes = new Set(["extra", `${name}x`, `x${name}`, "é", ...chars.keys().flatMap(index => [chars.slice(0, index).join("") + "Q",
      ...probeChars.map(other => chars.slice(0, index).join("") + other + chars.slice(index + 1).join("")), ...(index ? [chars.slice(0, index).join("")] : [])])]);
    probes.delete(name);
    const profile = join(root, "profile.sb"), list = join(root, "probes");
    await writeFile(profile, `(version 1)\n(allow default)\n(deny file-write* (with no-report)\n  ${rules.join("\n  ")})\n`);
    await writeFile(list, [...probes].join("\0") + "\0");
    const script = 'while IFS= read -r -d "" n; do if { : > "$1/$n"; } 2>/dev/null; then printf "%s\\n" "$n"; fi; done < "$2"; : > "$1/' + name + '/own" && echo own-ok || echo own-REFUSED';
    const out = execFileSync("sandbox-exec", ["-f", profile, "/bin/bash", "-c", script, "bash", data, list]).toString();
    assert.equal(out.trim(), "own-ok", `${name}: names the sandbox let through (${probes.size} probed, ${rules.length} rules):\n${out}`);
  }
});

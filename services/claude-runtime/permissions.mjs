import { readdirSync, realpathSync } from "node:fs";
import { lstat, realpath, stat } from "node:fs/promises";
import { homedir, tmpdir } from "node:os";
import { basename, dirname, isAbsolute, join, normalize, relative, resolve, sep } from "node:path";

// SDK schema parsing can reorder object properties. Correlation follows JSON
// values while preserving array order and every argument's value.
export function toolCallKey(name, input) {
  return JSON.stringify([name, input], (_key, value) => value && typeof value === "object" && !Array.isArray(value)
    ? Object.fromEntries(Object.keys(value).sort().map(key => [key, value[key]])) : value);
}

// Hosts sandboxed Project commands reach without asking. Any other host asks
// the owner, who can save it to the project's Claude Code settings.
export const ALLOWED_DOMAINS = ["github.com", "*.github.com", "*.githubusercontent.com",
  "registry.npmjs.org", "registry.yarnpkg.com", "repo.yarnpkg.com", "crates.io", "*.crates.io", "static.rust-lang.org",
  "pypi.org", "files.pythonhosted.org", "proxy.golang.org", "sum.golang.org", "formulae.brew.sh", "ghcr.io",
  "api.cloudflare.com", "api.anthropic.com", "api.openai.com"];
// Toolchains and their caches in $HOME. The rest of $HOME stays unreadable to
// sandboxed Project commands. Executable folders on PATH stay read-only so a
// command cannot plant a program that later runs outside the sandbox.
const HOME_READ = [".cargo", ".rustup", ".npm", ".nvm", ".yarn", ".bun", ".deno", ".volta", ".pyenv", ".rbenv", ".asdf",
  ".gradle", ".m2", "go", ".swiftpm", ".cache", ".local/bin", ".local/lib", ".gitconfig", ".gitignore_global", ".config/git",
  "Library/Caches", "Library/Developer", "Library/pnpm", ".pnpm-store"];
const HOME_WRITE = [".cargo/registry", ".cargo/git", ".npm", ".yarn/berry/cache", ".bun/install/cache", ".cache",
  "Library/Caches", "Library/pnpm/store", ".pnpm-store", ".gradle/caches", ".m2/repository", "go/pkg/mod",
  "Library/Developer/Xcode/DerivedData"];
// Agents load configuration from a folder in the repository or the account:
// settings can add permission rules, hooks, plugins or MCP servers, and the
// others carry commands or hooks of their own. An agent that edits them could
// widen its own access, so editing them asks. Claude Code's own and the other
// agents Cupola runs or that share a repository (OpenCode, Cursor, Codex) are
// treated alike. Instruction files (CLAUDE.md, AGENTS.md, rules) are ordinary text.
const OPENCODE_FOLDERS = /^(agents?|commands?|plugins?|tools?|modes?|skills?|themes?|hooks?)$/;
const AGENT_CONFIG = [
  [/^\.claude$/, [/^settings.*\.json$/, /^(hooks|agents|commands|skills)$/]],
  [/^\.opencode$/, [/^opencode\.jsonc?$/, OPENCODE_FOLDERS]],
  [/^opencode$/, [/^opencode\.jsonc?$/, OPENCODE_FOLDERS], /^\.config$/],
  [/^\.cursor$/, [/^(mcp|hooks|cli|cli-config|environment|sandbox|settings)(\..*)?\.json$/, /^(hooks|agents|commands|skills)$/]],
  [/^\.codex$/, [/^(config.*\.toml|hooks\.json|requirements\.toml)$/, /^(rules|prompts|hooks|agents|skills)$/]],
];
// Configuration files that are recognized by name wherever they sit.
const AGENT_CONFIG_FILES = /^(\.mcp\.json|opencode\.jsonc?)$/;
const real = path => { try { return realpathSync(path); } catch { return path; } };
// macOS opens every path on the user's volume a second way, through this
// firmlink; realpath keeps whichever spelling it was given.
const FIRMLINK = "/System/Volumes/Data";
const firmlinked = path => process.platform === "darwin" && (path === FIRMLINK || path.startsWith(`${FIRMLINK}/`));
const unlinked = path => firmlinked(path) ? path.slice(FIRMLINK.length) || "/" : path;
// Every spelling the command sandbox may be matched against.
const both = paths => [...new Set(paths.flatMap(path => {
  const spellings = [path, real(path)];
  if (process.platform === "darwin" && isAbsolute(path)) spellings.push(...spellings.map(unlinked), ...spellings.map(name => FIRMLINK + unlinked(name)));
  return spellings;
}))];

// Mach services beyond Claude Code's built-in list that Project commands need,
// each confirmed necessary by removing it (B3). FSEvents: git's fsmonitor.
// CoreSimulator: `xcrun simctl` (the simulator host is per architecture; without
// simdiskimaged and the host, runtimes and devices list as unavailable).
// runningboard: `open` launches apps through it. Nothing here reaches the
// keychain: `com.apple.SecurityServer` stays closed.
export const MACH_SERVICES = ["com.apple.FSEvents", "com.apple.CoreSimulator.CoreSimulatorService",
  "com.apple.CoreSimulator.simdiskimaged", `com.apple.CoreSimulator.SimLaunchHost-${process.arch === "x64" ? "x86" : process.arch}`,
  "com.apple.runningboard"];
const APPROVAL_MODES = ["ask", "accept_edits", "auto", "full_access"];
const EDIT_TOOLS = ["Edit", "Write", "NotebookEdit"];
export const isCommandTool = name => name === "Bash" || name === "PowerShell";
// Appended to every Project: macOS refuses a second sandbox inside the one that
// runs commands, so these tools fail until their own sandbox is turned off.
export const NESTED_SANDBOX_GUIDANCE = "Commands here run inside Cupola's protection, which keeps them out of Cupola's own data and the keychains. macOS doesn't let a tool start a sandbox of its own inside it, so such a tool fails with `sandbox_apply: Operation not permitted`. Run it without its own sandbox instead: add `--disable-sandbox` to `swift build`, `swift test`, `swift run` and `swift package`, and add `-IDEPackageSupportDisableManifestSandbox=YES -IDEPackageSupportDisablePluginExecutionSandbox=YES ENABLE_USER_SCRIPT_SANDBOXING=NO` to `xcodebuild`. Cupola's protection still applies. The keychains are closed to commands on purpose: `gh` and `git` cannot read stored sign-ins, so `gh auth status` reports an invalid token and `git push` over HTTPS fails with `could not read Username`. That is the protection, not a broken login; do not retry, log in again or look for the credentials. Public reads such as `git ls-remote` and `git clone` of public repositories work; tell the owner that pushing or `gh` actions on their account need to run in their own terminal. Likewise macOS refuses Apple Events to other apps from here, so `osascript` can compute (`return 1`) but `osascript -e 'quit app \"TextEdit\"'` fails with -600 or -10004; say so and ask the owner to quit the app instead of retrying.";
const DENIED = "This action is outside this conversation’s allowed tools or file access. Ask the owner to change access in Cupola.";
// Appended to Claude Code's own sandbox guidance, which names CLI-only controls.
export const SANDBOX_GUIDANCE = "Commands run in Cupola's sandbox. The owner steers this conversation from Cupola, not a terminal: they cannot use `/sandbox`, `!` commands or settings files. When the sandbox blocks a host or a command, retrying the command reaches the owner as an approval on their phone. To lift the sandbox for the whole conversation, the owner chooses Full access in the shield menu.";
export const WINDOWS_COMMAND_GUIDANCE = "Native Windows has no Claude Code sandbox. Commands run with the owner's Windows account access, subject to this conversation's approval mode. Cupola's protected-file checks still apply to file tools; they do not isolate shell commands.";
const PLAN_EDIT_DENIED = "Plan mode: describe the change instead of editing.";
const PLAN_SECRET_DENIED = "Plan mode: finish the plan before asking for or using a secret.";
// The daemon refuses these in a planning thread too; this keeps the call from reaching it.
const SECRET_TOOLS = ["mcp__wonder__wonder_request_secret", "mcp__wonder__wonder_run_with_secret"];
const PLAN_REVIEW_DENIED = "The owner reviews the plan in Cupola. Stop here and wait for their reply; do not edit or run anything for this plan yet.";

// Windows accepts mixed separators and case for the same file. Apply that
// identity to both grants and denials, including nonexistent write targets.
const identity = path => {
  if (process.platform !== "win32") return unlinked(path);
  // Rust canonicalization supplies verbatim drive/UNC paths for attachments;
  // Node realpath returns their ordinary spelling. They name the same scope.
  return normalize(path).replace(/^\\\\\?\\UNC\\/i, "\\\\").replace(/^\\\\\?\\(?=[a-z]:\\)/i, "").toLowerCase();
};
const samePath = (path, root) => identity(path) === identity(root);
const within = (path, root) => {
  path = identity(path); root = identity(root);
  return path === root || path.startsWith(root.endsWith(sep) ? root : `${root}${sep}`);
};
const inside = (path, root) => !samePath(path, root) && within(path, root);
// Spelling is not identity: a bind mount, a firmlink or a case variant on a
// case-insensitive volume names the same folder differently. `inner` is under
// `outer` when they spell it alike or when the folder `outer` names is `inner`
// itself or one of its existing ancestors.
async function fileId(path) {
  try { const { dev, ino } = await stat(path); return `${dev}:${ino}`; } catch { return null; }
}
async function contains(outer, inner) {
  if (within(inner, outer)) return true;
  const target = await fileId(outer);
  if (!target) return false;
  for (let folder = inner; ; folder = dirname(folder)) {
    if (await fileId(folder) === target) return true;
    if (dirname(folder) === folder) return false;
  }
}
// Case-insensitive, as macOS and Windows volumes usually are.
function agentConfig(path) {
  const parts = path.toLowerCase().split(/[\\/]+/);
  return AGENT_CONFIG_FILES.test(parts.at(-1))
    || parts.some((part, index) => AGENT_CONFIG.some(([folder, names, parent]) => folder.test(part)
      && (!parent || parent.test(parts[index - 1] ?? "")) && names.some(name => name.test(parts[index + 1] ?? ""))));
}

// A folder name no pattern could spell stays covered by listing, never by guessing.
const PATTERN_TEXT = /^[A-Za-z0-9_.-]+$/;
// Characters that stay literal when written in a sandbox pattern outside brackets,
// and the ones that can end a bracketed range there.
const LITERAL_CHAR = /^[.^$+{}()|\\-]$/;
const RANGE_END = /^[0-9a-z,/:@`~ \u{10FFFD}]$/u;
// Measured against the real sandbox: Seatbelt silently drops a rule whose range
// ends on a noncharacter (U+FFFE, U+FFFF, U+10FFFE, U+10FFFF), leaving the folder
// open, so the last character is U+10FFFD; and Claude Code mangles a control
// character in a path, so the first is the space. A name starting with a control
// character is therefore the one first character no pattern covers.
const FIRST = 0x20, LAST = 0x10FFFD;
// The sandbox has no "everything but" for names. Claude Code's patterns do take
// bracketed ranges, so "any first character but `char`" is written as the ranges
// below and above it. Seatbelt matches them case-insensitively, folding letters
// to lower case (so `char` excludes both cases, as the volume does, and a range
// never ends on a capital). A character next to `char` that a range cannot end
// on is named on its own instead; null when even that cannot be written.
function anyCharBut(char) {
  const code = char.toLowerCase().codePointAt(0), ranges = [], singles = [];
  for (let [from, to] of [[FIRST, code - 1], [code + 1, LAST]]) {
    while (from <= to && !RANGE_END.test(String.fromCodePoint(from))) singles.push(from++);
    while (to >= from && !RANGE_END.test(String.fromCodePoint(to))) singles.push(to--);
    if (from <= to) ranges.push(from === to ? String.fromCodePoint(from) : `${String.fromCodePoint(from)}-${String.fromCodePoint(to)}`);
  }
  const names = singles.map(code => String.fromCodePoint(code));
  if (!names.every(name => LITERAL_CHAR.test(name))) return null;
  return [`[${ranges.join("")}]`, ...names];
}
// Patterns for every direct child of `folder` except one named `part`, whatever
// the child is called and whenever it is created: names that differ from `part`
// at some character, that extend it, or that stop short of it.
function otherChildren(folder, part) {
  const chars = Array.from(part);
  if (!PATTERN_TEXT.test(part) || /[*?[\]]/.test(folder)) return [];
  const result = [`${folder}/${part}?*`];
  for (const [index, char] of chars.entries()) {
    const others = anyCharBut(char);
    if (!others) return [];
    result.push(...others.map(other => `${folder}/${chars.slice(0, index).join("")}${other}*`));
    if (index > 0) result.push(`${folder}/${chars.slice(0, index).join("")}`);
  }
  return result;
}

// `root` when it does not contain `owned`; otherwise everything on the way down
// to `owned` except the branch leading to it: every existing entry, and patterns
// for the names of entries created later, which the sandbox cannot exempt
// `owned` from. A branch name a pattern cannot spell is covered by the existing
// entries only.
function surrounding(root, owned) {
  const base = [root, real(root)].find(candidate => within(owned, candidate));
  if (!base) return [root];
  if (samePath(owned, base)) return [];
  const result = [];
  let folder = base;
  for (const part of relative(base, owned).split(sep)) {
    let names;
    try { names = readdirSync(folder); } catch { return [root]; }
    // Name each entry under the root as given too; the sandbox matches either spelling.
    result.push(...names.filter(name => !samePath(name, part)).flatMap(name => [join(folder, name), join(root, relative(base, folder), name)]));
    result.push(...otherChildren(folder, part), ...otherChildren(join(root, relative(base, folder)), part));
    folder = join(folder, part);
  }
  return result;
}

async function canonicalTarget(path) {
  try { return await realpath(path); }
  catch (error) {
    // Resolve existing ancestors for a new file, including symlinks. Other
    // errors fail closed; permission errors are not evidence of a missing file.
    if (error.code !== "ENOENT" || dirname(path) === path) throw error;
    // A link to nothing is not a new file: writing through it creates its
    // target, wherever that is. It cannot be resolved, so it is refused.
    let entry = null;
    try { entry = await lstat(path); } catch (missing) { if (missing.code !== "ENOENT") throw missing; }
    if (entry) throw Object.assign(new Error("The path is a link that leads nowhere."), { code: "EDANGLING" });
    return join(await canonicalTarget(dirname(path)), basename(path));
  }
}

export class ToolPolicy {
  constructor({ cwd, workspace = cwd, mode, approvalMode, planMode = false, readRoots, writeRoots, deniedRoots, projectRoots = [], structuredOutput = false, tools = [], skills = [], home = homedir() }) {
    if (!isAbsolute(cwd ?? "") || !["read_only", "workspace", "full_access"].includes(mode)
      || !APPROVAL_MODES.includes(approvalMode) || typeof planMode !== "boolean"
      // Accepting edits or running commands automatically refines Workspace only.
      || (["accept_edits", "auto"].includes(approvalMode) && mode !== "workspace")) throw new Error("Claude permission scope is missing or unsupported");
    for (const roots of [readRoots, writeRoots, deniedRoots, projectRoots]) {
      if (!Array.isArray(roots) || roots.some(root => typeof root !== "string" || !isAbsolute(root))) throw new Error("Invalid Claude file scope");
    }
    if (!isAbsolute(workspace) || !isAbsolute(home)) throw new Error("Invalid Claude workspace");
    Object.assign(this, { cwd, workspace, mode, approvalMode, planMode, readRoots, writeRoots, deniedRoots, projectRoots, structuredOutput, home });
    this.tools = new Set(tools);
    // Wonder's enabled skills (`wonder:<id>`). The owner's own skills load as
    // in Claude Code; only a Wonder skill that is off is refused.
    this.skills = new Set(skills);
    // Set once a skill of the owner's loads in this process. Its `allowed-tools`
    // become Claude Code rules, so from then on nothing defers to those rules.
    this.skillLoaded = false;
  }
  // Projects follow Claude Code: no sandbox, and its permission mode decides
  // what asks. Read only, no longer offered, keeps the sandbox that enforces it.
  get unsandboxed() { return this.mode !== "read_only"; }
  // Claude Code's own rules decide commands and web access, including its
  // read-only command checks and Auto's classifier. Whatever it would ask
  // reaches the owner through canUseTool.
  get defersToClaudeCode() { return this.unsandboxed && !this.bypassesApproval && !this.skillLoaded; }
  get permissionMode() { return this.planMode ? "plan" : this.unsandboxed && this.approvalMode === "auto" ? "auto" : "default"; }
  // Sandboxed Projects confine $HOME to the project and toolchains, for
  // commands and for Claude's own file tools alike.
  get confinesHome() { return !this.unsandboxed; }
  homeReadable() { return [...this.projectRoots, ...HOME_READ.map(path => join(this.home, path))]; }
  // The path Claude Code opens for a path argument: trimmed, a leading `~`
  // expanded to the account's home.
  expand(path) {
    const trimmed = path.trim();
    if (trimmed === "~") return this.home;
    return /^~[\\/]/.test(trimmed) ? join(this.home, trimmed.slice(2)) : trimmed;
  }
  async permits(path, write = false) {
    if (typeof path !== "string" || !path.trim() || path.includes("\0")) return false;
    return this.permitsAt(resolve(this.cwd, this.expand(path)), write);
  }
  async permitsAt(lexical, write = false) {
    let canonical;
    try { canonical = await canonicalTarget(lexical); } catch { return false; }
    if (this.confinesHome && await contains(real(this.home), canonical)) {
      let readable = false;
      for (const root of this.homeReadable()) readable ||= within(lexical, root) || await contains(real(root), canonical);
      if (!readable) return false;
    }
    for (const root of this.protectedRoots) {
      let resolvedRoot;
      try { resolvedRoot = await canonicalTarget(root); } catch { return false; }
      if (within(lexical, resolve(root)) || await contains(resolvedRoot, canonical)) {
        const owned = await canonicalTarget(this.workspace);
        const ownWorkspace = within(lexical, this.workspace) && within(canonical, owned)
          && inside(owned, resolvedRoot);
        if (!ownWorkspace) return false;
      }
    }
    if (write && this.mode === "read_only") return false;
    if (this.mode === "full_access") return true;
    for (const root of write ? this.writeRoots : [...this.readRoots, ...this.writeRoots]) {
      try {
        if (within(lexical, resolve(root)) && within(canonical, await canonicalTarget(root))) return true;
      } catch { /* A missing or inaccessible grant cannot widen access. */ }
    }
    return false;
  }
  async decision(name, input) {
    // The SDK emits schema-constrained answers through this intrinsic tool.
    // It returns data only, and is enabled only by the host's output schema.
    if (name === "StructuredOutput" && this.structuredOutput) return "allow";
    if (this.planMode && SECRET_TOOLS.includes(name)) return "deny";
    if (name.startsWith("mcp__")) return "ask";
    // In plan mode the owner reviews the plan in Wonder; Claude does not proceed on its own.
    if (name === "ExitPlanMode" && this.planMode) return "deny";
    if (["AskUserQuestion", "ExitPlanMode"].includes(name)) return "ask";
    // Loads tool definitions only. Invoking a discovered connector still asks
    // through this policy; large catalogs need not fill every model prompt.
    if (name === "ToolSearch") return "allow";
    // Loads a skill's instructions; what it then runs is decided here as usual.
    if (name === "Skill") return !String(input.skill ?? "").startsWith("wonder:") || this.skills.has(input.skill) ? "allow" : "deny";
    if (name === "Agent") return input.isolation === "remote" ? "deny" : "allow";
    // Coordination only: a helper agent hands its result back to the reply that started it.
    if (["TodoWrite", "TaskCreate", "TaskUpdate", "TaskGet", "TaskList", "TaskOutput", "TaskStop", "EnterPlanMode", "SubagentHandback"].includes(name)) return "allow";
    // Recursive built-ins execute outside the Bash sandbox and can traverse a
    // protected descendant of an otherwise readable folder. Use scoped Bash
    // for searches; Read checks each concrete file, including symlink targets.
    if (["Glob", "Grep"].includes(name)) return await this.searchable(input.path ?? this.cwd, name === "Glob" ? input.pattern : undefined) ? "allow" : "deny";
    if (name === "Read")
      return await this.permits(input.file_path ?? input.path ?? this.cwd) ? "allow" : "deny";
    if (EDIT_TOOLS.includes(name)) {
      // A PreToolUse allow skips the SDK's own plan-mode check, so plan mode is enforced here.
      const target = input.file_path ?? input.notebook_path;
      if (this.planMode || !await this.permits(target, true)) return "deny";
      // Claude Code configuration can add permission rules, hooks or MCP
      // servers, so changing it asks unless nothing asks.
      if (!this.bypassesApproval && await this.configures(target)) return "ask";
      return this.approvalMode === "ask" ? "ask" : "allow";
    }
    if (isCommandTool(name)) {
      // Commands never leave the sandbox in a Project; Read only may ask the owner.
      if (input.dangerouslyDisableSandbox) return this.unsandboxed || this.planMode ? "deny" : "ask";
      return this.autoRunsCommands ? "allow" : "ask";
    }
    // A sandboxed command reached a host outside the allowlist.
    if (name === "SandboxNetworkAccess") return "ask";
    if (["WebFetch", "WebSearch"].includes(name)) return this.bypassesApproval ? "allow" : "ask";
    return "deny";
  }
  // Wonder's own skills name no tools; any other skill or slash command might.
  noteSkill(skill) {
    const name = String(skill ?? "");
    if (!this.skills.has(name) && !this.skills.has(`wonder:${name}`)) this.skillLoaded = true;
  }
  // Bypass permissions: nothing asks, except while planning.
  get bypassesApproval() { return this.approvalMode === "full_access" && !this.planMode; }
  // Commands run without asking in Bypass, and in Auto only inside the sandbox;
  // never while planning.
  get autoRunsCommands() { return !this.planMode && (this.approvalMode === "full_access" || (this.approvalMode === "auto" && !this.unsandboxed)); }
  // Whether a file target is Claude Code configuration under either its
  // spelling or its resolved location: a link named like a settings file
  // changes what Claude Code reads, and a link elsewhere can reach one.
  async configures(path) {
    const lexical = resolve(this.cwd, this.expand(path));
    try { return agentConfig(lexical) || agentConfig(await canonicalTarget(lexical)); }
    catch { return true; }
  }
  // Folders closed to Claude's commands and file tools: Cupola's data and
  // credential folders, and the account's keychains, whose items a command
  // could otherwise ask `securityd` for.
  get protectedRoots() { return [...this.deniedRoots, join(this.home, "Library/Keychains")]; }
  // Claude Code itself runs unconfined, as in the CLI; only its commands are
  // sandboxed. Its rules, hooks and MCP servers are not commands, so Read only
  // loads no configuration from disk: no settings, hooks, MCP servers or
  // helpers of the Project's or the account's, only what Cupola passes.
  get settingSources() { return this.unsandboxed ? ["user", "project", "local"] : []; }
  // Text returned to Claude when a tool call is refused.
  denial(name) {
    if (this.planMode && EDIT_TOOLS.includes(name)) return PLAN_EDIT_DENIED;
    if (this.planMode && name === "ExitPlanMode") return PLAN_REVIEW_DENIED;
    if (this.planMode && SECRET_TOOLS.includes(name)) return PLAN_SECRET_DENIED;
    return DENIED;
  }
  // The folders a search reads: `path`, and for Glob the literal folder its
  // pattern starts in, which an absolute, `~` or `..` pattern makes the search
  // root whatever `path` says.
  searchRoots(path, pattern) {
    if (typeof path !== "string" || !path.trim() || path.includes("\0")) return null;
    const base = resolve(this.cwd, this.expand(path));
    if (pattern === undefined || pattern === null) return [base];
    if (typeof pattern !== "string" || pattern.includes("\0")) return null;
    const text = this.expand(pattern);
    const literal = [];
    for (const part of text.split(/[\\/]/)) { if (/[*?[\]{}()!]/.test(part)) break; literal.push(part); }
    const folder = literal.join(sep) || (isAbsolute(text) ? sep : ".");
    return [base, resolve(base, folder)];
  }
  // Project code search is allowed only where no protected folder can be
  // traversed below the search root, because Glob/Grep run outside the sandbox.
  async searchable(path, pattern) {
    const roots = this.searchRoots(path, pattern);
    if (!roots) return false;
    for (const root of roots) if (!await this.searchableAt(root)) return false;
    return true;
  }
  async searchableAt(lexical) {
    if (!await this.permitsAt(lexical)) return false;
    let base;
    try { base = await canonicalTarget(lexical); } catch { return false; }
    for (const root of this.protectedRoots) {
      let target;
      try { target = await canonicalTarget(root); } catch { return false; }
      if (await contains(base, target) || await contains(target, base)) return false;
    }
    return true;
  }
  async beforeTool(event) {
    const decision = await this.decision(event.tool_name, event.tool_input ?? {});
    if (event.tool_name === "Skill" && decision === "allow") this.noteSkill(event.tool_input?.skill);
    if (decision === "ask" && this.defersToClaudeCode && (isCommandTool(event.tool_name) || ["WebFetch", "WebSearch"].includes(event.tool_name))) return {};
    // Always pass Bash through canUseTool, including automatic approval, so
    // every execution applies the host-owned command policy.
    const result = { hookEventName: "PreToolUse", permissionDecision: isCommandTool(event.tool_name) && decision !== "deny" ? "ask" : decision };
    if (decision === "deny") result.permissionDecisionReason = this.denial(event.tool_name);
    if (event.tool_name === "Agent" && decision === "allow") {
      // Child work inherits the parent's selected model. In particular, Haiku
      // validation must not silently start a more expensive helper model.
      const { model, ...input } = event.tool_input;
      // Native children can outlive a reply; the bridge keeps their process.
      result.updatedInput = input;
    }
    return { hookSpecificOutput: result };
  }
  // Claude Code's own command sandbox, configured from the host-owned policy.
  // It covers shell commands only; file tools are checked by decision().
  // Project and user Claude Code settings merge into it, as in the CLI.
  sandbox(platform = process.platform) {
    // Anthropic has no native Windows command sandbox. Keep every mode and
    // the tool/approval policy above; only the unsupported isolation differs.
    if (platform === "win32") return { enabled: false, failIfUnavailable: false, autoAllowBashIfSandboxed: false, allowUnsandboxedCommands: true };
    const owned = real(this.workspace);
    const closed = both(this.protectedRoots);
    // This conversation's attachment folder is inside Wonder's data folder.
    const reopened = both([this.workspace]).filter(path => this.deniedRoots.some(root => within(path, root) || within(path, real(root))));
    // A protected folder holding the owned workspace is denied entry by entry
    // around it: the sandbox has no write exception inside a denied folder.
    const denyWrite = this.protectedRoots.flatMap(root => surrounding(root, owned));
    if (this.unsandboxed) {
      return {
        enabled: true, failIfUnavailable: true, autoAllowBashIfSandboxed: false,
        // The model could otherwise set `dangerouslyDisableSandbox` and
        // run a command outside the sandbox without asking.
        allowUnsandboxedCommands: false,
        network: { allowedDomains: ["*"], allowLocalBinding: true, allowAllUnixSockets: true, allowMachLookup: MACH_SERVICES },
        filesystem: { allowWrite: ["/"], denyWrite: both(denyWrite), denyRead: closed, allowRead: reopened },
      };
    }
    const writes = this.mode === "read_only" ? [] : this.writeRoots;
    const tmp = [tmpdir(), "/tmp", "/private/tmp"];
    const denyRead = both([this.home, ...this.protectedRoots]);
    const allowRead = both(this.homeReadable())
      .filter(path => !this.protectedRoots.some(root => within(path, root) || within(path, real(root))) || reopened.includes(path))
      .concat(reopened);
    return {
      enabled: true, failIfUnavailable: true, autoAllowBashIfSandboxed: false,
      // Read only may retry a command outside the sandbox; decision() asks the
      // owner. As in the CLI, this also honors the repository's committed
      // `sandbox.excludedCommands`, which canUseTool cannot tell apart; the
      // agent cannot add exclusions (settings edits ask, and commands cannot
      // write them).
      allowUnsandboxedCommands: true,
      // Tests bind local ports; git's fsmonitor uses a socket and FSEvents.
      network: { allowedDomains: ALLOWED_DOMAINS, allowLocalBinding: true, allowAllUnixSockets: true, allowMachLookup: ["com.apple.FSEvents"] },
      filesystem: {
        allowWrite: both([...writes, ...tmp, ...HOME_WRITE.map(path => join(this.home, path))]),
        // Claude Code always lets commands write the working folder, so Read
        // only denies it explicitly.
        denyWrite: both([...denyWrite, ...(this.mode === "read_only" ? [this.cwd, ...this.projectRoots] : [])]),
        denyRead: [...new Set(denyRead)], allowRead: [...new Set(allowRead)],
      },
    };
  }
}

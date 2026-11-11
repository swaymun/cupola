import test from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, mkdir, rm, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, parse, toNamespacedPath } from "node:path";
import { ToolPolicy } from "../permissions.mjs";

// Native filesystem contract: NTFS case aliases and junctions must preserve
// grants AND protected-file denials, including new files and owned workspaces.
test("Windows file tools retain scope in every mode without a command sandbox", { skip: process.platform !== "win32", timeout: 15_000 }, async t => {
  const root = await mkdtemp(join(tmpdir(), "cupola-policy-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const home = join(root, "Home"), data = join(home, ".Cupola"), cwd = join(data, "Threads", "Owned");
  const other = join(data, "Threads", "Other"), sibling = join(home, ".Cupola-other");
  for (const folder of [cwd, other, sibling]) await mkdir(folder, { recursive: true });
  await writeFile(join(data, "host-key"), "fixture only");
  const alias = join(root, "Alias");
  await symlink(data, alias, "junction");
  for (const [mode, approvalMode] of [["read_only", "ask"], ["workspace", "ask"], ["workspace", "accept_edits"],
    ["workspace", "auto"], ["full_access", "full_access"]]) {
    const policy = new ToolPolicy({ cwd, workspace: toNamespacedPath(cwd), home, mode, approvalMode, readRoots: [parse(cwd).root],
      writeRoots: mode === "read_only" ? [] : [cwd], deniedRoots: [data.toUpperCase()], projectRoots: [cwd] });
    assert.equal(policy.sandbox().enabled, false);
    assert.equal(policy.sandbox().failIfUnavailable, false);
    assert.equal(await policy.decision("Read", { file_path: cwd.toUpperCase().replaceAll("\\", "/") }), "allow");
    assert.equal(await policy.decision("Read", { file_path: toNamespacedPath(cwd) }), "allow");
    assert.equal(await policy.permits(join(cwd.toUpperCase(), "New.txt"), true), mode !== "read_only");
    for (const path of [join(data, "HOST-KEY"), toNamespacedPath(join(data, "host-key")), join(other, "new.txt"), join(alias, "host-key"), join(alias, "new.txt")]) {
      assert.equal(await policy.decision("Read", { file_path: path }), "deny", `${mode}: ${path}`);
      assert.equal(await policy.decision("Write", { file_path: path, content: "x" }), "deny", `${mode}: ${path}`);
    }
    assert.equal(await policy.decision("Grep", { path: data.toLowerCase() }), "deny");
    assert.equal(await policy.decision("PowerShell", { command: "Get-Location" }), approvalMode === "full_access" ? "allow" : "ask");
    if (mode !== "read_only") assert.equal(await policy.permits(sibling), true, "a sibling prefix is not a protected descendant");
  }
});

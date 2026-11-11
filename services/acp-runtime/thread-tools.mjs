// Cupola's Project tool names: packages/protocol/thread-tools.json, the list
// wonderd's registry is checked against. A packaged runtime carries a copy
// beside this file (scripts/package-claude-runtime.py, package-cli-release.py);
// a checkout reads the repository's. A missing list stops the runtime rather
// than silently offering no tools.
import { readFileSync } from "node:fs";

export function loadProjectTools(base = import.meta.url) {
  for (const path of ["./thread-tools.json", "../../packages/protocol/thread-tools.json"]) {
    let text;
    try { text = readFileSync(new URL(path, base), "utf8"); }
    catch (error) { if (error.code === "ENOENT") continue; throw error; }
    const names = JSON.parse(text).names;
    if (!Array.isArray(names) || !names.length || !names.every(name => /^wonder_[a-z_]+$/.test(name))) {
      throw new Error(`${path} does not list Cupola's tool names.`);
    }
    return new Set(names);
  }
  throw new Error("thread-tools.json is missing; package packages/protocol/thread-tools.json beside thread-tools.mjs.");
}

export const PROJECT_TOOLS = loadProjectTools();
export const isProjectTool = name => PROJECT_TOOLS.has(name);

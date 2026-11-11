#!/usr/bin/env node
// Replays a scrubbed Claude Code JSONL fixture through the host's own history
// projection (services/claude-runtime/project-history.mjs) and prints the turns.
// No SDK, model, bridge or provider home is touched; the file is read as data.
import { readFileSync } from "node:fs";
import { nativeTurns } from "../services/claude-runtime/project-history.mjs";

const [path] = process.argv.slice(2);
if (!path) { console.error("usage: claude-native-turns.mjs <fixture.jsonl>"); process.exit(2); }
const messages = readFileSync(path, "utf8").split("\n").filter(Boolean).map(line => {
  const { toolUseResult, ...entry } = JSON.parse(line);
  // The SDK reader hands the projection snake_case; transcripts use camelCase.
  return toolUseResult === undefined ? entry : { ...entry, tool_use_result: toolUseResult };
});
process.stdout.write(JSON.stringify(nativeTurns(messages), null, 2) + "\n");

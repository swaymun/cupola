// The stdio MCP server an ACP agent starts for one Wonder session. It holds no
// tools itself: it relays `tools/list` and `tools/call` to Wonder's OpenCode
// runtime over the socket named in its environment, with that session's
// credential, and exits when the credential is revoked or the agent leaves.
import { createConnection } from "node:net";
import { createInterface } from "node:readline";

const VERSIONS = ["2025-06-18", "2025-03-26", "2024-11-05"];
const socketPath = process.env.WONDER_TOOLS_SOCKET;
const token = process.env.WONDER_TOOLS_TOKEN;
// Nothing this process starts or reports carries the credential.
delete process.env.WONDER_TOOLS_TOKEN;

const write = frame => process.stdout.write(`${JSON.stringify({ jsonrpc: "2.0", ...frame })}\n`);
const quit = () => process.exit(0);
if (!socketPath || !token) quit();

const pending = new Map();
let nextId = 1, ready, buffered = "";
const connected = new Promise((resolve, reject) => { ready = { resolve, reject }; });
const socket = createConnection(socketPath);
socket.setEncoding("utf8");
socket.on("connect", () => socket.write(`${JSON.stringify({ type: "hello", token })}\n`));
socket.on("error", quit);
socket.on("close", quit);
socket.on("data", chunk => {
  buffered += chunk;
  let newline;
  while ((newline = buffered.indexOf("\n")) !== -1) {
    const line = buffered.slice(0, newline); buffered = buffered.slice(newline + 1);
    let frame;
    try { frame = JSON.parse(line); } catch { quit(); }
    if (frame.type === "ready") ready.resolve();
    else if (frame.type === "revoked") quit();
    else if (frame.type === "changed") write({ method: "notifications/tools/list_changed" });
    else if (pending.has(frame.id)) {
      const { resolve, reject } = pending.get(frame.id);
      pending.delete(frame.id);
      if (frame.error) reject(new Error(frame.error)); else resolve(frame.result);
    }
  }
});

async function ask(request) {
  await connected;
  const id = nextId++;
  return new Promise((resolve, reject) => {
    pending.set(id, { resolve, reject });
    socket.write(`${JSON.stringify({ id, ...request })}\n`);
  });
}

async function answer({ id, method, params }) {
  if (method === "initialize") {
    const protocolVersion = VERSIONS.includes(params?.protocolVersion) ? params.protocolVersion : VERSIONS[0];
    return { protocolVersion, capabilities: { tools: { listChanged: true } }, serverInfo: { name: "wonder", version: "1.0.0" } };
  }
  if (method === "ping") return {};
  if (method === "tools/list") return { tools: await ask({ type: "list" }) };
  if (method === "tools/call") {
    if (typeof params?.name !== "string") throw Object.assign(new Error("A tool name is required."), { code: -32602 });
    return await ask({ type: "call", requestId: id, name: params.name, arguments: params.arguments ?? {} });
  }
  throw Object.assign(new Error(`${method} is not offered by Cupola.`), { code: -32601 });
}

for await (const line of createInterface({ input: process.stdin })) {
  if (!line.trim()) continue;
  let frame;
  try { frame = JSON.parse(line); } catch { write({ id: null, error: { code: -32700, message: "Invalid JSON." } }); continue; }
  if (frame.id == null) continue; // notifications need no answer
  answer(frame).then(result => write({ id: frame.id, result }),
    error => write({ id: frame.id, error: { code: error.code ?? -32603, message: error.message } }));
}
quit();

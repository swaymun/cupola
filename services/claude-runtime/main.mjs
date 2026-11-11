import { resolve, join } from "node:path";
import { homedir } from "node:os";
import { once } from "node:events";
import { loadSdk } from "./sdk-runtime.mjs";
import { RuntimeUpdates } from "./runtime-updates.mjs";
import { Sessions } from "./sessions.mjs";
import { ClaudeBridge } from "./bridge.mjs";
import { serveFrames } from "./frames.mjs";

const args = process.argv.slice(2);
const options = {};
for (let i = 0; i < args.length; i += 2) {
  if (!["--state-dir", "--npm-cli"].includes(args[i]) || !args[i + 1]) throw new Error("Invalid Claude runtime launch arguments");
  options[args[i]] = args[i + 1];
}
const root = resolve(options["--state-dir"] ?? join(homedir(), ".wonder", "claude-runtime"));
const updates = await new RuntimeUpdates({ root: join(root, "sdk"), npm: options["--npm-cli"], bundled: await loadSdk() }).initialize();
const sessions = await new Sessions(join(root, "sessions")).initialize();
let writing = Promise.resolve();
const send = frame => {
  const line = `${JSON.stringify(frame)}\n`;
  writing = writing.then(async () => { if (!process.stdout.write(line)) await once(process.stdout, "drain"); });
  return writing;
};
const bridge = new ClaudeBridge({ updates, sessions, send, onFatal: () => {
  process.stderr.write("Claude runtime stopped: storage-or-transport\n");
  close().finally(() => process.exit(1));
} });
const timer = setInterval(() => { updates.refresh().catch(() => {}); }, 60 * 60 * 1000);
timer.unref();
// Independent installation checks run in the background and never submit a
// prompt. The bundled runtime serves requests while a candidate is staged.
if (options["--npm-cli"]) updates.refresh().catch(() => {});
let closing;
async function close() {
  if (closing) return closing;
  closing = (async () => { clearInterval(timer); await bridge.close(); await updates.close(); await writing; })();
  return closing;
}
process.once("SIGTERM", () => { close().finally(() => process.exit(0)); });
process.once("SIGINT", () => { close().finally(() => process.exit(0)); });
try {
  await serveFrames(process.stdin, { receive: frame => bridge.receive(frame), send });
} catch (error) {
  // The host keeps this classified line as the reason the runtime stopped.
  process.stderr.write(`Claude runtime stopped: ${error?.message === "Runtime frame exceeded the allowed size" ? "frame-too-large" : "invalid-frame"}\n`);
  throw error;
} finally { await close(); }

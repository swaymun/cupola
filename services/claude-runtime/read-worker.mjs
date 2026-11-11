import { pathToFileURL } from "node:url";
import { openNoFollow } from "./project-history.mjs";

// One reader process answers requests in turn until its owner retires it.
const methods = new Set(["getSessionMessages", "getSubagentMessages", "listSessions", "listSubagents", "getSessionInfo"]);
const send = message => process.send({ ...message, rss: process.memoryUsage.rss() });
const nextMessage = () => new Promise(resolve => process.once("message", resolve));
process.once("disconnect", () => process.exit(0));
for (;;) {
  const input = await nextMessage();
  try {
    let result;
    if (input.type === "sdk") {
      if (!methods.has(input.method)) throw new Error("Unavailable native history reader");
      const sdk = await import(pathToFileURL(input.entry).href);
      result = await sdk[input.method](...input.args);
    } else {
      const handle = await openNoFollow(input.path);
      try {
        const length = input.length ?? (await handle.stat()).size;
        const buffer = Buffer.alloc(length);
        let read = 0;
        while (read < length) {
          const { bytesRead } = await handle.read(buffer, read, length - read, (input.position ?? 0) + read);
          if (!bytesRead) throw new Error("Claude history changed during the read");
          read += bytesRead;
        }
        result = JSON.parse(buffer.toString("utf8"));
      } finally { await handle.close(); }
      if (input.summary) result = (await import("./session-file.mjs")).transcriptEntry(result, input.position ?? 0, input.length);
    }
    if (Array.isArray(result)) {
      let at = 0;
      while (at < result.length) {
        const start = at; let bytes = 0;
        while (at < result.length && at - start < 32 && bytes < 256 * 1024) {
          bytes += JSON.stringify(result[at]).length * 2; at++;
        }
        const next = nextMessage();
        send({ type: "rows", rows: result.slice(start, at) });
        await next;
      }
      send({ type: "done", array: true });
    } else send({ type: "done", value: result });
  } catch (error) {
    send({ type: "error", message: error.message, syntax: error instanceof SyntaxError, code: error.code });
  }
}

// Host stdin framing and request admission for the Claude runtime.

export const MAX_FRAME_BYTES = 64 * 1024 * 1024;
export const MAX_PENDING_REQUESTS = 128;
export const BUSY_MESSAGE = "Claude is handling too many requests right now. Try again in a moment.";

// Splits JSONL input into frames. Each chunk is scanned once and only the
// unterminated frame is held as a list of chunks, so a large frame costs
// linear time. Bytes are decoded per complete frame, keeping a UTF-8
// sequence split across chunks intact. The limit applies to one frame, never
// to a chunk carrying several valid frames.
export class LineFramer {
  constructor({ maxFrameBytes = MAX_FRAME_BYTES } = {}) {
    this.maxFrameBytes = maxFrameBytes; this.parts = []; this.bytes = 0;
  }
  *push(chunk) {
    if (typeof chunk === "string") chunk = Buffer.from(chunk, "utf8");
    let start = 0, newline;
    while ((newline = chunk.indexOf(10, start)) !== -1) {
      const tail = chunk.subarray(start, newline);
      if (this.bytes + tail.length > this.maxFrameBytes) throw new Error("Runtime frame exceeded the allowed size");
      const line = this.parts.length
        ? Buffer.concat([...this.parts, tail], this.bytes + tail.length).toString("utf8") : tail.toString("utf8");
      this.parts = []; this.bytes = 0; start = newline + 1;
      if (line.trim()) yield line;
    }
    if (start < chunk.length) {
      const rest = chunk.subarray(start);
      if (this.bytes + rest.length > this.maxFrameBytes) throw new Error("Runtime frame exceeded the allowed size");
      this.parts.push(rest); this.bytes += rest.length;
    }
  }
}

// Serves host frames until input ends. Responses (approvals and other
// control replies) and notifications never count as new work; a request
// beyond the pending bound is refused with an error for its own ID while the
// runtime keeps serving everything else.
export async function serveFrames(input, { receive, send, maxPending = MAX_PENDING_REQUESTS, framer = new LineFramer() }) {
  const requests = new Set();
  for await (const chunk of input) {
    for (const line of framer.push(chunk)) {
      const frame = JSON.parse(line);
      if (frame && typeof frame === "object" && frame.method && frame.id != null && requests.size >= maxPending) {
        send({ id: frame.id, error: { code: -32000, message: BUSY_MESSAGE } }).catch(() => {});
        continue;
      }
      const request = receive(frame);
      if (frame?.method && frame.id != null) {
        requests.add(request);
        request.finally(() => requests.delete(request)).catch(() => {});
      } else request.catch(() => {});
    }
  }
  return requests;
}

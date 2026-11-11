// Cua Driver transport and process lifecycle for Claude computer use.
import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { StdioClientTransport } from '@modelcontextprotocol/sdk/client/stdio.js';
import { z } from 'zod';

// All supported hosts: Cua Driver's tools, served in process so that Stop
// ends the action. Its tools then go through the same permission policy as
// every other tool.
//
// `CuaDriver` is the one owner of a session's driver lifecycle. Each process
// it starts is a generation; start, Stop, a timeout, a failed start and a
// replacement all end a generation through `end(run)`, which ends only that
// generation's process and clears `current` only if that generation is still
// current. The driver ignores `notifications/cancelled` (0.36) and finishes
// the call (a `run_script` keeps acting), so a cancelled call ends its
// generation; the next call starts a fresh one.
export class CuaDriver {
  constructor({ server, environment, timeoutMs = 10 * 60_000 }) {
    Object.assign(this, { server, environment, timeoutMs });
    this.generation = 0; this.current = null;
  }
  // The current generation, started if there is none.
  acquire() {
    if (this.current) return this.current;
    const client = new Client({ name: 'wonder-claude', version: '1.0.0' });
    const transport = new StdioClientTransport({ command: this.server.command, args: this.server.args,
      env: { ...this.environment, ...this.server.env }, stderr: 'ignore' });
    const run = this.current = { generation: ++this.generation, client, transport, ended: false };
    client.onclose = () => this.end(run);
    run.ready = client.connect(transport, { timeout: 30_000 }).then(() => {
      if (run.ended) throw new Error('The computer-use driver stopped.');
      return client;
    });
    run.ready.catch(() => this.end(run));
    return run;
  }
  // Ends generation `run` (the current one by default), signalled at once:
  // closing the client alone grants the driver 2 s, time enough to finish
  // what it is doing. An older generation's cleanup never reaches the
  // current one.
  end(run = this.current) {
    if (!run || run.ended) return;
    run.ended = true;
    if (this.current?.generation === run.generation) this.current = null;
    if (run.transport.pid) try { process.kill(run.transport.pid, 'SIGTERM'); } catch {}
    run.client.close().catch(() => {});
  }
  stop() { this.end(); }
  // Every use of the driver goes through here with the reply's signal: Stop
  // (or a cancelled call) ends the generation it acquired, also while that
  // generation is still starting or listing its tools. A use that times out
  // or fails in transport ends it too, or its action would go on after Stop.
  async using(signal, work) {
    signal?.throwIfAborted();
    const run = this.acquire(), end = () => this.end(run);
    signal?.addEventListener('abort', end, { once: true });
    try {
      const client = await run.ready;
      signal?.throwIfAborted();
      return await work(client);
    } catch (error) { end(); throw error; }
    finally { signal?.removeEventListener('abort', end); }
  }
  async tools(signal) {
    return (await this.using(signal, client => client.listTools({}, { signal, timeout: 30_000 }))).tools;
  }
  // A tool error is a result, not a transport failure.
  call(name, args, signal) {
    return this.using(signal, client => client.callTool({ name, arguments: args }, undefined, { signal, timeout: this.timeoutMs }));
  }
}

// Returns at once, before the driver is ready, so the reply owns it while it
// starts; its signal (Stop) ends it then. `ready` resolves to the SDK server.
export function createCuaDriver({ sdk, server, environment, timeoutMs, signal }) {
  const driver = new CuaDriver({ server, environment, timeoutMs });
  const ready = driver.tools(signal).then(tools => {
    const proxied = tools.flatMap(t => {
      let shape;
      try { shape = z.fromJSONSchema(t.inputSchema).shape; } catch { return []; }
      return [sdk.tool(t.name, t.description ?? '', shape, (input, extra) => driver.call(t.name, input, extra?.signal),
        { annotations: t.annotations })];
    });
    return sdk.createSdkMcpServer({ name: 'cua_driver', version: '1.0.0', tools: proxied });
  });
  ready.catch(() => driver.stop());
  return { ready, stop: () => driver.stop(), close: () => driver.stop() };
}

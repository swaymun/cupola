import test from "node:test";
import assert from "node:assert/strict";
import { createServer } from "node:http";
import { once } from "node:events";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { mkdtemp, mkdir, writeFile, rm } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
const execute = promisify(execFile);

// The bundled real SDK/CLI, a fresh HOME, and an HTTP fixture: no provider
// account, keychain credential, or real model is used. Exercise both places
// the owner can configure Claude's key and endpoint.
for (const source of ["shell", "settings.json", "auth-token"]) {
  test(`Claude reaches the configured endpoint from ${source}`, { timeout: 30_000 }, async t => {
    const root = await mkdtemp(join(tmpdir(), "cupola-endpoint-"));
    const config = join(root, "native-config");
    await mkdir(config);
    t.after(() => rm(root, { recursive: true, force: true, maxRetries: 20, retryDelay: 100 }));
    const requests = [];
    let stop;
    const server = createServer(async (request, response) => {
      let body = "";
      for await (const chunk of request) body += chunk;
      requests.push({ path: request.url, headers: request.headers, body });
      // A deliberate terminal authentication error proves transport without
      // pretending to emulate a model or provoking SDK retry backoff.
      response.writeHead(401, { "content-type": "application/json" });
      response.end(JSON.stringify({ type: "error", error: { type: "authentication_error", message: "fixture endpoint reached" } }));
      if (request.url.startsWith("/v1/messages")) stop?.();
    });
    server.listen(0, "127.0.0.1");
    await once(server, "listening");
    t.after(() => { server.closeAllConnections(); server.close(); });
    const provider = { ANTHROPIC_BASE_URL: `http://127.0.0.1:${server.address().port}`,
      [source === "auth-token" ? "ANTHROPIC_AUTH_TOKEN" : "ANTHROPIC_API_KEY"]: "fixture-only" };
    if (source === "settings.json") await writeFile(join(config, "settings.json"), JSON.stringify({ env: provider }));
    const script = `
      import { execFileSync } from "node:child_process";
      import { loadSdk, baseOptions, accessKind, classifyAuthStatus } from ${JSON.stringify(new URL("../sdk-runtime.mjs", import.meta.url).href)};
      const runtime = await loadSdk();
      const auth = JSON.parse(execFileSync(runtime.executable, ['auth', 'status', '--json'], { env: baseOptions(runtime).env, timeout: 10000, encoding: 'utf8' }));
      const status = classifyAuthStatus(auth, baseOptions(runtime).env);
      if (status.exitCode !== 0 || status.account !== 'API key') throw new Error('Native sign-in status rejected configured key: ' + JSON.stringify(auth));
      const done = Promise.withResolvers(), submit = Promise.withResolvers();
      const controller = new AbortController();
      const timer = setTimeout(() => controller.abort(), 20000);
      process.stdin.once("data", () => controller.abort());
      const query = runtime.sdk.query({ prompt: (async function* () { await submit.promise; yield { type: 'user', message: {role:'user',content:'fixture'}, parent_tool_use_id:null, session_id:'' }; })(),
        options: { ...baseOptions(runtime), cwd: process.env.HOME, abortController: controller, tools: [], mcpServers: {}, persistSession: false, maxTurns: 1 } });
      try {
        await query.initializationResult();
        const account = await query.accountInfo();
        if (accessKind(account) !== 'API key') throw new Error('API key was not accepted: ' + JSON.stringify(account));
        console.log(JSON.stringify({ accessKind: accessKind(account) }));
        submit.resolve();
        for await (const message of query) if (message.type === 'result') break;
      } catch (error) { console.log(JSON.stringify({ error: error.message })); } finally { done.resolve(); clearTimeout(timer); query.close(); controller.abort(); process.stdin.pause(); }
    `;
    const command = process.platform === "darwin" ? "/usr/bin/sandbox-exec" : process.execPath;
    const args = process.platform === "darwin" ? ["-p", '(version 1)(allow default)(deny network-outbound)(allow network-outbound (remote ip "localhost:*"))', process.execPath] : [];
    const running = execute(command, [...args, "--input-type=module", "-e", script], { timeout: 25_000,
      env: { HOME: root, PATH: process.env.PATH, CLAUDE_CONFIG_DIR: config, ...(source === "settings.json" ? { ANTHROPIC_API_KEY: "overridden-shell-key" } : provider) } });
    stop = () => running.child.stdin.end("stop");
    const result = await running;
    assert.equal(JSON.parse(result.stdout.trim().split("\n")[0]).accessKind, "API key");
    const message = requests.find(request => request.path.startsWith("/v1/messages"));
    assert.ok(message, `real Claude Code must reach the local messages endpoint: ${result.stdout}; requests=${JSON.stringify(requests.map(r=>r.path))}`);
    assert.equal(source === "auth-token" ? message.headers.authorization : message.headers["x-api-key"], source === "auth-token" ? "Bearer fixture-only" : "fixture-only");
  });
}

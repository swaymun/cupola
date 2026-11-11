import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";

const origin = (process.env.WONDER_PUBLIC_ORIGIN ?? "").replace(/\/$/, "");
assert.match(origin, /^https:\/\/[^\s/]+$/, "WONDER_PUBLIC_ORIGIN must be an HTTPS origin");

function curl(path, options = {}) {
  const args = ["-4", "--connect-timeout", "5", "--max-time", "15", "-sS", "-w", "\n%{http_code}"];
  if (options.method) args.push("-X", options.method);
  if (options.origin !== false) args.push("-H", `origin: ${origin}`);
  if (options.body) args.push("-H", "content-type: application/json", "--data", options.body);
  args.push(`${origin}${path}`);
  const output = execFileSync("curl", args, { encoding: "utf8", timeout: 20_000 });
  const split = output.lastIndexOf("\n");
  return { body: output.slice(0, split), status: Number(output.slice(split + 1)) };
}

const pairLink = curl("/pair", { origin: false });
assert.equal(pairLink.status, 200, "a pairing link opened in a browser should explain where to use it");
assert.match(pairLink.body, /Open Cupola on your iPhone or iPad/);
assert.doesNotMatch(pairLink.body, /</, "the pairing link response must not be a web page");

for (const path of ["/", "/pair/style.css", "/sw.js", "/manifest.webmanifest", "/precache-manifest.json", "/assets/index.js", "/index.html"]) {
  assert.equal(curl(path, { origin: false }).status, 404, `${path} must not serve a browser page`);
}

const protectedProjects = curl("/api/v1/projects");
assert.equal(protectedProjects.status, 401, "unpaired public clients must not read Project data");
assert.match(protectedProjects.body, /Cupola session required/);

const protectedConnections = curl("/api/v1/runtime/capabilities");
assert.equal(protectedConnections.status, 401, "unpaired public clients must not read runtime connection data");
assert.match(protectedConnections.body, /Cupola session required/);

const wrongOriginPairing = curl("/api/v1/pairing/code", {
  method: "POST",
  body: JSON.stringify({}),
  origin: false,
});
assert.equal(wrongOriginPairing.status, 403, "pairing claims must reject requests without the public origin");

const wrongOriginProtectedApi = (() => {
  const args = ["-4", "--connect-timeout", "5", "--max-time", "15", "-sS", "-w", "\n%{http_code}", "-H", "origin: https://not-wonder.example", `${origin}/api/v1/projects`];
  const output = execFileSync("curl", args, { encoding: "utf8", timeout: 20_000 });
  const split = output.lastIndexOf("\n");
  return { body: output.slice(0, split), status: Number(output.slice(split + 1)) };
})();
assert.equal(wrongOriginProtectedApi.status, 403, "protected API requests must reject a wrong origin before session handling");

console.log(`verified pairing-link text, no browser pages, and unpaired API boundary at ${origin}`);

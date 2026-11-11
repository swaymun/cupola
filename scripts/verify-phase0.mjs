import { createHash } from "node:crypto";
import { readFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const provenance = JSON.parse(await readFile(resolve(root, "provenance.json"), "utf8"));
const compatibility = JSON.parse(await readFile(resolve(root, "compatibility-manifest.json"), "utf8"));

for (const artifact of provenance.artifacts) {
  const bytes = await readFile(resolve(root, artifact.repositoryPath));
  const hash = createHash("sha256").update(bytes).digest("hex");
  if (hash !== artifact.sha256) {
    throw new Error(`provenance hash mismatch for ${artifact.repositoryPath}: ${hash}`);
  }
}

const stable = provenance.artifacts.find((artifact) => artifact.repositoryPath.endsWith("stable/codex_app_server_protocol.v2.schemas.json"));
const experimental = provenance.artifacts.find((artifact) => artifact.repositoryPath.endsWith("experimental/codex_app_server_protocol.v2.schemas.json"));
if (stable?.sha256 !== compatibility.codex.stableSchemaSha256 || experimental?.sha256 !== compatibility.codex.experimentalSchemaSha256) {
  throw new Error("compatibility manifest schema hashes do not match provenance");
}

const stableSchema = await readFile(resolve(root, stable.repositoryPath), "utf8");
const experimentalSchema = await readFile(resolve(root, experimental.repositoryPath), "utf8");
for (const capability of compatibility.requiredCapabilities.filter((value) => value.includes("/"))) {
  if (!stableSchema.includes(`"${capability}"`) && !experimentalSchema.includes(`"${capability}"`)) {
    throw new Error(`required App Server method is missing from both schema snapshots: ${capability}`);
  }
}
if (!stableSchema.includes('"clientUserMessageId"') || !experimentalSchema.includes('"clientUserMessageId"')) {
  throw new Error("clientUserMessageId is missing from a pinned schema snapshot");
}

console.log(JSON.stringify({
  status: "passed",
  checkedArtifacts: provenance.artifacts.length,
  codexVersion: compatibility.codex.version,
  stableSchemaSha256: compatibility.codex.stableSchemaSha256,
  experimentalSchemaSha256: compatibility.codex.experimentalSchemaSha256,
}, null, 2));

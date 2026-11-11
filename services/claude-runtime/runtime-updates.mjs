import { mkdir, readFile, rename, open, readdir, rm } from "node:fs/promises";
import { dirname, join } from "node:path";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { loadSdk, inspectSdk, checkHistoryReaders, maintenanceEnvironment } from "./sdk-runtime.mjs";

const execute = promisify(execFile);
const DAY = 24 * 60 * 60 * 1000;
const REGISTRY = "https://registry.npmjs.org";

export function compatibleVersion(candidate, baseline) {
  const parse = value => /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/.exec(value)?.slice(1).map(Number);
  const c = parse(candidate), b = parse(baseline);
  // Pre-1.0 SDK minor releases may break the host contract. They need an adapter
  // review; compatible patch updates need no Wonder app release.
  return !!(c && b && c[0] === b[0] && (b[0] !== 0 || c[1] === b[1])
    && (c[1] > b[1] || (c[1] === b[1] && c[2] >= b[2])));
}

const parts = value => /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/.exec(value)?.slice(1).map(Number);

// The highest release Cupola can take without an adapter review: the bundled
// minor's newest patch (or the bundled version itself).
export function newestPatch(versions, baseline) {
  let best = null;
  for (const version of versions ?? []) {
    if (!compatibleVersion(version, baseline)) continue;
    if (!best || compatibleVersion(version, best)) best = version;
  }
  return best;
}

// Whether `candidate` is a later release than `baseline` that the
// compatibility rule refuses (a new pre-1.0 minor or a new major).
export function newerMinor(candidate, baseline) {
  const c = parts(candidate), b = parts(baseline);
  return !!(c && b && !compatibleVersion(candidate, baseline)
    && (c[0] > b[0] || (c[0] === b[0] && c[1] > b[1])));
}

export async function writeJson(path, value) {
  const temporary = `${path}.${process.pid}.tmp`;
  const file = await open(temporary, "w", 0o600);
  try { await file.writeFile(JSON.stringify(value)); await file.sync(); } finally { await file.close(); }
  await rename(temporary, path);
  // Windows cannot flush a directory handle (EPERM); NTFS journals the rename.
  if (process.platform === "win32") return;
  const directory = await open(dirname(path), "r");
  try { await directory.sync(); } finally { await directory.close(); }
}

// The SDK the sidecar runs, for read-only callers such as Settings → Providers:
// the activated update when it still loads, otherwise the bundled one. Never
// writes state; the sidecar remains the only owner of activation and pruning.
export async function activeRuntime({ root, bundled, load = loadSdk }) {
  let state = {};
  try { state = JSON.parse(await readFile(join(root, "state.json"), "utf8")); } catch { return bundled; }
  const version = state.active;
  if (version === bundled.version || !compatibleVersion(version, bundled.version)) return bundled;
  try {
    const runtime = await load(join(root, "versions", version));
    return runtime.version === version ? runtime : bundled;
  } catch { return bundled; }
}

// One owner in the daemon sidecar. Staging never changes an in-flight Query or
// its executable. The bundled SDK remains the recovery path on every launch.
export class RuntimeUpdates {
  constructor({ root, npm, bundled, load = loadSdk, check = inspectSdk, readers = checkHistoryReaders, install, latest, releases, clock = Date.now }) {
    Object.assign(this, { root, npm, bundled, load, check, readers, clock, latest });
    this.abort = new AbortController();
    this.install = install ?? (version => this.installPackage(version));
    // Every published version, not only `latest`: after a new minor ships,
    // later patches of the bundled minor must keep arriving.
    this.releases = releases ?? (latest ? async () => {
      const version = await this.latest();
      return { latest: version, versions: [version] };
    } : () => this.registryReleases());
    this.users = 0; this.pending = null; this.state = {};
    this.current = bundled; this.refreshing = null; this.activating = null; this.listeners = new Set();
  }
  // Called after a different SDK becomes current, so owners of anything
  // derived from the old one (its model catalog) can drop and republish it.
  onActivated(listener) { this.listeners.add(listener); return () => this.listeners.delete(listener); }
  async initialize() {
    await mkdir(join(this.root, "versions"), { recursive: true, mode: 0o700 });
    try { this.state = JSON.parse(await readFile(join(this.root, "state.json"), "utf8")); } catch { this.state = {}; }
    for (const version of [this.state.active, this.state.lastGood]) {
      if (!compatibleVersion(version, this.bundled.version) || version === this.bundled.version) continue;
      try {
        const runtime = await this.load(join(this.root, "versions", version));
        if (runtime.version !== version) throw new Error("version_mismatch");
        await this.verify(runtime);
        this.current = runtime; break;
      } catch { this.state.lastFailure = "saved_runtime_incompatible"; }
    }
    this.state.active = this.current.version;
    if (compatibleVersion(this.state.staged, this.bundled.version) && this.state.staged !== this.current.version) {
      try {
        const candidate = await this.load(join(this.root, "versions", this.state.staged));
        if (candidate.version !== this.state.staged) throw new Error("version_mismatch");
        await this.verify(candidate); this.pending = candidate;
      } catch { this.state.staged = null; this.state.lastFailure = "saved_candidate_incompatible"; }
    }
    await this.save();
    await this.activate();
    await this.prune();
    return this;
  }
  // A runtime becomes current only after its history readers pass the offline
  // fixture and its control requests answer.
  async verify(runtime) { await this.readers(runtime); await this.check(runtime); }
  async save() { await writeJson(join(this.root, "state.json"), this.state); }
  async acquire() {
    if (this.activating) await this.activating;
    this.users += 1;
    let released = false;
    return { runtime: this.current, release: async () => {
      if (released) return;
      released = true; this.users -= 1;
      await this.activate();
    } };
  }
  async activate() {
    if (this.activating) return this.activating;
    if (this.abort.signal.aborted || this.users || this.staging || !this.pending) return;
    this.activating = this.activateIdle().finally(() => { this.activating = null; });
    return this.activating;
  }
  async activateIdle() {
    const candidate = this.pending;
    // Persist first: if storage fails, the known-good runtime stays active.
    const previous = { ...this.state };
    this.state = { ...this.state, active: candidate.version, lastGood: this.current.version, staged: null };
    try { await this.save(); } catch (error) { this.state = previous; throw error; }
    this.pending = null; this.current = candidate;
    // Listeners never hold up activation or a waiting lease.
    for (const listener of this.listeners) Promise.resolve().then(() => listener(candidate)).catch(() => {});
    await this.prune();
  }
  async refresh({ force = false } = {}) {
    if (this.abort.signal.aborted) return;
    if (this.refreshing) return this.refreshing;
    if (!force && this.clock() - (this.state.checkedAt ?? 0) < DAY) return;
    this.refreshing = this.stage().finally(() => { this.refreshing = null; });
    return this.refreshing;
  }
  async stage() {
    if (this.pending) { await this.activate(); return; }
    this.staging = true;
    this.state.checkedAt = this.clock();
    try {
      const { latest, versions } = await this.releases();
      const target = newestPatch(versions, this.bundled.version);
      this.state.latestVersion = typeof latest === "string" ? latest : null;
      this.state.newestCompatible = target;
      // A newer minor needs an adapter review and a Cupola release; it never
      // stops the bundled minor's patches.
      this.state.adapterUpdate = newerMinor(latest, this.bundled.version) ? latest : null;
      this.state.lastFailure = null;
      if (target && target !== this.current.version && compatibleVersion(target, this.current.version)) {
        const root = await this.install(target);
        const candidate = await this.load(root);
        if (candidate.version !== target) throw new Error("version_mismatch");
        await this.verify(candidate);
        this.pending = candidate; this.state.staged = target;
      }
    } catch { this.state.lastFailure = "candidate_unavailable_or_incompatible"; }
    try { await this.save(); } finally { this.staging = false; }
    await this.activate();
    await this.prune();
  }
  async registryReleases() {
    // The abbreviated packument lists every version and the dist-tags.
    const response = await fetch(`${REGISTRY}/@anthropic-ai%2Fclaude-agent-sdk`, {
      headers: { accept: "application/vnd.npm.install-v1+json" },
      signal: AbortSignal.any([this.abort.signal, AbortSignal.timeout(20_000)]) });
    if (!response.ok) throw new Error("registry_unavailable");
    const text = await response.text();
    if (text.length > 32 * 1024 * 1024) throw new Error("registry_response_too_large");
    const document = JSON.parse(text);
    const versions = Object.entries(document.versions ?? {})
      .filter(([, manifest]) => !manifest?.deprecated).map(([version]) => version);
    return { latest: document["dist-tags"]?.latest, versions };
  }
  async prune() {
    if (this.users) return;
    const keep = new Set([this.current.version, this.state.lastGood, this.pending?.version]);
    // Only manager-owned version directories. Never follow links or remove
    // bundled files, credential state, SDK transcripts or an in-flight runtime.
    for (const entry of await readdir(join(this.root, "versions"), { withFileTypes: true })) {
      if (entry.isDirectory() && /^\d+\.\d+\.\d+$/.test(entry.name) && !keep.has(entry.name))
        await rm(join(this.root, "versions", entry.name), { recursive: true, force: true });
    }
  }
  async installPackage(version) {
    if (!this.npm || !compatibleVersion(version, this.bundled.version)) throw new Error("installer_unavailable");
    const root = join(this.root, "versions", version);
    await mkdir(root, { recursive: true, mode: 0o700 });
    await writeJson(join(root, "package.json"), { private: true, dependencies: {
      "@anthropic-ai/claude-agent-sdk": version, zod: "4.6.5" } });
    // npm verifies registry integrity hashes. No lifecycle scripts, user npm
    // credentials, custom registries, or inherited NODE_OPTIONS are accepted.
    await execute(process.execPath, [this.npm, "install", "--ignore-scripts", "--no-audit", "--no-fund", "--no-bin-links",
      `--registry=${REGISTRY}`, "--userconfig=/dev/null"], { cwd: root, timeout: 180_000, maxBuffer: 128 * 1024,
      signal: this.abort.signal,
      env: { ...maintenanceEnvironment(), NPM_CONFIG_IGNORE_SCRIPTS: "true", NPM_CONFIG_REGISTRY: REGISTRY } });
    return root;
  }
  // What Settings → Providers shows: the SDK running now, a staged one that
  // switches over once no turn uses the runtime, and whether a newer minor
  // needs a Cupola release.
  status() {
    return { activeVersion: this.current.version, bundledVersion: this.bundled.version,
      pendingVersion: this.pending?.version ?? null, checkedAtMs: this.state.checkedAt ?? null,
      checking: Boolean(this.refreshing), lastFailure: this.state.lastFailure ?? null,
      latestVersion: this.state.latestVersion ?? null, newestCompatibleVersion: this.state.newestCompatible ?? null,
      adapterUpdateVersion: this.state.adapterUpdate ?? null };
  }
  async close() {
    this.abort.abort();
    await Promise.allSettled([this.refreshing, this.activating].filter(Boolean));
  }
}

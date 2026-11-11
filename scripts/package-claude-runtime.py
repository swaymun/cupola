#!/usr/bin/env python3
"""Bundle the pinned Node runtime and production-only Claude adapter."""
import hashlib
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import tarfile
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
NODE_VERSION = "24.21.0"
NODE_HASHES = {
    "arm64": "bed7eea5325e1108f32ce5228ddd6a5f0f08a499ee42aa7442aea583702f6057",
    "x64": "1462cb3b3046b815cf8ea436d3da450ec1a9f11dac7e5a46b0ada5305d7e8097",
}


def package(resources):
    arch = {"arm64": "arm64", "x86_64": "x64"}.get(platform.machine())
    if sys.platform != "darwin" or arch is None:
        raise SystemExit("Package the Claude runtime on a supported Mac.")
    resources = resources.resolve()
    if resources.name != "Resources" or resources.parent.name != "Contents":
        raise SystemExit("Expected the staging app's Contents/Resources directory.")
    if Path("/Applications") in resources.parents or Path.home() / "Applications" in resources.parents:
        raise SystemExit("Build in staging, then install the verified app.")
    cache = ROOT / ".local/build/claude-package"
    cache.mkdir(parents=True, exist_ok=True)
    name = f"node-v{NODE_VERSION}-darwin-{arch}"
    archive = cache / f"{name}.tar.gz"
    if not archive.exists():
        partial = archive.with_suffix(".partial")
        with urllib.request.urlopen(f"https://nodejs.org/dist/v{NODE_VERSION}/{archive.name}", timeout=60) as source, partial.open("wb") as target:
            shutil.copyfileobj(source, target)
        partial.replace(archive)
    if hashlib.sha256(archive.read_bytes()).hexdigest() != NODE_HASHES[arch]:
        raise SystemExit("Node archive integrity check failed; no runtime was installed.")
    with tarfile.open(archive) as bundle:
        bundle.extractall(cache, filter="data")
    node = resources / "node"
    if node.exists():
        shutil.rmtree(node)
    node.mkdir(parents=True)
    for directory in ["bin", "lib"]:
        shutil.copytree(cache / name / directory, node / directory, symlinks=True)
    shutil.copy2(cache / name / "LICENSE", node / "LICENSE")
    adapter = resources / "claude-runtime"
    if adapter.exists():
        shutil.rmtree(adapter)
    adapter.mkdir()
    source = ROOT / "services/claude-runtime"
    for path in [*source.glob("*.mjs"), source / "package.json", source / "package-lock.json"]:
        shutil.copy2(path, adapter / path.name)
    # The runtime reads Cupola's tool names from beside thread-tools.mjs.
    shutil.copy2(ROOT / "packages/protocol/thread-tools.json", adapter / "thread-tools.json")
    env = {key: os.environ[key] for key in ["HOME", "USER", "LOGNAME", "TMPDIR", "LANG"] if key in os.environ}
    env.update(PATH=f"{node / 'bin'}:/usr/bin:/bin:/usr/sbin:/sbin", NPM_CONFIG_CACHE=str(cache / "npm-cache"))
    subprocess.run([str(node / "bin/node"), str(node / "lib/node_modules/npm/bin/npm-cli.js"),
                    "ci", "--ignore-scripts", "--omit=dev", "--no-bin-links", "--no-audit", "--no-fund",
                    "--userconfig=/dev/null", "--registry=https://registry.npmjs.org"], cwd=adapter, env=env, check=True)
    # Control-plane validation is separate from a subscription/model request.
    subprocess.run([str(node / "bin/node"), "--input-type=module", "-e",
                    "await import('./thread-tools.mjs');const {loadSdk}=await import('./sdk-runtime.mjs');const r=await loadSdk();console.log('Bundled Claude SDK '+r.version);"],
                   cwd=adapter, env=env, check=True)
    # The experimental OpenCode (ACP) runtime has no dependencies; wonderd
    # uses it to discover the owner’s installed OpenCode and configured models.
    acp = resources / "acp-runtime"
    if acp.exists():
        shutil.rmtree(acp)
    acp.mkdir()
    acp_source = ROOT / "services/acp-runtime"
    for path in [*acp_source.glob("*.mjs"), acp_source / "package.json"]:
        shutil.copy2(path, acp / path.name)
    shutil.copy2(ROOT / "packages/protocol/thread-tools.json", acp / "thread-tools.json")
    print(f"Bundled Node {NODE_VERSION} ({arch}) and the locked Claude runtime.")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit("Usage: package-claude-runtime.py APP/Contents/Resources")
    package(Path(sys.argv[1]))

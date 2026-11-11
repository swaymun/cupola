"""Fixed Diagnostics payload. No provider authentication or existing HOME data."""
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import time


def interrupted(_signal, _frame):
    raise InterruptedError("probe interrupted")


signal.signal(signal.SIGHUP, interrupted)
signal.signal(signal.SIGTERM, interrupted)
os.umask(0o077)
search = "/usr/local/bin:/usr/bin:/bin"
codex = shutil.which("codex", path=search)
if codex is None:
    raise SystemExit(69)

with tempfile.TemporaryDirectory(prefix="cupola-ssh-spike-") as root:
    home = Path(root)
    (home / "codex").mkdir()
    (home / "work").mkdir()
    # No inherited provider tokens, SSH agent, config, MCP servers or log paths.
    env = {
        "PATH": search,
        "HOME": root,
        "CODEX_HOME": str(home / "codex"),
        "XDG_CONFIG_HOME": str(home / "config"),
        "XDG_CACHE_HOME": str(home / "cache"),
        "XDG_DATA_HOME": str(home / "data"),
        "TMPDIR": root,
        "RUST_LOG": "off",
    }
    child = subprocess.Popen(
        [codex, "app-server"],
        cwd=home / "work",
        env=env,
        stdin=sys.stdin,
        stdout=sys.stdout,
        stderr=subprocess.DEVNULL,
        start_new_session=True,
    )
    try:
        deadline = time.monotonic() + 25
        while True:
            # Keep the exited group leader unreaped until group cleanup so its
            # PID/group ID cannot be reused by an unrelated process meanwhile.
            status = os.waitid(os.P_PID, child.pid, os.WEXITED | os.WNOHANG | os.WNOWAIT)
            if status is not None:
                code = status.si_status if status.si_code == os.CLD_EXITED else 128 + status.si_status
                break
            if time.monotonic() >= deadline:
                code = 124
                break
            time.sleep(0.025)
    except InterruptedError:
        code = 125
    finally:
        # Kill only this probe's group, including any descendants, before
        # deleting the temp root. The process group is never a shared service.
        try:
            os.killpg(child.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        child.wait(timeout=3)

print(json.dumps({"method": "cupolaSpike/cleaned", "params": {"removed": not home.exists()}}), flush=True)
raise SystemExit(code)

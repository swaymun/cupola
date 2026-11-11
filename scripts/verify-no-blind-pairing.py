#!/usr/bin/env python3
"""Release gate: a packaged `wonder` must not approve a phone without a person.

Usage: verify-no-blind-pairing.py PATH_TO_WONDER

Runs the binary itself: `pair --help` must not mention --yes or --watch-stdin,
and both flags must be rejected as unknown arguments (usage error, exit 2).
`pair --approve` must require --code containing the verification text the
person reads from their phone. The daemon compares it before confirming.
An isolated RPC fixture also checks
that JSON and non-interactive output hide the text, piped yes never confirms,
and approval forwards the phone code without echoing it. The published RPC
schema must require the code too. Unix builds also run real TTY probes: only
phone text can approve, and JSON mode never reveals it even in a terminal.
The dev-flag probes include --cancel so even a dev build fails argument parsing.
The approval probe uses an isolated empty data directory and a nonexistent
device ID, so a regressed binary cannot approve a phone or touch the host.
"""
import base64
import hashlib
import json
import os
from pathlib import Path
import struct
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import subprocess
import sys
import tempfile
import time


def run(binary, *args, env=None, input=None):
    return subprocess.run([binary, *args], capture_output=True, text=True,
                          timeout=30, input=input,
                          stdin=subprocess.DEVNULL if input is None else None, env=env)


def run_tty(binary, *args, env, answer=None, on_output=None):
    """Unix release check using real terminal input/output, without a screen."""
    import pty
    import select
    master, slave = pty.openpty()
    output = bytearray()
    before_answer = None
    process = None
    try:
        process = subprocess.Popen([binary, *args], stdin=slave, stdout=slave,
                                   stderr=slave, env=env, start_new_session=True)
        os.close(slave)
        slave = None
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            if select.select([master], [], [], 0.05)[0]:
                try:
                    chunk = os.read(master, 65536)
                except OSError:
                    break
                if not chunk:
                    break
                output.extend(chunk)
                if on_output:
                    on_output(chunk.decode(errors="replace"))
                # Answer only after the app requests it; this lets the gate
                # distinguish host disclosure from the terminal's input echo.
                if answer is not None and before_answer is None and (
                        b"Verification text from your phone" in output or b"[y/N]" in output):
                    before_answer = output.decode(errors="replace")
                    os.write(master, (answer + "\n").encode())
            elif process.poll() is not None:
                break
        if process.poll() is None:
            process.kill()
        result = subprocess.CompletedProcess([binary, *args], process.wait(),
                                             output.decode(errors="replace"), "")
        return result, before_answer
    finally:
        if process is not None and process.poll() is None:
            process.kill()
            process.wait()
        os.close(master)
        if slave is not None:
            os.close(slave)


PHONE_TEXT = "B36D1B62BB5F"  # independently pinned by the native transcript fixture
PENDING = [{"deviceId": "device-01", "label": "Release gate phone", "offerId": "o1",
            "hostInstallationId": "install-1", "expiresAtMs": 9000000000000,
            "display": None,
            # A stale/compromised daemon must not make the CLI disclose a
            # transcript it no longer needs. Real-daemon tests ensure this
            # challenge is absent at the source as well.
            "challenge": {"deviceId": "device-01", "challengeId": "challenge-01",
                          "nonce": "nonce-01", "origin": "https://wonder.example.ts.net",
                          "hostInstallationId": "install-1", "offerId": "o1",
                          "issuedAtMs": 1000, "expiresAtMs": 61000}}]


class PairingFixture(BaseHTTPRequestHandler):
    """An isolated, standard-library RPC peer; never connects to a real host."""
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def exact(self, size):
        result = self.rfile.read(size)
        if len(result) != size:
            raise EOFError
        return result

    def frame(self, payload, opcode=1):
        size = len(payload)
        header = bytes([0x80 | opcode])
        if size < 126:
            header += bytes([size])
        elif size < 65536:
            header += bytes([126]) + struct.pack("!H", size)
        else:
            header += bytes([127]) + struct.pack("!Q", size)
        self.wfile.write(header + payload)
        self.wfile.flush()

    def do_GET(self):
        if self.path != "/api/v1/rpc" or self.headers.get("Upgrade", "").lower() != "websocket":
            self.send_error(404)
            return
        accept = base64.b64encode(hashlib.sha1(
            (self.headers["Sec-WebSocket-Key"] + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest()).decode()
        self.send_response(101)
        self.send_header("Upgrade", "websocket")
        self.send_header("Connection", "Upgrade")
        self.send_header("Sec-WebSocket-Accept", accept)
        self.end_headers()
        try:
            while True:
                first, second = self.exact(2)
                length = second & 127
                if length == 126:
                    length = struct.unpack("!H", self.exact(2))[0]
                elif length == 127:
                    length = struct.unpack("!Q", self.exact(8))[0]
                if length > 1024 * 1024:
                    raise EOFError
                mask = self.exact(4) if second & 128 else None
                payload = self.exact(length)
                if mask:
                    payload = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
                opcode = first & 15
                if opcode == 8:
                    break
                if opcode == 9:
                    self.frame(payload, 10)
                    continue
                if opcode != 1:
                    continue
                request = json.loads(payload)
                if "id" not in request:
                    continue
                method = request["method"]
                params = request.get("params") or {}
                self.server.calls.append((method, params))
                error = None
                if method == "initialize":
                    result = {"protocolVersion": 1,
                              "serverInfo": {"name": "pair-release-gate", "version": "0", "apiVersion": "v1"},
                              "authority": "owner", "hostEpoch": "e", "hostInstallationId": "i",
                              "methods": [], "notifications": [],
                              "capabilities": {"compactView": False, "maxInFlight": 32, "maxRequestBytes": 1024}}
                elif method == "host.status":
                    result = {}
                elif method == "pairing.offer":
                    result = {"offerId": "o1", "url": "https://wonder.example.ts.net/pair#fixture",
                              "humanCode": "ABCD", "expiresAtMs": 9000000000000}
                elif method == "pairing.pending":
                    result = PENDING
                elif method == "pairing.confirm":
                    result = {}
                    if params.get("code") != PHONE_TEXT:
                        error = {"code": -32003, "message": "verification text does not match"}
                elif method in ("pairing.cancel", "pairing.reject"):
                    result = {}
                else:
                    error = {"code": -32601, "message": "unexpected fixture method"}
                response = {"jsonrpc": "2.0", "id": request["id"]}
                response["error" if error else "result"] = error if error else result
                self.frame(json.dumps(response).encode())
        except (EOFError, OSError, ValueError):
            pass
        finally:
            self.close_connection = True


def secrecy_probes(binary):
    failures = []
    with tempfile.TemporaryDirectory(prefix="wonder-pair-secrecy-") as state:
        server = ThreadingHTTPServer(("127.0.0.1", 0), PairingFixture)
        server.daemon_threads = True
        server.calls = []
        worker = threading.Thread(target=server.serve_forever, daemon=True)
        worker.start()
        try:
            directory = Path(state) / "headless"
            directory.mkdir()
            (directory / "config.json").write_text(json.dumps({
                "listenAddr": f"127.0.0.1:{server.server_port}", "tunnel": "tailscale"}))
            (directory / "loopback-capability").write_text("isolated-release-gate")
            env = dict(os.environ, WONDER_DATA_DIR=state)
            for mode in ([], ["--json"]):
                server.calls.clear()
                probe = run(binary, "pair", "--timeout", "1", *mode, env=env, input="yes\n")
                output = probe.stdout + probe.stderr
                if PHONE_TEXT in output or "verificationText" in output or "nonce-01" in output:
                    failures.append(f"pair {' '.join(mode)} exposed the phone comparison")
                if not any(m == "pairing.pending" for m, _ in server.calls):
                    failures.append(f"pair {' '.join(mode)} never reached verification")
                if any(m in ("pairing.confirm", "pairing.reject") for m, _ in server.calls):
                    failures.append(f"pair {' '.join(mode)} accepted piped yes as a decision")
                if "--json" in mode:
                    events = [json.loads(line) for line in probe.stdout.splitlines()]
                    verification = [e for e in events if e.get("event") == "verification"]
                    if len(verification) != 1:
                        failures.append("JSON pair did not emit one verification event")
            if os.name == "posix":
                for answer, should_pass in [("yes", False), (PHONE_TEXT, True)]:
                    probe, before = run_tty(binary, "pair", "--timeout", "2", env=env, answer=answer)
                    if before is None:
                        failures.append("interactive pair never requested the phone text")
                    elif PHONE_TEXT in before:
                        failures.append("interactive pair disclosed the comparison before phone input")
                    if (probe.returncode == 0) != should_pass:
                        failures.append("terminal pair did not require the phone's verification text")
                    if should_pass and "Verification text: " + PHONE_TEXT not in probe.stdout:
                        failures.append("terminal did not display the matched text after verification")
                probe, _ = run_tty(binary, "pair", "--json", "--timeout", "1", env=env)
                if PHONE_TEXT in probe.stdout or "verificationText" in probe.stdout:
                    failures.append("JSON pair exposed the comparison on a real TTY")
            for code, should_pass in [("wrong", False), (PHONE_TEXT, True)]:
                probe = run(binary, "pair", "--approve", "device-01", "--code", code, "--json", env=env)
                if (probe.returncode == 0) != should_pass:
                    failures.append(f"phone-code {'approval' if should_pass else 'refusal'} failed (exit {probe.returncode})")
                if PHONE_TEXT in probe.stdout + probe.stderr:
                    failures.append("pair approval echoed the phone comparison")
        finally:
            server.shutdown()
            server.server_close()
            worker.join(timeout=5)
    schema_probe = run(binary, "rpc", "--schema")
    if schema_probe.returncode:
        failures.append("RPC schema could not be inspected")
    else:
        schema = json.loads(schema_probe.stdout)
        params = schema["methods"]["pairing.confirm"]["params"]
        if "$ref" in params:
            params = schema["$defs"][params["$ref"].split("/")[-1]]
        if "code" not in params.get("required", []):
            failures.append("RPC pairing.confirm does not require the phone's code")
    return failures



def check(binary):
    failures = []
    help_text = run(binary, "pair", "--help")
    if help_text.returncode:
        failures.append(f"`pair --help` failed (exit {help_text.returncode})")
    shown = help_text.stdout + help_text.stderr
    for flag in ("--yes", "--watch-stdin"):
        if flag in shown:
            failures.append(f"`pair --help` mentions {flag}")
        probe = run(binary, "pair", "--cancel", "release-gate-no-offer", flag)
        text = probe.stdout + probe.stderr
        if probe.returncode != 2 or f"unexpected argument '{flag}'" not in text:
            failures.append(f"`pair {flag}` was not rejected as an unknown argument (exit {probe.returncode})")
    with tempfile.TemporaryDirectory(prefix="wonder-pair-release-gate-") as state:
        probe = run(binary, "pair", "--approve", "release-gate-no-device",
                    env=dict(os.environ, WONDER_DATA_DIR=state))
    text = probe.stdout + probe.stderr
    if probe.returncode != 2 or "required arguments" not in text or "--code <TEXT>" not in text:
        failures.append(f"`pair --approve` did not require --code <TEXT> (exit {probe.returncode})")
    failures.extend(secrecy_probes(binary))
    return failures


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit(__doc__)
    problems = check(sys.argv[1])
    if problems:
        raise SystemExit("Blind pairing present in release binary:\n  " + "\n  ".join(problems))
    print("ok: no blind flags; agent output hides the comparison; CLI and RPC approval require the phone code")

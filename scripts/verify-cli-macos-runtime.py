#!/usr/bin/env python3
"""Staged-bundle smoke: permissions, JSONL framing and helper EOF.

No grants, provider requests, input events, service installation or updates.
An explicit --window-source also checks real capture metadata for that exact
owned window; it stores no pixels and never asks for missing permissions.
"""
import argparse
import json
import os
from pathlib import Path
import queue
import re
import subprocess
import tempfile
import threading
import time
import uuid


def capture_window(app, source):
    """The real signed helper owns the ScreenCaptureKit regression contract."""
    assert re.fullmatch(r"app:[A-Za-z0-9.-]+#[1-9][0-9]*", source), \
        "Use an exact owned window: app:<bundle-id>#<window-number>"
    nonce = str(uuid.uuid4())
    session = str(uuid.uuid4())
    common = dict(handshake=nonce, sessionID=session, generation=1)
    worker = subprocess.Popen([str(app / "Contents/Helpers/wonder-computer-use")],
        env=dict(os.environ, WONDER_COMPUTER_USE_HANDSHAKE=nonce),
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
        text=True, bufsize=1)
    replies = queue.Queue(maxsize=64)

    def read():
        for line in worker.stdout:
            try:
                replies.put(json.loads(line), timeout=1)
            except (ValueError, queue.Full):
                return

    threading.Thread(target=read, daemon=True).start()

    def send(id, method, **extra):
        worker.stdin.write(json.dumps(dict(id=id, method=method, params=dict(common, **extra))) + "\n")
        worker.stdin.flush()

    began = time.monotonic()
    metadata = []
    first_frame = None
    started = False
    try:
        send(1, "capabilities")
        send(2, "capture.prepare", sourceID=source, viewerCount=1, videoQuality="auto")
        send(3, "capture.pick", sourceID=source)
        deadline = time.monotonic() + 12
        while time.monotonic() < deadline and len(metadata) < 2:
            try:
                reply = replies.get(timeout=0.5)
            except queue.Empty:
                continue
            assert not reply.get("error"), f"Window helper rejected a request: {reply.get('error')}"
            assert reply.get("event") != "capture.failed", \
                f"Window capture failed: {reply.get('status', {}).get('reason')}"
            if reply.get("event") == "capture.sourceSelected" and not started:
                send(4, "capture.start")
                started = True
            if reply.get("event") == "capture.frameMetadata":
                frame = reply["metadata"]
                assert frame["sessionID"] == session and frame["generation"] == 1
                assert reply["status"]["sourceID"] == "window:" + source.rsplit("#", 1)[1]
                first_frame = first_frame or time.monotonic() - began
                metadata.append(frame)
        assert len(metadata) == 2, "The exact window produced fewer than two capture metadata samples"
        assert metadata[1]["frameSequence"] > metadata[0]["frameSequence"]
        return dict(name="exact-window-capture", source=source, metadataSamples=len(metadata),
                    firstFrameSeconds=round(first_frame, 3),
                    frameSequences=[frame["frameSequence"] for frame in metadata],
                    seconds=round(time.monotonic() - began, 3))
    finally:
        try:
            send(5, "capture.stop")
            send(6, "stop")
        except (OSError, ValueError):
            pass
        try:
            worker.stdin.close()
        except OSError:
            pass
        try:
            worker.wait(timeout=5)
        except subprocess.TimeoutExpired:
            worker.kill()
            worker.wait()


def run(app, window_source=None):
    cli = app / "Contents/MacOS/cupola"
    helper = app / "Contents/Helpers/wonder-computer-use"
    checks = []
    with tempfile.TemporaryDirectory(prefix="wonder-cli-link-") as directory:
        link = Path(directory) / "cupola"
        link.symlink_to(cli)
        for name, executable in [("permissions-status-bundle", cli), ("permissions-status-path-symlink", link)]:
            began = time.monotonic()
            reply = subprocess.run([str(executable), "permissions", "status", "--json"],
                                   capture_output=True, text=True, check=True, timeout=25)
            value = json.loads(reply.stdout)
            permissions = value["permissions"]
            assert [p["id"] for p in permissions] == ["screenRecording", "accessibility"]
            assert all(p["state"] in ("granted", "denied") for p in permissions)
            checks.append(dict(name=name, seconds=round(time.monotonic() - began, 3), result=value))
    requests = [dict(apps=["invalid app"]), dict(method="click", params={})]
    began = time.monotonic()
    reply = subprocess.run([str(helper), "--discover-sources"],
        input="".join(json.dumps(value) + "\n" for value in requests),
        capture_output=True, text=True, check=True, timeout=25)
    errors = [json.loads(line)["error"] for line in reply.stdout.splitlines()]
    assert errors == ["invalid_discovery_request"] * 2, errors
    checks.append(dict(name="discovery-framing-and-eof", requests=2,
                       seconds=round(time.monotonic() - began, 3), result=errors))
    began = time.monotonic()
    reply = subprocess.run([str(helper), "--watch-dialogs"], input="",
                           capture_output=True, text=True, check=True, timeout=25)
    # An EOF can arrive before the first observation. Never retain dialog titles.
    for line in reply.stdout.splitlines():
        assert isinstance(json.loads(line)["dialogs"], list)
    checks.append(dict(name="dialog-watcher-eof", seconds=round(time.monotonic() - began, 3)))
    if window_source:
        checks.append(capture_window(app, window_source))
    return dict(status="passed", environment="Mac signed staging bundle", app=str(app), checks=checks)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("bundle", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--window-source", help="Capture an explicitly owned app:<bundle-id>#<window-number>; metadata only")
    args = parser.parse_args()
    try:
        report = run(args.bundle.resolve(strict=True), args.window_source)
    except subprocess.CalledProcessError as error:
        report = dict(status="failed", command=error.cmd, exitCode=error.returncode,
                      stdout=error.stdout, stderr=error.stderr)
        args.output.write_text(json.dumps(report, indent=2) + "\n")
        print(json.dumps(report, indent=2))
        raise SystemExit(1)
    except (AssertionError, subprocess.TimeoutExpired) as error:
        report = dict(status="failed", error=str(error))
        args.output.write_text(json.dumps(report, indent=2) + "\n")
        print(json.dumps(report, indent=2))
        raise SystemExit(1)
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))

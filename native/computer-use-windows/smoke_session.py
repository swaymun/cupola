"""End-to-end smoke test of the Windows helper in the signed-in desktop session.

Plays wonderd and the phone at once: starts the helper over its JSON-lines
protocol (with a handshake), picks a source, answers the helper's WebRTC offer
with a local aiortc peer, receives video frames, takes control and types into
the target window. Then it covers the target with another window (the stream
must keep showing only the target) and switches the source to that window.
Writes a JSON report and PNG frames to --out; --record also saves what the
phone received (stream.mp4) next to the whole desktop (desktop.mp4).

    python smoke_session.py --out C:\\path\\smoke [--window notepad.exe] [--type "hello"] [--record]

Run it in the interactive session (for example through a scheduled task with
/IT); a service or SSH session has no desktop to capture or drive.
"""
import argparse
import asyncio
from fractions import Fraction
import json
import os
from pathlib import Path
import subprocess
import sys
import time
import uuid

from aiortc import RTCPeerConnection, RTCSessionDescription

HELPER = Path(__file__).with_name("wonder_computer_windows.py")
# A plain Win32 (Tk) target that saves its text box to a file every 200 ms.
# The new WinUI Notepad restores old tabs and drops some injected characters.
TARGET = r"""
import sys, tkinter as tk
root = tk.Tk(); root.title("Wonder smoke target"); root.geometry("640x360+80+80")
text = tk.Text(root, font=("Segoe UI", 16)); text.pack(fill="both", expand=True)
def save():
    open(sys.argv[1], "w", encoding="utf-8").write(text.get("1.0", "end-1c")); root.after(200, save)
root.after(200, save); root.after(120000, root.destroy); root.mainloop()
"""
# A red window placed over the target, later picked as the second source.
COVER = r"""
import tkinter as tk
root = tk.Tk(); root.title("Wonder cover window"); root.geometry("420x260+200+160")
root.configure(bg="#c0392b"); root.attributes("-topmost", True)
tk.Label(root, text="COVER WINDOW", bg="#c0392b", fg="white", font=("Segoe UI", 28, "bold")).pack(expand=True)
root.after(120000, root.destroy); root.mainloop()
"""


class Recorder:
    """Letterboxes BGR frames of any size into a fixed-size H.264 file."""

    def __init__(self, path, fps, size=(1280, 720)):
        import av
        self.container = av.open(str(path), "w")
        self.stream = self.container.add_stream("libx264", rate=fps)
        self.stream.width, self.stream.height = size
        self.stream.pix_fmt = "yuv420p"
        self.size = size
        self.started = time.monotonic()
        self.fps = fps
        self.pts = -1

    def add(self, bgr):
        import av
        import cv2
        import numpy
        width, height = self.size
        scale = min(width / bgr.shape[1], height / bgr.shape[0])
        fitted = cv2.resize(bgr, (max(2, int(bgr.shape[1] * scale)), max(2, int(bgr.shape[0] * scale))))
        canvas = numpy.full((height, width, 3), 40, dtype=numpy.uint8)
        top, left = (height - fitted.shape[0]) // 2, (width - fitted.shape[1]) // 2
        canvas[top:top + fitted.shape[0], left:left + fitted.shape[1]] = fitted
        frame = av.VideoFrame.from_ndarray(canvas, format="bgr24")
        self.pts = max(int((time.monotonic() - self.started) * self.fps), self.pts + 1)
        frame.pts, frame.time_base = self.pts, Fraction(1, self.fps)
        for packet in self.stream.encode(frame):
            self.container.mux(packet)

    def close(self):
        for packet in self.stream.encode():
            self.container.mux(packet)
        self.container.close()


async def record_desktop(path, stop):
    import mss
    import numpy
    recorder = Recorder(path, 10)
    with (getattr(mss, "MSS", None) or mss.mss)() as sct:
        while not stop.is_set():
            shot = sct.grab(sct.monitors[1])
            recorder.add(numpy.frombuffer(shot.bgra, dtype=numpy.uint8).reshape(shot.height, shot.width, 4)[:, :, :3])
            await asyncio.sleep(0.1)
    recorder.close()


def red_share(frame):
    """Share of pixels in the cover window's red (#c0392b)."""
    bgr = frame.to_ndarray(format="bgr24").astype(int)
    red = (abs(bgr[:, :, 2] - 0xC0) < 40) & (abs(bgr[:, :, 1] - 0x39) < 40) & (abs(bgr[:, :, 0] - 0x2B) < 40)
    return round(float(red.mean()), 4)


class Helper:
    def __init__(self, handshake):
        env = dict(os.environ, WONDER_COMPUTER_USE_HANDSHAKE=handshake)
        self.handshake = handshake
        self.proc = subprocess.Popen([sys.executable, "-X", "utf8", str(HELPER)], env=env,
                                     stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=sys.stderr)
        self.next_id = 1
        self.pending = {}
        self.events = asyncio.Queue()

    async def pump(self):
        loop = asyncio.get_running_loop()
        while True:
            line = await loop.run_in_executor(None, self.proc.stdout.readline)
            if not line:
                return
            message = json.loads(line)
            if "id" in message and message["id"] in self.pending:
                self.pending.pop(message["id"]).set_result(message)
            else:
                await self.events.put(message)

    async def call(self, method, **params):
        request_id = self.next_id
        self.next_id += 1
        future = asyncio.get_running_loop().create_future()
        self.pending[request_id] = future
        params["handshake"] = self.handshake
        self.proc.stdin.write((json.dumps({"id": request_id, "method": method, "params": params}) + "\n").encode())
        self.proc.stdin.flush()
        message = await asyncio.wait_for(future, 15)
        if "error" in message:
            raise RuntimeError(f"{method}: {message['error']}")
        return message["result"]

    async def event(self, name, timeout=20):
        deadline = time.monotonic() + timeout
        while True:
            message = await asyncio.wait_for(self.events.get(), max(0.1, deadline - time.monotonic()))
            if message.get("event") == name:
                return message


async def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--out", required=True)
    parser.add_argument("--window", default="python.exe")
    parser.add_argument("--type", default="hello from Wonder é 😀")
    parser.add_argument("--record", action="store_true")
    args = parser.parse_args()
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    report = {"started": time.time(), "steps": []}
    stop_desktop = asyncio.Event()
    desktop = asyncio.create_task(record_desktop(out / "desktop.mp4", stop_desktop)) if args.record else None
    cover = None

    def step(name, **fields):
        report["steps"].append({"step": name, "t": round(time.time() - report["started"], 2), **fields})
        (out / "report.json").write_text(json.dumps(report, indent=2))

    target = None
    typed_file = out / "typed.txt"
    typed_file.unlink(missing_ok=True)
    if args.window == "python.exe":
        target = subprocess.Popen([sys.executable, "-c", TARGET, str(typed_file)])
        await asyncio.sleep(3)
    listing = json.loads(subprocess.run([sys.executable, str(HELPER), "--discover-sources"],
                                        input=json.dumps({"apps": [args.window]}) + "\n",
                                        capture_output=True, text=True, timeout=30).stdout)
    step("listed", sources=listing["sources"])
    chosen = [s for s in listing["sources"] if s["name"] == "Wonder smoke target"] or listing["sources"]
    source_id = chosen[0]["id"] if chosen else "display:1"

    helper = Helper(str(uuid.uuid4()))
    pump = asyncio.create_task(helper.pump())
    try:
        caps = await helper.call("capabilities")
        step("capabilities", control=caps["control"])
        session = {"sessionID": str(uuid.uuid4()).upper(), "generation": 1}
        prepared = await helper.call("capture.prepare", viewerCount=1, videoQuality="standard", **session)
        step("prepared", accepted=prepared["accepted"])
        picked = await helper.call("capture.pick", sourceID=source_id, **session)
        if not picked["accepted"]:
            raise RuntimeError(f"pick refused: {picked.get('errorCode')}")
        selected = await helper.event("capture.sourceSelected")
        step("selected", source=selected["source"])

        offer = await helper.event("signal.offer")
        phone = RTCPeerConnection()
        frames = []
        got_frames = asyncio.Event()
        stream = Recorder(out / "stream.mp4", 10) if args.record else None
        import cv2  # windows-capture already depends on OpenCV

        def snapshot(name):
            cv2.imwrite(str(out / f"{name}.png"), frames[-1].to_ndarray(format="bgr24"))

        @phone.on("track")
        def on_track(track):
            async def consume():
                while True:
                    try:
                        frame = await track.recv()
                    except Exception:  # noqa: BLE001 - track ended
                        return
                    frames.append(frame)
                    if stream is not None:
                        stream.add(frame.to_ndarray(format="bgr24"))
                    if len(frames) in (5, 30):
                        snapshot(f"frame-{len(frames)}")
                    if len(frames) >= 30:
                        got_frames.set()
            asyncio.ensure_future(consume())

        await phone.setRemoteDescription(RTCSessionDescription(sdp=offer["sdp"], type="offer"))
        await phone.setLocalDescription(await phone.createAnswer())
        revision = offer["peerRevision"]
        await helper.call("signal.answer", peerRevision=revision, sdp=phone.localDescription.sdp, **session)
        for line in phone.localDescription.sdp.split("\r\n"):
            if line.startswith("a=candidate:"):
                await helper.call("signal.candidate", peerRevision=revision, candidate=line[2:],
                                  sdpMid="0", sdpMLineIndex=0, **session)
        await helper.call("capture.start", **session)
        started = time.monotonic()
        await asyncio.wait_for(got_frames.wait(), 60)
        elapsed = time.monotonic() - started
        step("frames", count=len(frames), seconds=round(elapsed, 2),
             size=[frames[-1].width, frames[-1].height], fps=round(len(frames) / elapsed, 1))

        status = await helper.call("capture.status", **session)
        identity = {"leaseID": "smoke-lease", "requestID": "smoke-request", "sourceID": source_id,
                    "geometryRevision": status["geometryRevision"], **session}
        consent = await helper.call("control.consent", **identity)
        activated = await helper.call("control.activate", **identity)
        step("control", consent=consent["granted"], activated=activated["activated"])
        actions = [{"type": "pointer", "x": 0.5, "y": 0.5, "phase": "down", "button": "left"},
                   {"type": "pointer", "x": 0.5, "y": 0.5, "phase": "up", "button": "left"},
                   {"type": "text", "text": args.type},
                   {"type": "key", "key": "return", "phase": "press", "modifiers": 0}]
        typed = await helper.call("control.input", sequence=1, actions=actions, **identity)
        step("input", result=typed)
        await helper.call("control.input", sequence=2, actions=[
            {"type": "key", "key": "a", "phase": "press", "modifiers": 2},
            {"type": "key", "key": "c", "phase": "press", "modifiers": 2}], **identity)
        # SendInput returns before the app handles the keys.
        await asyncio.sleep(0.7)
        copied = await helper.call("control.input", sequence=3, actions=[
            {"type": "clipboard", "operation": "copyToPhone"}], **identity)
        step("clipboard", text=copied.get("clipboardText"))
        await asyncio.sleep(1)
        released = await helper.call("control.release", **identity)
        step("released", released=released["released"])

        # Privacy: cover the target; the stream must still show only the target.
        cover = subprocess.Popen([sys.executable, "-c", COVER])
        await asyncio.sleep(3)
        snapshot("covered")
        step("covered", frames=len(frames), red_share=red_share(frames[-1]))

        # Window switch: pick the cover window while streaming.
        listing = json.loads(subprocess.run([sys.executable, str(HELPER), "--discover-sources"],
                                            input=json.dumps({"apps": [args.window]}) + "\n",
                                            capture_output=True, text=True, timeout=30).stdout)
        second = [s for s in listing["sources"] if s["name"] == "Wonder cover window"]
        if second:
            before = len(frames)
            switched = await helper.call("capture.pick", sourceID=second[0]["id"], **session)
            selected = await helper.event("capture.sourceSelected")
            await helper.call("capture.start", **session)
            await asyncio.sleep(3)
            snapshot("switched")
            step("switched", accepted=switched["accepted"], source=selected["source"],
                 frames=len(frames) - before, size=[frames[-1].width, frames[-1].height],
                 red_share=red_share(frames[-1]))
        else:
            step("switched", error="cover window not listed")
        await helper.call("capture.stop", reason="smoke_done", **session)
        await phone.close()
        if stream is not None:
            stream.close()
        await asyncio.sleep(0.5)
        typed = typed_file.read_text(encoding="utf-8") if typed_file.exists() else None
        step("typed", text=typed)
        steps = {s["step"]: s for s in report["steps"]}
        report["ok"] = bool(frames) and typed is not None and typed.startswith(args.type) \
            and copied.get("clipboardText", "").startswith(args.type) \
            and steps["covered"]["red_share"] < 0.01 and steps["switched"].get("red_share", 0) > 0.5
        step("done", ok=report["ok"])
    except Exception as error:  # noqa: BLE001 - recorded in the report
        step("failed", error=repr(error))
        raise
    finally:
        try:
            await helper.call("stop")
        except Exception:  # noqa: BLE001
            pass
        helper.proc.wait(timeout=10)
        pump.cancel()
        for window in (target, cover):
            if window is not None and window.poll() is None:
                window.kill()
        if desktop is not None:
            stop_desktop.set()
            await desktop


if __name__ == "__main__":
    asyncio.run(main())

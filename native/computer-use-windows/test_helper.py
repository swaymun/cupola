"""Windows helper tests. Run on Windows (x64 Python with requirements.txt):

    python -m unittest native\\computer-use-windows\\test_helper.py

Only DesktopInputTargetTests sends input, and only to its own two Tk windows
(in the signed-in desktop session). `deliver` is replaced in lease tests,
input targeting uses fakes, and the encoding tests only build INPUT records.
"""
import asyncio
import ctypes
import importlib.util
import json
import io
import os
from pathlib import Path
import subprocess
import sys
import time
import unittest
from unittest.mock import patch
import base64

import numpy

spec = importlib.util.spec_from_file_location("helper", Path(__file__).with_name("wonder_computer_windows.py"))
helper = importlib.util.module_from_spec(spec)
spec.loader.exec_module(helper)


class WindowInventoryTests(unittest.IsolatedAsyncioTestCase):
    def test_jsonl_no_display_uses_the_same_capture_capability(self):
        replies = []
        with patch.object(helper.sys, "stdin", io.StringIO('{"apps":[],"allWindows":true}\n')), \
             patch.object(helper, "write", side_effect=replies.append), \
             patch.object(helper, "virtual_screen", return_value=(0, 0, 0, 0)), \
             patch.object(helper, "list_windows", side_effect=AssertionError("headless enumeration")):
            helper.discover_sources()
        self.assertEqual(replies, [{"available": False, "sources": [], "capture": {"available": False, "reason": "no_display"}}])

    def test_inventory_error_is_a_failed_poll_and_jsonl_worker_recovers(self):
        with patch.object(helper, "virtual_screen", side_effect=OSError("desktop read failed")):
            with self.assertRaises(helper.HelperError):
                helper.capture_capability()
        replies = []
        with patch.object(helper.sys, "stdin", io.StringIO('{"allWindows":true}\n{"allWindows":true}\n')), \
             patch.object(helper, "write", side_effect=replies.append), \
             patch.object(helper, "capture_capability", side_effect=[helper.HelperError("capture_unavailable"), {"available": True}]), \
             patch.object(helper, "list_windows", return_value=[]):
            helper.discover_sources()
        self.assertEqual(replies, [{"error": "source_discovery_failed"},
                                  {"available": True, "sources": [], "capture": {"available": True}}])

    def test_discovery_bounds_the_complete_json_line_with_escaped_titles_and_icons(self):
        sources = [{"id": f"app:editor#{index}", "appId": "e" * 140,
                    "appName": '"' * 160, "name": '"' * 160,
                    "windowTitle": '"' * 160, "iconPNG": "x" * 3072} for index in range(1, 33)]
        answer = helper.discovery_answer(True, sources)
        self.assertTrue(answer["sources"])
        self.assertEqual(answer["sources"], sources[:len(answer["sources"])])
        self.assertLessEqual(len(json.dumps(answer, ensure_ascii=False, separators=(",", ":")).encode()), helper.MAX_LINE_BYTES - 512)
        self.assertEqual(helper.discovery_answer(False, sources), {"available": False, "sources": []})

    def test_all_window_inventory_excludes_minimized_hidden_owned_and_tiny_windows(self):
        class Desktop:
            def EnumWindows(self, callback, context):
                for window in range(1, 7):
                    if not callback(window, context):
                        break

            def IsWindowVisible(self, hwnd): return hwnd != 2
            def IsIconic(self, hwnd): return hwnd == 3
            def GetAncestor(self, hwnd, _): return 1 if hwnd == 4 else hwnd
            def GetWindowLongW(self, hwnd, _): return 0

        with patch.object(helper, "user32", Desktop()), \
             patch.object(helper, "window_class", return_value="Notepad"), \
             patch.object(helper, "cloaked", return_value=False), \
             patch.object(helper, "window_exe", side_effect=lambda hwnd: "notepad.exe" if hwnd == 1 else "other.exe"), \
             patch.object(helper, "window_title", side_effect=lambda hwnd: f"Document {hwnd}"), \
             patch.object(helper, "window_rect", side_effect=lambda hwnd: (0, 0, 79 if hwnd == 5 else 800, 600)), \
             patch.object(helper, "app_icon", return_value="fixture-png"):
            sources = helper.list_windows([], all_windows=True)
            self.assertEqual([source["id"] for source in sources], ["app:notepad.exe#1", "app:other.exe#6"])
            self.assertEqual(sources[0]["appName"], "notepad")
            self.assertEqual(sources[0]["windowTitle"], "Document 1")
            self.assertEqual(sources[0]["iconPNG"], "fixture-png")
            self.assertEqual([source["id"] for source in helper.list_windows(["notepad.exe"])], ["app:notepad.exe#1"])
            self.assertEqual(helper.list_windows([]), [])

    async def test_capture_capability_uses_the_actual_desktop_in_negotiation(self):
        for secure, bounds, expected in [(False, (0, 0, 800, 600), True),
                                          (True, (0, 0, 800, 600), False),
                                          (False, (0, 0, 0, 0), False)]:
            with patch.object(helper, "secure_desktop_active", return_value=secure), \
                 patch.object(helper, "virtual_screen", return_value=bounds):
                self.assertEqual(helper.capture_capability()["available"], expected)
                response = await helper.Helper().handle("capabilities", {})
                self.assertEqual(response["capture"]["available"], expected)
                self.assertEqual(response["capture"].get("reason"),
                                 "no_display" if not bounds[2] else "secure_desktop" if secure else None)

    def test_inventory_excludes_overlay_styles_and_untitled_utility_layers(self):
        class Desktop:
            def EnumWindows(self, callback, context): callback(41, context)
            def IsWindowVisible(self, _): return True
            def IsIconic(self, _): return False
            def GetAncestor(self, hwnd, _): return hwnd
            def GetWindowLongW(self, _hwnd, _index): return self.style
            def GetLayeredWindowAttributes(self, _hwnd, _color, alpha, flags):
                alpha._obj.value, flags._obj.value = self.alpha, 2
                return 1
            style = 0
            alpha = 255

        desktop = Desktop()
        with patch.object(helper, "user32", desktop), \
             patch.object(helper, "window_class", return_value="Notepad") as window_class, \
             patch.object(helper, "cloaked", return_value=False), \
             patch.object(helper, "window_exe", return_value="notepad.exe"), \
             patch.object(helper, "window_title", return_value="Software Cursor") as title, \
             patch.object(helper, "window_rect", return_value=(0, 0, 800, 600)) as rect, \
             patch.object(helper, "app_icon", return_value=None):
            for style, included in [(0, True), (0x20, False), (0x80, False), (0x08000000, False), (0x80000, True)]:
                with self.subTest(style=style):
                    desktop.style = style
                    self.assertEqual(bool(helper.list_windows([], True)), included)
            desktop.style, desktop.alpha = 0x80000, 0
            self.assertEqual(helper.list_windows([], True), [])
            desktop.style, title.return_value = 0, ""
            self.assertEqual(helper.list_windows([], True), [])
            title.return_value = "Notes"
            for (width, height), included in [((0, 600), False), ((800, 0), False), ((79, 600), False), ((800, 59), False), ((80, 60), True)]:
                with self.subTest(size=(width, height)):
                    rect.return_value = (0, 0, width, height)
                    self.assertEqual(bool(helper.list_windows([], True)), included)
            rect.return_value = (0, 0, 800, 600)
            title.return_value = "Program Manager"
            for name in ("Progman", "WorkerW", "Shell_TrayWnd", "Shell_SecondaryTrayWnd", "#32768", "SysShadow", "tooltips_class32"):
                with self.subTest(window_class=name):
                    window_class.return_value = name
                    self.assertEqual(helper.list_windows([], True), [])


class EncodingTests(unittest.TestCase):
    def test_canonical_key_names(self):
        for name, vk in [("insert", 0x2D), ("printscreen", 0x2C), ("menu", 0x5D),
                         ("f13", 0x7C), ("f14", 0x7D), ("f15", 0x7E), ("f16", 0x7F),
                         ("f17", 0x80), ("f18", 0x81), ("f19", 0x82), ("volumeup", 0xAF),
                         ("volumedown", 0xAE), ("mute", 0xAD), ("playpause", 0xB3),
                         ("next", 0xB0), ("previous", 0xB1), ("rightoption", 0xA5),
                         ("super", 0x5B), ("win", 0x5B), ("ctrl", 0xA2), ("alt", 0xA4)]:
            self.assertEqual(helper.virtual_key(name), vk)
            self.assertEqual(helper.virtual_key(name.upper()), vk)
            for up in (False, True):
                item = helper.key_input(vk, up)
                extended = vk in {0x2D, 0x2C, 0x5D, 0xA5, 0x5B, 0xAD, 0xAE, 0xAF, 0xB0, 0xB1, 0xB3}
                flags = helper.KEYEVENTF_EXTENDEDKEY if extended else 0
                self.assertEqual(item.ki.dwFlags, flags | (helper.KEYEVENTF_KEYUP if up else 0))
        self.assertEqual(helper.IGNORED_KEYS, {"brightnessup", "brightnessdown"})
        host = helper.Helper()
        host.source = {"kind": "display", "width": 100, "height": 100}
        with patch.object(helper, "secure_desktop_active", return_value=False), \
             patch.object(helper, "send_inputs") as send:
            for name in ("BrightnessUp", "brightnessdown"):
                host.deliver({"type": "key", "key": name, "phase": "press"}, {})
            send.assert_not_called()
            host.deliver({"type": "key", "key": "f14", "phase": "press"}, {})
            self.assertEqual(send.call_count, 1)
        with self.assertRaises(helper.HelperError):
            helper.virtual_key("f20")

    def test_keys_map_to_virtual_keys_and_extended_flags(self):
        self.assertEqual(helper.virtual_key("a"), ord("A"))
        self.assertEqual(helper.virtual_key("Return"), 0x0D)
        self.assertEqual(helper.virtual_key("f12"), 0x7B)
        for name, expected in (("win", 0x5B), ("ctrl", 0xA2), ("alt", 0xA4)):
            with self.subTest(platform_key=name):
                vk = helper.virtual_key(name)
                self.assertEqual(vk, expected)
                item = helper.key_input(vk, up=True)
                self.assertEqual(item.ki.dwFlags, helper.KEYEVENTF_KEYUP |
                                 (helper.KEYEVENTF_EXTENDEDKEY if name == "win" else 0))
        with self.assertRaises(helper.HelperError):
            helper.virtual_key("not-a-key")
        left = helper.key_input(helper.VK["left"], up=True)
        self.assertEqual(left.ki.dwFlags, helper.KEYEVENTF_KEYUP | helper.KEYEVENTF_EXTENDEDKEY)

    def test_text_is_sent_as_utf16_units_including_surrogates(self):
        items = helper.unicode_inputs("a\U0001F600")
        self.assertEqual(len(items), 6)  # 'a' plus a surrogate pair, each down and up
        self.assertTrue(all(i.ki.dwFlags & helper.KEYEVENTF_UNICODE for i in items))
        self.assertEqual(items[2].ki.wScan, 0xD83D)

    def test_window_titles_are_bounded_and_printable(self):
        self.assertTrue(all(len(w["name"].encode()) <= 160 for w in helper.list_windows(["explorer.exe"])))


class SourceTests(unittest.TestCase):
    def test_primary_display_resolves_and_bad_ids_do_not(self):
        source = helper.Helper().resolve_source("display:1")
        self.assertEqual(source["kind"], "display")
        self.assertGreater(source["width"], 0)
        for bad in ("display:99", "app:notepad.exe#0", "app:notepad.exe#x", "window:1"):
            with self.assertRaises(helper.HelperError):
                helper.Helper().resolve_source(bad)

    def test_discovery_worker_answers_each_request_line(self):
        # The daemon's persistent --discover-sources worker: one answer per
        # request line; an empty app list asks for simulators (none here).
        # A titled Tk window of our own makes sure the list is not empty.
        session = ctypes.c_ulong()
        ctypes.windll.kernel32.ProcessIdToSessionId(os.getpid(), ctypes.byref(session))
        if session.value == 0:
            self.skipTest("session 0 (SSH/services) shows no windows; run in the signed-in desktop session")
        title = "Wonder discovery fixture"
        fixture = subprocess.Popen([sys.executable, "-c",
                                    "import tkinter, sys; r = tkinter.Tk(); r.title(sys.argv[1]);"
                                    " r.geometry('320x200'); r.after(15000, r.destroy); r.mainloop()", title])
        self.addCleanup(fixture.wait)
        self.addCleanup(fixture.kill)
        exe = Path(sys._base_executable).name.lower()
        for _ in range(50):
            if any(w["name"] == title for w in helper.list_windows([exe])):
                break
            time.sleep(0.1)
        requests = [{"apps": [exe], "browserTab": None}, {"apps": []}, {"apps": [], "allWindows": True}]
        output = subprocess.run([sys.executable, str(Path(helper.__file__)), "--discover-sources"],
                                input="".join(json.dumps(r) + "\n" for r in requests),
                                capture_output=True, text=True, timeout=30).stdout
        windows, simulators, all_windows = [json.loads(line) for line in output.splitlines()]
        self.assertTrue(windows["available"])
        self.assertIn(title, [source["name"] for source in windows["sources"]])
        for source in windows["sources"]:
            self.assertEqual(source["kind"], "window")
            app, _, number = source["id"][4:].rpartition("#")
            self.assertEqual(app, source["appId"])
            self.assertTrue(number.isdigit() and int(number) > 0)
        self.assertEqual(simulators, {"available": False, "sources": []})
        fixture = next(source for source in all_windows["sources"] if source["name"] == title)
        self.assertTrue(fixture["appName"])
        self.assertEqual(fixture["windowTitle"], title)
        self.assertLessEqual(len(fixture["iconPNG"]), 3072)
        self.assertTrue(base64.b64decode(fixture["iconPNG"]).startswith(b"\x89PNG\r\n\x1a\n"))


class FakeCapture:
    """Stands in for windows_capture.WindowsCapture; frames arrive only when
    the test calls `deliver`."""
    instances = []
    start_error = None

    def __init__(self, **kwargs):
        self.kwargs, self.handlers, self.running = kwargs, {}, False
        FakeCapture.instances.append(self)

    def event(self, handler):
        self.handlers[handler.__name__] = handler
        return handler

    def start_free_threaded(self):
        if FakeCapture.start_error:
            raise FakeCapture.start_error
        self.running = True
        return self

    def stop(self):
        self.running = False

    def deliver(self, pixels):
        self.handlers["on_frame_arrived"](type("Frame", (), {"frame_buffer": pixels}), self)


class NoScreen:
    def __init__(self):
        raise AssertionError("a window source read the screen")


class WindowFailsClosedTests(unittest.IsolatedAsyncioTestCase):
    window = {"id": "app:notepad.exe#1", "kind": "window", "width": 64, "height": 48, "hwnd": 1}

    def setUp(self):
        for name, value in (("WindowsCapture", FakeCapture), ("_MSS", NoScreen)):
            original = getattr(helper, name)
            self.addCleanup(setattr, helper, name, original)
            setattr(helper, name, value)

    def test_window_shows_only_its_own_frames_never_the_screen(self):
        grabber = helper.Grabber(dict(self.window))
        grabber.start()
        self.assertIsNone(grabber.grab())  # no frame yet: nothing, not the desktop
        pixels = numpy.full((48, 64, 4), 200, dtype=numpy.uint8)
        FakeCapture.instances[-1].deliver(pixels)
        self.assertTrue((grabber.grab() == 200).all())
        FakeCapture.instances[-1].handlers["on_closed"]()
        self.assertIsNone(grabber.grab())

    async def test_window_without_its_own_capture_is_refused(self):
        helper.WindowsCapture = None
        with self.assertRaises(helper.HelperError):
            helper.Grabber(dict(self.window))
        host = helper.Helper()
        host.session_id, host.generation = "capture", 1
        host.resolve_source = lambda _: dict(self.window)
        picked = await host.handle("capture.pick", {"sourceID": self.window["id"]})
        self.assertFalse(picked["accepted"])
        self.assertEqual(picked["errorCode"], "capture_unavailable")
        self.assertEqual((picked["status"]["state"], picked["status"]["reason"]), ("failed", "capture_unavailable"))
        self.assertIsNone(host.grabber)

    async def test_waiting_window_streams_a_blank_frame(self):
        host = helper.Helper()
        host.source, host.state = dict(self.window), "starting"
        host.grabber = helper.Grabber(host.source)
        host.capturing.set()
        frame = await helper.ScreenTrack(host).recv()
        self.assertEqual((frame.width, frame.height), (64, 48))
        self.assertFalse(frame.to_ndarray(format="bgra").any())
        self.assertEqual((host.frame_sequence, host.state), (0, "starting"))


    def test_capture_runs_only_between_start_and_stop_at_the_stream_rate(self):
        grabber = helper.Grabber(dict(self.window), fps=12)
        capture = FakeCapture.instances[-1]
        self.assertEqual(capture.kwargs["minimum_update_interval"], 83)  # WGC holds updates to the stream rate
        self.assertFalse(capture.running)  # picked, not streaming: no capture
        capture.deliver(numpy.full((48, 64, 4), 1, dtype=numpy.uint8))
        self.assertIsNone(grabber.grab())
        grabber.start()
        self.assertTrue(capture.running)
        capture.deliver(numpy.full((48, 64, 4), 2, dtype=numpy.uint8))  # the first frame after start
        self.assertTrue((grabber.grab() == 2).all())
        grabber.stop()
        self.assertFalse(capture.running)
        capture.deliver(numpy.full((48, 64, 4), 3, dtype=numpy.uint8))
        self.assertTrue((grabber.grab() == 2).all())  # a stopped capture updates nothing
        grabber.start()
        self.assertFalse(capture.running)  # stopped for good; a new pick starts another

    async def picked_host(self, events):
        host = helper.Helper()
        host.session_id, host.generation, host.quality = "capture", 1, "high"
        host.resolve_source = lambda _: dict(self.window)
        self.assertTrue((await host.handle("capture.pick", {"sourceID": self.window["id"]}))["accepted"])
        return host

    async def test_capture_runs_from_start_until_stop(self):
        events = []
        with patch.object(helper, "write", side_effect=events.append):
            host = await self.picked_host(events)
            grabber = host.grabber
            self.assertEqual(FakeCapture.instances[-1].kwargs["minimum_update_interval"], 67)
            self.assertFalse(FakeCapture.instances[-1].running)
            await host.handle("capture.start", {"sessionID": "capture", "generation": 1})
            self.assertTrue(FakeCapture.instances[-1].running)
            await host.handle("capture.stop", {"sessionID": "capture", "generation": 1})
            self.assertFalse(FakeCapture.instances[-1].running)
            FakeCapture.instances[-1].handlers["on_closed"]()
            await asyncio.sleep(0.01)
        self.assertNotIn("capture.sourceRemoved", [event.get("event") for event in events])

    async def test_a_window_that_cannot_start_capture_is_refused_at_start(self):
        with patch.object(helper, "write"):
            host = await self.picked_host([])
        with patch.object(FakeCapture, "start_error", RuntimeError("closed")):
            started = await host.handle("capture.start", {"sessionID": "capture", "generation": 1})
        self.assertFalse(started["accepted"])
        self.assertEqual((started["errorCode"], host.state), ("capture_unavailable", "ready"))
        self.assertFalse(host.capturing.is_set())

    async def test_closed_window_ends_the_view_and_releases_control(self):
        events, sent = [], []
        with patch.object(helper, "write", side_effect=events.append), \
             patch.object(helper, "send_inputs", side_effect=sent.extend):
            host = await self.picked_host(events)
            await host.handle("capture.start", {"sessionID": "capture", "generation": 1})
            host.state = "capturing"
            host.lease = {"leaseID": "lease", "sourceID": self.window["id"]}
            host.held_buttons.add("left")
            FakeCapture.instances[-1].handlers["on_closed"]()  # WGC's thread in the app
            await asyncio.sleep(0.01)
        self.assertEqual((host.state, host.reason), ("sourceRemoved", "source_not_found"))
        self.assertFalse(host.capturing.is_set())
        self.assertIsNone(host.lease)
        self.assertEqual([item.mi.dwFlags for item in sent], [helper.MOUSEEVENTF_LEFTUP])
        self.assertIn({"event": "control.revoked", "leaseID": "lease", "sourceID": self.window["id"],
                       "reason": "source_not_found"}, events)
        self.assertEqual([event["event"] for event in events if event.get("event") == "capture.sourceRemoved"],
                         ["capture.sourceRemoved"])

    async def test_a_late_close_from_a_replaced_window_keeps_the_replacement(self):
        events = []
        with patch.object(helper, "write", side_effect=events.append):
            host = await self.picked_host(events)
            replaced = FakeCapture.instances[-1]
            self.assertTrue((await host.handle("capture.pick", {"sourceID": self.window["id"]}))["accepted"])
            current = host.grabber
            host.lease = {"leaseID": "lease"}
            replaced.handlers["on_closed"]()
            await asyncio.sleep(0.01)
        self.assertIs(host.grabber, current)
        self.assertEqual(host.state, "ready")
        self.assertEqual(host.lease, {"leaseID": "lease"})
        self.assertNotIn("capture.sourceRemoved", [event.get("event") for event in events])


class ClipboardCopyTests(unittest.TestCase):
    def test_an_empty_clipboard_is_a_delivered_copy_with_a_reason(self):
        host = helper.Helper()
        host.source = {"id": "display:1", "kind": "display", "width": 100, "height": 100, "origin": (0, 0)}
        for text, expected in (("", {"clipboardReason": "clipboard_no_text"}), ("hi", {"clipboardText": "hi"})):
            result = {}
            with patch.object(helper, "clipboard_read", return_value=text):
                host.deliver({"type": "clipboard", "operation": "copyToPhone"}, result)
            self.assertEqual(result, expected)


class WindowInputTargetTests(unittest.TestCase):
    """Input for a window source reaches that window or nothing: SendInput
    itself goes to the focused window (keys) or the one under the cursor."""
    target, other = 101, 202
    window = {"id": "app:notepad.exe#101", "kind": "window", "width": 100, "height": 100,
              "hwnd": 101, "contentRect": {"x": 0, "y": 0, "width": 100, "height": 100}}

    def setUp(self):
        self.sent, self.clipboard, self.focused, self.on_top = [], [], self.other, self.other
        self.activation_works = False

        def activate(hwnd):
            if self.activation_works:
                self.focused = hwnd
        fakes = {"send_inputs": lambda items: self.sent.append(list(items)),
                 "window_exe": lambda hwnd: "notepad.exe",
                 "foreground_root": lambda: self.focused,
                 "root_window_at": lambda x, y: self.on_top,
                 "cursor_position": lambda: (10, 10),
                 "activate_window": activate,
                 "clipboard_write": self.clipboard.append,
                 "secure_desktop_active": lambda: False}
        for name, value in fakes.items():
            original = getattr(helper, name)
            self.addCleanup(setattr, helper, name, original)
            setattr(helper, name, value)
        is_window = helper.user32.IsWindow
        self.addCleanup(setattr, helper.user32, "IsWindow", is_window)
        helper.user32.IsWindow = lambda hwnd: hwnd == self.target
        self.host = helper.Helper()
        self.host.source = dict(self.window)
        self.host.source_origin = lambda: (0, 0)
        original_rect = helper.window_rect
        self.addCleanup(setattr, helper, "window_rect", original_rect)
        helper.window_rect = lambda hwnd: (0, 0, 100, 100)

    def refused(self, action, code):
        with self.assertRaises(helper.HelperError) as raised:
            self.host.deliver(action, {})
        self.assertEqual(raised.exception.code, code)

    def test_altgr_releases_only_its_paired_control(self):
        self.activation_works = True
        for held_control, mask in ((None, 0), (None, 2), (None, 8),
                                   (0xA2, 0), (0xA2, 2), (0xA2, 8), (0xA3, 0)):
            with self.subTest(held_control=held_control, mask=mask):
                self.host.held_keys = {held_control} if held_control else set()
                self.sent.clear()
                for phase in ("down", "down", "up"):
                    self.host.deliver({"type": "key", "key": "rightoption", "phase": phase,
                                       "modifiers": mask}, {})
                encoded = [(item.ki.wVk, bool(item.ki.dwFlags & helper.KEYEVENTF_KEYUP))
                           for batch in self.sent for item in batch]
                self.assertEqual(encoded, ([(0xA2, False)] if not held_control else []) +
                                 [(0xA5, False), (0xA5, False), (0xA5, True)] +
                                 ([(0xA2, True)] if not held_control else []))
                self.assertEqual(self.host.held_keys, {held_control} if held_control else set())
        self.host.held_keys.clear()
        self.sent.clear()
        self.host.deliver({"type": "key", "key": "rightoption", "phase": "press"}, {})
        self.assertEqual([(i.ki.wVk, bool(i.ki.dwFlags & helper.KEYEVENTF_KEYUP))
                          for batch in self.sent for i in batch],
                         [(0xA2, False), (0xA5, False), (0xA5, True), (0xA2, True)])
        self.assertFalse(self.host.held_keys)
        for key in ("rightoption", "control"):
            self.host.deliver({"type": "key", "key": key, "phase": "down"}, {})
        self.sent.clear()
        self.host.deliver({"type": "key", "key": "rightoption", "phase": "up"}, {})
        self.assertEqual([i.ki.wVk for batch in self.sent for i in batch], [0xA5])
        self.assertEqual(self.host.held_keys, {0xA2})

    def test_text_keys_and_paste_never_reach_another_focused_window(self):
        self.refused({"type": "text", "text": "secret"}, "window_not_focused")
        self.refused({"type": "key", "key": "a", "phase": "press"}, "window_not_focused")
        self.refused({"type": "clipboard", "operation": "pasteFromPhone", "text": "secret"},
                     "window_not_focused")
        self.assertEqual((self.sent, self.clipboard), ([], []))

    def test_paste_is_refused_when_focus_moves_during_clipboard_preparation(self):
        self.activation_works = True

        def write_while_user_switches(text):
            self.clipboard.append(text)
            self.focused = self.other
        helper.clipboard_write = write_while_user_switches
        host = self.host
        host.session_id, host.generation, host.state = "capture", 2, "capturing"
        identity = {"leaseID": "lease", "requestID": "request", "sessionID": "capture",
                    "generation": 2, "geometryRevision": 1, "sourceID": self.window["id"]}

        async def paste():
            host.consents[identity["requestID"]] = identity  # as if allowed on the computer
            self.assertTrue((await host.handle("control.activate", identity))["activated"])
            with self.assertRaises(helper.HelperError) as raised:
                await host.handle("control.input", dict(identity, sequence=1, actions=[
                    {"type": "clipboard", "operation": "pasteFromPhone", "text": "secret"}]))
            self.assertEqual(raised.exception.code, "window_not_focused")
        asyncio.run(paste())
        self.assertEqual([items for items in self.sent if items], [])
        self.assertEqual(self.focused, self.other)
        self.assertIsNone(host.lease)

    def test_paste_reaches_the_window_when_focus_holds(self):
        self.activation_works = True
        self.host.deliver({"type": "clipboard", "operation": "pasteFromPhone", "text": "hi"}, {})
        self.assertEqual((self.clipboard, len(self.sent)), (["hi"], 1))

    def test_text_is_typed_once_the_window_has_focus(self):
        self.activation_works = True
        self.host.deliver({"type": "text", "text": "hi"}, {})
        self.assertEqual(self.focused, self.target)
        self.assertEqual(len(self.sent), 1)

    def test_clicks_and_wheel_never_reach_a_covering_window(self):
        self.activation_works = True
        self.refused({"type": "pointer", "x": .5, "y": .5, "phase": "down"}, "window_covered")
        self.refused({"type": "scroll", "deltaX": 0, "deltaY": 40}, "window_covered")
        self.assertEqual(self.sent, [])
        self.on_top = self.target
        self.host.deliver({"type": "pointer", "x": .5, "y": .5, "phase": "down"}, {})
        self.assertEqual(self.focused, self.target)
        self.assertEqual(len(self.sent), 1)

    def test_drag_up_moves_to_the_final_point_or_releases_without_moving_into_a_cover(self):
        self.activation_works = True
        self.focused = self.target
        self.host.held_buttons = {"left"}
        self.on_top = self.target
        with patch.object(helper, "virtual_screen", return_value=(0, 0, 100, 100)):
            self.host.deliver({"type": "pointer", "phase": "up", "x": 1, "y": 1}, {})
        self.assertEqual((self.sent[0][0].mi.dx, self.sent[0][0].mi.dy), (65535, 65535))
        self.assertEqual(len(self.sent), 2)
        self.assertEqual(self.sent[0][0].mi.dwFlags & helper.MOUSEEVENTF_MOVE, helper.MOUSEEVENTF_MOVE)
        self.assertEqual(self.sent[1][0].mi.dwFlags, helper.MOUSEEVENTF_LEFTUP)
        self.sent.clear()
        self.host.held_buttons = {"left"}
        self.on_top = self.other
        self.refused({"type": "pointer", "phase": "up", "x": 1, "y": 1}, "window_covered")
        self.assertEqual([[item.mi.dwFlags for item in batch] for batch in self.sent], [[helper.MOUSEEVENTF_LEFTUP]])
        self.assertEqual(self.host.held_buttons, set())

    def test_pointer_waits_for_the_raised_window_to_receive_the_point(self):
        self.activation_works = True
        with patch.object(helper, "root_window_at", side_effect=[self.other, self.other, self.target]), patch.object(helper.time, "sleep"):
            self.host.deliver({"type": "pointer", "x": .5, "y": .5, "phase": "down"}, {})
        self.assertEqual(self.focused, self.target)
        self.assertEqual(len(self.sent), 1)

    def test_held_keys_and_buttons_are_always_released(self):
        self.host.held_keys.add(ord("A"))
        self.host.held_buttons.add("left")
        self.host.deliver({"type": "key", "key": "a", "phase": "up"}, {})
        with self.assertRaises(helper.HelperError):
            self.host.deliver({"type": "pointer", "x": .5, "y": .5, "phase": "up"}, {})
        self.assertEqual(len(self.sent), 2)
        self.assertEqual(self.sent[-1][0].mi.dwFlags, helper.MOUSEEVENTF_LEFTUP)

    def test_windows_platform_chords_deliver_and_release_the_held_modifier(self):
        self.activation_works = True
        for modifier, key, expected in (("win", "tab", 0x5B), ("alt", "tab", 0xA4),
                                         ("ctrl", "a", 0xA2)):
            with self.subTest(modifier=modifier):
                self.sent.clear()
                self.host.deliver({"type": "key", "key": modifier, "phase": "down"}, {})
                self.assertIn(expected, self.host.held_keys)
                self.host.deliver({"type": "key", "key": key, "phase": "press"}, {})
                self.host.deliver({"type": "key", "key": modifier, "phase": "up"}, {})
                self.assertEqual([item.ki.wVk for batch in self.sent for item in batch],
                                 [expected, ord("A") if key == "a" else 0x09,
                                  ord("A") if key == "a" else 0x09, expected])
                self.assertEqual(self.host.held_keys, set())

    def test_a_closed_or_reused_window_takes_no_keys(self):
        self.host.source["hwnd"] = 999
        self.refused({"type": "text", "text": "x"}, "source_not_found")
        self.host.source["hwnd"] = self.target
        with patch.object(helper, "window_exe", return_value="another.exe"):
            self.refused({"type": "text", "text": "x"}, "source_not_found")
        self.assertEqual(self.sent, [])


class QualityTests(unittest.IsolatedAsyncioTestCase):
    async def test_video_scales_without_reporting_a_physical_resize_or_releasing_control(self):
        host = helper.Helper()
        host.source = {"id": "display:1", "kind": "display", "width": 3840, "height": 2160}
        host.state = "capturing"
        host.lease = {"sourceID": "display:1", "geometryRevision": 1}
        host.capturing.set()
        class Grabber:
            def grab(self):
                return numpy.zeros((2160, 3840, 4), dtype=numpy.uint8)
        host.grabber = Grabber()
        track = helper.ScreenTrack(host)
        with patch.object(helper, "write"):
            for quality, expected in (("high", (2560, 1440)), ("standard", (1280, 720)), ("medium", (1920, 1080))):
                host.quality = quality
                frame = await asyncio.wait_for(track.recv(), 10)
                self.assertEqual((frame.width, frame.height), expected)
                self.assertEqual((host.last_frame["sourceWidth"], host.last_frame["sourceHeight"]), (3840, 2160))
                self.assertEqual(host.geometry_revision, 1)
                self.assertIsNotNone(host.lease)
        track.stop()


class DesktopInputTargetTests(unittest.TestCase):
    """On the real desktop: with another window focused, text for the
    streamed window lands in it (after activation) or is refused; the
    focused window never receives it."""

    def test_text_for_a_background_window_never_reaches_the_focused_one(self):
        session = ctypes.c_ulong()
        ctypes.windll.kernel32.ProcessIdToSessionId(os.getpid(), ctypes.byref(session))
        if session.value == 0:
            self.skipTest("session 0 (SSH/services) has no input desktop; run in the signed-in desktop session")
        script = ("import tkinter, sys\n"
                  "r = tkinter.Tk(); r.title(sys.argv[1]); r.geometry(sys.argv[2])\n"
                  "e = tkinter.Entry(r); e.pack(fill='both', expand=True); e.focus_set()\n"
                  "def report():\n"
                  "    print(e.get(), flush=True); r.after(200, report)\n"
                  "r.after(200, report); r.after(20000, r.destroy); r.mainloop()\n")
        windows = {}
        for name, geometry in (("Wonder input target", "300x120+100+100"),
                               ("Wonder input bystander", "300x120+120+120")):
            process = subprocess.Popen([sys.executable, "-c", script, name, geometry],
                                       stdout=subprocess.PIPE, text=True)
            self.addCleanup(process.stdout.close)
            self.addCleanup(process.wait)
            self.addCleanup(process.kill)
            windows[name] = process
        exe = Path(sys._base_executable).name.lower()
        hwnds = {}
        for _ in range(50):
            hwnds = {w["name"]: int(w["id"].rpartition("#")[2]) for w in helper.list_windows([exe])}
            if all(name in hwnds for name in windows):
                break
            time.sleep(0.1)
        target, bystander = hwnds["Wonder input target"], hwnds["Wonder input bystander"]
        helper.activate_window(bystander)
        time.sleep(0.3)
        self.assertEqual(helper.foreground_root(), bystander, "could not focus the bystander")
        host = helper.Helper()
        host.source = host.resolve_source(f"app:{exe}#{target}")
        try:
            host.deliver({"type": "pointer", "x": .5, "y": .5, "phase": "move"}, {})
            host.deliver({"type": "pointer", "x": .5, "y": .5, "phase": "down"}, {})
            host.deliver({"type": "pointer", "x": .5, "y": .5, "phase": "up"}, {})
            host.deliver({"type": "text", "text": "wonder-typed"}, {})
            delivered = True
        except helper.HelperError as error:
            self.assertIn(error.code, ("window_not_focused", "window_covered"))
            hit = helper.root_window_at(host.source_origin()[0] + round((helper.window_rect(target)[2] - 1) / 2), host.source_origin()[1] + round((helper.window_rect(target)[3] - 1) / 2))
            print(json.dumps({"refused": error.code, "target": target, "foreground": helper.foreground_root(),
                "coverTitle": helper.window_title(hit), "coverClass": helper.window_class(hit), "coverRect": helper.window_rect(hit),
                "coverTopmost": bool(helper.user32.GetWindowLongW(hit, -20) & 0x8),
                "rect": helper.window_rect(target), "centerHit": helper.root_window_at(
                    host.source_origin()[0] + round((helper.window_rect(target)[2] - 1) / 2),
                    host.source_origin()[1] + round((helper.window_rect(target)[3] - 1) / 2))}), file=sys.stderr)
            delivered = False
        time.sleep(0.6)
        for process in windows.values():
            process.kill()
        texts = {name: process.stdout.read().splitlines()[-1:] for name, process in windows.items()}
        self.assertNotIn("wonder-typed", texts["Wonder input bystander"])
        if delivered:
            self.assertEqual(texts["Wonder input target"], ["wonder-typed"])
        print(f"\ndesktop input target: delivered={delivered}", file=sys.stderr)


class LeaseTests(unittest.IsolatedAsyncioTestCase):
    def setUp(self):
        self.host = helper.Helper()
        self.host.session_id = "capture"
        self.host.generation = 2
        self.host.source = {"id": "display:1", "kind": "display", "width": 1280, "height": 800,
                            "origin": (0, 0)}
        self.host.state = "capturing"
        self.identity = {"leaseID": "lease", "requestID": "request", "sessionID": "capture",
                         "generation": 2, "geometryRevision": 1, "sourceID": "display:1"}
        self.delivered = []
        self.host.deliver = lambda action, result: self.delivered.append(action)

    async def activate(self):
        # This helper refuses consent; the lease rules below assume an approval.
        self.host.consents[self.identity["requestID"]] = dict(self.identity)
        self.assertTrue((await self.host.handle("control.activate", self.identity))["activated"])

    async def test_take_control_is_refused_without_local_approval(self):
        with patch.object(helper, "virtual_screen", return_value=(0, 0, 800, 600)), \
             patch.object(helper, "secure_desktop_active", return_value=False):
            capabilities = await self.host.handle("capabilities", {})
        self.assertEqual(capabilities["control"]["available"], False)
        self.assertEqual(capabilities["control"]["reason"], "local_approval_unavailable")
        answer = await self.host.handle("control.consent", self.identity)
        self.assertEqual((answer["granted"], answer["reason"]), (False, "local_approval_unavailable"))
        self.assertEqual(self.host.consents, {})
        self.assertEqual((await self.host.handle("control.activate", self.identity))["reason"],
                         "consent_required")
        self.assertIsNone(self.host.lease)
        with self.assertRaises(helper.HelperError):
            await self.host.handle("control.consent", dict(self.identity, generation=3))

    async def test_rebind_closed_window_refuses_without_moving_view_or_lease(self):
        await self.activate()
        source = self.host.source
        lease = dict(self.host.lease)
        with patch.object(self.host, "resolve_source", side_effect=helper.HelperError("source_not_found")):
            changed = await self.host.handle("capture.rebind", dict(self.identity, nextSourceID="app:editor#42"))
        self.assertFalse(changed["accepted"])
        self.assertEqual(changed["status"]["state"], "capturing")
        self.assertEqual(changed["status"]["sourceID"], source["id"])
        self.assertEqual(changed["status"]["geometryRevision"], 1)
        self.assertEqual(self.host.source, source)
        self.assertEqual(self.host.lease, lease)
        self.assertTrue((await self.host.handle("control.heartbeat", self.identity))["renewed"])

    async def test_view_rebind_retains_consent_sequence_receipts_and_releases_held_input(self):
        await self.activate()
        actions = [{"type": "text", "text": "before"}]
        self.assertTrue((await self.host.handle("control.input", dict(self.identity, sequence=1, actions=actions)))["accepted"])
        self.host.held_buttons.add("left")
        self.host.held_keys.add("shift")
        source = {"id": "app:editor#42", "kind": "window", "width": 800, "height": 600,
                  "windowTitle": "Second window", "title": "Second window", "scale": 1.0,
                  "contentRect": {"x": 0, "y": 0, "width": 800, "height": 600}}
        from unittest.mock import Mock
        release = Mock(side_effect=lambda: (self.host.held_buttons.clear(), self.host.held_keys.clear()))
        self.host.release_held = release
        with patch.object(self.host, "resolve_source", return_value=source), patch.object(helper, "Grabber"):
            changed = await self.host.handle("capture.rebind", dict(self.identity, nextSourceID=source["id"]))
        self.assertTrue(changed["accepted"])
        release.assert_called_once()
        self.assertFalse(self.host.held_buttons)
        self.assertFalse(self.host.held_keys)
        selected = dict(self.identity, sourceID=source["id"], geometryRevision=2)
        self.assertEqual(self.host.lease, selected)
        self.assertEqual(self.host.last_input_sequence, 1)
        with self.assertRaises(helper.HelperError):
            await self.host.handle("control.input", dict(self.identity, sequence=2, actions=actions))
        self.assertTrue((await self.host.handle("control.input", dict(selected, sequence=1, actions=actions)))["duplicate"])
        self.assertFalse((await self.host.handle("control.input", dict(selected, sequence=1, actions=[{"type": "text", "text": "conflict"}])))["accepted"])
        self.assertTrue((await self.host.handle("control.input", dict(selected, sequence=2, actions=actions)))["accepted"])
        self.assertEqual(self.delivered, actions + actions)
        self.assertTrue((await self.host.handle("control.heartbeat", selected))["renewed"])
        self.assertTrue((await self.host.handle("control.activate", selected))["replayed"])

    async def test_activation_requires_matching_consent(self):
        self.assertFalse((await self.host.handle("control.activate", self.identity))["activated"])
        self.host.consents[self.identity["requestID"]] = dict(self.identity)
        with self.assertRaises(helper.HelperError):
            await self.host.handle("control.activate", dict(self.identity, generation=3))

    async def test_queued_input_cannot_cross_release_and_reactivation(self):
        await self.activate()
        await self.host.input_lock.acquire()
        request = asyncio.create_task(self.host.handle("control.input", dict(self.identity, sequence=1,
                                      actions=[{"type": "text", "text": "stale"}])))
        await asyncio.sleep(0)
        with patch.object(helper, "write"), patch.object(helper, "send_inputs"):
            await self.host.handle("control.release", self.identity)
        await self.activate()
        self.host.input_lock.release()
        with self.assertRaises(helper.HelperError) as failed:
            await request
        self.assertEqual(failed.exception.code, "lease_not_active")
        self.assertEqual(self.delivered, [])

    async def test_input_sequence_and_replay(self):
        await self.activate()
        actions = [{"type": "pointer", "x": .5, "y": .5, "phase": "move"}]
        first = await self.host.handle("control.input", dict(self.identity, sequence=1, actions=actions))
        self.assertTrue(first["accepted"])
        replay = await self.host.handle("control.input", dict(self.identity, sequence=1, actions=actions))
        self.assertTrue(replay["duplicate"])
        skipped = await self.host.handle("control.input", dict(self.identity, sequence=3, actions=actions))
        self.assertFalse(skipped["accepted"])
        self.assertEqual(len(self.delivered), 1)

    async def test_resize_revokes_control_and_releases_a_held_button_first(self):
        await self.activate()
        self.host.held_buttons.add("left")
        events, sent = [], []
        with patch.object(helper, "write", side_effect=events.append), \
             patch.object(helper, "send_inputs", side_effect=sent.extend):
            self.host.frame_captured(1280, 800)  # same size: control stays
            self.assertIsNotNone(self.host.lease)
            events.clear()
            self.host.frame_captured(1024, 768)
        self.assertIsNone(self.host.lease)
        self.assertFalse(self.host.held_buttons)
        self.assertEqual([item.mi.dwFlags for item in sent], [helper.MOUSEEVENTF_LEFTUP])
        self.assertEqual(events[0], {"event": "control.revoked", **self.identity, "reason": "source_geometry_changed"})
        self.assertEqual(events[1]["event"], "capture.frameMetadata")
        self.assertEqual(events[1]["status"]["geometryRevision"], 2)
        with self.assertRaises(helper.HelperError) as refused:
            await self.host.handle("control.input", dict(self.identity, sequence=1, actions=[{"type": "releaseAll"}]))
        self.assertEqual(refused.exception.code, "invalid_capture_session")

    async def test_input_timeout_stops_the_batch_and_releases_control(self):
        await self.activate()
        def slow(action, result):
            self.delivered.append(action)
            self.host.held_buttons.add("left")
            time.sleep(.03)
        self.host.deliver = slow
        events, sent = [], []
        actions = [{"type": "pointer", "x": .5, "y": .5, "phase": "down"}, {"type": "text", "text": "late"}]
        with patch.object(helper, "INPUT_TIMEOUT", .01), \
             patch.object(helper, "write", side_effect=events.append), \
             patch.object(helper, "send_inputs", side_effect=sent.extend):
            with self.assertRaises(helper.HelperError) as timed_out:
                await self.host.handle("control.input", dict(self.identity, sequence=1, actions=actions))
        self.assertEqual(timed_out.exception.code, "input_timed_out")
        self.assertEqual(self.delivered, actions[:1])
        self.assertIsNone(self.host.lease)
        self.assertEqual([item.mi.dwFlags for item in sent], [helper.MOUSEEVENTF_LEFTUP])
        self.assertEqual(events, [{"event": "control.revoked", **self.identity, "reason": "input_timed_out"}])

    async def test_a_last_action_past_the_deadline_is_a_timeout(self):
        await self.activate()
        def slow(action, result):
            self.delivered.append(action)
            time.sleep(.03)
        self.host.deliver = slow
        events = []
        with patch.object(helper, "INPUT_TIMEOUT", .01), patch.object(helper, "write", side_effect=events.append):
            with self.assertRaises(helper.HelperError) as timed_out:
                await self.host.handle("control.input", dict(self.identity, sequence=1, actions=[{"type": "releaseAll"}]))
        self.assertEqual(timed_out.exception.code, "input_timed_out")
        self.assertEqual(self.host.last_input_sequence, 0)  # no receipt for an uncertain batch
        self.assertEqual(events, [{"event": "control.revoked", **self.identity, "reason": "input_timed_out"}])

    async def test_request_timeout_answers_it_and_releases_control(self):
        await self.activate()
        self.host.held_keys.add(0xA2)
        async def stuck(method, params):
            await asyncio.sleep(1)
        self.host.handle = stuck
        events, sent = [], []
        with patch.object(helper, "REQUEST_TIMEOUT", .01), \
             patch.object(helper, "write", side_effect=events.append), \
             patch.object(helper, "send_inputs", side_effect=sent.extend):
            reply = await helper.respond(self.host, 7, "signal.answer", {})
        self.assertEqual(reply, {"id": 7, "error": {"code": "request_timed_out"}})
        self.assertIsNone(self.host.lease)
        self.assertEqual([(item.ki.wVk, item.ki.dwFlags & helper.KEYEVENTF_KEYUP) for item in sent], [(0xA2, helper.KEYEVENTF_KEYUP)])
        self.assertEqual(events, [{"event": "control.revoked", **self.identity, "reason": "request_timed_out"}])

    async def test_release_revokes(self):
        await self.activate()
        released = await self.host.handle("control.release", self.identity)
        self.assertTrue(released["released"])
        refused = await self.host.handle("control.input", dict(self.identity, sequence=1,
                                                               actions=[{"type": "releaseAll"}]))
        self.assertFalse(refused["accepted"])


if __name__ == "__main__":
    unittest.main()

"""Offline helper boundary tests; no desktop input is delivered by these cases."""
import asyncio
import importlib.util
from pathlib import Path
import unittest
from unittest.mock import patch
import base64
import json
import ctypes
import io
import os
import tempfile

spec = importlib.util.spec_from_file_location("helper", Path(__file__).with_name("wonder_computer_linux.py"))
helper = importlib.util.module_from_spec(spec)
spec.loader.exec_module(helper)


class WindowTests(unittest.IsolatedAsyncioTestCase):
    def test_window_pointer_maps_the_current_offset_frame_and_rejects_bad_coordinates(self):
        self.assertEqual(helper.map_window_point(.25, .5, -800, 120, 641, 481), (-640, 360))
        self.assertEqual(helper.map_window_point(1, 1, 500, 200, 100, 50), (599, 249))
        for point in [(float("nan"), .5), (1.01, .5), (.5, -.1)]:
            with self.assertRaises(helper.HelperError):
                helper.map_window_point(*point, 0, 0, 800, 600)

    async def test_window_pointer_raises_and_maps_before_posting_and_refuses_a_cover(self):
        host = helper.Helper()
        host.source = {"id": "app:editor#41", "kind": "window", "width": 800, "height": 600}
        commands = []
        async def command(*args): commands.append(args)
        with patch.object(helper, "xdotool", side_effect=command), patch.object(helper, "window_input_frame", return_value=(420, 310)) as frame:
            await host.deliver({"type": "pointer", "x": .5, "y": .5, "phase": "down"}, {})
            frame.assert_called_once_with(host.source, (.5, .5), False, True)
        self.assertEqual(commands, [("mousemove", "420", "310"), ("mousedown", "1")])
        commands.clear()
        with patch.object(helper, "xdotool", side_effect=command), patch.object(helper, "window_input_frame", side_effect=helper.HelperError("window_covered")):
            with self.assertRaises(helper.HelperError):
                await host.deliver({"type": "pointer", "x": .5, "y": .5, "phase": "move"}, {})
        self.assertEqual(commands, [])
        host.held_keys.add("Shift_L")
        with patch.object(helper, "xdotool", side_effect=command), patch.object(helper, "window_input_frame", return_value=(500, 400)):
            await host.deliver({"type": "pointer", "x": .5, "y": .5, "phase": "up"}, {})
        self.assertEqual(commands, [("mousemove", "500", "400"), ("mouseup", "1")])
        self.assertEqual(host.held_keys, {"Shift_L"})
        self.assertEqual(host.held_buttons, set())

    def test_discovery_bounds_the_complete_json_line_with_escaped_titles_and_icons(self):
        sources = [{"id": f"app:editor#{index}", "appId": "e" * 140,
                    "appName": '"' * 160, "name": '"' * 160,
                    "windowTitle": '"' * 160, "iconPNG": "x" * 3072} for index in range(1, 33)]
        answer = helper.discovery_answer(True, sources)
        self.assertTrue(answer["sources"])
        self.assertEqual(answer["sources"], sources[:len(answer["sources"])])
        self.assertLessEqual(len(json.dumps(answer, ensure_ascii=False, separators=(",", ":")).encode()), helper.MAX_LINE_BYTES - 512)
        self.assertEqual(helper.discovery_answer(False, sources), {"available": False, "sources": []})

    def test_window_metadata_requires_a_visible_normal_non_minimized_client(self):
        class XServer:
            mapped, override, width = 2, 0, 800

            def XGetWindowAttributes(self, _display, _window, pointer):
                attrs = ctypes.cast(pointer, ctypes.POINTER(helper.XWindowAttributes)).contents
                attrs.map_state, attrs.override_redirect = self.mapped, self.override
                attrs.width, attrs.height = self.width, 600
                return 1

            def XInternAtom(self, _display, name, _exists):
                return name

        server = XServer()
        properties = {"WM_CLASS": b"editor\0org.gnome.TextEditor\0", "_NET_WM_NAME": b"Notes"}
        with patch.object(helper, "x11_property", side_effect=lambda _x, _d, _w, key: properties.get(key)):
            source = helper.x11_window(server, None, 41)
            self.assertEqual(source["id"], "app:org.gnome.TextEditor#41")
            self.assertEqual(source["appName"], "TextEditor")
            self.assertEqual(source["windowTitle"], "Notes")
            for mapped in (0, 1):
                server.mapped = mapped
                self.assertIsNone(helper.x11_window(server, None, 41))
            server.mapped = 2
            for state in ([3],):
                properties["WM_STATE"] = state
                self.assertIsNone(helper.x11_window(server, None, 41))
            properties.pop("WM_STATE")
            properties["_NET_WM_STATE"] = [b"_NET_WM_STATE_HIDDEN"]
            self.assertIsNone(helper.x11_window(server, None, 41))
            properties.pop("_NET_WM_STATE")
            server.override = 1
            self.assertIsNone(helper.x11_window(server, None, 41))
            server.override = 0
            for width in (0, 79):
                server.width = width
                self.assertIsNone(helper.x11_window(server, None, 41))
            server.width = 800
            for kind in ("DESKTOP", "DOCK", "MENU", "DROPDOWN_MENU", "POPUP_MENU", "TOOLTIP", "NOTIFICATION", "COMBO", "DND", "SPLASH", "UTILITY"):
                with self.subTest(kind=kind):
                    properties["_NET_WM_WINDOW_TYPE"] = [("_NET_WM_WINDOW_TYPE_" + kind).encode()]
                    self.assertIsNone(helper.x11_window(server, None, 41))
            properties["_NET_WM_WINDOW_TYPE"] = [b"_NET_WM_WINDOW_TYPE_NORMAL"]
            self.assertIsNotNone(helper.x11_window(server, None, 41))
            properties["_NET_WM_WINDOW_OPACITY"] = [0]
            self.assertIsNone(helper.x11_window(server, None, 41))
            properties["_NET_WM_WINDOW_OPACITY"] = [0xFFFFFFFF]
            self.assertIsNotNone(helper.x11_window(server, None, 41))
            properties["_NET_WM_NAME"] = b""
            self.assertIsNone(helper.x11_window(server, None, 41))

    def test_window_capture_reuses_one_redirect_and_releases_it_with_the_owner(self):
        closed, created = [], []
        class Grabber:
            def __init__(self, source):
                self.source_id = source["id"]
                created.append(self.source_id)
            def grab(self): return self.source_id
            def close(self): closed.append(self.source_id)
        helper.release_window_capture()
        with patch.object(helper, "WindowGrabber", Grabber), patch.object(helper, "grab_screen", side_effect=AssertionError("window capture read the screen")):
            source = {"id": "app:editor#41"}
            self.assertEqual(helper.grab_window(source), "app:editor#41")
            self.assertEqual(helper.grab_window(source), "app:editor#41")
            self.assertEqual(created, ["app:editor#41"])
            self.assertEqual(helper.grab_window({"id": "app:editor#42"}), "app:editor#42")
            self.assertEqual(closed, ["app:editor#41"])
            helper.release_window_capture()
            self.assertEqual(closed, ["app:editor#41", "app:editor#42"])

    def test_icons_are_png_bounded_and_reject_truncated_dimensions(self):
        icon = helper.x11_icon([2, 2, 0xFFFF0000, 0xFF00FF00, 0xFF0000FF, 0xFFFFFFFF])
        self.assertLessEqual(len(icon), 3072)
        self.assertTrue(base64.b64decode(icon).startswith(b"\x89PNG\r\n\x1a\n"))
        for invalid in ([], [256, 256, 1], [0, 2], [2, 2, 0]):
            self.assertIsNone(helper.x11_icon(invalid))

    def test_format_32_properties_drop_xlib_sign_extension_so_opaque_icons_survive(self):
        values = (ctypes.c_long * 4)(2, 1, -0x00CCBBBC, -1)

        class FakeX:
            def XInternAtom(self, *_):
                return 1

            def XGetWindowProperty(self, _d, _w, _a, _o, _l, _del, _t, kind, size, count, remaining, pointer):
                size._obj.value, count._obj.value = 32, len(values)
                pointer._obj.value = ctypes.addressof(values)
                return 0

            def XFree(self, _):
                pass

        icon = helper.x11_property(FakeX(), None, 1, "_NET_WM_ICON")
        self.assertEqual(icon, [2, 1, 0xFF334444, 0xFFFFFFFF])
        self.assertIsNotNone(helper.x11_icon(icon))

    async def test_no_display_reports_no_capture_using_the_same_capability(self):
        with patch.object(helper, "display_size", side_effect=RuntimeError("no display")):
            self.assertFalse(helper.capture_capability()["available"])
            response = await helper.Helper().handle("capabilities", {})
            self.assertFalse(response["capture"]["available"])
            self.assertEqual(response["capture"]["reason"], "no_display")

    def test_jsonl_distinguishes_no_display_from_a_transient_inventory_failure(self):
        replies = []
        request = '{"apps":[],"allWindows":true}\n'
        with patch.object(helper.sys, "stdin", io.StringIO(request)), patch.object(helper, "write", side_effect=replies.append), \
             patch.object(helper, "display_size", side_effect=RuntimeError("no display")):
            helper.discover_sources()
        self.assertEqual(replies, [{"available": False, "sources": [], "capture": {"available": False, "reason": "no_display"}}])
        replies.clear()
        with patch.object(helper.sys, "stdin", io.StringIO(request)), patch.object(helper, "write", side_effect=replies.append), \
             patch.object(helper, "capture_capability", return_value={"available": True}), \
             patch.object(helper, "list_windows", side_effect=RuntimeError("X11 disconnected")):
            helper.discover_sources()
        self.assertEqual(replies, [{"error": "source_discovery_failed"}], "A failed poll must not authoritatively hide the computer")

    async def test_window_pick_pins_identity_and_never_substitutes_another_source(self):
        source = {"id": "app:editor#41", "kind": "window", "appId": "editor", "windowTitle": "Notes", "width": 800, "height": 600}
        host = helper.Helper()
        host.session_id, host.generation = "fixture", 1
        with patch.object(helper, "list_windows", return_value=[source]), patch.object(helper, "write"), patch.object(helper, "display_size", side_effect=AssertionError("window picker read display")):
            result = await host.handle("capture.pick", {"sourceID": "app:editor#41"})
            self.assertTrue(result["accepted"])
            self.assertEqual(host.source["id"], "app:editor#41")
            for missing in ("app:editor#42", "app:other#41"):
                refused = await host.handle("capture.pick", {"sourceID": missing})
                self.assertFalse(refused["accepted"])
                self.assertEqual(host.source["id"], "app:editor#41")
                # A gone window ends the capture; the host fails the view
                # instead of leaving it preparing.
                self.assertEqual((refused["status"]["state"], refused["status"]["reason"]), ("failed", "source_not_found"))


class DisplayTests(unittest.TestCase):
    def test_a_missing_display_names_wayland_and_otherwise_x11(self):
        code = helper.missing_display_code
        self.assertEqual(code({"WAYLAND_DISPLAY": "wayland-0"}), "wayland_unsupported")
        self.assertEqual(code({"XDG_SESSION_TYPE": "Wayland"}), "wayland_unsupported")
        self.assertEqual(code({}), "x11_display_required")
        self.assertIsNone(code({"DISPLAY": ":0", "WAYLAND_DISPLAY": "wayland-0"}))

    def test_capabilities_name_wayland_when_there_is_no_x11_display(self):
        with patch.dict(os.environ, {"WAYLAND_DISPLAY": "wayland-0"}), patch.object(helper, "x11_libraries", side_effect=OSError):
            os.environ.pop("DISPLAY", None)
            self.assertEqual(helper.capture_capability(), {"available": False, "reason": "wayland_unsupported"})


class KeyTests(unittest.IsolatedAsyncioTestCase):
    def test_platform_key_names_resolve_to_x11_keys(self):
        for name, expected in [("super", "Super_L"), ("win", "Super_L"),
                               ("alt", "Alt_L"), ("ctrl", "Control_L")]:
            self.assertEqual(helper.keysym(name), expected)
        self.assertEqual(helper.keysym("command"), "Control_L")

    def test_canonical_key_names(self):
        for name, expected in [
            ("insert", "Insert"), ("printscreen", "Print"), ("menu", "Menu"),
            ("f13", "F13"), ("f19", "F19"), ("volumeup", "XF86AudioRaiseVolume"),
            ("volumedown", "XF86AudioLowerVolume"), ("mute", "XF86AudioMute"),
            ("playpause", "XF86AudioPlay"), ("next", "XF86AudioNext"),
            ("previous", "XF86AudioPrev"), ("rightoption", "ISO_Level3_Shift")]:
            self.assertEqual(helper.keysym(name), expected)
        self.assertIn("brightnessup", helper.IGNORED_KEYS)

    async def test_alt_tab_keeps_alt_held_through_cycles_and_releases_once(self):
        commands = []
        original = helper.xdotool
        async def capture(*args):
            commands.append(args)
        helper.xdotool = capture
        self.addCleanup(setattr, helper, "xdotool", original)
        host = helper.Helper()
        for key, phase, modifiers in [("alt", "down", 4), ("tab", "press", 4),
                                       ("tab", "press", 4), ("alt", "up", 0)]:
            await host.deliver({"type": "key", "key": key, "phase": phase, "modifiers": modifiers}, {})
            if phase != "up":
                self.assertEqual(host.held_keys, {"Alt_L"})
        self.assertEqual(commands, [("keydown", "Alt_L"), ("key", "Tab"),
                                    ("key", "Tab"), ("keyup", "Alt_L")])
        self.assertEqual(host.held_keys, set())

    async def test_super_chord_and_right_click_use_native_input(self):
        commands = []
        original = helper.xdotool
        async def capture(*args):
            commands.append(args)
        helper.xdotool = capture
        self.addCleanup(setattr, helper, "xdotool", original)
        host = helper.Helper()
        host.source = {"width": 1280, "height": 800}
        for key, phase in [("super", "down"), ("d", "press"), ("super", "up")]:
            await host.deliver({"type": "key", "key": key, "phase": phase}, {})
        for phase in ["down", "up"]:
            await host.deliver({"type": "pointer", "x": 0.5, "y": 0.5,
                                "phase": phase, "button": "right"}, {})
        self.assertEqual(commands[:3], [("keydown", "Super_L"), ("key", "d"), ("keyup", "Super_L")])
        self.assertIn(("mousedown", "3"), commands)
        self.assertIn(("mouseup", "3"), commands)

    async def test_release_all_releases_the_actual_held_key(self):
        commands = []
        original = helper.xdotool
        async def capture(*args):
            commands.append(args)
        helper.xdotool = capture
        self.addCleanup(setattr, helper, "xdotool", original)
        host = helper.Helper()
        await host.deliver({"type": "key", "key": "ctrl", "phase": "down"}, {})
        await host.release_held()
        self.assertEqual(commands, [("keydown", "Control_L"), ("keyup", "Control_L")])


    async def test_every_held_button_is_released_even_if_one_release_fails(self):
        commands = []
        async def command(*args):
            commands.append(args)
            if args == ("mouseup", "1"):
                raise helper.HelperError("input_failed")
        host = helper.Helper()
        host.held_buttons.update({"1", "3"})
        host.held_keys.add("Shift_L")
        with patch.object(helper, "xdotool", side_effect=command):
            await host.release_held()
        # xdotool reads `mouseup 1 3` as a release of 1 and an unknown command "3".
        self.assertEqual(commands, [("mouseup", "1"), ("mouseup", "3"), ("keyup", "Shift_L")])
        self.assertEqual((host.held_buttons, host.held_keys), ({"1"}, set()))  # retried by the next release


class ClipboardCopyTests(unittest.IsolatedAsyncioTestCase):
    """`xclip -o` stand-ins with the exits and messages measured on Xvfb."""
    def xclip(self, script):
        folder = tempfile.TemporaryDirectory()
        self.addCleanup(folder.cleanup)
        if script is not None:
            path = Path(folder.name, "xclip")
            path.write_text("#!/bin/sh\n" + script + "\n")
            path.chmod(0o755)
        environment = patch.dict(os.environ, {"PATH": folder.name})
        environment.start()
        self.addCleanup(environment.stop)

    async def copy(self):
        host = helper.Helper()
        result = {}
        await host.deliver({"type": "clipboard", "operation": "copyToPhone"}, result)
        return result

    async def test_text_is_copied(self):
        self.xclip("printf 'caf\\303\\251'")
        self.assertEqual(await self.copy(), {"clipboardText": "café"})

    async def test_no_text_is_a_delivered_copy_with_a_reason(self):
        for script in ("echo 'Error: target STRING not available' >&2; exit 1", "exit 0"):
            with self.subTest(script=script):
                self.xclip(script)
                self.assertEqual(await self.copy(), {"clipboardReason": "clipboard_no_text"})

    async def test_failures_stay_distinct_errors(self):
        cases = [
            (None, "xclip_required"),
            ("echo \"Error: Can't open display: :99\" >&2; exit 1", "display_unavailable"),
            ("/usr/bin/head -c 1000000 /dev/zero | /usr/bin/tr '\\0' a", "clipboard_too_large"),
            ("echo 'Error: something else' >&2; exit 2", "input_failed"),
            ("exec /usr/bin/sleep 30", "input_timed_out"),
        ]
        for script, code in cases:
            # Only the hanging xclip gets a short limit; the others finish on their own.
            limit = 0 if code == "input_timed_out" else helper.CLIPBOARD_TIMEOUT
            with self.subTest(code=code), patch.object(helper, "CLIPBOARD_TIMEOUT", limit):
                self.xclip(script)
                with self.assertRaises(helper.HelperError) as failed:
                    await asyncio.wait_for(self.copy(), timeout=10)
                self.assertEqual(failed.exception.code, code)


class CaptureRegressionTests(unittest.IsolatedAsyncioTestCase):
    async def test_lower_quality_changes_encoded_pixels_but_preserves_physical_geometry_and_lease(self):
        import numpy
        host = helper.Helper()
        host.session_id, host.generation, host.state = "capture", 1, "capturing"
        host.source = {"id": "display:x11", "kind": "display", "width": 3840, "height": 2160}
        host.lease = {"sourceID": "display:x11", "geometryRevision": 1}
        host.lease_expires = helper.time.monotonic() + 10
        host.capturing.set()
        track = helper.ScreenTrack(host)
        pixels = numpy.zeros((2160, 3840, 4), dtype=numpy.uint8)
        with patch.object(helper, "grab_screen", return_value=pixels), patch.object(helper, "write"):
            for quality, expected in (("high", (2560, 1440)), ("standard", (1280, 720)), ("medium", (1920, 1080))):
                result = await host.handle("capture.setQuality", {"sessionID": "capture", "generation": 1, "videoQuality": quality})
                frame = await asyncio.wait_for(track.recv(), 10)
                self.assertTrue(result["accepted"])
                self.assertEqual((frame.width, frame.height), expected)
                self.assertEqual((host.last_frame["sourceWidth"], host.last_frame["sourceHeight"]), (3840, 2160))
                self.assertEqual((host.source["width"], host.source["height"], host.geometry_revision), (3840, 2160, 1))
                self.assertIsNotNone(host.lease)
        track.stop()

    def test_resize_retry_is_bounded_and_does_not_mask_closed_or_replaced_sources(self):
        grabber = helper.WindowGrabber.__new__(helper.WindowGrabber)
        grabber.x, grabber.display, grabber.window, grabber.source_id = object(), 1, 41, "app:editor#41"
        resized = object()
        with patch.object(grabber, "grab_once", side_effect=[helper.HelperError("capture_geometry_changed"), resized]) as grab, \
             patch.object(helper, "x11_window", return_value={"id": grabber.source_id}) as validate:
            self.assertIs(grabber.grab(), resized)
            self.assertEqual((grab.call_count, validate.call_count), (2, 1))
        with patch.object(grabber, "grab_once", side_effect=helper.HelperError("capture_geometry_changed")) as grab, \
             patch.object(helper, "x11_window", return_value={"id": grabber.source_id}):
            with self.assertRaises(helper.HelperError):
                grabber.grab()
            self.assertEqual(grab.call_count, 3)
        for reason in ("source_not_found", "capture_unavailable"):
            with patch.object(grabber, "grab_once", side_effect=helper.HelperError(reason)) as grab, \
                 patch.object(helper, "x11_window", side_effect=AssertionError("must not retry")):
                with self.assertRaises(helper.HelperError) as failed:
                    grabber.grab()
                self.assertEqual(failed.exception.code, reason)
                self.assertEqual(grab.call_count, 1)
        with patch.object(grabber, "grab_once", side_effect=helper.HelperError("capture_geometry_changed")) as grab, \
             patch.object(helper, "x11_window", return_value={"id": "app:editor#42"}):
            with self.assertRaises(helper.HelperError) as failed:
                grabber.grab()
            self.assertEqual(failed.exception.code, "source_not_found")
            self.assertEqual(grab.call_count, 1)

    async def test_drag_up_moves_before_release_and_still_releases_if_target_is_invalid(self):
        host = helper.Helper()
        host.source = {"id": "app:editor#41", "kind": "window", "width": 800, "height": 600}
        action = {"type": "pointer", "phase": "up", "x": .75, "y": .5}
        for mapping, expected in (((600, 300), [("mousemove", "600", "300"), ("mouseup", "1")]),
                                  (helper.HelperError("window_covered"), [("mouseup", "1")])):
            commands = []
            async def command(*args):
                commands.append(args)
            host.held_buttons = {"1"}
            host.held_keys = {"Shift_L"}
            with patch.object(helper, "xdotool", side_effect=command), \
                 patch.object(helper, "window_input_frame", **({"side_effect": mapping} if isinstance(mapping, Exception) else {"return_value": mapping})):
                if isinstance(mapping, Exception):
                    with self.assertRaises(helper.HelperError):
                        await host.deliver(action, {})
                else:
                    await host.deliver(action, {})
            self.assertEqual(commands, expected)
            self.assertEqual(host.held_buttons, set())
            self.assertEqual(host.held_keys, {"Shift_L"})


class LeaseTests(unittest.IsolatedAsyncioTestCase):
    def setUp(self):
        self.host = helper.Helper()
        self.host.session_id = "capture"
        self.host.generation = 2
        self.host.source = {"id": "display:x11", "width": 1280, "height": 800}
        self.host.state = "capturing"
        self.identity = {"leaseID": "lease", "requestID": "request", "sessionID": "capture",
                         "generation": 2, "geometryRevision": 1, "sourceID": "display:x11"}
        self.delivered = []
        async def deliver(action, result):
            self.delivered.append(action)
        self.host.deliver = deliver

    async def activate(self):
        consent = await self.host.handle("control.consent", self.identity)
        self.assertTrue(consent["granted"])
        result = await self.host.handle("control.activate", self.identity)
        self.assertTrue(result["activated"])

    async def test_view_rebind_without_control_requires_capture_identity(self):
        source = {"id": "app:editor#42", "kind": "window", "width": 800, "height": 600,
                  "windowTitle": "Second window", "title": "Second window", "scale": 1.0,
                  "contentRect": {"x": 0, "y": 0, "width": 800, "height": 600}}
        identity = {key: self.identity[key] for key in
                    ("sessionID", "generation", "geometryRevision", "sourceID")}
        with patch.object(helper, "list_windows", return_value=[source]):
            for missing in ("sessionID", "generation"):
                incomplete = {key: value for key, value in identity.items() if key != missing}
                with self.assertRaises(helper.HelperError):
                    await self.host.handle("capture.rebind", dict(incomplete, nextSourceID=source["id"]))
            changed = await self.host.handle("capture.rebind", dict(identity, nextSourceID=source["id"]))
        self.assertTrue(changed["accepted"])
        self.assertEqual(changed["status"]["state"], "capturing")
        self.assertEqual(changed["status"]["sessionID"], "capture")
        self.assertEqual(changed["status"]["generation"], 2)
        self.assertEqual(changed["status"]["sourceID"], source["id"])
        self.assertEqual(changed["status"]["geometryRevision"], 2)
        self.assertIsNone(self.host.lease)

    async def test_rebind_closed_window_refuses_without_moving_view_or_lease(self):
        await self.activate()
        source = self.host.source
        lease = dict(self.host.lease)
        with patch.object(helper, "list_windows", return_value=[]):
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
        from unittest.mock import AsyncMock
        release = AsyncMock(side_effect=lambda: (self.host.held_buttons.clear(), self.host.held_keys.clear()))
        self.host.release_held = release
        with patch.object(helper, "list_windows", return_value=[source]):
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
        result = await self.host.handle("control.activate", self.identity)
        self.assertFalse(result["activated"])
        await self.host.handle("control.consent", self.identity)
        with self.assertRaises(helper.HelperError):
            await self.host.handle("control.activate", dict(self.identity, generation=3))

    async def test_input_rejects_changed_capture_identity(self):
        await self.activate()
        for field, wrong in [("sessionID", "other"), ("generation", 3), ("geometryRevision", 2), ("sourceID", "other")]:
            with self.assertRaises(helper.HelperError, msg=field):
                await self.host.handle("control.input", dict(self.identity, **{field: wrong}, sequence=1,
                                       actions=[{"type": "text", "text": "a"}]))
        self.assertEqual(self.delivered, [])

    async def test_sequence_replay_must_match_and_be_contiguous(self):
        await self.activate()
        original = dict(self.identity, sequence=1, actions=[{"type": "text", "text": "a"}])
        self.assertTrue((await self.host.handle("control.input", original))["accepted"])
        self.assertTrue((await self.host.handle("control.input", original))["duplicate"])
        conflict = await self.host.handle("control.input", dict(original, actions=[{"type": "text", "text": "b"}]))
        self.assertFalse(conflict["accepted"])
        skipped = await self.host.handle("control.input", dict(original, sequence=3))
        self.assertFalse(skipped["accepted"])
        replay = await self.host.handle("control.activate", self.identity)
        self.assertTrue(replay["replayed"])
        self.assertTrue((await self.host.handle("control.input", original))["duplicate"])
        self.assertEqual(len(self.delivered), 1)

    async def test_input_timeout_cancels_the_command_and_releases_control(self):
        await self.activate()
        async def stuck(action, result):
            self.host.held_buttons.add("1")
            await asyncio.sleep(1)
        self.host.deliver = stuck
        commands, events = [], []
        async def command(*args):
            commands.append(args)
        with patch.object(helper, "INPUT_TIMEOUT", .01), patch.object(helper, "xdotool", side_effect=command), \
             patch.object(helper, "write", side_effect=events.append):
            with self.assertRaises(helper.HelperError) as timed_out:
                await self.host.handle("control.input", dict(self.identity, sequence=1, actions=[{"type": "releaseAll"}]))
        self.assertEqual(timed_out.exception.code, "input_timed_out")
        self.assertIsNone(self.host.lease)
        self.assertEqual(commands, [("mouseup", "1")])
        self.assertEqual(events, [{"event": "control.revoked", **self.identity, "reason": "input_timed_out"}])

    async def test_request_timeout_answers_it_and_releases_control(self):
        await self.activate()
        self.host.held_keys.add("Control_L")
        async def stuck(method, params):
            await asyncio.sleep(1)
        self.host.handle = stuck
        commands, events = [], []
        async def command(*args):
            commands.append(args)
        with patch.object(helper, "REQUEST_TIMEOUT", .01), patch.object(helper, "xdotool", side_effect=command), \
             patch.object(helper, "write", side_effect=events.append):
            reply = await helper.respond(self.host, 7, "signal.answer", {})
        self.assertEqual(reply, {"id": 7, "error": {"code": "request_timed_out"}})
        self.assertIsNone(self.host.lease)
        self.assertEqual(commands, [("keyup", "Control_L")])
        self.assertEqual(events, [{"event": "control.revoked", **self.identity, "reason": "request_timed_out"}])

    async def test_a_release_cut_short_by_a_timeout_is_finished_by_the_revoke(self):
        await self.activate()
        del self.host.deliver  # the real releaseAll
        self.host.held_buttons.update({"1", "3"})
        self.host.held_keys.add("Shift_L")
        commands, events, hung = [], [], []
        async def command(*args):
            commands.append(args)
            if args == ("mouseup", "1") and not hung:
                hung.append(args)
                await asyncio.sleep(1)  # cancelled by the request timeout
        actions = [{"type": "releaseAll"}]
        with patch.object(helper, "REQUEST_TIMEOUT", .01), patch.object(helper, "xdotool", side_effect=command), \
             patch.object(helper, "write", side_effect=events.append):
            reply = await helper.respond(self.host, 7, "control.input", dict(self.identity, sequence=1, actions=actions))
        self.assertEqual(reply, {"id": 7, "error": {"code": "request_timed_out"}})
        self.assertEqual(commands, [("mouseup", "1"), ("mouseup", "1"), ("mouseup", "3"), ("keyup", "Shift_L")])
        self.assertFalse(self.host.held_buttons or self.host.held_keys)
        self.assertEqual([event["event"] for event in events], ["control.revoked"])

    async def test_slow_batch_keeps_heartbeat_and_status_live_and_release_cancels_effects(self):
        await self.activate()
        started, finished = asyncio.Event(), asyncio.Event()
        effects = []
        async def slow(action, result):
            started.set()
            try:
                await asyncio.sleep(3)
                effects.append(action)
            finally:
                finished.set()
        self.host.deliver = slow
        request = asyncio.create_task(self.host.handle("control.input", dict(self.identity, sequence=1,
                                      actions=[{"type": "text", "text": "first"}, {"type": "text", "text": "second"}])))
        await asyncio.wait_for(started.wait(), 10)
        heartbeat = await asyncio.wait_for(self.host.handle("control.heartbeat", self.identity), 10)
        self.assertTrue(heartbeat["renewed"])
        self.assertFalse(request.done())
        status = await asyncio.wait_for(self.host.handle("capture.status", {}), 10)
        self.assertEqual(status["state"], "capturing")
        with patch.object(helper, "write"):
            released = await asyncio.wait_for(self.host.handle("control.release", self.identity), 10)
        self.assertTrue(released["released"])
        self.assertTrue(finished.is_set())
        with self.assertRaises(helper.HelperError) as failed:
            await request
        self.assertEqual(failed.exception.code, "lease_not_active")
        self.assertEqual(effects, [])
        self.assertEqual(self.host.payloads, {})

    async def test_focus_reports_changes_with_the_lease_and_discards_a_read_completed_after_release(self):
        import threading
        await self.activate()
        started, finish = threading.Event(), threading.Event()
        events = []
        def read_focus(source):
            started.set()
            finish.wait(10)
            return True
        with patch.object(helper, "text_input_focused", side_effect=read_focus), patch.object(helper, "write", side_effect=events.append):
            watcher = asyncio.create_task(self.host.watch_lease())
            try:
                await asyncio.wait_for(asyncio.to_thread(started.wait, 10), 10)
                await self.host.handle("control.release", self.identity)
                finish.set()
                # A round trip through the executor drains the stale focus read.
                await asyncio.to_thread(lambda: None)
                await asyncio.sleep(0)
            finally:
                finish.set()
                watcher.cancel()
                await asyncio.gather(watcher, return_exceptions=True)
        self.assertEqual([event["event"] for event in events], ["control.revoked"])
        await self.activate()
        focused = asyncio.Event()
        events = []
        def emit(event):
            events.append(event)
            if event["event"] == "control.textFocus":
                focused.set()
        with patch.object(helper, "text_input_focused", return_value=True), patch.object(helper, "write", side_effect=emit):
            watcher = asyncio.create_task(self.host.watch_lease())
            try:
                await asyncio.wait_for(focused.wait(), 10)
            finally:
                watcher.cancel()
                await asyncio.gather(watcher, return_exceptions=True)
        self.assertEqual(events, [{"event": "control.textFocus", **self.identity, "textInput": True}])

    async def test_queued_input_cannot_cross_release_and_reactivation(self):
        await self.activate()
        await self.host.input_lock.acquire()
        request = asyncio.create_task(self.host.handle("control.input", dict(self.identity, sequence=1,
                                      actions=[{"type": "text", "text": "stale"}])))
        await asyncio.sleep(0)
        with patch.object(helper, "write"):
            await self.host.handle("control.release", self.identity)
        await self.activate()
        self.host.input_lock.release()
        with self.assertRaises(helper.HelperError) as failed:
            await request
        self.assertEqual(failed.exception.code, "lease_not_active")
        self.assertEqual(self.delivered, [])

    async def test_expired_lease_cannot_deliver_or_be_renewed(self):
        await self.activate()
        self.host.lease_expires = 0
        result = await self.host.handle("control.heartbeat", self.identity)
        self.assertFalse(result["renewed"])
        result = await self.host.handle("control.input", dict(self.identity, sequence=1,
                                       actions=[{"type": "text", "text": "a"}]))
        self.assertFalse(result["accepted"])
        self.assertEqual(self.delivered, [])


if __name__ == "__main__":
    unittest.main()

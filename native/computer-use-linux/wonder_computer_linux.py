#!/usr/bin/env python3
"""EXPERIMENTAL: Cupola's computer helper for a headless Linux host.

wonderd runs this as WONDER_COMPUTER_USE_BIN and speaks the same JSON-lines
protocol it uses with the macOS helper (native/computer-use): capture
lifecycle, WebRTC signaling relay and the control lease. Differences:

* Source: the X11 display in $DISPLAY (an Xvfb virtual screen started by
  `cupola --computer`), captured with MIT-SHM through `mss`. Visible app
  windows use XComposite's exact window pixmap, never a screen rectangle.
* Video: an `aiortc` peer connection publishes VP8 using host candidates.
  A normal Tailscale TUN interface provides the tailnet candidate. Userspace
  Tailscale, NAT traversal and TURN are not supported by this prototype.
* Input: XTEST through `xdotool`; provider `xtest-x11-v1`, no permission
  prompt because the helper only drives its own virtual display.

Requires: Python 3.11+, aiortc, mss, numpy, libX11, libXcomposite, xdotool (and xclip for the
clipboard actions).
"""

import asyncio
from fractions import Fraction
import hashlib
import math
import json
import os
import shutil
import sys
import threading
import time
import uuid

MAX_LINE_BYTES = 128 * 1024
HANDSHAKE = os.environ.get("WONDER_COMPUTER_USE_HANDSHAKE", "")
HELPER_INSTANCE = str(uuid.uuid4())
QUALITY_FPS = {"standard": 10, "medium": 12, "auto": 12, "high": 15}
QUALITY_BOX = {"standard": (1280, 720), "medium": (1920, 1080), "auto": (2560, 1440), "high": (2560, 1440)}


def encoded_size(width, height, quality):
    max_width, max_height = QUALITY_BOX[quality]
    factor = min(max_width / width, max_height / height, 1)
    return max(2, int(width * factor) & ~1), max(2, int(height * factor) & ~1)


INPUT_TIMEOUT, REQUEST_TIMEOUT, CLIPBOARD_TIMEOUT = 8, 10, 3
_OUTPUT = None

from aiortc import RTCConfiguration, RTCPeerConnection, RTCSessionDescription  # noqa: E402
from aiortc.codecs import vpx  # noqa: E402
from aiortc.mediastreams import VideoStreamTrack  # noqa: E402
from aiortc.sdp import candidate_from_sdp  # noqa: E402
import av  # noqa: E402
import mss  # noqa: E402
import numpy  # noqa: E402

# Screen text needs more than aiortc's 500 kbit/s default to stay legible.
vpx.DEFAULT_BITRATE = 1_500_000
vpx.MAX_BITRATE = 3_000_000


def log(*parts):
    print("wonder-computer-linux:", *parts, file=sys.stderr, flush=True)


def write(obj):
    data = (json.dumps(obj, ensure_ascii=False, separators=(",", ":")) + "\n").encode()
    if _OUTPUT is None:
        sys.stdout.write(data.decode())
        sys.stdout.flush()
        return
    if _OUTPUT.get_write_buffer_size() + len(data) > 1024 * 1024:
        raise SystemExit("computer helper output stalled")
    _OUTPUT.write(data)


class HelperError(Exception):
    def __init__(self, code):
        super().__init__(code)
        self.code = code


# ------------------------------------------------------------------ screen


_grabber = threading.local()
_MSS = getattr(mss, "MSS", None) or mss.mss  # mss 10 renamed the factory


def grab_screen():
    """Captures the display as a BGRA ndarray (runs on a worker thread)."""
    if not hasattr(_grabber, "sct"):
        _grabber.sct = _MSS()
    shot = _grabber.sct.grab(_grabber.sct.monitors[1])
    return numpy.frombuffer(shot.bgra, dtype=numpy.uint8).reshape(shot.height, shot.width, 4)


def display_size():
    code = missing_display_code()
    if code:
        raise HelperError(code)
    with _MSS() as sct:
        monitor = sct.monitors[1]
        return monitor["width"], monitor["height"]


class ScreenTrack(VideoStreamTrack):
    kind = "video"

    def __init__(self, helper):
        super().__init__()
        self.helper = helper
        self.started = time.monotonic()
        self.next_frame = self.started

    async def recv(self):
        await self.helper.capturing.wait()
        now = time.monotonic()
        await asyncio.sleep(max(0, self.next_frame - now))
        now = time.monotonic()
        self.next_frame = now + 1 / QUALITY_FPS[self.helper.quality]
        pts, time_base = int((now - self.started) * 90000), Fraction(1, 90000)
        try:
            source = self.helper.source
            grab = (lambda: grab_window(source)) if source and source.get("kind") == "window" else grab_screen
            pixels = await asyncio.get_running_loop().run_in_executor(None, grab)
        except Exception as error:
            if source is not self.helper.source:
                return await self.recv()
            reason = error.code if isinstance(error, HelperError) else "capture_failed"
            removed = reason == "source_not_found"
            self.helper.state, self.helper.reason = "sourceRemoved" if removed else "failed", reason
            self.helper.capturing.clear()
            await self.helper.revoke(reason)
            self.helper.emit_capture("capture.sourceRemoved" if removed else "capture.failed", message=reason)
            raise
        frame = av.VideoFrame.from_ndarray(pixels, format="bgra")
        frame.pts = pts
        frame.time_base = time_base
        if source is not self.helper.source:
            # Never stamp a previous view's in-flight frame with new geometry.
            return await self.recv()
        await self.helper.frame_captured(frame.width, frame.height)
        width, height = encoded_size(frame.width, frame.height, self.helper.quality)
        if (width, height) != (frame.width, frame.height):
            frame = frame.reformat(width=width, height=height)
            frame.pts, frame.time_base = pts, time_base
        return frame


# X11 application windows use the same app:<class>#<XID> identity as other
# helpers. Querying the server (not a persisted app inventory) drops unmapped,
# minimized and destroyed windows. XComposite supplies only the target pixmap;
# a window source never falls back to pixels from the whole display.
import base64
import configparser
from pathlib import Path
import ctypes
import ctypes.util
from collections import OrderedDict
import re
import struct
import zlib


class XWindowAttributes(ctypes.Structure):
    _fields_ = [("x", ctypes.c_int), ("y", ctypes.c_int), ("width", ctypes.c_int),
                ("height", ctypes.c_int), ("border_width", ctypes.c_int), ("depth", ctypes.c_int),
                ("visual", ctypes.c_void_p), ("root", ctypes.c_ulong), ("window_class", ctypes.c_int),
                ("bit_gravity", ctypes.c_int), ("win_gravity", ctypes.c_int), ("backing_store", ctypes.c_int),
                ("backing_planes", ctypes.c_ulong), ("backing_pixel", ctypes.c_ulong),
                ("save_under", ctypes.c_int), ("colormap", ctypes.c_ulong),
                ("map_installed", ctypes.c_int), ("map_state", ctypes.c_int),
                ("all_event_masks", ctypes.c_long), ("your_event_mask", ctypes.c_long),
                ("do_not_propagate_mask", ctypes.c_long), ("override_redirect", ctypes.c_int),
                ("screen", ctypes.c_void_p)]


class XImage(ctypes.Structure):
    _fields_ = [("width", ctypes.c_int), ("height", ctypes.c_int), ("xoffset", ctypes.c_int),
                ("format", ctypes.c_int), ("data", ctypes.c_void_p), ("byte_order", ctypes.c_int),
                ("bitmap_unit", ctypes.c_int), ("bitmap_bit_order", ctypes.c_int),
                ("bitmap_pad", ctypes.c_int), ("depth", ctypes.c_int),
                ("bytes_per_line", ctypes.c_int), ("bits_per_pixel", ctypes.c_int)]


_X11 = None
_XCOMPOSITE = None
_XERROR = None
_ICON_CACHE = OrderedDict()
_APP_CACHE = OrderedDict()
_ATOMS = {}
_XERROR_LOCAL = threading.local()
_INPUT_PERMISSION = threading.local()
_CAPTURED_POPUPS = {}


class XErrorEvent(ctypes.Structure):
    _fields_ = [("type", ctypes.c_int), ("display", ctypes.c_void_p), ("resource", ctypes.c_ulong),
                ("serial", ctypes.c_ulong), ("error_code", ctypes.c_ubyte),
                ("request_code", ctypes.c_ubyte), ("minor_code", ctypes.c_ubyte)]


def x11_atom(x, display, name):
    key = (display, name)
    if key not in _ATOMS:
        _ATOMS[key] = x.XInternAtom(display, name.encode(), False)
    return _ATOMS[key]


def x11_close(x, display):
    for key in list(_ATOMS):
        if key[0] == display:
            del _ATOMS[key]
    x.XCloseDisplay(display)


def transient_for(x, display, popup, selected):
    seen = set()
    for _ in range(32):
        if popup == selected:
            return True
        if popup in seen:
            return False
        seen.add(popup)
        owner = x11_property(x, display, popup, "WM_TRANSIENT_FOR")
        if not owner or len(owner) != 1:
            return False
        popup = owner[0]
    return False


def popup_was_captured(source, popup):
    entry = _CAPTURED_POPUPS.get(source["id"])
    return bool(entry and time.monotonic() - entry[0] < .5 and popup in entry[1])



def x11_libraries():
    global _X11, _XCOMPOSITE, _XERROR
    if _X11 is None:
        x = ctypes.CDLL(ctypes.util.find_library("X11") or "libX11.so.6")
        x.XInitThreads()
        x.XOpenDisplay.argtypes, x.XOpenDisplay.restype = (ctypes.c_char_p,), ctypes.c_void_p
        x.XCloseDisplay.argtypes = (ctypes.c_void_p,)
        x.XDefaultRootWindow.argtypes, x.XDefaultRootWindow.restype = (ctypes.c_void_p,), ctypes.c_ulong
        x.XInternAtom.argtypes, x.XInternAtom.restype = (ctypes.c_void_p, ctypes.c_char_p, ctypes.c_int), ctypes.c_ulong
        x.XGetWindowProperty.argtypes = (ctypes.c_void_p, ctypes.c_ulong, ctypes.c_ulong, ctypes.c_long,
            ctypes.c_long, ctypes.c_int, ctypes.c_ulong, ctypes.POINTER(ctypes.c_ulong),
            ctypes.POINTER(ctypes.c_int), ctypes.POINTER(ctypes.c_ulong), ctypes.POINTER(ctypes.c_ulong),
            ctypes.POINTER(ctypes.c_void_p))
        x.XQueryTree.argtypes = (ctypes.c_void_p, ctypes.c_ulong, ctypes.POINTER(ctypes.c_ulong),
            ctypes.POINTER(ctypes.c_ulong), ctypes.POINTER(ctypes.POINTER(ctypes.c_ulong)), ctypes.POINTER(ctypes.c_uint))
        x.XGetWindowAttributes.argtypes = (ctypes.c_void_p, ctypes.c_ulong, ctypes.POINTER(XWindowAttributes))
        x.XTranslateCoordinates.argtypes = (ctypes.c_void_p, ctypes.c_ulong, ctypes.c_ulong, ctypes.c_int, ctypes.c_int,
            ctypes.POINTER(ctypes.c_int), ctypes.POINTER(ctypes.c_int), ctypes.POINTER(ctypes.c_ulong))
        x.XRaiseWindow.argtypes = (ctypes.c_void_p, ctypes.c_ulong)
        x.XSetInputFocus.argtypes = (ctypes.c_void_p, ctypes.c_ulong, ctypes.c_int, ctypes.c_ulong)
        x.XGetInputFocus.argtypes = (ctypes.c_void_p, ctypes.POINTER(ctypes.c_ulong), ctypes.POINTER(ctypes.c_int))
        x.XQueryPointer.argtypes = (ctypes.c_void_p, ctypes.c_ulong, ctypes.POINTER(ctypes.c_ulong), ctypes.POINTER(ctypes.c_ulong),
            ctypes.POINTER(ctypes.c_int), ctypes.POINTER(ctypes.c_int), ctypes.POINTER(ctypes.c_int), ctypes.POINTER(ctypes.c_int), ctypes.POINTER(ctypes.c_uint))
        x.XFree.argtypes, x.XFreePixmap.argtypes = (ctypes.c_void_p,), (ctypes.c_void_p, ctypes.c_ulong)
        x.XSync.argtypes = (ctypes.c_void_p, ctypes.c_int)
        x.XGetImage.argtypes = (ctypes.c_void_p, ctypes.c_ulong, ctypes.c_int, ctypes.c_int,
                              ctypes.c_uint, ctypes.c_uint, ctypes.c_ulong, ctypes.c_int)
        x.XGetImage.restype = ctypes.POINTER(XImage)
        x.XDestroyImage.argtypes = (ctypes.c_void_p,)
        # Closing a window between a metadata read and XGetImage is ordinary.
        # The request returns no image; the default Xlib error handler exits.
        callback = ctypes.CFUNCTYPE(ctypes.c_int, ctypes.c_void_p, ctypes.c_void_p)
        def record_error(_display, error):
            _XERROR_LOCAL.code = ctypes.cast(error, ctypes.POINTER(XErrorEvent)).contents.error_code
            return 0
        _XERROR = callback(record_error)
        x.XSetErrorHandler.argtypes = (callback,)
        x.XSetErrorHandler(_XERROR)
        c = ctypes.CDLL(ctypes.util.find_library("Xcomposite") or "libXcomposite.so.1")
        c.XCompositeRedirectWindow.argtypes = (ctypes.c_void_p, ctypes.c_ulong, ctypes.c_int)
        c.XCompositeNameWindowPixmap.argtypes = (ctypes.c_void_p, ctypes.c_ulong)
        c.XCompositeNameWindowPixmap.restype = ctypes.c_ulong
        _X11, _XCOMPOSITE = x, c
    return _X11, _XCOMPOSITE


def missing_display_code(environ=os.environ):
    """Why there is no X11 display to share: a Wayland login gets its own code."""
    if environ.get("DISPLAY"):
        return None
    wayland = environ.get("WAYLAND_DISPLAY") or environ.get("XDG_SESSION_TYPE", "").lower() == "wayland"
    return "wayland_unsupported" if wayland else "x11_display_required"


def x11_open():
    code = missing_display_code()
    if code:
        raise HelperError(code)
    x, _ = x11_libraries()
    display = x.XOpenDisplay(None)
    if not display:
        raise HelperError("capture_unavailable")
    return x, display


def x11_property(x, display, window, name):
    kind, size, count, remaining, pointer = ctypes.c_ulong(), ctypes.c_int(), ctypes.c_ulong(), ctypes.c_ulong(), ctypes.c_void_p()
    atom = x11_atom(x, display, name)
    status = x.XGetWindowProperty(display, window, atom, 0, 4096, False, 0,
                                 ctypes.byref(kind), ctypes.byref(size), ctypes.byref(count),
                                 ctypes.byref(remaining), ctypes.byref(pointer))
    try:
        if status or not pointer or (remaining.value and name != "_NET_WM_ICON"):
            return None
        if size.value == 8:
            return ctypes.string_at(pointer, count.value)
        if size.value == 32:
            # Xlib stores format-32 items in C longs, sign-extending values such
            # as opaque ARGB pixels on 64-bit hosts; only the low 32 bits are data.
            return [value & 0xFFFFFFFF for value in ctypes.cast(pointer, ctypes.POINTER(ctypes.c_ulong))[:count.value]]
        return None
    finally:
        if pointer:
            x.XFree(pointer)


def printable(text):
    return "".join(c if c.isprintable() else " " for c in text).encode()[:160].decode("utf-8", "ignore").strip()


def application_name(app):
    # Desktop metadata owns a human name; WM_CLASS remains source identity.
    for path in (Path("/usr/share/applications") / (app + ".desktop"),
                 Path("/usr/share/applications") / (app.lower() + ".desktop")):
        try:
            key = (str(path), path.stat().st_mtime_ns)
            if key in _APP_CACHE:
                _APP_CACHE.move_to_end(key)
                return _APP_CACHE[key]
            if path.stat().st_size > 64 * 1024:
                continue
            entry = configparser.ConfigParser(interpolation=None, strict=False)
            entry.read(path, encoding="utf-8")
            name = printable(entry.get("Desktop Entry", "Name", fallback=""))
            if name:
                _APP_CACHE[key] = name
                while len(_APP_CACHE) > 64:
                    _APP_CACHE.popitem(last=False)
                return name
        except (OSError, configparser.Error):
            continue
    return printable(app.rsplit(".", 1)[-1].replace("-", " "))


def x11_window(x, display, window):
    attrs = XWindowAttributes()
    if not x.XGetWindowAttributes(display, window, ctypes.byref(attrs)) or attrs.map_state != 2 or attrs.override_redirect:
        return None
    if attrs.width < 80 or attrs.height < 60:
        return None
    state = x11_property(x, display, window, "WM_STATE") or []
    hidden = x11_atom(x, display, "_NET_WM_STATE_HIDDEN")
    if (state and state[0] == 3) or hidden in (x11_property(x, display, window, "_NET_WM_STATE") or []):
        return None
    opacity = x11_property(x, display, window, "_NET_WM_WINDOW_OPACITY")
    if opacity and opacity[0] == 0:
        return None
    excluded = {x11_atom(x, display, "_NET_WM_WINDOW_TYPE_" + kind)
                for kind in ("DESKTOP", "DOCK", "MENU", "DROPDOWN_MENU", "POPUP_MENU", "TOOLTIP",
                             "NOTIFICATION", "COMBO", "DND", "SPLASH", "UTILITY")}
    if excluded.intersection(x11_property(x, display, window, "_NET_WM_WINDOW_TYPE") or []):
        return None
    classes = (x11_property(x, display, window, "WM_CLASS") or b"").decode("utf-8", "replace").strip("\0").split("\0")
    app = classes[-1]
    if not app or len(app) > 150 or len(f"app:{app}#{window}") > 160 or not re.fullmatch(r"[A-Za-z0-9._-]+", app):
        return None
    raw_title = x11_property(x, display, window, "_NET_WM_NAME") or x11_property(x, display, window, "WM_NAME") or b""
    title = printable(raw_title.decode("utf-8", "replace"))
    if not title:
        return None
    return {"id": f"app:{app}#{window}", "kind": "window", "appId": app, "appName": application_name(app),
            "windowTitle": title, "name": title, "width": attrs.width, "height": attrs.height,
            "scale": 1.0}


def window_input_frame(source, normalized=None, wheel=False, raise_window=True):
    """Resolve, raise and hit-test the exact X11 client; never a cached rectangle.

    Input is global under XTEST. Compare the root's actual child (including WM
    decoration frames and override-redirect popups) before posting a pointer.
    """
    x, _ = x11_libraries()
    display = x.XOpenDisplay(None)
    if not display:
        raise HelperError("input_unavailable")
    try:
        window = int(source["id"].rpartition("#")[2])
        current = x11_window(x, display, window)
        if not current or current["id"] != source["id"]:
            raise HelperError("source_not_found")
        root = x.XDefaultRootWindow(display)
        def parent_of(child):
            root_out, parent, children, count = ctypes.c_ulong(), ctypes.c_ulong(), ctypes.POINTER(ctypes.c_ulong)(), ctypes.c_uint()
            if not x.XQueryTree(display, child, ctypes.byref(root_out), ctypes.byref(parent), ctypes.byref(children), ctypes.byref(count)):
                raise HelperError("source_not_found")
            if children:
                x.XFree(children)
            return parent.value
        frame = window
        for _ in range(64):
            parent = parent_of(frame)
            if parent == root:
                break
            if not parent:
                raise HelperError("source_not_found")
            frame = parent
        else:
            raise HelperError("source_not_found")
        before, revert = ctypes.c_ulong(), ctypes.c_int()
        x.XGetInputFocus(display, ctypes.byref(before), ctypes.byref(revert))
        if raise_window and before.value != window and not transient_for(x, display, before.value, window):
            permission = getattr(_INPUT_PERMISSION, "check", None)
            if permission:
                permission()
            x.XRaiseWindow(display, frame)
            if permission:
                permission()
            x.XSetInputFocus(display, window, 2, 0)
            x.XSync(display, False)
        focused, revert = ctypes.c_ulong(), ctypes.c_int()
        x.XGetInputFocus(display, ctypes.byref(focused), ctypes.byref(revert))
        focus = focused.value
        for _ in range(64):
            if focus == window or transient_for(x, display, focus, window):
                break
            if focus in (0, 1, root):
                raise HelperError("window_not_focused")
            focus = parent_of(focus)
        else:
            raise HelperError("window_not_focused")
        ox, oy, child = ctypes.c_int(), ctypes.c_int(), ctypes.c_ulong()
        if not x.XTranslateCoordinates(display, window, root, 0, 0, ctypes.byref(ox), ctypes.byref(oy), ctypes.byref(child)):
            raise HelperError("source_not_found")
        width, height = current["width"], current["height"]
        point = None
        if normalized is not None:
            point = map_window_point(normalized[0], normalized[1], ox.value, oy.value, width, height)
            unused_x, unused_y = ctypes.c_int(), ctypes.c_int()
            x.XTranslateCoordinates(display, root, root, *point, ctypes.byref(unused_x), ctypes.byref(unused_y), ctypes.byref(child))
        elif wheel:
            root_out, rx, ry, wx, wy, mask = ctypes.c_ulong(), ctypes.c_int(), ctypes.c_int(), ctypes.c_int(), ctypes.c_int(), ctypes.c_uint()
            x.XQueryPointer(display, root, ctypes.byref(root_out), ctypes.byref(child), ctypes.byref(rx), ctypes.byref(ry), ctypes.byref(wx), ctypes.byref(wy), ctypes.byref(mask))
        if ((normalized is not None or wheel) and child.value != frame
                and not (transient_for(x, display, child.value, window) and popup_was_captured(source, child.value))):
            raise HelperError("window_covered")
        return point
    finally:
        x11_close(x, display)


def map_window_point(x, y, origin_x, origin_y, width, height):
    if not (math.isfinite(x) and math.isfinite(y) and 0 <= x <= 1 and 0 <= y <= 1 and width > 0 and height > 0):
        raise HelperError("invalid_input")
    return origin_x + round(x * (width - 1)), origin_y + round(y * (height - 1))


def x11_icon(values):
    # _NET_WM_ICON is untrusted client data: cap dimensions and decoded bytes.
    if not values or len(values) > 4096 or any(not isinstance(value, int) or not 0 <= value <= 0xFFFFFFFF for value in values):
        return None
    key = hashlib.sha256(struct.pack(f"<{len(values)}I", *values)).digest()
    if key in _ICON_CACHE:
        _ICON_CACHE.move_to_end(key)
        return _ICON_CACHE[key]
    choices, offset = [], 0
    while offset + 2 <= len(values):
        width, height = values[offset:offset + 2]
        count = width * height
        if not 0 < width <= 256 or not 0 < height <= 256 or offset + 2 + count > len(values):
            break
        choices.append((width, height, values[offset + 2:offset + 2 + count]))
        offset += 2 + count
    if not choices:
        return None
    width, height, pixels = min(choices, key=lambda image: abs(max(image[:2]) - 20))
    size, rows = 20, bytearray()
    for y in range(size):
        rows.append(0)
        for column in range(size):
            argb = pixels[(y * height // size) * width + column * width // size]
            rows.extend(((argb >> 16) & 255, (argb >> 8) & 255, argb & 255, (argb >> 24) & 255))
    def chunk(kind, value):
        return struct.pack(">I", len(value)) + kind + value + struct.pack(">I", zlib.crc32(kind + value))
    png = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0))
    png += chunk(b"IDAT", zlib.compress(bytes(rows))) + chunk(b"IEND", b"")
    encoded = base64.b64encode(png).decode()
    _ICON_CACHE[key] = encoded
    while len(_ICON_CACHE) > 32:
        _ICON_CACHE.popitem(last=False)
    return encoded if len(encoded) <= 3072 else None


def list_windows(apps, all_windows=False):
    x, display = x11_open()
    try:
        root = x.XDefaultRootWindow(display)
        windows = x11_property(x, display, root, "_NET_CLIENT_LIST_STACKING")
        if windows is None:
            # Xvfb without a window manager has no EWMH list. Only actual
            # root children with WM_CLASS/title and viewable attributes qualify.
            parent, root_out, children, count = ctypes.c_ulong(), ctypes.c_ulong(), ctypes.POINTER(ctypes.c_ulong)(), ctypes.c_uint()
            x.XQueryTree(display, root, ctypes.byref(root_out), ctypes.byref(parent), ctypes.byref(children), ctypes.byref(count))
            windows = list(children[:min(count.value, 512)]) if children else []
            if children:
                x.XFree(children)
        sources = []
        for window in reversed(windows[-512:]):
            source = x11_window(x, display, window)
            if source and (all_windows or source["appId"] in apps):
                icon = x11_icon(x11_property(x, display, window, "_NET_WM_ICON"))
                if icon:
                    source["iconPNG"] = icon
                sources.append(source)
                if len(sources) == 32:
                    break
        return sources
    finally:
        x11_close(x, display)


def capture_capability():
    try:
        # XInitThreads must precede mss opening its first X11 connection.
        x11_libraries()
        available = all(value > 0 for value in display_size())
    except Exception:
        available = False
    if available:
        return {"available": True}
    return {"available": False, "reason": "wayland_unsupported" if missing_display_code() == "wayland_unsupported" else "no_display"}


class WindowGrabber:
    """One retained XComposite redirect for the source's capture lifetime."""
    def __init__(self, source):
        self.source_id = source["id"]
        self.window = int(self.source_id.rpartition("#")[2])
        self.x, self.display = x11_open()
        _, self.composite = x11_libraries()
        self.composite.XCompositeRedirectWindow(self.display, self.window, 0)
        self.x.XSync(self.display, False)

    def close(self):
        if self.display:
            x11_close(self.x, self.display)
            self.display = None

    def grab(self):
        for attempt in range(3):
            try:
                return self.grab_once()
            except HelperError as error:
                if error.code != "capture_geometry_changed" or attempt == 2:
                    raise
                current = x11_window(self.x, self.display, self.window)
                if current is None or current["id"] != self.source_id:
                    raise HelperError("source_not_found") from None

    def grab_once(self):
        _XERROR_LOCAL.code = 0
        x, display, pixmap, image = self.x, self.display, 0, None
        try:
            current = x11_window(x, display, self.window)
            if current is None or current["id"] != self.source_id:
                raise HelperError("source_not_found")
            pixmap = self.composite.XCompositeNameWindowPixmap(display, self.window)
            x.XSync(display, False)
            width, height = current["width"], current["height"]
            if not pixmap or width > 8192 or height > 8192:
                raise HelperError("capture_unavailable")
            image = x.XGetImage(display, pixmap, 0, 0, width, height, ctypes.c_ulong(-1), 2)
            if not image:
                code = getattr(_XERROR_LOCAL, "code", 0)
                raise HelperError("source_not_found" if code == 3 else "capture_geometry_changed" if code in (8, 9) else "capture_unavailable")
            if image.contents.bits_per_pixel != 32 or image.contents.byte_order != 0:
                raise HelperError("capture_unavailable")
            view = image.contents
            raw = ctypes.string_at(view.data, view.bytes_per_line * height)
            pixels = numpy.frombuffer(raw, dtype=numpy.uint8).reshape(height, view.bytes_per_line)[:, :width * 4].reshape(height, width, 4).copy()
            return self.composite_popups(pixels)
        finally:
            if image:
                x.XDestroyImage(image)
            if pixmap:
                x.XFreePixmap(display, pixmap)

    def composite_popups(self, pixels):
        x, display = self.x, self.display
        root = x.XDefaultRootWindow(display)
        root_out, parent, children, count = ctypes.c_ulong(), ctypes.c_ulong(), ctypes.POINTER(ctypes.c_ulong)(), ctypes.c_uint()
        if not x.XQueryTree(display, root, ctypes.byref(root_out), ctypes.byref(parent), ctypes.byref(children), ctypes.byref(count)):
            return pixels
        popups = list(children[:min(count.value, 512)]) if children else []
        if children:
            x.XFree(children)
        captured = set()
        if hasattr(self, "redirected"):
            self.redirected.intersection_update(popups)
        height, width = pixels.shape[:2]
        for popup in popups[-32:]:
            if popup == self.window or not transient_for(x, display, popup, self.window):
                continue
            attrs = XWindowAttributes()
            if not x.XGetWindowAttributes(display, popup, ctypes.byref(attrs)) or attrs.map_state != 2:
                continue
            ox, oy, child = ctypes.c_int(), ctypes.c_int(), ctypes.c_ulong()
            if not x.XTranslateCoordinates(display, popup, self.window, 0, 0, ctypes.byref(ox), ctypes.byref(oy), ctypes.byref(child)):
                continue
            left, top = max(0, ox.value), max(0, oy.value)
            right, bottom = min(width, ox.value + attrs.width), min(height, oy.value + attrs.height)
            if right <= left or bottom <= top:
                continue
            if not hasattr(self, "redirected"):
                self.redirected = set()
            if popup not in self.redirected:
                self.composite.XCompositeRedirectWindow(display, popup, 0)
                self.redirected.add(popup)
            pixmap = self.composite.XCompositeNameWindowPixmap(display, popup)
            image = None
            try:
                image = x.XGetImage(display, pixmap, left - ox.value, top - oy.value, right-left, bottom-top, ctypes.c_ulong(-1), 2)
                if not image or image.contents.bits_per_pixel != 32 or image.contents.byte_order != 0:
                    continue
                view = image.contents
                raw = ctypes.string_at(view.data, view.bytes_per_line * (bottom-top))
                popup_pixels = numpy.frombuffer(raw, dtype=numpy.uint8).reshape(bottom-top, view.bytes_per_line)[:, :(right-left)*4].reshape(bottom-top, right-left, 4)
                pixels[top:bottom, left:right] = popup_pixels
                captured.add(popup)
            finally:
                if image:
                    x.XDestroyImage(image)
                if pixmap:
                    x.XFreePixmap(display, pixmap)
        _CAPTURED_POPUPS.clear()
        _CAPTURED_POPUPS[self.source_id] = (time.monotonic(), captured)
        return pixels


_window_grabber = None
_window_grabber_lock = threading.Lock()


def release_window_capture():
    global _window_grabber
    with _window_grabber_lock:
        if _window_grabber:
            _window_grabber.close()
            _window_grabber = None


def grab_window(source):
    global _window_grabber
    with _window_grabber_lock:
        if _window_grabber is None or _window_grabber.source_id != source["id"]:
            if _window_grabber:
                _window_grabber.close()
            _window_grabber = WindowGrabber(source)
        return _window_grabber.grab()


def discovery_answer(available, sources):
    # Per-icon and row caps also need an envelope cap: quoted/non-ASCII
    # titles can expand on the JSON wire. Keep each retained row complete.
    answer = {"available": available, "sources": sources[:32] if available else []}
    while len(json.dumps(answer, ensure_ascii=False, separators=(",", ":")).encode()) > MAX_LINE_BYTES - 512:
        answer["sources"].pop()
    return answer


def discover_sources():
    if hasattr(sys.stdout, "reconfigure"):
        sys.stdout.reconfigure(encoding="utf-8")
    for line in sys.stdin:
        try:
            request = json.loads(line)
            apps, all_windows = request.get("apps") or [], request.get("allWindows") is True
            capture = capture_capability()
            available = capture["available"]
            sources = list_windows(apps, all_windows) if available and (apps or all_windows) else []
            answer = discovery_answer(available, sources)
            answer["capture"] = capture
            write(answer)
        except Exception:
            write({"error": "source_discovery_failed"})


# ------------------------------------------------------------------ input


KEYSYMS = {
    "return": "Return", "enter": "Return", "tab": "Tab", "space": "space",
    "delete": "BackSpace", "backspace": "BackSpace", "forwarddelete": "Delete",
    "escape": "Escape", "esc": "Escape", "left": "Left", "right": "Right",
    "up": "Up", "down": "Down", "home": "Home", "end": "End",
    "pageup": "Prior", "pagedown": "Next", "capslock": "Caps_Lock",
    "shift": "Shift_L", "control": "Control_L", "option": "Alt_L",
    "command": "Control_L", "ctrl": "Control_L", "alt": "Alt_L",
    "super": "Super_L", "win": "Super_L", "rightshift": "Shift_R", "rightcontrol": "Control_R",
    "rightoption": "ISO_Level3_Shift", "rightcommand": "Control_R",
    "insert": "Insert", "printscreen": "Print", "menu": "Menu",
    "volumeup": "XF86AudioRaiseVolume", "volumedown": "XF86AudioLowerVolume",
    "mute": "XF86AudioMute", "playpause": "XF86AudioPlay",
    "next": "XF86AudioNext", "previous": "XF86AudioPrev",
    "-": "minus", "=": "equal", "[": "bracketleft", "]": "bracketright",
    "\\": "backslash", ";": "semicolon", "'": "apostrophe", ",": "comma",
    ".": "period", "/": "slash", "`": "grave",
}
# Accepted everywhere, no key here: the action is skipped, not the batch.
IGNORED_KEYS = {"brightnessup", "brightnessdown"}
for _n in range(1, 20):
    KEYSYMS[f"f{_n}"] = f"F{_n}"

# Wonder's modifier bits (ControlInputTranslator). Command maps to Control so
# the phone's Cmd+C / Cmd+V shortcuts mean copy/paste on Linux too.
MODIFIERS = [(1 << 1, "ctrl"), (1 << 3, "ctrl"), (1 << 2, "alt"), (1 << 0, "shift")]
BUTTONS = {"left": "1", "middle": "2", "right": "3"}
SCROLL_STEP = 40.0


def keysym(key):
    name = key.lower()
    if name in KEYSYMS:
        return KEYSYMS[name]
    if len(name) == 1 and name.isalnum():
        return name
    raise HelperError("invalid_input")


def key_args(key, phase, modifiers, held_keys=()):
    native = keysym(key)
    combo = []
    for bit, name in MODIFIERS:
        modifier = keysym(name)
        if (modifiers & bit and modifier != native and modifier not in combo
                and (phase == "up" or modifier not in held_keys)):
            combo.append(modifier)
    combo.append(native)
    verb = {"press": "key", "down": "keydown", "up": "keyup"}.get(phase)
    if verb is None:
        raise HelperError("invalid_input")
    # Clearing/restoring a held Alt would select an app between switcher cycles.
    clear = ["--clearmodifiers"] if phase == "press" and not held_keys else []
    return [verb, *clear, "+".join(combo)]


async def run_input(*args, data=None):
    process = await asyncio.create_subprocess_exec(
        *args, stdin=asyncio.subprocess.PIPE if data is not None else asyncio.subprocess.DEVNULL,
        stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.DEVNULL,
    )
    try:
        async with asyncio.timeout(3):
            if data is not None:
                process.stdin.write(data)
                await process.stdin.drain()
                process.stdin.close()
            await process.wait()
            if process.returncode != 0:
                raise HelperError("input_failed")
    except TimeoutError as error:
        raise HelperError("input_timed_out") from error
    finally:
        if process.returncode is None:
            process.kill()
        await process.wait()


async def input_frame_work(*args, allowed=None):
    def work():
        _INPUT_PERMISSION.check = allowed
        try:
            return window_input_frame(*args)
        finally:
            del _INPUT_PERMISSION.check
    task = asyncio.create_task(asyncio.to_thread(work))
    try:
        return await asyncio.shield(task)
    except asyncio.CancelledError:
        await asyncio.gather(task, return_exceptions=True)
        raise


async def xdotool(*args):
    return await run_input("xdotool", *args)


async def read_bounded(stream, limit):
    """Up to `limit + 1` bytes, or the whole stream if shorter. One `read`
    returns only what has arrived so far."""
    data = b""
    while len(data) <= limit:
        chunk = await stream.read(limit + 1 - len(data))
        if not chunk:
            break
        data += chunk
    return data


def clipboard_copy_result(returncode, output, stderr):
    """What a finished `xclip -o` means: text, None when the clipboard holds
    no text (still a delivered copy), or why the copy failed."""
    if returncode == 0:
        return output or None
    message = stderr.decode(errors="replace")
    if "not available" in message:  # "Error: target STRING not available"
        return None
    if "Can't open display" in message:
        raise HelperError("display_unavailable")
    raise HelperError("input_failed")


async def clipboard_read():
    try:
        process = await asyncio.create_subprocess_exec(
            "xclip", "-selection", "clipboard", "-o", stdin=asyncio.subprocess.DEVNULL,
            stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE,
        )
    except FileNotFoundError as error:
        raise HelperError("xclip_required") from error
    try:
        async with asyncio.timeout(CLIPBOARD_TIMEOUT):
            # Stdout first: an oversized copy stops here while xclip is still
            # writing. Its few stderr bytes fit the pipe meanwhile.
            output = await read_bounded(process.stdout, 65536)
            if len(output) > 65536:
                raise HelperError("clipboard_too_large")
            stderr = await read_bounded(process.stderr, 4096)
            await process.wait()
    except TimeoutError as error:
        raise HelperError("input_timed_out") from error
    finally:
        if process.returncode is None:
            process.kill()
        # A full stdout pipe can keep wait() pending even after xclip exits.
        # Discard remaining bytes without retaining an oversized clipboard.
        async def discard(stream):
            while await stream.read(4096):
                pass
        await asyncio.gather(discard(process.stdout), discard(process.stderr))
        await process.wait()
    return clipboard_copy_result(process.returncode, output, stderr)


async def clipboard_paste(text):
    if not isinstance(text, str) or len(text.encode()) > 65536:
        raise HelperError("invalid_input")
    # Keep selection ownership in this child until the application consumes it;
    # no detached xclip process survives the helper or a failed paste.
    process = await asyncio.create_subprocess_exec(
        "xclip", "-selection", "clipboard", "-i", "-quiet", "-loops", "1",
        stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.DEVNULL,
        stderr=asyncio.subprocess.DEVNULL,
    )
    try:
        async with asyncio.timeout(3):
            process.stdin.write(text.encode())
            await process.stdin.drain()
            process.stdin.close()
            # xclip must have read the selection before the application asks.
            await asyncio.sleep(.05)
            await xdotool("key", "--clearmodifiers", "ctrl+v")
            await process.wait()
    except TimeoutError as error:
        raise HelperError("clipboard_unavailable") from error
    finally:
        if process.returncode is None:
            process.kill()
        await process.wait()


_FOCUS_LOCAL = threading.local()


def text_input_focused(source):
    """AT-SPI focused/editable states only; do not read field text."""
    try:
        if not hasattr(_FOCUS_LOCAL, "api"):
            api = ctypes.CDLL(ctypes.util.find_library("atspi") or "libatspi.so.0")
            obj = ctypes.c_void_p
            api.atspi_init.restype = ctypes.c_int
            api.atspi_set_timeout.argtypes = (ctypes.c_int, ctypes.c_int)
            api.atspi_get_desktop.argtypes, api.atspi_get_desktop.restype = (ctypes.c_int,), obj
            api.atspi_accessible_get_child_count.argtypes = (obj, obj)
            api.atspi_accessible_get_child_at_index.argtypes, api.atspi_accessible_get_child_at_index.restype = (obj, ctypes.c_int, obj), obj
            api.atspi_accessible_get_process_id.argtypes, api.atspi_accessible_get_process_id.restype = (obj, obj), ctypes.c_uint
            api.atspi_accessible_clear_cache.argtypes = (obj,)
            api.atspi_accessible_get_state_set.argtypes, api.atspi_accessible_get_state_set.restype = (obj,), obj
            api.atspi_state_set_contains.argtypes = (obj, ctypes.c_int)
            api.g_object_unref.argtypes = (obj,)
            api.atspi_set_timeout(100, 100)
            _FOCUS_LOCAL.api = api if api.atspi_init() == 0 else None
        api = _FOCUS_LOCAL.api
        if api is None:
            return False
        x, display = x11_open()
        try:
            if source.get("kind") == "window":
                window_input_frame(source, None, False, False)
                window = int(source["id"].rpartition("#")[2])
            else:
                active = x11_property(x, display, x.XDefaultRootWindow(display), "_NET_ACTIVE_WINDOW")
                if not active:
                    return False
                window = active[0]
            process = x11_property(x, display, window, "_NET_WM_PID")
            if not process:
                return False
            pid = process[0]
        finally:
            x11_close(x, display)
        deadline, remaining = time.monotonic() + .15, [256]
        def visit(node, depth):
            if not node or depth > 12 or not remaining[0] or time.monotonic() >= deadline:
                return False
            remaining[0] -= 1
            api.atspi_accessible_clear_cache(node)
            states = api.atspi_accessible_get_state_set(node)
            if states:
                try:
                    if api.atspi_state_set_contains(states, 7) and api.atspi_state_set_contains(states, 12):
                        return True
                finally:
                    api.g_object_unref(states)
            for index in range(max(0, min(64, api.atspi_accessible_get_child_count(node, None)))):
                child = api.atspi_accessible_get_child_at_index(node, index, None)
                if child:
                    try:
                        if visit(child, depth + 1):
                            return True
                    finally:
                        api.g_object_unref(child)
            return False
        desktop = api.atspi_get_desktop(0)
        if not desktop:
            return False
        try:
            for index in range(max(0, min(64, api.atspi_accessible_get_child_count(desktop, None)))):
                if time.monotonic() >= deadline:
                    break
                app = api.atspi_accessible_get_child_at_index(desktop, index, None)
                if app:
                    try:
                        if api.atspi_accessible_get_process_id(app, None) == pid and visit(app, 0):
                            return True
                    finally:
                        api.g_object_unref(app)
        finally:
            api.g_object_unref(desktop)
    except (OSError, AttributeError, HelperError):
        return False
    return False


# ------------------------------------------------------------------ helper


class Helper:
    def __init__(self):
        self.session_id = None
        self.generation = 0
        self.state = "idle"
        self.reason = None
        self.quality = "standard"
        self.viewer_count = 0
        self.source = None
        self.geometry_revision = 1
        self.frame_sequence = 0
        self.last_frame = None
        self.last_metadata_emit = 0.0
        self.capturing = asyncio.Event()
        self.pc = None
        self.peer_revision = None
        self.lease = None
        self.consents = {}
        self.lease_expires = 0
        self.payloads = {}
        self.lock = asyncio.Lock()
        self.input_lock = asyncio.Lock()
        self.running_input = None
        self.input_deadline = None
        self.execution_generation = 0
        self.last_focus = None
        self.peer_task = None
        self.last_input_sequence = 0
        self.held_buttons = set()
        self.held_keys = set()

    # ---- status

    def configuration(self):
        width, height = (self.source["width"], self.source["height"]) if self.source else (1280, 720)
        width, height = encoded_size(width, height, self.quality)
        return {"width": width, "height": height,
                "framesPerSecond": QUALITY_FPS.get(self.quality, 10),
                "audioEnabled": False, "queueDepth": 2}

    def status(self, message=None):
        return {
            "state": self.state, "sessionID": self.session_id, "generation": self.generation,
            "helperInstanceID": HELPER_INSTANCE,
            "sourceID": self.source["id"] if self.source else None,
            "viewerCount": self.viewer_count, "queuedFrameCount": 0, "droppedFrameCount": 0,
            "frameSequence": self.frame_sequence, "geometryRevision": self.geometry_revision,
            "pickerAvailable": False, "screenRecordingAuthorized": True, "audioEnabled": False,
            "configuration": self.configuration(), "lastFrame": self.last_frame,
            "message": message, "reason": self.reason,
        }

    def emit_capture(self, event, source=None, message=None):
        payload = {"event": event, "status": self.status(message)}
        if source is not None:
            payload["source"] = source
        if self.last_frame is not None and event == "capture.frameMetadata":
            payload["metadata"] = self.last_frame
        write(payload)

    def result(self, action, accepted=True, error_code=None, message=None):
        result = {"accepted": accepted, "action": action, "status": self.status(message)}
        if error_code:
            result["errorCode"] = error_code
        return result

    def pick_failed(self, error_code, message=None):
        # The chosen source is gone before sharing began. Fail the capture so
        # the host ends the view instead of leaving it preparing.
        self.state, self.reason = "failed", error_code
        return self.result("pick", False, error_code, message)

    def require_identity(self, params):
        if params.get("sessionID") != self.session_id or params.get("generation") != self.generation:
            raise HelperError("invalid_capture_session")

    # ---- frames

    async def frame_captured(self, width, height):
        if self.source and (self.source["width"], self.source["height"]) != (width, height):
            async with self.lock:
                await self.revoke("source_geometry_changed")
                self.geometry_revision += 1
                self.source["width"], self.source["height"] = width, height
                self.source["contentRect"] = {"x": 0, "y": 0, "width": width, "height": height}
        if self.state == "starting":
            self.state = "capturing"
        self.frame_sequence += 1
        self.last_frame = {
            "captureTimestamp": time.time(), "geometryRevision": self.geometry_revision,
            "sourceWidth": width, "sourceHeight": height, "scale": 1.0,
            "contentRect": {"x": 0, "y": 0, "width": width, "height": height},
            "cropRect": {"x": 0, "y": 0, "width": width, "height": height},
            "frameSequence": self.frame_sequence,
            "sessionID": self.session_id, "generation": self.generation,
        }
        now = time.monotonic()
        if now - self.last_metadata_emit >= 1.0:
            self.last_metadata_emit = now
            self.emit_capture("capture.frameMetadata")

    # ---- WebRTC

    def signal(self, event, **fields):
        write({"event": event, "sessionID": self.session_id, "generation": self.generation,
               "peerRevision": self.peer_revision, **fields})

    async def close_peer(self):
        pc, self.pc = self.pc, None
        if pc is not None:
            await pc.close()

    async def start_peer(self):
        await self.close_peer()
        pc = RTCPeerConnection(RTCConfiguration(iceServers=[]))
        self.pc = pc
        self.peer_revision = str(uuid.uuid4()).upper()
        revision = self.peer_revision
        pc.addTrack(ScreenTrack(self))

        @pc.on("connectionstatechange")
        async def on_state():
            if self.pc is not pc or self.peer_revision != revision:
                return
            mapped = {"connecting": "checking", "connected": "connected",
                      "disconnected": "disconnected", "failed": "failed", "closed": "closed"}.get(pc.connectionState)
            log("peer", pc.connectionState)
            if mapped:
                self.signal("signal.peerState", state=mapped)

        self.signal("signal.peerReady")
        offer = await pc.createOffer()
        await pc.setLocalDescription(offer)  # gathers host candidates
        if self.pc is not pc:
            return
        sdp = pc.localDescription.sdp
        self.signal("signal.offer", type="offer", sdp=sdp)
        for index, line in enumerate(l for l in sdp.split("\r\n") if l.startswith("a=candidate:")):
            self.signal("signal.candidate", candidate=line[2:], sdpMid="0", sdpMLineIndex=0)

    # ---- control

    def control_identity(self, params):
        self.require_identity(params)
        if not self.source or params.get("sourceID") != self.source["id"] or params.get("geometryRevision") != self.geometry_revision:
            raise HelperError("invalid_capture_session")
        fields = ("leaseID", "requestID", "sessionID", "generation", "geometryRevision", "sourceID")
        if any(not isinstance(params.get(key), str) or not 1 <= len(params[key]) <= 160
               for key in ("leaseID", "requestID", "sessionID", "sourceID")):
            raise HelperError("invalid_input")
        return {key: params[key] for key in fields}

    async def cancel_input(self):
        self.execution_generation += 1
        task = self.running_input
        if task is not None and not task.done() and task is not asyncio.current_task():
            task.cancel()
            await asyncio.gather(task, return_exceptions=True)

    async def revoke(self, reason):
        await self.cancel_input()
        lease, self.lease = self.lease, None
        self.payloads.clear()
        await self.release_held()
        if lease:
            write({"event": "control.revoked", **lease, "reason": reason})

    async def expire(self):
        if self.lease is not None and self.lease_expires <= time.monotonic():
            await self.revoke("lease_expired")

    async def watch_lease(self):
        from concurrent.futures import ThreadPoolExecutor
        focus_worker = ThreadPoolExecutor(max_workers=1, thread_name_prefix="computer-focus")
        try:
            while True:
                await asyncio.sleep(.25)
                async with self.lock:
                    await self.expire()
                    identity, source, generation = self.lease, self.source, self.execution_generation
                if identity and source:
                    focused = await asyncio.get_running_loop().run_in_executor(focus_worker, text_input_focused, source)
                    async with self.lock:
                        if self.lease == identity and self.source is source and self.execution_generation == generation and self.last_focus != (identity, generation, focused):
                            write({"event": "control.textFocus", **identity, "textInput": focused})
                            self.last_focus = (identity, generation, focused)
                else:
                    self.last_focus = None
        finally:
            focus_worker.shutdown(wait=False, cancel_futures=True)

    async def release_held(self):
        # `mouseup` takes one button; attempt every release even if one fails.
        # Each item stays tracked until its release succeeds, so a release cut
        # short by a timeout or cancellation is retried by the next revoke.
        releases = [("mouseup", {button}, self.held_buttons) for button in sorted(self.held_buttons)]
        if self.held_keys:
            releases.append(("keyup", set(self.held_keys), self.held_keys))
        for verb, items, held in releases:
            try:
                await xdotool(verb, *sorted(items))
            except HelperError as error:
                log("release failed:", verb, error.code)
                continue
            held.difference_update(items)

    async def deliver(self, action, result):
        generation, deadline = self.execution_generation, self.input_deadline
        def allowed():
            if generation != self.execution_generation:
                raise HelperError("lease_not_active")
            if deadline is not None and time.monotonic() >= deadline:
                raise HelperError("input_timed_out")
        kind = action.get("type")
        width = self.source["width"] if self.source else 1280
        height = self.source["height"] if self.source else 720
        if kind == "pointer":
            x, y = float(action["x"]), float(action["y"])
            if not (0 <= x <= 1 and 0 <= y <= 1):
                raise HelperError("invalid_input")
            point = map_window_point(x, y, 0, 0, width, height)
            button = BUTTONS.get(action.get("button") or "left")
            if button is None:
                raise HelperError("invalid_input")
            held_up = action.get("phase") == "up" and button in self.held_buttons
            try:
                if self.source and self.source.get("kind") == "window":
                    point = await input_frame_work(self.source, (x, y), False, not self.held_buttons, allowed=allowed)
                if held_up:
                    await xdotool("mousemove", str(point[0]), str(point[1]))
            finally:
                if held_up:
                    await xdotool("mouseup", button)
                    self.held_buttons.discard(button)
            if held_up:
                return
            await xdotool("mousemove", str(point[0]), str(point[1]))
            button = BUTTONS.get(action.get("button") or "left")
            if button is None:
                raise HelperError("invalid_input")
            phase = action.get("phase")
            if phase == "down":
                self.held_buttons.add(button)
                await xdotool("mousedown", button)
            elif phase == "up":
                await xdotool("mouseup", button)
                self.held_buttons.discard(button)
            elif phase != "move":
                raise HelperError("invalid_input")
        elif kind == "scroll":
            if self.source and self.source.get("kind") == "window":
                await input_frame_work(self.source, None, True, allowed=allowed)
            for delta, negative, positive in ((float(action["deltaY"]), "4", "5"),
                                              (float(action["deltaX"]), "6", "7")):
                if not math.isfinite(delta) or abs(delta) > 4096:
                    raise HelperError("invalid_input")
                clicks = round(abs(delta) / SCROLL_STEP) or (1 if delta else 0)
                if clicks:
                    await xdotool("click", "--repeat", str(min(clicks, 20)),
                                  positive if delta > 0 else negative)
        elif kind == "key":
            if str(action.get("key", "")).lower() in IGNORED_KEYS:
                return
            if self.source and self.source.get("kind") == "window" and action.get("phase") != "up":
                await input_frame_work(self.source, allowed=allowed)
            args = key_args(action["key"], action["phase"], int(action.get("modifiers", 0)), self.held_keys)
            if action["phase"] == "down":
                self.held_keys.update(args[-1].split("+"))
            await xdotool(*args)
            if action["phase"] == "down":
                self.held_keys.update(args[-1].split("+"))
            elif action["phase"] == "up":
                self.held_keys.difference_update(args[-1].split("+"))
        elif kind == "text":
            if self.source and self.source.get("kind") == "window":
                await input_frame_work(self.source, allowed=allowed)
            text = action.get("text") or ""
            if len(text) > 4096:
                raise HelperError("invalid_input")
            if text:
                await xdotool("type", "--clearmodifiers", "--delay", "0", "--", text)
        elif kind == "clipboard":
            if action.get("operation") == "copyToPhone":
                text = await clipboard_read()
                if text is None:  # Nothing to copy is still delivered input; control stays.
                    result["clipboardReason"] = "clipboard_no_text"
                else:
                    result["clipboardText"] = text.decode(errors="replace")
            elif action.get("operation") == "pasteFromPhone":
                await clipboard_paste(action.get("text") or "")
            else:
                raise HelperError("invalid_input")
        elif kind == "releaseAll":
            await self.release_held()
        else:
            raise HelperError("invalid_input")

    # ---- requests

    async def input(self, params):
        queued_generation = self.execution_generation
        async with self.input_lock:
            async with self.lock:
                if queued_generation != self.execution_generation:
                    raise HelperError("lease_not_active")
                generation = self.execution_generation
                return await self.input_locked(params, generation)

    async def input_locked(self, params, generation):
        identity = self.control_identity(params)
        if self.lease != identity:
            return {"accepted": False, "reason": "lease_not_active"}
        if self.state != "capturing":
            return {"accepted": False, "reason": "capture_not_live"}
        sequence, actions = params.get("sequence"), params.get("actions")
        if type(sequence) is not int or sequence <= 0 or not isinstance(actions, list) or not 1 <= len(actions) <= 32:
            raise HelperError("invalid_input")
        payload = hashlib.sha256(json.dumps(actions, sort_keys=True, separators=(",", ":")).encode()).digest()
        if sequence in self.payloads:
            matching = self.payloads[sequence] == payload
            return {"accepted": matching, "duplicate": matching,
                    "reason": None if matching else "sequence_payload_conflict"}
        if sequence != self.last_input_sequence + 1:
            return {"accepted": False, "reason": "input_sequence_must_increase_by_one"}
        result = {"accepted": True, "duplicate": False}
        async def deliver_batch():
            self.input_deadline = time.monotonic() + INPUT_TIMEOUT
            async with asyncio.timeout(INPUT_TIMEOUT):
                for action in actions:
                    if generation != self.execution_generation or self.lease != identity:
                        raise HelperError("lease_not_active")
                    if self.lease_expires <= time.monotonic():
                        raise HelperError("lease_expired")
                    await self.deliver(action, result)
        task = asyncio.create_task(deliver_batch())
        self.running_input = task
        # Admission is complete; the batch has a separate serialization lock.
        self.lock.release()
        try:
            await task
        except (Exception, asyncio.CancelledError) as error:
            await self.lock.acquire()
            external_cancel = isinstance(error, asyncio.CancelledError) and asyncio.current_task().cancelling()
            if generation == self.execution_generation:
                reason = "request_timed_out" if external_cancel else "input_timed_out" if isinstance(error, TimeoutError) else "input_delivery_failed"
                await self.revoke(reason)
            if external_cancel:
                raise
            if isinstance(error, TimeoutError):
                raise HelperError("input_timed_out") from None
            if isinstance(error, asyncio.CancelledError):
                raise HelperError("lease_not_active") from None
            raise
        else:
            await self.lock.acquire()
        if generation != self.execution_generation:
            raise HelperError("lease_not_active")
        self.last_input_sequence = sequence
        self.payloads[sequence] = payload
        while len(self.payloads) > 64:
            del self.payloads[next(iter(self.payloads))]
        return result

    async def handle(self, method, params):
        if method == "control.input":
            return await self.input(params)
        async with self.lock:
            await self.expire()
            return await self.handle_locked(method, params)

    async def handle_locked(self, method, params):
        if method == "capabilities":
            return {"protocolVersion": 1,
                    "capture": {**capture_capability(), "liveQualityChange": True},
                    "control": {"available": shutil.which("xdotool") is not None and bool(os.environ.get("DISPLAY")),
                                "reason": "wayland_input_unavailable" if os.environ.get("WAYLAND_DISPLAY") and not os.environ.get("DISPLAY") else "xdotool_required" if shutil.which("xdotool") is None else "x11_display_required" if not os.environ.get("DISPLAY") else None,
                                "provider": "xtest-x11-v1", "requiresAccessibility": False}}
        if method in ("capture.prepare", "prepare"):
            quality = params.get("videoQuality") or "standard"
            if quality not in QUALITY_FPS:
                raise HelperError("invalid_input")
            await self.revoke("capture_replaced")
            self.consents.clear()
            await self.stop_peer()
            self.capturing.clear()
            self.session_id = params.get("sessionID") or str(uuid.uuid4())
            self.generation = int(params.get("generation") or 0)
            self.viewer_count = int(params.get("viewerCount") or 1)
            self.quality = quality
            self.source, self.last_frame, self.reason = None, None, None
            self.frame_sequence = 0
            self.state = "awaitingSource" if self.viewer_count > 0 else "stopped"
            self.emit_capture("capture.prepared")
            self.peer_task = asyncio.create_task(self.run_peer())
            return self.result("prepare", accepted=self.viewer_count > 0)
        if method == "capture.rebind":
            self.require_identity(params)
            if (not self.source or params.get("sourceID") != self.source["id"]
                    or params.get("geometryRevision") != self.geometry_revision):
                raise HelperError("invalid_capture_session")
            if self.lease is not None and self.control_identity(params) != self.lease:
                raise HelperError("lease_not_active")
            if self.lease is None and params.get("leaseID") is not None:
                raise HelperError("lease_not_active")
            requested = params.get("nextSourceID")
            if isinstance(requested, str) and requested.startswith("app:"):
                found = next((s for s in list_windows([], True) if s["id"] == requested), None)
                if found is None:
                    return self.result("rebind", False, "source_not_found", "That window is no longer available. Choose another window.")
                source = {**found, "title": found["windowTitle"],
                          "contentRect": {"x": 0, "y": 0, "width": found["width"], "height": found["height"]}}
            elif requested in (None, "display:x11"):
                width, height = display_size()
                source = {"id": "display:x11", "kind": "display", "title": os.environ.get("DISPLAY", "Display"),
                          "width": width, "height": height, "scale": 1.0,
                          "contentRect": {"x": 0, "y": 0, "width": width, "height": height}}
            else:
                return self.result("rebind", False, "source_not_found", "That window is no longer available. Choose another window.")
            await self.cancel_input()
            await self.release_held()
            self.source = source
            self.geometry_revision += 1
            self.last_frame = None
            if self.lease is not None:
                self.lease = dict(self.lease, sourceID=source["id"], geometryRevision=self.geometry_revision)
                self.lease_expires = time.monotonic() + 10
                self.consents[self.lease["requestID"]] = self.lease
            return {**self.result("rebind"), "source": source}
        if method in ("capture.pick", "pick"):
            if self.session_id is None:
                return self.result("pick", False, "capture_not_prepared")
            if params.get("sessionID") is not None:
                self.require_identity(params)
            requested = params.get("sourceID")
            pending = self.result("pick", message="Selecting the source")
            if isinstance(requested, str) and requested.startswith("app:"):
                # Re-resolve the full identity. Closing a window cannot select
                # another window of the same app or fall back to the screen.
                source = next((source for source in list_windows([], True) if source["id"] == requested), None)
                if source is None:
                    return self.pick_failed("source_not_found")
                self.source = {**source, "title": source["windowTitle"]}
                self.source["contentRect"] = {"x": 0, "y": 0, "width": source["width"], "height": source["height"]}
            elif requested in (None, "display:x11"):
                width, height = display_size()
                self.source = {
                    "id": "display:x11", "kind": "display", "title": os.environ.get("DISPLAY", "Display"),
                    "width": width, "height": height, "scale": 1.0,
                    "contentRect": {"x": 0, "y": 0, "width": width, "height": height},
                }
            else:
                return self.pick_failed("source_not_found")
            self.state = "ready"
            asyncio.get_running_loop().call_soon(
                lambda: self.emit_capture("capture.sourceSelected", source=self.source))
            return pending
        if method in ("capture.start", "startCapture"):
            self.require_identity(params)
            if self.source is None:
                return self.result("start", False, "source_not_selected")
            if self.state in ("ready", "paused"):
                self.state = "starting"
                self.capturing.set()
            return self.result("start")
        if method in ("capture.stop", "stopCapture"):
            self.require_identity(params)
            self.capturing.clear()
            self.state, self.reason = "stopped", params.get("reason") or "stopped_by_request"
            await self.revoke("capture_stopped")
            await self.stop_peer()
            return self.result("stop")
        if method == "capture.setQuality":
            self.require_identity(params)
            if params.get("videoQuality") not in QUALITY_FPS:
                raise HelperError("invalid_input")
            self.quality = params["videoQuality"]
            return self.result("setQuality")
        if method in ("capture.status", "status"):
            return self.status()
        if method == "signal.answer":
            if params.get("peerRevision") != self.peer_revision or self.pc is None:
                raise HelperError("invalid_input")
            await self.pc.setRemoteDescription(RTCSessionDescription(sdp=params["sdp"], type="answer"))
            self.signal("signal.answerApplied")
            return {"accepted": True}
        if method == "signal.candidate":
            if params.get("peerRevision") != self.peer_revision or self.pc is None:
                raise HelperError("invalid_input")
            text = params.get("candidate") or ""
            if text.startswith("candidate:") and ".local " not in text:
                candidate = candidate_from_sdp(text.split(":", 1)[1])
                candidate.sdpMid = params.get("sdpMid")
                candidate.sdpMLineIndex = params.get("sdpMLineIndex")
                await self.pc.addIceCandidate(candidate)
            return {"accepted": True}
        if method == "signal.close":
            if params.get("peerRevision") == self.peer_revision:
                await self.close_peer()
            return {"closed": True, "sessionID": self.session_id, "generation": self.generation}
        if method == "control.consent":
            identity = self.control_identity(params)
            previous = self.consents.get(identity["requestID"])
            if previous is not None and previous != identity:
                raise HelperError("request_identity_conflict")
            self.consents[identity["requestID"]] = identity
            while len(self.consents) > 64:
                del self.consents[next(iter(self.consents))]
            return {"granted": True, "decision": "allowOnce", "replayed": previous is not None}
        if method == "control.activate":
            identity = self.control_identity(params)
            if self.lease is not None:
                return {"activated": self.lease == identity, "replayed": self.lease == identity,
                        "reason": None if self.lease == identity else "control_busy"}
            if self.consents.get(identity["requestID"]) != identity:
                return {"activated": False, "reason": "consent_required"}
            self.lease = identity
            self.lease_expires = time.monotonic() + 10
            self.payloads.clear()
            self.last_input_sequence = 0
            return {"activated": True, "replayed": False}
        if method == "control.heartbeat":
            identity = self.control_identity(params)
            renewed = self.lease == identity
            if renewed:
                self.lease_expires = time.monotonic() + 10
            return {"renewed": renewed}
        if method in ("control.release", "control.releaseAll", "control.revoke"):
            released = self.lease is not None
            if method == "control.release":
                released = self.lease == self.control_identity(params)
            if released:
                await self.revoke("released")
            return {"released": released}
        raise HelperError("unsupported_action")

    async def stop_peer(self):
        task, self.peer_task = self.peer_task, None
        if task is not None:
            task.cancel()
            await asyncio.gather(task, return_exceptions=True)
        await self.close_peer()
        await asyncio.get_running_loop().run_in_executor(None, release_window_capture)

    async def run_peer(self):
        try:
            await self.start_peer()
        except Exception as error:  # noqa: BLE001 - reported to the daemon
            log("peer setup failed:", repr(error))
            if self.peer_revision:
                self.signal("signal.failed", reason="offer_failed")


async def respond(helper, request_id, method, params):
    """The reply line for one request. A request that outlives its deadline
    releases control and held input and says so."""
    try:
        try:
            async with asyncio.timeout(REQUEST_TIMEOUT):
                result = await helper.handle(method, params)
        except TimeoutError:
            async with helper.lock:
                await helper.revoke("request_timed_out")
            raise HelperError("request_timed_out") from None
        if isinstance(result, dict) and "action" not in result:
            result["action"] = method
        return {"id": request_id, "result": result}
    except HelperError as error:
        return {"id": request_id, "error": {"code": error.code}}
    except Exception as error:
        log("request failed:", type(error).__name__)
        return {"id": request_id, "error": {"code": "computer_use_failed"}}


async def main():
    global _OUTPUT
    helper = Helper()
    loop = asyncio.get_running_loop()
    _OUTPUT, _ = await loop.connect_write_pipe(asyncio.Protocol, sys.stdout)
    reader = asyncio.StreamReader(limit=MAX_LINE_BYTES)
    await loop.connect_read_pipe(lambda: asyncio.StreamReaderProtocol(reader), sys.stdin)
    watchdog = asyncio.create_task(helper.watch_lease())
    pending_input = set()

    async def input_reply(request_id, params):
        write(await respond(helper, request_id, "control.input", params))
    try:
        while True:
            try:
                raw = await reader.readline()
            except ValueError:
                break  # A truncated request is not safe to interpret or recover.
            if not raw or len(raw) > MAX_LINE_BYTES:
                break
            request_id = None
            try:
                request = json.loads(raw)
                request_id = request.get("id")
                method = request["method"]
                params = request.get("params") or {}
                if not HANDSHAKE or params.get("handshake") != HANDSHAKE:
                    raise HelperError("handshake_required")
                if method == "stop":
                    write({"id": request_id, "result": {"stopped": True}})
                    break
            except HelperError as error:
                write({"id": request_id, "error": {"code": error.code}})
                continue
            except Exception as error:
                log("request failed:", type(error).__name__)
                write({"id": request_id, "error": {"code": "computer_use_failed"}})
                continue
            if method == "control.input":
                if len(pending_input) >= 8:
                    write({"id": request_id, "result": {"accepted": False, "reason": "control_busy"}})
                else:
                    task = asyncio.create_task(input_reply(request_id, params))
                    pending_input.add(task)
                    task.add_done_callback(pending_input.discard)
            else:
                write(await respond(helper, request_id, method, params))
    finally:
        watchdog.cancel()
        await asyncio.gather(watchdog, return_exceptions=True)
        helper.capturing.clear()
        await helper.revoke("helper_stopped")
        for task in pending_input:
            task.cancel()
        await asyncio.gather(*pending_input, return_exceptions=True)
        await helper.stop_peer()
        _OUTPUT.close()


if __name__ == "__main__":
    if "--discover-sources" in sys.argv:
        discover_sources()
    else:
        asyncio.run(main())

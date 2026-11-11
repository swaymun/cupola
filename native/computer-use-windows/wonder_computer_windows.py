#!/usr/bin/env python3
"""EXPERIMENTAL: Cupola's computer helper for a Windows host.

wonderd runs this as WONDER_COMPUTER_USE_BIN (through a small launcher) and
speaks the JSON-lines protocol it uses with the macOS and Linux helpers:
capture lifecycle, WebRTC signaling relay and the control lease. It must run
in the signed-in user's interactive session (`cupola` started at logon),
never as a service: session 0 has no desktop to capture or drive.

* Sources: a display (`display:N`) or one top-level window
  (`app:<exe>#<HWND>`), listed by the `--discover-sources` worker.
* Capture: Windows.Graphics.Capture through the `windows-capture` package
  when it imports, else GDI through `mss`. A window is captured as its
  on-screen rectangle; WGC follows it while it moves or is covered.
* Video: an `aiortc` peer connection publishes VP8 using host candidates
  (Tailscale gives the tailnet candidate).
* Input: SendInput into the interactive desktop; provider `sendinput-win32-v1`.
  Windows refuses input to the secure desktop (UAC prompts, Ctrl-Alt-Del,
  the lock screen) and to windows of elevated processes when this helper is
  not elevated (UIPI); those inputs fail with `input_blocked`. For a window
  source, keys, text and paste go only to that window once it has focus
  (else `window_not_focused`), and clicks and wheel only where it is on top
  (else `window_covered`); nothing is sent to another application.

Python: aiortc depends on cryptography and google-crc32c, which publish no
win_arm64 wheels, so on Windows on ARM this runs on x64 Python under the
built-in emulation.
"""

import asyncio
import base64
from collections import OrderedDict
import struct
import zlib
import ctypes
import ctypes.wintypes as wt
from fractions import Fraction
import hashlib
import json
import math
import os
import sys
import threading
import time
import uuid

MAX_LINE_BYTES = 128 * 1024
HANDSHAKE = os.environ.get("WONDER_COMPUTER_USE_HANDSHAKE", "")
HELPER_INSTANCE = str(uuid.uuid4())
# Take control needs the native helper, which asks on screen and shows Stop.
LOCAL_APPROVAL_UNAVAILABLE = "local_approval_unavailable"
QUALITY_FPS = {"standard": 10, "medium": 12, "auto": 12, "high": 15}
QUALITY_BOX = {"standard": (1280, 720), "medium": (1920, 1080), "auto": (2560, 1440), "high": (2560, 1440)}


def encoded_size(width, height, quality):
    max_width, max_height = QUALITY_BOX[quality]
    factor = min(max_width / width, max_height / height, 1)
    return max(2, int(width * factor) & ~1), max(2, int(height * factor) & ~1)


INPUT_TIMEOUT, REQUEST_TIMEOUT = 8, 10
_OUTPUT = None


def log(*parts):
    print("wonder-computer-windows:", *parts, file=sys.stderr, flush=True)


class HelperError(Exception):
    def __init__(self, code):
        super().__init__(code)
        self.code = code


# ------------------------------------------------------------------ Win32

user32 = ctypes.WinDLL("user32", use_last_error=True)
kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
try:
    # Physical pixels everywhere: capture, window rectangles and input agree.
    user32.SetProcessDpiAwarenessContext(ctypes.c_void_p(-4))  # PER_MONITOR_AWARE_V2
except (AttributeError, OSError):
    pass

ULONG_PTR = ctypes.c_size_t
INPUT_MOUSE, INPUT_KEYBOARD = 0, 1
MOUSEEVENTF_MOVE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_VIRTUALDESK = 0x1, 0x8000, 0x4000
MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP = 0x2, 0x4
MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP = 0x8, 0x10
MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP = 0x20, 0x40
MOUSEEVENTF_WHEEL, MOUSEEVENTF_HWHEEL = 0x800, 0x1000
KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE = 0x1, 0x2, 0x4
SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN = 76, 77, 78, 79
WHEEL_DELTA = 120


class MOUSEINPUT(ctypes.Structure):
    _fields_ = [("dx", wt.LONG), ("dy", wt.LONG), ("mouseData", wt.DWORD),
                ("dwFlags", wt.DWORD), ("time", wt.DWORD), ("dwExtraInfo", ULONG_PTR)]


class KEYBDINPUT(ctypes.Structure):
    _fields_ = [("wVk", wt.WORD), ("wScan", wt.WORD), ("dwFlags", wt.DWORD),
                ("time", wt.DWORD), ("dwExtraInfo", ULONG_PTR)]


class HARDWAREINPUT(ctypes.Structure):
    _fields_ = [("uMsg", wt.DWORD), ("wParamL", wt.WORD), ("wParamH", wt.WORD)]


class _INPUTUNION(ctypes.Union):
    _fields_ = [("mi", MOUSEINPUT), ("ki", KEYBDINPUT), ("hi", HARDWAREINPUT)]


class INPUT(ctypes.Structure):
    _anonymous_ = ("u",)
    _fields_ = [("type", wt.DWORD), ("u", _INPUTUNION)]


user32.SendInput.argtypes = (wt.UINT, ctypes.POINTER(INPUT), ctypes.c_int)
user32.SendInput.restype = wt.UINT
user32.GetWindowRect.argtypes = (wt.HWND, ctypes.POINTER(wt.RECT))
user32.IsWindow.argtypes = (wt.HWND,)
user32.IsWindowVisible.argtypes = (wt.HWND,)
user32.GetWindowTextW.argtypes = (wt.HWND, wt.LPWSTR, ctypes.c_int)
user32.GetWindowTextLengthW.argtypes = (wt.HWND,)
user32.GetClassNameW.argtypes = (wt.HWND, wt.LPWSTR, ctypes.c_int)
user32.GetWindowThreadProcessId.argtypes = (wt.HWND, ctypes.POINTER(wt.DWORD))
user32.GetAncestor.argtypes = (wt.HWND, wt.UINT)
user32.GetAncestor.restype = wt.HWND
user32.GetWindowLongW.argtypes = (wt.HWND, ctypes.c_int)
user32.GetWindowLongW.restype = wt.LONG
user32.GetLayeredWindowAttributes.argtypes = (wt.HWND, ctypes.POINTER(wt.DWORD), ctypes.POINTER(wt.BYTE), ctypes.POINTER(wt.DWORD))
user32.GetLayeredWindowAttributes.restype = wt.BOOL
user32.SetForegroundWindow.argtypes = (wt.HWND,)
user32.GetForegroundWindow.restype = wt.HWND
user32.OpenInputDesktop.restype = wt.HANDLE
user32.CloseDesktop.argtypes = (wt.HANDLE,)
user32.GetSystemMetrics.argtypes = (ctypes.c_int,)
kernel32.OpenProcess.restype = wt.HANDLE
kernel32.QueryFullProcessImageNameW.argtypes = (wt.HANDLE, wt.DWORD, wt.LPWSTR, ctypes.POINTER(wt.DWORD))
kernel32.CloseHandle.argtypes = (wt.HANDLE,)
DWMWA_EXTENDED_FRAME_BOUNDS, DWMWA_CLOAKED = 9, 14
try:
    dwmapi = ctypes.WinDLL("dwmapi")
except OSError:
    dwmapi = None


def send_inputs(inputs):
    if not inputs:
        return
    array = (INPUT * len(inputs))(*inputs)
    sent = user32.SendInput(len(inputs), array, ctypes.sizeof(INPUT))
    if sent != len(inputs):
        # UIPI (an elevated target) or the secure desktop.
        raise HelperError("input_blocked")


user32.GetUserObjectInformationW.argtypes = (wt.HANDLE, ctypes.c_int, ctypes.c_void_p, wt.DWORD,
                                             ctypes.POINTER(wt.DWORD))


def input_desktop_name():
    """Name of the desktop receiving input: "Default" for the user's desktop,
    "Winlogon" while UAC consent, Ctrl-Alt-Del or the lock screen is up.
    None when it cannot be opened (also a secure desktop)."""
    handle = user32.OpenInputDesktop(0, False, 0)
    if not handle:
        return None
    try:
        buffer = ctypes.create_unicode_buffer(256)
        needed = wt.DWORD()
        if not user32.GetUserObjectInformationW(handle, 2, buffer, ctypes.sizeof(buffer), ctypes.byref(needed)):
            return None  # UOI_NAME
        return buffer.value
    finally:
        user32.CloseDesktop(handle)


user32.GetCursorPos.argtypes = (ctypes.POINTER(wt.POINT),)


def secure_desktop_active():
    """True while UAC consent, Ctrl-Alt-Del or the lock screen owns input.

    SendInput then reports success but Windows drops the events, and the
    input desktop still reads "Default" from a non-elevated process. What does
    change (measured on Windows 11 26H2 with a UAC prompt up): GetCursorPos
    fails with ERROR_ACCESS_DENIED and there is no foreground window."""
    point = wt.POINT()
    if not user32.GetCursorPos(ctypes.byref(point)) and ctypes.get_last_error() == 5:
        return True
    return input_desktop_name() != "Default"


def window_rect(hwnd):
    rect = wt.RECT()
    if dwmapi is not None and dwmapi.DwmGetWindowAttribute(
            wt.HWND(hwnd), DWMWA_EXTENDED_FRAME_BOUNDS, ctypes.byref(rect), ctypes.sizeof(rect)) == 0:
        return rect.left, rect.top, rect.right - rect.left, rect.bottom - rect.top
    if not user32.GetWindowRect(hwnd, ctypes.byref(rect)):
        raise HelperError("source_not_found")
    return rect.left, rect.top, rect.right - rect.left, rect.bottom - rect.top


# Input targeting for a window source. SendInput goes to whatever has focus
# (keys, text) or whatever is under the cursor (pointer, wheel), not to the
# streamed window, so each delivery first proves the target will receive it.
GA_ROOT = 2
user32.WindowFromPoint.argtypes = (wt.POINT,)
user32.WindowFromPoint.restype = wt.HWND
user32.IsIconic.argtypes = (wt.HWND,)
user32.ShowWindow.argtypes = (wt.HWND, ctypes.c_int)
user32.BringWindowToTop.argtypes = (wt.HWND,)
user32.AttachThreadInput.argtypes = (wt.DWORD, wt.DWORD, wt.BOOL)
kernel32.GetCurrentThreadId.restype = wt.DWORD


def root_window(hwnd):
    return user32.GetAncestor(hwnd, GA_ROOT) if hwnd else None


def foreground_root():
    """The top-level window that receives keyboard input now."""
    return root_window(user32.GetForegroundWindow())


def root_window_at(x, y):
    """The top-level window a click at screen point (x, y) would reach."""
    return root_window(user32.WindowFromPoint(wt.POINT(x, y)))


def cursor_position():
    point = wt.POINT()
    if not user32.GetCursorPos(ctypes.byref(point)):
        return None
    return point.x, point.y


def activate_window(hwnd):
    """Asks Windows to give `hwnd` keyboard focus. Windows may refuse a
    background process (foreground lock); attaching to the foreground
    thread's input queue is the documented way to be allowed. The caller
    still checks the result."""
    if user32.IsIconic(hwnd):
        user32.ShowWindow(hwnd, 9)  # SW_RESTORE
    user32.BringWindowToTop(hwnd)
    if user32.SetForegroundWindow(hwnd) and foreground_root() == hwnd:
        return
    current = user32.GetForegroundWindow()
    ours = kernel32.GetCurrentThreadId()
    theirs = user32.GetWindowThreadProcessId(current, None) if current else 0
    attached = bool(theirs) and theirs != ours and user32.AttachThreadInput(ours, theirs, True)
    try:
        user32.BringWindowToTop(hwnd)
        user32.SetForegroundWindow(hwnd)
    finally:
        if attached:
            user32.AttachThreadInput(ours, theirs, False)


def window_title(hwnd):
    """The window's title without control characters, at most 160 UTF-8 bytes."""
    length = user32.GetWindowTextLengthW(hwnd)
    buffer = ctypes.create_unicode_buffer(length + 1)
    user32.GetWindowTextW(hwnd, buffer, length + 1)
    text = "".join(c for c in buffer.value if c.isprintable())
    return text.encode()[:160].decode(errors="ignore")


def window_exe(hwnd):
    pid = wt.DWORD()
    user32.GetWindowThreadProcessId(hwnd, ctypes.byref(pid))
    process = kernel32.OpenProcess(0x1000, False, pid.value)  # QUERY_LIMITED_INFORMATION
    if not process:
        return None
    try:
        size = wt.DWORD(1024)
        buffer = ctypes.create_unicode_buffer(size.value)
        if not kernel32.QueryFullProcessImageNameW(process, 0, buffer, ctypes.byref(size)):
            return None
        return os.path.basename(buffer.value).lower()
    finally:
        kernel32.CloseHandle(process)


def cloaked(hwnd):
    if dwmapi is None:
        return False
    value = wt.DWORD()
    dwmapi.DwmGetWindowAttribute(wt.HWND(hwnd), DWMWA_CLOAKED, ctypes.byref(value), ctypes.sizeof(value))
    return value.value != 0


def capture_capability():
    """One capability owner for negotiation and observation-only discovery."""
    try:
        if not all(value > 0 for value in virtual_screen()[2:]):
            return {"available": False, "reason": "no_display"}
        if secure_desktop_active():
            return {"available": False, "reason": "secure_desktop"}
        return {"available": True}
    except (OSError, HelperError) as error:
        # A failed desktop probe says nothing about whether a display exists.
        # Discovery reports a failed poll; negotiation reports the helper error.
        raise HelperError("capture_unavailable") from error


_ICON_CACHE = OrderedDict()


def app_icon(hwnd, exe):
    """Bounded app icons only. This worker never captures a window thumbnail."""
    now = time.monotonic()
    # WM_GETICON can target a hung app: bound the wait, then use the class icon.
    result = ctypes.c_size_t()
    user32.SendMessageTimeoutW.argtypes = (wt.HWND, wt.UINT, wt.WPARAM, wt.LPARAM,
                                          wt.UINT, wt.UINT, ctypes.POINTER(ctypes.c_size_t))
    user32.SendMessageTimeoutW(hwnd, 0x7F, 1, 0, 2, 50, ctypes.byref(result))
    get_class_icon = getattr(user32, "GetClassLongPtrW", user32.GetClassLongW)
    get_class_icon.argtypes, get_class_icon.restype = (wt.HWND, ctypes.c_int), ctypes.c_void_p
    user32.LoadIconW.argtypes, user32.LoadIconW.restype = (wt.HINSTANCE, ctypes.c_void_p), wt.HANDLE
    icon = result.value or get_class_icon(hwnd, -14) or user32.LoadIconW(None, ctypes.c_void_p(32512))
    # Hosts such as ApplicationFrameHost can give different applications the
    # same executable name. Cache the native icon identity, not just the exe.
    key = (exe, icon or 0)
    if key in _ICON_CACHE:
        observed, encoded = _ICON_CACHE.pop(key)
        if now - observed < 60:
            _ICON_CACHE[key] = (observed, encoded)
            return encoded
    encoded = None
    if icon:
        gdi = ctypes.WinDLL("gdi32")
        gdi.CreateCompatibleDC.restype = wt.HDC
        gdi.CreateDIBSection.argtypes = (wt.HDC, ctypes.c_void_p, wt.UINT,
                                         ctypes.POINTER(ctypes.c_void_p), wt.HANDLE, wt.DWORD)
        gdi.CreateDIBSection.restype = wt.HBITMAP
        gdi.SelectObject.argtypes, gdi.SelectObject.restype = (wt.HDC, wt.HANDLE), wt.HANDLE
        gdi.DeleteObject.argtypes, gdi.DeleteDC.argtypes = (wt.HANDLE,), (wt.HDC,)
        user32.DrawIconEx.argtypes = (wt.HDC, ctypes.c_int, ctypes.c_int, wt.HANDLE,
                                      ctypes.c_int, ctypes.c_int, wt.UINT, wt.HBRUSH, wt.UINT)
        # A 20px RGBA image, even uncompressed, fits the 3072-byte base64 cap.
        size = 20
        info = ctypes.create_string_buffer(struct.pack("<IiiHHIIiiII", 40, size, -size, 1, 32, 0, size * size * 4, 0, 0, 0, 0))
        bits = ctypes.c_void_p()
        dc = gdi.CreateCompatibleDC(None)
        bitmap = gdi.CreateDIBSection(dc, info, 0, ctypes.byref(bits), None, 0)
        previous = gdi.SelectObject(dc, bitmap) if bitmap else None
        try:
            if bitmap and bits.value:
                ctypes.memset(bits, 0, size * size * 4)
            if bitmap and bits.value and user32.DrawIconEx(dc, 0, 0, icon, size, size, 0, None, 3):
                pixels = ctypes.string_at(bits, size * size * 4)
                legacy = not any(pixels[3::4])
                mask = None
                if legacy:
                    ctypes.memset(bits, 255, size * size * 4)
                    if user32.DrawIconEx(dc, 0, 0, icon, size, size, 0, None, 1):
                        mask = ctypes.string_at(bits, size * size * 4)
                rows = bytearray()
                for y in range(size):
                    rows.append(0)
                    for x in range(size):
                        b, g, r, alpha = pixels[(y * size + x) * 4:][:4]
                        if legacy:
                            alpha = 255 - mask[(y * size + x) * 4] if mask else 255
                        elif 0 < alpha < 255:
                            r, g, b = (min(255, (channel * 255 + alpha // 2) // alpha) for channel in (r, g, b))
                        rows.extend((r, g, b, alpha))
                def chunk(kind, value):
                    return struct.pack(">I", len(value)) + kind + value + struct.pack(">I", zlib.crc32(kind + value))
                png = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0))
                png += chunk(b"IDAT", zlib.compress(bytes(rows))) + chunk(b"IEND", b"")
                encoded = base64.b64encode(png).decode()
        finally:
            if previous:
                gdi.SelectObject(dc, previous)
            if bitmap:
                gdi.DeleteObject(bitmap)
            if dc:
                gdi.DeleteDC(dc)
    _ICON_CACHE[key] = (now, encoded)
    while len(_ICON_CACHE) > 64:
        _ICON_CACHE.popitem(last=False)
    return encoded if encoded and len(encoded) <= 3072 else None


def window_class(hwnd):
    name = ctypes.create_unicode_buffer(256)
    user32.GetClassNameW(hwnd, name, len(name))
    return name.value


def list_windows(apps, all_windows=False):
    """Visible, non-minimized, uncloaked top-level application windows."""
    wanted = {app.lower() for app in apps}
    found = []
    callback_type = ctypes.WINFUNCTYPE(wt.BOOL, wt.HWND, wt.LPARAM)

    def visit(hwnd, _):
        if len(found) >= 32:
            return False
        if not user32.IsWindowVisible(hwnd) or user32.IsIconic(hwnd) or cloaked(hwnd):
            return True
        if user32.GetAncestor(hwnd, 3) != hwnd:
            return True
        if window_class(hwnd) in {"Progman", "WorkerW", "Shell_TrayWnd", "Shell_SecondaryTrayWnd", "#32768", "SysShadow", "tooltips_class32"}:
            return True
        style = user32.GetWindowLongW(hwnd, -20)  # GWL_EXSTYLE
        if style & (0x00000020 | 0x00000080 | 0x08000000):  # TRANSPARENT, TOOLWINDOW, NOACTIVATE
            return True
        if style & 0x00080000:  # WS_EX_LAYERED: normal translucent app windows remain eligible.
            alpha, flags = wt.BYTE(255), wt.DWORD()
            if user32.GetLayeredWindowAttributes(hwnd, None, ctypes.byref(alpha), ctypes.byref(flags)) and flags.value & 2 and alpha.value == 0:
                return True
        exe, title = window_exe(hwnd), window_title(hwnd)
        valid_exe = exe and len(exe) <= 150 and exe.isascii() and all(c.isalnum() or c in ".-_" for c in exe)
        if valid_exe and (all_windows or exe in wanted) and title:
            handle = ctypes.cast(hwnd, ctypes.c_void_p).value or 0
            if 0 < handle <= 0xFFFFFFFF and len(f"app:{exe}#{handle}") <= 160:
                try:
                    _, _, width, height = window_rect(handle)
                except HelperError:
                    return True  # closed during enumeration
                if width >= 80 and height >= 60:
                    source = {"kind": "window", "id": f"app:{exe}#{handle}", "appId": exe,
                              "name": title, "appName": exe.removesuffix(".exe"), "windowTitle": title}
                    icon = app_icon(hwnd, exe)
                    if icon:
                        source["iconPNG"] = icon
                    found.append(source)
        return True

    user32.EnumWindows(callback_type(visit), 0)
    return found


def virtual_screen():
    return (user32.GetSystemMetrics(SM_XVIRTUALSCREEN), user32.GetSystemMetrics(SM_YVIRTUALSCREEN),
            user32.GetSystemMetrics(SM_CXVIRTUALSCREEN), user32.GetSystemMetrics(SM_CYVIRTUALSCREEN))


# ------------------------------------------------------------------ capture

from aiortc import RTCConfiguration, RTCPeerConnection, RTCSessionDescription  # noqa: E402
from aiortc.codecs import vpx  # noqa: E402
from aiortc.mediastreams import VideoStreamTrack  # noqa: E402
from aiortc.sdp import candidate_from_sdp  # noqa: E402
import av  # noqa: E402
import mss  # noqa: E402
import numpy  # noqa: E402

try:  # Windows.Graphics.Capture
    from windows_capture import WindowsCapture  # noqa: E402
except Exception:  # noqa: BLE001 - optional; GDI fallback below
    WindowsCapture = None

vpx.DEFAULT_BITRATE = 1_500_000
vpx.MAX_BITRATE = 3_000_000
_MSS = getattr(mss, "MSS", None) or mss.mss


class Grabber:
    """Latest frame of one source. WGC pushes frames from its own thread;
    GDI pulls displays on demand. Both hand back BGRA ndarrays.

    Window sources fail closed: only WGC reads a window's own pixels, so a
    window never falls back to copying its screen rectangle (which would show
    whatever overlaps it). Until WGC delivers, and after it closes, `grab`
    returns None and the track sends a blank waiting frame.

    WGC runs only from `start` (capture.start) to `stop` (capture.stop or a
    new source) and delivers at most `fps` updates a second. `on_closed(grabber)`
    runs on the WGC thread when the window closes, never after `stop`."""

    def __init__(self, source, fps=QUALITY_FPS["standard"], on_closed=None):
        self.source = source
        self.lock = threading.Lock()
        self.latest = None
        self.control = None
        self.fps = fps
        self.on_closed = on_closed
        self.capture = None
        self.streaming = False
        self.stopped = False
        self.backend = "gdi"
        self.local = threading.local()
        wanted = WindowsCapture is not None and os.environ.get("WONDER_CAPTURE_BACKEND", "wgc") == "wgc"
        if wanted:
            try:
                self.capture = self._create_wgc()
                self.backend = "wgc"
            except Exception as error:  # noqa: BLE001
                if source["kind"] == "window":
                    raise HelperError("capture_unavailable") from error
                log("Windows.Graphics.Capture unavailable, using GDI:", repr(error))
        elif source["kind"] == "window":
            raise HelperError("capture_unavailable")

    def _create_wgc(self):
        # Hold updates to the stream's rate before any frame is mapped or copied.
        kwargs = {"cursor_capture": True, "draw_border": False,
                  "minimum_update_interval": max(1, round(1000 / self.fps))}
        if self.source["kind"] == "window":
            kwargs["window_hwnd"] = self.source["hwnd"]
        else:
            kwargs["monitor_index"] = self.source["monitor"]
        capture = WindowsCapture(**kwargs)

        @capture.event
        def on_frame_arrived(frame, control):
            with self.lock:
                if self.stopped or not self.streaming:
                    return  # Nothing sends this frame: skip the copy.
            # The mapped buffer is valid only during this callback: copy it.
            pixels = numpy.array(frame.frame_buffer, copy=True)
            with self.lock:
                if not self.stopped:
                    self.latest = pixels

        @capture.event
        def on_closed():
            with self.lock:
                self.latest = None
                report = not self.stopped
            if report and self.on_closed is not None:
                self.on_closed(self)

        return capture

    def grab(self):
        if self.backend == "wgc":
            with self.lock:
                frame = self.latest
            if frame is not None or self.source["kind"] == "window":
                return frame
        if not hasattr(self.local, "sct"):
            self.local.sct = _MSS()
        shot = self.local.sct.grab(self.local.sct.monitors[self.source["monitor"]])
        return numpy.frombuffer(shot.bgra, dtype=numpy.uint8).reshape(shot.height, shot.width, 4)

    def start(self):
        """Begins capture; WGC's first frame follows at once. Idempotent."""
        with self.lock:
            if self.stopped or self.streaming:
                return
            self.streaming = True
        if self.backend != "wgc" or self.control is not None:
            return
        try:
            self.control = self.capture.start_free_threaded()
        except Exception as error:  # noqa: BLE001
            if self.source["kind"] == "window":
                with self.lock:
                    self.streaming = False
                raise HelperError("capture_unavailable") from error
            log("Windows.Graphics.Capture failed to start, using GDI:", repr(error))
            self.backend = "gdi"

    def stop(self):
        with self.lock:
            self.stopped, self.streaming = True, False
        control, self.control = self.control, None
        if control is not None:
            try:
                control.stop()
            except Exception:  # noqa: BLE001
                pass


def waiting_frame(source):
    width = max(2, (source or {}).get("width", 2) & ~1)
    height = max(2, (source or {}).get("height", 2) & ~1)
    return av.VideoFrame.from_ndarray(numpy.zeros((height, width, 4), dtype=numpy.uint8), format="bgra")


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
        grabber = self.helper.grabber
        try:
            pixels = await asyncio.get_running_loop().run_in_executor(None, grabber.grab)
            if grabber is not self.helper.grabber:
                # A frame already in flight belongs to the previous view.
                frame = waiting_frame(self.helper.source)
                frame.pts, frame.time_base = pts, time_base
                return frame
            if pixels is None:  # a window with no frame yet (or closed): blank, not counted
                frame = waiting_frame(self.helper.source)
                frame.pts, frame.time_base = pts, time_base
                return frame
            # VP8 needs even dimensions.
            height, width = pixels.shape[0] & ~1, pixels.shape[1] & ~1
            frame = av.VideoFrame.from_ndarray(numpy.ascontiguousarray(pixels[:height, :width]), format="bgra")
        except Exception as error:  # aiortc drops the track silently otherwise
            if grabber is not self.helper.grabber:
                return await self.recv()
            log("frame failed:", repr(error))
            raise
        frame.pts = pts
        frame.time_base = time_base
        self.helper.frame_captured(frame.width, frame.height)
        width, height = encoded_size(frame.width, frame.height, self.helper.quality)
        if (width, height) != (frame.width, frame.height):
            frame = frame.reformat(width=width, height=height)
            frame.pts, frame.time_base = pts, time_base
        return frame


# ------------------------------------------------------------------ input

VK = {
    "return": 0x0D, "enter": 0x0D, "tab": 0x09, "space": 0x20, "delete": 0x08,
    "backspace": 0x08, "forwarddelete": 0x2E, "escape": 0x1B, "esc": 0x1B,
    "left": 0x25, "up": 0x26, "right": 0x27, "down": 0x28, "home": 0x24, "end": 0x23,
    "pageup": 0x21, "pagedown": 0x22, "capslock": 0x14,
    "shift": 0xA0, "control": 0xA2, "option": 0xA4, "command": 0xA2,
    "win": 0x5B, "ctrl": 0xA2, "alt": 0xA4,
    "insert": 0x2D, "printscreen": 0x2C, "menu": 0x5D, "volumeup": 0xAF,
    "volumedown": 0xAE, "mute": 0xAD, "playpause": 0xB3, "next": 0xB0, "previous": 0xB1,
    "super": 0x5B,
    "rightshift": 0xA1, "rightcontrol": 0xA3, "rightoption": 0xA5, "rightcommand": 0xA3,
    "-": 0xBD, "=": 0xBB, "[": 0xDB, "]": 0xDD, "\\": 0xDC, ";": 0xBA, "'": 0xDE,
    ",": 0xBC, ".": 0xBE, "/": 0xBF, "`": 0xC0,
}
IGNORED_KEYS = {"brightnessup", "brightnessdown"}
for _n in range(1, 20):
    VK[f"f{_n}"] = 0x6F + _n
EXTENDED = {0x25, 0x26, 0x27, 0x28, 0x24, 0x23, 0x21, 0x22, 0x2E, 0x2D, 0x2C, 0x5D, 0x5B, 0xA3, 0xA5,
            0xAD, 0xAE, 0xAF, 0xB0, 0xB1, 0xB3}
# Wonder's modifier bits (ControlInputTranslator). Command maps to Control so
# the phone's Cmd+C / Cmd+V mean copy/paste on Windows too.
MODIFIERS = [(1 << 1, 0xA2), (1 << 3, 0xA2), (1 << 2, 0xA4), (1 << 0, 0xA0)]
BUTTONS = {
    "left": (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
    "middle": (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
    "right": (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
}
SCROLL_STEP = 40.0


def virtual_key(key):
    name = key.lower()
    if name in VK:
        return VK[name]
    if len(name) == 1 and name.isalnum():
        return ord(name.upper())
    raise HelperError("invalid_input")


def key_input(vk, up):
    flags = (KEYEVENTF_KEYUP if up else 0) | (KEYEVENTF_EXTENDEDKEY if vk in EXTENDED else 0)
    item = INPUT(type=INPUT_KEYBOARD)
    item.ki = KEYBDINPUT(vk, 0, flags, 0, 0)
    return item


def unicode_inputs(text):
    items = []
    data = text.encode("utf-16-le")
    for index in range(0, len(data), 2):
        unit = int.from_bytes(data[index:index + 2], "little")
        for up in (False, True):
            item = INPUT(type=INPUT_KEYBOARD)
            item.ki = KEYBDINPUT(0, unit, KEYEVENTF_UNICODE | (KEYEVENTF_KEYUP if up else 0), 0, 0)
            items.append(item)
    return items


def mouse_input(flags, x=0, y=0, data=0):
    item = INPUT(type=INPUT_MOUSE)
    item.mi = MOUSEINPUT(x, y, ctypes.c_uint32(data & 0xFFFFFFFF).value, flags, 0, 0)
    return item


def absolute_move(screen_x, screen_y):
    left, top, width, height = virtual_screen()
    nx = round((screen_x - left) * 65535 / max(width - 1, 1))
    ny = round((screen_y - top) * 65535 / max(height - 1, 1))
    return mouse_input(MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK, nx, ny)


CF_UNICODETEXT, GMEM_MOVEABLE = 13, 0x2
kernel32.GlobalAlloc.restype = wt.HGLOBAL
kernel32.GlobalAlloc.argtypes = (wt.UINT, ctypes.c_size_t)
kernel32.GlobalLock.restype = ctypes.c_void_p
kernel32.GlobalLock.argtypes = (wt.HGLOBAL,)
kernel32.GlobalUnlock.argtypes = (wt.HGLOBAL,)
kernel32.GlobalFree.argtypes = (wt.HGLOBAL,)
user32.GetClipboardData.restype = wt.HANDLE
user32.SetClipboardData.argtypes = (wt.UINT, wt.HANDLE)
user32.SetClipboardData.restype = wt.HANDLE


def _open_clipboard():
    for _ in range(20):
        if user32.OpenClipboard(None):
            return
        time.sleep(0.02)
    raise HelperError("clipboard_unavailable")


def clipboard_read():
    _open_clipboard()
    try:
        handle = user32.GetClipboardData(CF_UNICODETEXT)
        if not handle:
            return ""
        pointer = kernel32.GlobalLock(handle)
        try:
            text = ctypes.wstring_at(pointer)
        finally:
            kernel32.GlobalUnlock(handle)
        if len(text.encode()) > 65536:
            raise HelperError("clipboard_too_large")
        return text
    finally:
        user32.CloseClipboard()


def clipboard_write(text):
    data = text.encode("utf-16-le") + b"\0\0"
    _open_clipboard()
    try:
        user32.EmptyClipboard()
        handle = kernel32.GlobalAlloc(GMEM_MOVEABLE, len(data))
        pointer = kernel32.GlobalLock(handle)
        ctypes.memmove(pointer, data, len(data))
        kernel32.GlobalUnlock(handle)
        if not user32.SetClipboardData(CF_UNICODETEXT, handle):
            kernel32.GlobalFree(handle)
            raise HelperError("clipboard_unavailable")
    finally:
        user32.CloseClipboard()


# ------------------------------------------------------------------ helper


def write(obj):
    data = (json.dumps(obj, ensure_ascii=False, separators=(",", ":")) + "\n").encode()
    if _OUTPUT is None:
        sys.stdout.write(data.decode())
        sys.stdout.flush()
        return
    _OUTPUT(data)


class Helper:
    def __init__(self):
        self.session_id = None
        self.generation = 0
        self.state = "idle"
        self.reason = None
        self.quality = "standard"
        self.viewer_count = 0
        self.source = None
        self.grabber = None
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
        self.execution_generation = 0
        self.peer_task = None
        self.last_input_sequence = 0
        self.held_buttons = set()
        self.held_keys = set()
        self.altgr_ctrl = False

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
            payload["source"] = {k: v for k, v in source.items() if k not in ("hwnd", "monitor")}
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

    def frame_captured(self, width, height):
        if self.state == "starting":
            self.state = "capturing"
        resized = bool(self.source) and (width, height) != (self.source["width"] & ~1, self.source["height"] & ~1)
        if resized:
            # The window was resized: input mapped to the old size stops and
            # held buttons and keys are released before the new geometry.
            self.revoke_now("source_geometry_changed")
            self.source["width"], self.source["height"] = width, height
            self.source["contentRect"] = {"x": 0, "y": 0, "width": width, "height": height}
            self.geometry_revision += 1
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
        if resized or now - self.last_metadata_emit >= 1.0:
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
        await pc.setLocalDescription(offer)
        if self.pc is not pc:
            return
        sdp = pc.localDescription.sdp
        self.signal("signal.offer", type="offer", sdp=sdp)
        for line in (part for part in sdp.split("\r\n") if part.startswith("a=candidate:")):
            self.signal("signal.candidate", candidate=line[2:], sdpMid="0", sdpMLineIndex=0)

    # ---- sources

    def resolve_source(self, source_id):
        if source_id in (None, "display:1") or (isinstance(source_id, str) and source_id.startswith("display:")):
            index = 1 if source_id is None else int(source_id.split(":", 1)[1] or 1)
            with _MSS() as sct:
                if not 1 <= index < len(sct.monitors):
                    raise HelperError("source_not_found")
                monitor = sct.monitors[index]
            width, height = monitor["width"], monitor["height"]
            return {"id": f"display:{index}", "kind": "display", "title": f"Display {index}",
                    "width": width, "height": height, "scale": 1.0, "monitor": index,
                    "origin": (monitor["left"], monitor["top"]),
                    "contentRect": {"x": 0, "y": 0, "width": width, "height": height}}
        if isinstance(source_id, str) and source_id.startswith("app:") and "#" in source_id:
            app, _, number = source_id[4:].rpartition("#")
            if not number.isdigit():
                raise HelperError("source_not_found")
            hwnd = int(number)
            if not user32.IsWindow(hwnd) or window_exe(hwnd) != app.lower():
                raise HelperError("source_not_found")
            _, _, width, height = window_rect(hwnd)
            return {"id": source_id, "kind": "window", "title": window_title(hwnd),
                    "appId": app, "width": width, "height": height, "scale": 1.0, "hwnd": hwnd,
                    "contentRect": {"x": 0, "y": 0, "width": width, "height": height}}
        raise HelperError("source_not_found")

    def grabber_for(self, source):
        loop = asyncio.get_running_loop()

        def closed(grabber):
            # WGC's thread: the loop decides whether this grabber is still current.
            try:
                loop.call_soon_threadsafe(lambda: asyncio.ensure_future(self.source_closed(grabber)))
            except RuntimeError:
                pass  # the helper is shutting down

        return Grabber(source, QUALITY_FPS[self.quality], closed)

    async def source_closed(self, grabber):
        """The captured window closed. A late close from a replaced or stopped
        grabber changes nothing."""
        async with self.lock:
            if grabber is not self.grabber or grabber.stopped:
                return
            grabber.stop()
            self.capturing.clear()
            self.state, self.reason = "sourceRemoved", "source_not_found"
            await self.revoke("source_not_found")
            self.emit_capture("capture.sourceRemoved", message="source_not_found")

    def source_origin(self):
        if self.source["kind"] == "window":
            x, y, _, _ = window_rect(self.source["hwnd"])
            return x, y
        return self.source["origin"]

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
        self.revoke_now(reason)

    def revoke_now(self, reason):
        self.execution_generation += 1
        task = self.running_input
        if task is not None and not task.done() and task is not asyncio.current_task():
            task.cancel()
        lease, self.lease = self.lease, None
        self.payloads.clear()
        self.release_held()
        if lease:
            write({"event": "control.revoked", **lease, "reason": reason})

    async def expire(self):
        if self.lease is not None and self.lease_expires <= time.monotonic():
            await self.revoke("lease_expired")

    async def watch_lease(self):
        while True:
            await asyncio.sleep(.25)
            async with self.lock:
                await self.expire()

    def release_held(self):
        buttons, keys = self.held_buttons, self.held_keys
        self.held_buttons, self.held_keys = set(), set()
        self.altgr_ctrl = False
        items = [mouse_input(BUTTONS[b][1]) for b in sorted(buttons)]
        items += [key_input(vk, True) for vk in sorted(keys)]
        try:
            send_inputs(items)
        except HelperError:
            pass

    def require_focus(self):
        """A window source takes keys, text and paste only while it has
        keyboard focus; otherwise they would reach another application."""
        if self.source["kind"] != "window":
            return
        hwnd = self.source["hwnd"]
        expected_app = self.source["id"][4:].rpartition("#")[0].lower()
        if not user32.IsWindow(hwnd) or window_exe(hwnd) != expected_app:
            raise HelperError("source_not_found")
        if foreground_root() != hwnd:
            activate_window(hwnd)
        if foreground_root() != hwnd:
            raise HelperError("window_not_focused")

    def require_still_focused(self):
        """Recheck focus after a wait (clipboard acquisition) without
        reactivating: the user may have switched applications meanwhile."""
        if secure_desktop_active():
            raise HelperError("input_blocked")
        if self.source["kind"] != "window":
            return
        hwnd = self.source["hwnd"]
        expected_app = self.source["id"][4:].rpartition("#")[0].lower()
        if not user32.IsWindow(hwnd) or window_exe(hwnd) != expected_app:
            raise HelperError("source_not_found")
        if foreground_root() != hwnd:
            raise HelperError("window_not_focused")

    def require_under(self, point):
        """A window source takes a click or wheel only where it is the window
        on top; a window covering it (not in the stream) would get it."""
        if self.source["kind"] != "window":
            return
        if point is not None:
            # Focus and hit-test z-order can settle on different queue turns.
            # Wait at most 50 ms for the actual recipient, never bypass it.
            for attempt in range(6):
                if root_window_at(*point) == self.source["hwnd"]:
                    return
                if attempt < 5:
                    time.sleep(.01)
        raise HelperError("window_covered")

    def deliver(self, action, result):
        kind = action.get("type")
        button = action.get("button") or "left"
        if kind == "pointer" and action.get("phase") == "up" and button in self.held_buttons:
            try:
                self.deliver(dict(action, phase="move"), result)
            finally:
                send_inputs([mouse_input(BUTTONS[button][1])])
                self.held_buttons.discard(button)
            return
        if kind != "clipboard" and secure_desktop_active():
            raise HelperError("input_blocked")
        width = self.source["width"]
        height = self.source["height"]
        if kind == "pointer":
            x, y = float(action["x"]), float(action["y"])
            if not (0 <= x <= 1 and 0 <= y <= 1):
                raise HelperError("invalid_input")
            # Raise the exact target before the first pointer event. Keep the
            # hit test below: Windows may refuse activation or a panel can cover it.
            if not self.held_buttons:
                self.require_focus()
            if self.source["kind"] == "window":
                _, _, width, height = window_rect(self.source["hwnd"])
            ox, oy = self.source_origin()
            point = (ox + round(x * (width - 1)), oy + round(y * (height - 1)))
            button = action.get("button") or "left"
            if button not in BUTTONS:
                raise HelperError("invalid_input")
            phase = action.get("phase")
            # Releasing a held button must always reach Windows.
            if not (phase == "up" and button in self.held_buttons):
                self.require_under(point)
            items = [absolute_move(*point)]
            if phase == "down":
                items.append(mouse_input(BUTTONS[button][0]))
                self.held_buttons.add(button)
            elif phase == "up":
                items.append(mouse_input(BUTTONS[button][1]))
                self.held_buttons.discard(button)
            elif phase != "move":
                raise HelperError("invalid_input")
            send_inputs(items)
        elif kind == "scroll":
            if not self.held_buttons:
                self.require_focus()
            items = []
            for delta, flag, sign in ((float(action["deltaY"]), MOUSEEVENTF_WHEEL, -1),
                                      (float(action["deltaX"]), MOUSEEVENTF_HWHEEL, 1)):
                if not math.isfinite(delta) or abs(delta) > 4096:
                    raise HelperError("invalid_input")
                clicks = min(round(abs(delta) / SCROLL_STEP) or (1 if delta else 0), 20)
                if clicks:
                    items.append(mouse_input(flag, data=sign * (1 if delta > 0 else -1) * clicks * WHEEL_DELTA))
            if items:
                self.require_under(cursor_position())
            send_inputs(items)
        elif kind == "key":
            if str(action.get("key", "")).lower() in IGNORED_KEYS:
                return
            vk = virtual_key(action["key"])
            phase = action["phase"]
            modifiers = []
            for bit, mod in MODIFIERS:
                if (int(action.get("modifiers", 0)) & bit and mod != vk and mod not in modifiers
                        and (mod in self.held_keys if phase == "up" else mod not in self.held_keys)
                        and not (phase == "up" and vk == 0xA5 and mod == 0xA2)):
                    modifiers.append(mod)
            paired_ctrl = (vk == 0xA5 and phase != "up" and 0xA2 not in modifiers
                           and not self.held_keys.intersection({0xA2, 0xA3}))
            if paired_ctrl:
                modifiers.insert(0, 0xA2)  # AltGr is Right Alt with Control
            # Releasing a held key must always reach Windows.
            if not (phase == "up" and vk in self.held_keys):
                self.require_focus()
            if phase == "press":
                items = [key_input(m, False) for m in modifiers] + [key_input(vk, False), key_input(vk, True)]
                items += [key_input(m, True) for m in reversed(modifiers)]
            elif phase == "down":
                items = [key_input(m, False) for m in modifiers] + [key_input(vk, False)]
                if vk == 0xA2:  # An explicit Control down takes ownership from AltGr.
                    self.altgr_ctrl = False
                self.held_keys.add(vk)
                self.held_keys.update(modifiers)
                if vk == 0xA5 and 0xA2 in modifiers:
                    self.altgr_ctrl = True
            elif phase == "up":
                if vk == 0xA5 and self.altgr_ctrl:
                    if 0xA2 in self.held_keys and 0xA2 not in modifiers:
                        modifiers.insert(0, 0xA2)
                    self.altgr_ctrl = False
                if vk == 0xA2:
                    self.altgr_ctrl = False
                items = [key_input(vk, True)] + [key_input(m, True) for m in reversed(modifiers)]
                self.held_keys.discard(vk)
                self.held_keys.difference_update(modifiers)
            else:
                raise HelperError("invalid_input")
            send_inputs(items)
        elif kind == "text":
            text = action.get("text") or ""
            if len(text) > 4096:
                raise HelperError("invalid_input")
            if text:
                self.require_focus()
            send_inputs(unicode_inputs(text))
        elif kind == "clipboard":
            if action.get("operation") == "copyToPhone":
                text = clipboard_read()
                if text:
                    result["clipboardText"] = text
                else:  # Nothing to copy is still delivered input; control stays.
                    result["clipboardReason"] = "clipboard_no_text"
            elif action.get("operation") == "pasteFromPhone":
                text = action.get("text") or ""
                if not isinstance(text, str) or len(text.encode()) > 65536:
                    raise HelperError("invalid_input")
                if secure_desktop_active():
                    raise HelperError("input_blocked")
                self.require_focus()
                clipboard_write(text)
                # Clipboard acquisition can wait; never paste into whatever
                # took focus meanwhile (the caller revokes control).
                self.require_still_focused()
                send_inputs([key_input(0xA2, False), key_input(0x56, False),
                             key_input(0x56, True), key_input(0xA2, True)])
            else:
                raise HelperError("invalid_input")
        elif kind == "releaseAll":
            self.release_held()
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
            deadline = time.monotonic() + INPUT_TIMEOUT
            async with asyncio.timeout(INPUT_TIMEOUT):
                for action in actions:
                    if generation != self.execution_generation or self.lease != identity:
                        raise HelperError("lease_not_active")
                    if self.lease_expires <= time.monotonic():
                        raise HelperError("lease_expired")
                    self.deliver(action, result)
                    if time.monotonic() >= deadline:
                        raise HelperError("input_timed_out")
                    await asyncio.sleep(0)
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
                reason = "request_timed_out" if external_cancel else "input_timed_out" if isinstance(error, TimeoutError) or getattr(error, "code", None) == "input_timed_out" else "input_delivery_failed"
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
                    "capture": {**capture_capability(), "liveQualityChange": False},
                    "control": {"available": False, "reason": LOCAL_APPROVAL_UNAVAILABLE,
                                "provider": "sendinput-win32-v1", "requiresAccessibility": False}}
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
            self.drop_source()
            self.last_frame, self.reason = None, None
            self.frame_sequence = 0
            self.state = "awaitingSource" if self.viewer_count > 0 else "stopped"
            self.emit_capture("capture.prepared")
            self.peer_task = asyncio.create_task(self.run_peer())
            return self.result("prepare", accepted=self.viewer_count > 0)
        if method in ("capture.pick", "pick"):
            if self.session_id is None:
                return self.result("pick", False, "capture_not_prepared")
            if params.get("sessionID") is not None:
                self.require_identity(params)
            try:
                source = self.resolve_source(params.get("sourceID"))
            except HelperError:
                return self.pick_failed("source_not_found")
            pending = self.result("pick", message="Selecting the source")
            self.drop_source()
            try:
                grabber = self.grabber_for(source)
            except HelperError as error:
                self.capturing.clear()
                return self.pick_failed(error.code, "This window can't be captured right now.")
            self.source = source
            self.grabber = grabber
            log("capture backend", self.grabber.backend, source["id"])
            self.state = "ready"
            asyncio.get_running_loop().call_soon(
                lambda: self.emit_capture("capture.sourceSelected", source=self.source))
            return pending
        if method == "capture.rebind":
            self.require_identity(params)
            if (not self.source or params.get("sourceID") != self.source["id"]
                    or params.get("geometryRevision") != self.geometry_revision):
                raise HelperError("invalid_capture_session")
            if self.lease is not None and self.control_identity(params) != self.lease:
                raise HelperError("lease_not_active")
            if self.lease is None and params.get("leaseID") is not None:
                raise HelperError("lease_not_active")
            grabber = None
            try:
                source = self.resolve_source(params.get("nextSourceID"))
                grabber = self.grabber_for(source)  # Validate before moving either binding.
                if self.capturing.is_set():
                    grabber.start()
            except HelperError as error:
                if grabber is not None:
                    grabber.stop()
                return self.result("rebind", False, error.code, "That window is no longer available. Choose another window.")
            await self.cancel_input()
            self.release_held()
            self.drop_source()
            self.source, self.grabber = source, grabber
            self.geometry_revision += 1
            self.last_frame = None
            if self.lease is not None:
                self.lease = dict(self.lease, sourceID=source["id"], geometryRevision=self.geometry_revision)
                self.lease_expires = time.monotonic() + 10
                self.consents[self.lease["requestID"]] = self.lease
            # Keep the existing publisher, lease, sequence and payload receipts.
            return {**self.result("rebind"), "source": {k: v for k, v in source.items() if k not in ("hwnd", "monitor")}}
        if method in ("capture.start", "startCapture"):
            self.require_identity(params)
            if self.source is None:
                return self.result("start", False, "source_not_selected")
            if self.state in ("ready", "paused"):
                try:
                    if self.grabber is not None:
                        self.grabber.start()  # Capture runs only while it streams.
                except HelperError as error:
                    return self.result("start", False, error.code, "This window can't be captured right now.")
                self.state = "starting"
                self.capturing.set()
            return self.result("start")
        if method in ("capture.stop", "stopCapture"):
            self.require_identity(params)
            self.capturing.clear()
            if self.grabber is not None:
                self.grabber.stop()  # Capture lives only while it streams.
            self.state, self.reason = "stopped", params.get("reason") or "stopped_by_request"
            await self.revoke("capture_stopped")
            await self.stop_peer()
            return self.result("stop")
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
            # Control reaches the signed-in desktop, and this helper cannot ask
            # the person at the computer or show them Stop: always refuse.
            self.control_identity(params)
            return {"granted": False, "decision": "deny", "replayed": False,
                    "reason": LOCAL_APPROVAL_UNAVAILABLE}
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

    def drop_source(self):
        grabber, self.grabber = self.grabber, None
        if grabber is not None:
            grabber.stop()
        self.source = None

    async def stop_peer(self):
        task, self.peer_task = self.peer_task, None
        if task is not None:
            task.cancel()
            await asyncio.gather(task, return_exceptions=True)
        await self.close_peer()

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
    except Exception as error:  # noqa: BLE001
        log("request failed:", type(error).__name__, error)
        return {"id": request_id, "error": {"code": "computer_use_failed"}}


async def main():
    global _OUTPUT
    helper = Helper()
    loop = asyncio.get_running_loop()
    # Windows pipes do not support asyncio's connect_read_pipe on the default
    # loop; read stdin on a thread and write stdout synchronously (lines are small).
    lines = asyncio.Queue(maxsize=64)
    out_lock = threading.Lock()

    def emit(data):
        with out_lock:
            sys.stdout.buffer.write(data)
            sys.stdout.buffer.flush()

    _OUTPUT = emit

    def pump():
        stream = sys.stdin.buffer
        while True:
            raw = stream.readline(MAX_LINE_BYTES + 1)
            asyncio.run_coroutine_threadsafe(lines.put(raw), loop).result()
            if not raw or len(raw) > MAX_LINE_BYTES or not raw.endswith(b"\n"):
                return

    threading.Thread(target=pump, name="stdin", daemon=True).start()
    watchdog = asyncio.create_task(helper.watch_lease())
    pending_input = set()

    async def input_reply(request_id, params):
        write(await respond(helper, request_id, "control.input", params))
    try:
        while True:
            raw = await lines.get()
            if not raw or len(raw) > MAX_LINE_BYTES or not raw.endswith(b"\n"):
                break  # EOF or a truncated request: not safe to interpret.
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
            except Exception as error:  # noqa: BLE001
                log("request failed:", type(error).__name__, error)
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
        helper.drop_source()


def discovery_answer(available, sources):
    # Per-icon and row caps also need an envelope cap: quoted/non-ASCII
    # titles can expand on the JSON wire. Keep each retained row complete.
    answer = {"available": available, "sources": sources[:32] if available else []}
    while len(json.dumps(answer, ensure_ascii=False, separators=(",", ":")).encode()) > MAX_LINE_BYTES - 512:
        answer["sources"].pop()
    return answer


def discover_sources():
    """The daemon's persistent discovery worker: one JSON request per line
    ({"apps": [...], "browserTab": n}), one answer line each, until stdin closes.
    An empty app list asks for simulators, which Windows does not have."""
    if hasattr(sys.stdout, "reconfigure"):
        sys.stdout.reconfigure(encoding="utf-8")
    for line in sys.stdin:
        try:
            request = json.loads(line)
            apps = request.get("apps") or []
            all_windows = request.get("allWindows") is True
            capture = capture_capability()
            available = capture["available"] and bool(apps or all_windows)
            sources = list_windows(apps, all_windows) if available else []
            answer = discovery_answer(available, sources)
            if apps or all_windows:
                answer["capture"] = capture
            write(answer)
        except Exception:
            write({"error": "source_discovery_failed"})


def cli():
    if len(sys.argv) >= 2 and sys.argv[1] == "--discover-sources":
        discover_sources()
        return
    asyncio.run(main())
    # The stdin reader thread may still be blocked in readline; interpreter
    # shutdown would then abort on the stdin lock. Everything is flushed.
    sys.stdout.flush()
    os._exit(0)


if __name__ == "__main__":
    cli()

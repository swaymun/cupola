"""Isolated Linux X11 feasibility check; never attaches to an existing display.

Run over verified SSH with python3 on stdin. Uses system Xvfb/Xlib/xdotool,
prints counters only, and deletes its display, anonymous cookie and temp files.
This measures raw capture/input primitives, not the product WebRTC pipeline.
"""
import ctypes as C
import ctypes.util
import hashlib
import json
import os
from pathlib import Path
import selectors
import statistics
import struct
import subprocess
import tempfile
import time


def main():
    os.umask(0o077)
    with tempfile.TemporaryDirectory(prefix="cupola-ssh-screen-") as root:
        auth_fd = os.memfd_create("cupola-ssh-screen-auth", os.MFD_CLOEXEC)
        os.fchmod(auth_fd, 0o600)
        authority = f"/proc/{os.getpid()}/fd/{auth_fd}"
        cookie = os.urandom(16)
        # Server loads this cookie; the display number is allocated atomically
        # by Xvfb, then a matching client entry is added in memory below.
        def authorize(number):
            fields = [b"", number.encode(), b"MIT-MAGIC-COOKIE-1", cookie]
            data = struct.pack(">H", 65535) + b"".join(
                struct.pack(">H", len(field)) + field for field in fields)
            os.lseek(auth_fd, 0, os.SEEK_SET)
            os.ftruncate(auth_fd, 0)
            os.write(auth_fd, data)

        authorize("0")
        read_fd, write_fd = os.pipe()
        process = subprocess.Popen(
            ["Xvfb", "-displayfd", str(write_fd), "-screen", "0", "1280x720x24",
             "-nolisten", "tcp", "-auth", str(authority), "-noreset"],
            pass_fds=(write_fd,), stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        os.close(write_fd)
        display = None
        try:
            with selectors.DefaultSelector() as selector:
                selector.register(read_fd, selectors.EVENT_READ)
                if not selector.select(timeout=8):
                    raise RuntimeError("isolated display startup timed out")
                number = os.read(read_fd, 32).decode().strip()
            if not number.isdecimal():
                raise RuntimeError("invalid allocated display")
            name = ":" + number
            # Auth material stays in anonymous RAM, never disk, argv or logs.
            authorize(number)
            env = {"PATH": "/usr/local/bin:/usr/bin:/bin", "HOME": root,
                   "DISPLAY": name, "XAUTHORITY": str(authority)}
            # Explicit display + owned Xauthority only; no user's desktop/bus.
            os.environ["XAUTHORITY"] = str(authority)
            x = C.CDLL(ctypes.util.find_library("X11"))

            def bind(name, args, result):
                function = getattr(x, name)
                function.argtypes, function.restype = args, result
                return function

            ptr, ulong, uint, integer = C.c_void_p, C.c_ulong, C.c_uint, C.c_int
            open_display = bind("XOpenDisplay", [C.c_char_p], ptr)
            close = bind("XCloseDisplay", [ptr], integer)
            root_window = bind("XDefaultRootWindow", [ptr], ulong)
            create = bind("XCreateSimpleWindow", [ptr, ulong, integer, integer, uint, uint,
                                                  uint, ulong, ulong], ulong)
            map_window = bind("XMapWindow", [ptr, ulong], integer)
            create_gc = bind("XCreateGC", [ptr, ulong, ulong, ptr], ptr)
            color = bind("XSetForeground", [ptr, ptr, ulong], integer)
            fill = bind("XFillRectangle", [ptr, ulong, ptr, integer, integer, uint, uint], integer)
            sync = bind("XSync", [ptr, integer], integer)
            query = bind("XQueryExtension", [ptr, C.c_char_p, C.POINTER(integer),
                                             C.POINTER(integer), C.POINTER(integer)], integer)

            class Image(C.Structure):
                _fields_ = [("width", integer), ("height", integer), ("xoffset", integer),
                            ("format", integer), ("data", ptr), ("byte_order", integer),
                            ("bitmap_unit", integer), ("bitmap_bit_order", integer),
                            ("bitmap_pad", integer), ("depth", integer),
                            ("bytes_per_line", integer), ("bits_per_pixel", integer)]

            capture = bind("XGetImage", [ptr, ulong, integer, integer, uint, uint, ulong, integer],
                           C.POINTER(Image))
            destroy = bind("XDestroyImage", [C.POINTER(Image)], integer)
            display = open_display(name.encode())
            if not display:
                raise RuntimeError("isolated display authentication failed")
            extensions = {}
            for extension in ["MIT-SHM", "Composite", "XTEST"]:
                a, b, c = integer(), integer(), integer()
                extensions[extension] = bool(query(display, extension.encode(),
                                                   C.byref(a), C.byref(b), C.byref(c)))
            window = create(display, root_window(display), 40, 40, 640, 480, 0, 0, 0)
            map_window(display, window)
            gc = create_gc(display, window, 0, None)
            samples, hashes = [], set()
            for index in range(30):
                color(display, gc, 0x336699 + index)
                fill(display, window, gc, 0, 0, 640, 480)
                sync(display, 0)
                started = time.monotonic()
                image = capture(display, root_window(display), 0, 0, 1280, 720, ulong(-1), 2)
                if not image:
                    raise RuntimeError("X11 readback failed")
                try:
                    size = image.contents.bytes_per_line * image.contents.height
                    if not 0 < size <= 1280 * 720 * 8:
                        raise RuntimeError("unexpected frame size")
                    pixels = C.string_at(image.contents.data, size)
                finally:
                    destroy(image)
                samples.append((time.monotonic() - started) * 1000)
                hashes.add(hashlib.sha256(pixels).digest())
            subprocess.run(["xdotool", "mousemove", "--sync", "180", "140"],
                           env=env, check=True, timeout=3, stdout=subprocess.DEVNULL,
                           stderr=subprocess.DEVNULL)
            location = subprocess.check_output(["xdotool", "getmouselocation", "--shell"],
                                               env=env, timeout=3, stderr=subprocess.DEVNULL)
            pointer = b"X=180\n" in location and b"Y=140\n" in location
            if len(hashes) != 30 or not pointer or not all(extensions.values()):
                raise RuntimeError("X11 capture/input contract failed")
            result = {"width": 1280, "height": 720, "samples": 30,
                      "distinctFrames": len(hashes), "pointerRoundTrip": pointer,
                      "extensions": extensions, "frameBytes": size,
                      "readbackMedianMs": round(statistics.median(samples), 3),
                      "readbackMaxMs": round(max(samples), 3), "existingDesktop": False}
        finally:
            os.close(read_fd)
            if display:
                close(display)
            process.terminate()
            try:
                process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=3)
            os.close(auth_fd)
    result["remoteCleaned"] = not Path(root).exists()
    print(json.dumps(result), flush=True)


if __name__ == "__main__":
    main()

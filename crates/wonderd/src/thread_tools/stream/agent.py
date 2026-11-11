import json, os, select, shutil, signal, socket, subprocess, sys, tempfile, time
# Cupola remote-screen agent. Fixed and reviewed: the daemon sends this source as the only remote command,
# `python3 -c SOURCE MODE ARG...`. It reads no file but /tmp/.X11-unix, never listens on a TCP port, runs only the
# fixed commands below and ends when stdin closes. No quote, backslash or exclamation mark appears in it, so every
# login shell passes it unchanged (a test enforces that).
DN = subprocess.DEVNULL
X11_DIR = "/tmp/.X11-unix"
LIMIT = 8 * 1024 * 1024


def out(line):
    sys.stdout.write(line + chr(10))
    sys.stdout.flush()


def fail(code):
    out("CUPOLA-ERROR " + code)
    sys.exit(3)


def stop(proc):
    # Ask first, so an X server removes its own socket and lock file; kill only if it does not leave.
    if proc is None:
        return
    if proc.poll() is None:
        proc.terminate()
    try:
        proc.wait(3)
    except Exception:
        proc.kill()
        try:
            proc.wait(3)
        except Exception:
            pass


def displays():
    found = []
    try:
        for name in sorted(os.listdir(X11_DIR)):
            if name[:1] == "X" and name[1:].isdigit() and int(name[1:]) < 1000:
                found.append(":" + name[1:])
    except OSError:
        pass
    return found[:20]


def probe():
    ports = []
    for port in range(5900, 6000):
        s = socket.socket()
        s.settimeout(0.2)
        try:
            if s.connect_ex(("127.0.0.1", port)) == 0:
                ports.append(port)
        except OSError:
            pass
        s.close()
    tools = {}
    for tool in ["x11vnc", "Xvfb", "import", "scrot", "grim", "screencapture", "nc"]:
        tools[tool] = shutil.which(tool) is not None
    out("CUPOLA-PROBE " + json.dumps({"displays": displays(), "vncPorts": ports[:20], "tools": tools, "platform": sys.platform}))


def pump(sock):
    # stdin <-> sock until either closes
    while True:
        ready = select.select([0, sock], [], [], 5)[0]
        if 0 in ready:
            data = os.read(0, 65536)
            if not data:
                return
            sock.sendall(data)
        if sock in ready:
            data = sock.recv(65536)
            if not data:
                return
            os.write(1, data)


def x11(display, width, height):
    xvfb = None
    vnc = None
    try:
        if display == "new":
            if shutil.which("Xvfb") is None:
                fail("no-xvfb")
            chosen = None
            for n in range(90, 100):
                if os.path.exists(X11_DIR + "/X%d" % n) or os.path.exists("/tmp/.X%d-lock" % n):
                    continue
                xvfb = subprocess.Popen(["Xvfb", ":%d" % n, "-screen", "0", "%dx%dx24" % (width, height), "-nolisten", "tcp"], stdin=DN, stdout=DN, stderr=DN)
                for _ in range(50):
                    if os.path.exists(X11_DIR + "/X%d" % n):
                        chosen = n
                        break
                    if xvfb.poll() is not None:
                        break
                    time.sleep(0.1)
                if chosen is not None:
                    break
                stop(xvfb)
                xvfb = None
            if chosen is None:
                fail("no-display")
            display = ":%d" % chosen
        elif display.split(".")[0] not in displays():
            fail("no-display")
        if shutil.which("x11vnc") is None:
            fail("no-x11vnc")
        a, b = socket.socketpair()
        # x11vnc -inetd wants a socket, not the pipes of ssh: it gets one end, we pump the other.
        vnc = subprocess.Popen(["x11vnc", "-inetd", "-viewonly", "-shared", "-once", "-noxdamage", "-nopw", "-nolookup", "-display", display, "-q"], stdin=b, stdout=b, stderr=DN)
        b.close()
        out("CUPOLA-STREAM 1 x11 " + display)
        pump(a)
    finally:
        stop(vnc)
        stop(xvfb)


def vnc(port):
    s = socket.socket()
    s.settimeout(5)
    try:
        s.connect(("127.0.0.1", port))
    except OSError:
        fail("no-vnc-server")
    s.settimeout(None)
    out("CUPOLA-STREAM 1 vnc " + str(port))
    pump(s)


def capture_command(display, path):
    # (argv, env additions, writes to a file)
    env = {}
    if sys.platform == "darwin" and shutil.which("screencapture"):
        return "screencapture", ["screencapture", "-x", "-t", "jpg", path], env, True
    if display and shutil.which("import"):
        env["DISPLAY"] = display
        return "import", ["import", "-window", "root", "jpeg:" + path], env, True
    if display and shutil.which("scrot"):
        env["DISPLAY"] = display
        return "scrot", ["scrot", "-o", "-q", "70", "-z", path], env, True
    if shutil.which("grim"):
        return "grim", ["grim", "-t", "jpeg", "-q", "70", path], env, True
    return None, None, env, True


def screenshot(display, interval):
    if display == "none":
        display = ""
    elif display.split(".")[0] not in displays():
        fail("no-display")
    folder = tempfile.mkdtemp(prefix="cupola-stream-")
    path = os.path.join(folder, "frame.jpg")
    try:
        tool, argv, env, _ = capture_command(display, path)
        if tool is None:
            fail("no-capture-tool")
        out("CUPOLA-STREAM 1 screenshot " + tool)
        failures = 0
        while True:
            started = time.time()
            data = b""
            try:
                if os.path.exists(path):
                    os.remove(path)
                full = dict(os.environ)
                full.update(env)
                subprocess.run(argv, stdin=DN, stdout=DN, stderr=DN, timeout=20, env=full, check=True)
                with open(path, "rb") as handle:
                    data = handle.read(LIMIT + 1)
            except Exception:
                data = b""
            if data and len(data) <= LIMIT:
                failures = 0
                out("CUPOLA-FRAME %d" % len(data))
                sys.stdout.flush()
                sys.stdout.buffer.write(data)
                sys.stdout.buffer.flush()
            else:
                failures += 1
                if failures >= 3:
                    fail("capture-failed")
            time.sleep(max(0.2, interval - (time.time() - started)))
    finally:
        shutil.rmtree(folder, ignore_errors=True)


def main():
    def leave(*_):
        sys.exit(0)

    for name in ["SIGHUP", "SIGTERM"]:
        signal.signal(getattr(signal, name), leave)
    mode = sys.argv[1] if len(sys.argv) > 1 else ""
    try:
        if mode == "probe":
            probe()
        elif mode == "x11":
            x11(sys.argv[2], int(sys.argv[3]), int(sys.argv[4]))
        elif mode == "vnc":
            vnc(int(sys.argv[2]))
        elif mode == "screenshot":
            screenshot(sys.argv[2], float(sys.argv[3]))
        else:
            fail("bad-mode")
    except (BrokenPipeError, KeyboardInterrupt):
        pass


main()

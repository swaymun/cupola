"""lldb helper for scripts/check-stack-depth: peak main-thread stack at every Wonder view body.

Loaded with `command script import`; `stackrun SECONDS` resumes the attached app, then
prints `DEPTH <peak bytes> <frame bytes> <symbol>` for the deepest hits, `PEAK <bytes> hits=N`, and
`METADATA <peak bytes> hits=N call=.. decoder=.. <view>` for the deepest type-metadata resolution and
`TYPENAME <bytes> <view>` for the longest mangled type name requested.

Two kinds of sample, because the build-213 and New chat crashes were different: a view `body` needing
a huge frame shows at body entry, but a deep generic type (about 24 un-erased modifiers) is demangled
recursively inside swift_getTypeByMangledNameInContext2 / TypeDecoder::decodeMangledType, far below any
body entry. The metadata breakpoints record the stack used at the call and at the deepest decoder frame,
named after the nearest Wonder frame above them. The decoder recursion itself is only sampled when
WONDER_STACK_DECODER_REGEX is set (e.g. `TypeDecoder.*decodeMangledType`): a Python callback on every decoder
frame slows the app by orders of magnitude, so by default the guard records the length of each requested
mangled type name instead. The decoder's stack grows with the nesting of that name, so a longer name is a
deeper decode; it is a proxy, not a measured depth.
"""
import os
import time

stats = {}
DECODER = os.environ.get("WONDER_STACK_DECODER_REGEX", "")


stack_top = []  # the main thread's outermost SP never changes; unwinding to it on every hit starves the app


def main_stack_top(thread):
    if not stack_top:
        stack_top.append(thread.GetFrameAtIndex(thread.GetNumFrames() - 1).GetSP())
    return stack_top[0]


def hit(frame, bp_loc, extra, internal_dict):
    thread = frame.GetThread()
    if thread.GetIndexID() != 1:  # main thread only
        return False
    depth = main_stack_top(thread) - frame.GetSP()
    caller = thread.GetFrameAtIndex(1).GetSP() if depth else frame.GetSP()
    name = (frame.GetFunctionName() or "?")[:90]
    entry = stats.setdefault(name, [0, 0, 0])
    entry[0] += 1
    entry[1] = max(entry[1], depth)
    entry[2] = max(entry[2], caller - frame.GetSP())
    return False


metadata = {"hits": 0, "depth": 0, "decoder": 0, "view": "?", "name_len": 0, "name_view": "?"}
CALL = "swift_getTypeByMangledNameInContext2"


def nearest_wonder_frame(thread, total):
    for index in range(1, min(total, 400)):
        name = thread.GetFrameAtIndex(index).GetFunctionName() or ""
        if name.startswith("Wonder"):
            return name[:90]
    return "?"


def hit_metadata(frame, bp_loc, extra, internal_dict):
    thread = frame.GetThread()
    if thread.GetIndexID() != 1:
        return False
    depth = main_stack_top(thread) - frame.GetSP()
    metadata["hits"] += 1
    if frame.GetFunctionName() == CALL:
        # Entry of a metadata request; the decoder recurses below this point.
        length = frame.FindRegister("x1").GetValueAsUnsigned()  # arm64: (name, length, context, generic args)
        if length > metadata["name_len"] and length < 1 << 20:
            metadata["name_len"] = length
            metadata["name_view"] = nearest_wonder_frame(thread, thread.GetNumFrames())
        if depth > metadata["depth"]:
            metadata["depth"] = depth
            metadata["view"] = nearest_wonder_frame(thread, thread.GetNumFrames())
    elif depth > metadata["decoder"]:
        metadata["decoder"] = depth  # deepest TypeDecoder frame seen: the real stack the demangler used
    return False


def run(debugger, command, result, internal_dict):
    target = debugger.GetSelectedTarget()
    process = target.GetProcess()
    points = []
    for regex in (r"^Wonder\..*\.body\.getter", r"^Wonder\..*\.body\.modify"):
        points.append(target.BreakpointCreateByRegex(regex))
        points[-1].SetScriptCallbackFunction("stack_depth_lldb.hit")
    points.append(target.BreakpointCreateByName(CALL))
    points[-1].SetScriptCallbackFunction("stack_depth_lldb.hit_metadata")
    # The recursion itself: sampling every decoder frame is slow, so it is opt-in per run.
    if DECODER:
        points.append(target.BreakpointCreateByRegex(DECODER))
        points[-1].SetScriptCallbackFunction("stack_depth_lldb.hit_metadata")
    # Every body and metadata lookup stops the main thread in a Python callback, which starves the
    # accessibility queries the driver uses to find its screen. With an arm file the breakpoints stay
    # off until the driver has the screen it wants, then sampling covers the transition it triggers.
    arm = os.environ.get("WONDER_STACK_ARM_FILE")
    armed = not arm
    for point in points:
        point.SetEnabled(armed)
    debugger.SetAsync(True)
    process.Continue()
    deadline = time.monotonic() + float(command or 20)
    done = os.environ.get("WONDER_STACK_DONE_FILE")  # the driver touches it once its steps are over
    while time.monotonic() < deadline and not (done and os.path.exists(done)):
        if not armed and os.path.exists(arm):
            armed = True
            process.Stop()  # change breakpoints with the process stopped, not under a running app
            time.sleep(1)
            for point in points:
                point.SetEnabled(True)
            process.Continue()
            print(f"ARMED after {time.monotonic() - (deadline - float(command or 20)):.0f}s, state={process.GetState()}")
        time.sleep(0.5)
    process.Stop()
    time.sleep(1)
    rows = sorted(stats.items(), key=lambda kv: -kv[1][1])[:8]
    for name, (hits, depth, frame_bytes) in rows:
        print(f"DEPTH {depth} {frame_bytes} {name}")
    print(f"PEAK {rows[0][1][1] if rows else 0} hits={sum(v[0] for v in stats.values())}")
    print(f"METADATA {max(metadata['depth'], metadata['decoder'])} hits={metadata['hits']} call={metadata['depth']} "
          f"decoder={metadata['decoder']} {metadata['view']}")
    print(f"TYPENAME {metadata['name_len']} {metadata['name_view']}")


def __lldb_init_module(debugger, internal_dict):
    debugger.HandleCommand("command script add -f stack_depth_lldb.run stackrun")

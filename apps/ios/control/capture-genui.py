#!/usr/bin/env python3
"""Capture HTML fixtures with wonderctl; assert WebKit content through XCTest.

AXe omits WebKit's remote accessibility tree. Pass the exact agent-device launcher,
config/session and UDID returned by T3 device_open; no browser/DOM mock is used.
"""

import argparse, json, os, re, shutil, subprocess, time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--output", type=Path, required=True)
    p.add_argument("--phase", choices=["before", "after"], required=True)
    p.add_argument("--agent-device", required=True)
    p.add_argument("--device-config", required=True)
    p.add_argument("--device-session", required=True)
    p.add_argument("--udid", required=True)
    p.add_argument("--simulator-suffix", default="genui")
    p.add_argument("--appearance", choices=["light", "dark"], default="light")
    p.add_argument("--only")
    p.add_argument("--extra-page")
    p.add_argument("--extra-title")
    a = p.parse_args()
    os.environ["WONDER_CONTROL_SIMULATOR_SUFFIX"] = a.simulator_suffix
    manifest = json.loads((a.output / "corpus/manifest.json").read_text())
    if a.extra_page:
        manifest.append({"name": a.extra_page, "title": a.extra_title or a.extra_page})
    if a.only:
        manifest = [x for x in manifest if x["name"] in a.only.split(",")]
    assert manifest, "No selected pages"
    folder = (
        a.output / "captures" / (a.phase + ("-dark" if a.appearance == "dark" else ""))
    )
    folder.mkdir(parents=True, exist_ok=True)
    device = [a.agent_device]
    flags = [
        "--platform",
        "ios",
        "--udid",
        a.udid,
        "--config",
        a.device_config,
        "--session",
        a.device_session,
    ]

    def run(cmd, timeout=60):
        r = subprocess.run(
            cmd, cwd=ROOT, capture_output=True, text=True, timeout=timeout
        )
        if r.returncode:
            raise RuntimeError(
                f"{cmd[0]} returned {r.returncode}: {r.stdout}\n{r.stderr}"
            )
        return r.stdout

    def snapshot():
        return run(device + ["snapshot", "-i"] + flags)

    def shot(name):
        result = run([str(ROOT / "scripts/wonderctl"), "shot", name])
        path = Path(result.strip().splitlines()[-1])
        path = path if path.is_absolute() else ROOT / path
        target = folder / (name + ".png")
        shutil.copy2(path, target)
        return str(target)

    results = []
    for page in manifest:
        name = page["name"]
        started = time.monotonic()
        result = {"name": name, "title": page["title"], "appearance": a.appearance}
        try:
            scenario = folder / "current.wctl"
            scenario.write_text(
                f"launch --fixture read-preview --fixture send-preview --fixture html-render-preview --arg=-html-render-page --arg={name} --arg=-diagnostics-theme --arg=cupola-{a.appearance} --arg=-diagnostics-font --arg=system\nwait --id html-render-page --timeout 30\nreveal down --id html-render-expand --in conversation-scroll\ntap --id html-render-expand\nwait --id expanded-message --timeout 10\n"
            )
            run([str(ROOT / "scripts/wonderctl"), "run", str(scenario)], timeout=90)
            run(device + ["open", "com.swaymun.wonder"] + flags)
            deadline = time.monotonic() + 20
            while True:
                tree = snapshot()
                if page["title"] in tree:
                    break
                if time.monotonic() > deadline:
                    raise RuntimeError(
                        "Rendered heading absent from XCTest snapshot: " + tree
                    )
                time.sleep(0.25)
            # These are rendered results, not merely a successful tap or source text.
            if name == "genui-js-table":
                assert "Apples" in tree and "Plums" in tree, tree
            if name == "genui-cdn-chart":
                assert (
                    "External chart unavailable offline." in tree
                    and "Chart loaded" not in tree
                ), tree
            if name == "genui-dynamic-height":
                while (
                    "Delayed section is visible." not in tree
                    and time.monotonic() < deadline
                ):
                    tree = snapshot()
                assert "Delayed section is visible." in tree, tree
            if name == "genui-forms" and a.phase == "after":
                ref = next(
                    re.match(r"(@e\d+)", line).group(1)
                    for line in tree.splitlines()
                    if "[button]" in line and '"Save locally"' in line
                )
                run(device + ["click", ref] + flags)
                tree = snapshot()
                assert "Saved locally: Alex" in tree, tree
            (folder / (name + ".txt")).write_text(tree)
            result["capture"] = shot(
                "genui-" + a.phase + "-" + name.removeprefix("genui-")
            )
            if name == "genui-wide-table" and a.phase == "after":
                ref = next(
                    re.match(r"(@e\d+)", line).group(1)
                    for line in tree.splitlines()
                    if "[text]" in line and '"North"' in line
                )
                attrs = run(device + ["get", "attrs", ref] + flags)
                (folder / "wide-table-attrs.txt").write_text(attrs)
                cell = json.loads(attrs)["rect"]
                webref = next(
                    re.match(r"(@e\d+)", line).group(1)
                    for line in tree.splitlines()
                    if "[webview]" in line and "expanded-message" in line
                )
                frame = json.loads(run(device + ["get", "attrs", webref] + flags))[
                    "rect"
                ]
                y = round(cell["y"] + cell["height"] / 2)
                run(
                    device
                    + [
                        "swipe",
                        str(round(frame["x"] + frame["width"] * 0.9)),
                        str(y),
                        str(round(frame["x"] + frame["width"] * 0.1)),
                        str(y),
                    ]
                    + flags
                )
                shifted = snapshot()
                assert '"Jun"' in shifted, shifted
                (folder / "wide-table-scrolled.txt").write_text(shifted)
                result["scrolledCapture"] = shot("genui-after-wide-table-scrolled")
            result["result"] = "passed"
        except Exception as e:
            result["result"] = "failed"
            result["error"] = str(e)
            try:
                result["capture"] = shot("genui-" + a.phase + "-" + name + "-failed")
            except Exception:
                pass
        result["seconds"] = round(time.monotonic() - started, 2)
        results.append(result)
        (folder / "results.json").write_text(json.dumps(results, indent=2) + "\n")
        print(name, result["result"], result.get("error", ""), flush=True)
    print(
        f"{sum(r['result'] == 'passed' for r in results)}/{len(results)} passed; {folder}",
        flush=True,
    )
    raise SystemExit(any(r["result"] != "passed" for r in results))


if __name__ == "__main__":
    main()

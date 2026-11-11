#!/usr/bin/env python3
"""Check UX6's native AX captures, including positive before/revealed controls.

Only synthetic acceptance values are inspected. Pass the preview's phase folder;
the checker requires all four device layouts rather than accepting a partial set.
"""
import argparse
import base64
import json
from pathlib import Path

DEVICES = ("iphone", "ipad", "duo-folded", "duo-unfolded")
SECRETS = ("192.168.50.20", "fixture.tail000000.ts.net",
           base64.urlsafe_b64encode(bytes([7]) * 32).decode().rstrip("="))
RELAY = "relay.example.invalid"


def nodes(value):
    if isinstance(value, dict):
        yield value
        for child in value.values():
            yield from nodes(child)
    elif isinstance(value, list):
        for child in value:
            yield from nodes(child)


def read(path):
    tree = list(nodes(json.loads(path.read_text())))
    text = "\n".join(str(node.get(key) or "") for node in tree
                     for key in ("AXLabel", "AXValue", "AXHint"))
    return tree, text


def check(folder, state):
    for device in DEVICES:
        screen = "connection-details-revealed" if state == "revealed" else "connection-details"
        path = folder / f"{screen}-{device}.ax.json"
        tree, text = read(path)
        exposed = state in {"before", "revealed"}
        for secret in SECRETS:
            assert (secret in text) == exposed, f"{path}: expected {state} secret state"
        assert (RELAY in text) == (state == "before"), f"{path}: relay host exposure"
        if state == "hidden":
            for kind in ("home", "tailscale", "key"):
                controls = [node for node in tree if node.get("AXUniqueId") == f"connection-reveal:{kind}"]
                assert len(controls) == 1 and controls[0].get("AXValue") == "Hidden", \
                    f"{path}: {kind} must report Hidden to accessibility"
        print(f"PASS {state} {device}: three secrets {'visible' if exposed else 'hidden'}; "
              f"relay {'visible in baseline' if state == 'before' else 'absent'}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("state", choices=("before", "hidden", "revealed"))
    parser.add_argument("folder", type=Path)
    args = parser.parse_args()
    check(args.folder, args.state)


if __name__ == "__main__":
    main()

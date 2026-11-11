#!/usr/bin/env python3
"""Check that a tool's control panel shares the chat composer's baseline.

On a split (Duo unfolded, iPad) the chat composer is on one pane and the
computer bar or the terminal keys on the other. From one screenshot this
measures, at a column inside each, the run of non-page pixels that touches the
bottom: its bottom edge, its height and, with --radius-rows, the corner curve's
inset. Edges must agree within one pixel (--tolerance), which is the 1 px
baseline check of UX-DUO-4.

  measure-tool-panel-baseline.py SHOT.png --composer-x 200 --panel-x 700 [--scale 3]

x values are in points. The page colour is the most common colour of the
screenshot's bottom row, so run it on a capture whose bottom edge is the page.
With the software keyboard up, the bottom row is the keyboard: pass
--page-at X,Y (points, a page pixel) and --floor Y (points, just above the
keyboard's top) so the scan starts above it.
"""
import argparse
from collections import Counter
from PIL import Image


def run_at(im, x, page, floor):
    """The bottom edge and height of the non-page run that ends at/above `floor`."""
    px = im.load()
    width, height = im.size
    x = min(max(0, x), width - 1)
    bottom = None
    for y in range(floor, -1, -1):
        if px[x, y] != page:
            bottom = y
            break
    if bottom is None:
        return None
    top = bottom
    while top > 0 and px[x, top - 1] != page:
        top -= 1
    return bottom, top


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("shot")
    parser.add_argument("--composer-x", type=float, required=True)
    parser.add_argument("--panel-x", type=float, required=True)
    parser.add_argument("--scale", type=float, default=3)
    parser.add_argument("--page-at", help="X,Y in points of a page-coloured pixel")
    parser.add_argument("--floor", type=float, help="Y in points where the upward scan starts")
    parser.add_argument("--tolerance", type=int, default=1, help="pixels")
    args = parser.parse_args()
    im = Image.open(args.shot).convert("RGB")
    width, height = im.size
    px = im.load()
    if args.page_at:
        page_x, page_y = (float(v) for v in args.page_at.split(","))
        page = px[round(page_x * args.scale), round(page_y * args.scale)]
    else:
        page = Counter(px[x, height - 1] for x in range(0, width, 4)).most_common(1)[0][0]
    floor = round(args.floor * args.scale) if args.floor else height - 1
    result = {}
    for name, x in (("composer", args.composer_x), ("panel", args.panel_x)):
        found = run_at(im, round(x * args.scale), page, floor)
        if found is None:
            raise SystemExit(f"{name}: nothing but page colour at x={x}")
        bottom, top = found
        result[name] = {"bottom": bottom, "top": top, "height": bottom - top + 1}
        print(f"{name}: bottom={bottom}px top={top}px height={bottom - top + 1}px "
              f"({(bottom - top + 1) / args.scale:.1f} pt)")
    bad = [key for key in ("bottom", "height") if abs(result["composer"][key] - result["panel"][key]) > args.tolerance]
    if bad:
        raise SystemExit("FAIL: " + ", ".join(f"{key} differs by {abs(result['composer'][key] - result['panel'][key])}px" for key in bad))
    print("PASS: the panel shares the composer's bottom edge and height")


if __name__ == "__main__":
    main()

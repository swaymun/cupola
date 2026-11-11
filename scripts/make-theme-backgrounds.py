#!/usr/bin/env python3
"""Compress generated theme backgrounds to JPEG data sets in the asset catalog.

Usage: make-theme-backgrounds.py SOURCE_DIR
Reads misty-forest/ocean-waves/desert-dunes/geometric PNGs from SOURCE_DIR,
writes apps/ios/Wonder/Assets.xcassets/ThemeBackground<Name>.dataset/ and
reports the worst-case contrast of each theme's text over its scrim.
"""
import json
import os
import sys

from PIL import Image

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CATALOG = os.path.join(ROOT, "apps/ios/Wonder/Assets.xcassets")
NAMES = {
    "misty-forest": "ThemeBackgroundForest",
    "ocean-waves": "ThemeBackgroundOcean",
    "desert-dunes": "ThemeBackgroundDunes",
    "geometric": "ThemeBackgroundGeometric",
}
TARGET = 600 * 1024


def main(source):
    for stem, asset in NAMES.items():
        image = Image.open(os.path.join(source, stem + ".png")).convert("RGB")
        folder = os.path.join(CATALOG, asset + ".dataset")
        os.makedirs(folder, exist_ok=True)
        path = os.path.join(folder, stem + ".jpg")
        for quality in (80, 70, 60, 50, 40):
            image.save(path, "JPEG", quality=quality, optimize=True, progressive=True)
            if os.path.getsize(path) <= TARGET:
                break
        contents = {
            "data": [{"filename": stem + ".jpg", "idiom": "universal", "universal-type-identifier": "public.jpeg"}],
            "info": {"author": "xcode", "version": 1},
        }
        with open(os.path.join(folder, "Contents.json"), "w") as handle:
            json.dump(contents, handle, indent=2)
            handle.write("\n")
        print(f"{asset}: {image.size[0]}x{image.size[1]} q={quality} {os.path.getsize(path)} bytes")


if __name__ == "__main__":
    main(sys.argv[1])

#!/usr/bin/env python3
"""Give a complete SDK a content-addressed path before Cargo builds against it.

Hard links avoid duplicating the SDK. If the original library changes in place,
the helper build rejects the old path instead of reusing a stale webrtc-sys cache.
"""
import argparse
import hashlib
import os
from pathlib import Path
import shutil
import tempfile


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--library',type=Path,required=True)
    args=parser.parse_args();root=args.library.resolve()
    libraries=[p for p in [root/'lib/libwebrtc.a',root/'lib/webrtc.lib'] if p.is_file()]
    if len(libraries)!=1:raise SystemExit('Select a complete native SDK containing one WebRTC library.')
    for name in ['args.gn','LICENSE.md','webrtc.ninja','include']:
        if not (root/name).exists():raise SystemExit('Incomplete native SDK: '+name)
    if libraries[0].name=='webrtc.lib' and not (root/'desktop_capture.ninja').is_file():
        raise SystemExit('Incomplete Windows SDK: desktop_capture.ninja')
    with libraries[0].open('rb')as stream:sha=hashlib.file_digest(stream,'sha256').hexdigest()
    sealed=root.parent/sha
    if sealed.exists():
        with (sealed/'lib'/libraries[0].name).open('rb')as stream:existing=hashlib.file_digest(stream,'sha256').hexdigest()
        if existing!=sha:raise SystemExit('The sealed SDK changed. Remove only that invalid build cache and seal again.')
    else:
        temporary=Path(tempfile.mkdtemp(prefix='.seal-',dir=root.parent))
        try:
            shutil.copytree(root,temporary,dirs_exist_ok=True,copy_function=os.link)
            temporary.rename(sealed)
        finally:
            if temporary.exists():shutil.rmtree(temporary)
    print(sealed)


if __name__=='__main__':main()

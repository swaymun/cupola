#!/usr/bin/env python3
"""Test-only 30 fps baseline; the production Python helper is loaded unchanged."""
import asyncio
import importlib.util
import os
from pathlib import Path
import sys
import time

script=Path(os.environ['WONDER_PARITY_PYTHON_HELPER'])
spec=importlib.util.spec_from_file_location('parity_python_helper',script)
helper=importlib.util.module_from_spec(spec)
spec.loader.exec_module(helper)
helper.QUALITY_FPS['benchmark30']=30
if sys.platform=='win32' and os.environ.get('WONDER_PARITY_CAPTURE_PROFILE')=='1':
    # Observe distinct WGC arrays in the joined executor, avoiding I/O in
    # the native callback while Python may be shutting down.
    original=helper.Grabber.grab
    def grab(capture):
        pixels=original(capture)
        now=time.monotonic()
        if not hasattr(capture,'_parity_profile'):
            capture._parity_profile=dict(start=now,reads=0,changed=0,last=None)
        profile=capture._parity_profile;profile['reads']+=1
        if pixels is not profile['last']:
            profile['changed']+=1;profile['last']=pixels
        if now-profile['start']>=1:
            print('WONDER_PARITY_CAPTURE '+__import__('json').dumps(dict(reads=profile['reads'],changed=profile['changed'],seconds=now-profile['start'])),file=sys.stderr,flush=True)
            profile.update(start=now,reads=0,changed=0)
        return pixels
    helper.Grabber.grab=grab
if '--discover-sources' in sys.argv:helper.discover_sources()
else:asyncio.run(helper.main())

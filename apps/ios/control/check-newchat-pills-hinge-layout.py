#!/usr/bin/env python3
"""New chat's setup pills with a hinge across the row: see check-newchat-pills-layout.py."""
import os
import runpy
from pathlib import Path

os.environ["PILLS_HINGE"] = "1"
runpy.run_path(str(Path(__file__).with_name("check-newchat-pills-layout.py")), run_name="__main__")

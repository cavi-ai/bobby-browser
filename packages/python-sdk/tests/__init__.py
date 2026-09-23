"""Make `bobby_browser` importable for `python3 -m unittest discover -s
packages/python-sdk/tests` run straight from a checkout, with no `pip
install` step first (that's what CI's `node · typescript · docs` job runs,
and what scripts/check-version-agreement.py-adjacent gates expect to work
unaided). `packages/python-sdk` (this file's parent) is the source package
root; add it to `sys.path` ahead of anything already installed under that
name so the checkout's own source is what gets tested.
"""

import sys
from pathlib import Path

_PACKAGE_ROOT = str(Path(__file__).resolve().parent.parent)
if _PACKAGE_ROOT not in sys.path:
    sys.path.insert(0, _PACKAGE_ROOT)

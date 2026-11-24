"""Offline unit test suite for turbotap.

Ensures the local checkout's src/ package is always imported first when running
`python -m unittest discover -s tests -v`, even without PYTHONPATH=src or an
editable install.
"""
import os
import sys

_SRC_DIR = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "src"))
if _SRC_DIR not in sys.path:
    sys.path.insert(0, _SRC_DIR)
elif sys.path[0] != _SRC_DIR:
    sys.path.remove(_SRC_DIR)
    sys.path.insert(0, _SRC_DIR)

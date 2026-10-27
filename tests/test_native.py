"""Run on native CI; inspect native layouts without posting any keystrokes."""

import ctypes
import sys
from types import SimpleNamespace

import pytest

from whisp.desktop import Mac, Target, Windows


@pytest.mark.skipif(sys.platform != "win32", reason="Win32 ABI")
def test_windows_input_layout_and_ctrl_v_batch():
    native = Windows()
    captured = []

    class SendInput:
        def __call__(self, count, events, size):
            captured.append(
                (count, size, [(e.payload.ki.wVk, e.payload.ki.dwFlags) for e in events])
            )
            return count

    native.user = SimpleNamespace(SendInput=SendInput())
    target = Target(5, 6, 7)
    native.capture = lambda: target
    native.modifiers_down = lambda: False
    native.paste(target)
    assert captured == [
        (
            4,
            40 if ctypes.sizeof(ctypes.c_void_p) == 8 else 28,
            [(0x11, 0), (0x56, 0), (0x56, 2), (0x11, 2)],
        )
    ]


@pytest.mark.skipif(sys.platform != "darwin", reason="macOS frameworks")
def test_mac_accessibility_and_event_symbols_exist():
    native = Mac()
    for symbol in (
        "AXIsProcessTrusted",
        "AXUIElementCreateApplication",
        "AXUIElementCopyAttributeValue",
        "kAXFocusedWindowAttribute",
        "kAXFocusedUIElementAttribute",
    ):
        assert hasattr(native.ax, symbol), symbol
    for symbol in ("CGEventCreateKeyboardEvent", "CGEventPostToPid", "CGEventSourceFlagsState"):
        assert hasattr(native.q, symbol), symbol

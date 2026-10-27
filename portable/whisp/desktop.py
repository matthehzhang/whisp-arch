"""Native focus checks. Never steal focus to paste into a different window."""

import os
import sys
from dataclasses import dataclass


@dataclass(frozen=True)
class Target:
    pid: int
    window: object
    field: object


class Desktop:
    def __init__(self):
        self.native = (
            Windows() if sys.platform == "win32" else Mac() if sys.platform == "darwin" else None
        )

    def capture(self):
        try:
            target = self.native.capture() if self.native else None
            return target if target and target.pid != os.getpid() else None
        except Exception:
            # Permissions, protected fields, or unavailable accessibility APIs:
            # retain the transcript rather than guessing a paste destination.
            return None

    def copy(self, text: str):
        import pyperclip

        pyperclip.copy(text)

    def deliver(self, text: str, target, auto_paste: bool) -> str:
        self.copy(text)
        if not auto_paste:
            return "Copied to clipboard."
        if target is None or self.native is None:
            return "Copied. Paste into your app when ready."
        if self.native.modifiers_down():
            return "Copied. Release modifier keys, then paste."
        current = self.capture()
        if current is None or not self.native.same(target, current):
            return "Copied. The destination changed; paste when ready."
        self.native.paste(target)
        return "Paste sent. Transcript is also on the clipboard."


class Windows:
    def __init__(self):
        import ctypes as c
        from ctypes import wintypes as w

        self.c, self.w = c, w
        self.user = c.WinDLL("user32", use_last_error=True)
        self.user.GetForegroundWindow.restype = w.HWND
        self.user.GetWindowThreadProcessId.argtypes = (w.HWND, c.POINTER(w.DWORD))
        self.user.GetWindowThreadProcessId.restype = w.DWORD
        self.user.GetAsyncKeyState.argtypes = (c.c_int,)
        self.user.GetAsyncKeyState.restype = c.c_short

        class GUIThreadInfo(c.Structure):
            _fields_ = [
                ("cbSize", w.DWORD),
                ("flags", w.DWORD),
                ("hwndActive", w.HWND),
                ("hwndFocus", w.HWND),
                ("hwndCapture", w.HWND),
                ("hwndMenuOwner", w.HWND),
                ("hwndMoveSize", w.HWND),
                ("hwndCaret", w.HWND),
                ("rcCaret", w.RECT),
            ]

        self.GUIThreadInfo = GUIThreadInfo
        self.user.GetGUIThreadInfo.argtypes = (w.DWORD, c.POINTER(GUIThreadInfo))
        self.user.GetGUIThreadInfo.restype = w.BOOL

    def capture(self):
        c, w = self.c, self.w
        hwnd = self.user.GetForegroundWindow()
        if not hwnd:
            return None
        pid = w.DWORD()
        thread_id = self.user.GetWindowThreadProcessId(hwnd, c.byref(pid))
        info = self.GUIThreadInfo(cbSize=c.sizeof(self.GUIThreadInfo))
        if not thread_id or not self.user.GetGUIThreadInfo(thread_id, c.byref(info)):
            return None
        if not info.hwndFocus:
            return None
        return Target(pid.value, hwnd, info.hwndFocus)

    @staticmethod
    def same(first, second):
        return first == second

    def modifiers_down(self):
        return any(self.user.GetAsyncKeyState(k) & 0x8000 for k in (0x10, 0x11, 0x12, 0x5B, 0x5C))

    def paste(self, target):
        c, w = self.c, self.w

        class Keyboard(c.Structure):
            _fields_ = [
                ("wVk", w.WORD),
                ("wScan", w.WORD),
                ("dwFlags", w.DWORD),
                ("time", w.DWORD),
                ("dwExtraInfo", c.c_size_t),
            ]

        class Mouse(c.Structure):
            _fields_ = [
                ("dx", w.LONG),
                ("dy", w.LONG),
                ("mouseData", w.DWORD),
                ("dwFlags", w.DWORD),
                ("time", w.DWORD),
                ("dwExtraInfo", c.c_size_t),
            ]

        class Hardware(c.Structure):
            _fields_ = [("uMsg", w.DWORD), ("wParamL", w.WORD), ("wParamH", w.WORD)]

        class Payload(c.Union):
            _fields_ = [("ki", Keyboard), ("mi", Mouse), ("hi", Hardware)]

        class Input(c.Structure):
            _fields_ = [("type", w.DWORD), ("payload", Payload)]

        self.user.SendInput.argtypes = (w.UINT, c.POINTER(Input), c.c_int)
        self.user.SendInput.restype = w.UINT
        # One batch so user keystrokes cannot interleave between Ctrl and V.
        events = (Input * 4)(
            *(
                Input(1, Payload(ki=Keyboard(key, 0, flags, 0, 0)))
                for key, flags in ((0x11, 0), (0x56, 0), (0x56, 2), (0x11, 2))
            )
        )
        if self.capture() != target or self.modifiers_down():
            raise RuntimeError("Destination changed. Transcript is on the clipboard.")
        if self.user.SendInput(4, events, c.sizeof(Input)) != 4:
            # Release our synthetic modifiers if Windows accepted only part.
            releases = (Input * 2)(events[2], events[3])
            self.user.SendInput(2, releases, c.sizeof(Input))
            raise RuntimeError(
                "Windows blocked paste. Paste manually (especially into administrator apps)."
            )


class Mac:
    def __init__(self):
        import ApplicationServices as ax
        import CoreFoundation
        import Quartz

        self.ax, self.q, self.cf = ax, Quartz, CoreFoundation

    def _attribute(self, element, name):
        error, value = self.ax.AXUIElementCopyAttributeValue(element, name, None)
        if error:
            return None
        return value

    def capture(self):
        from AppKit import NSWorkspace

        if not self.ax.AXIsProcessTrusted():
            return None
        app = NSWorkspace.sharedWorkspace().frontmostApplication()
        if app is None:
            return None
        pid = app.processIdentifier()
        element = self.ax.AXUIElementCreateApplication(pid)
        window = self._attribute(element, self.ax.kAXFocusedWindowAttribute)
        field = self._attribute(element, self.ax.kAXFocusedUIElementAttribute)
        if window is None or field is None:
            return None
        return Target(pid, window, field)

    def same(self, first, second):
        return (
            first.pid == second.pid
            and self.cf.CFEqual(first.window, second.window)
            and self.cf.CFEqual(first.field, second.field)
        )

    def modifiers_down(self):
        q = self.q
        mask = (
            q.kCGEventFlagMaskShift
            | q.kCGEventFlagMaskControl
            | q.kCGEventFlagMaskAlternate
            | q.kCGEventFlagMaskCommand
        )
        return bool(q.CGEventSourceFlagsState(q.kCGEventSourceStateCombinedSessionState) & mask)

    def paste(self, target):
        current = self.capture()
        if current is None or not self.same(target, current) or self.modifiers_down():
            raise RuntimeError("Destination changed. Transcript is on the clipboard.")
        q = self.q
        for pressed in (True, False):
            event = q.CGEventCreateKeyboardEvent(None, 9, pressed)  # physical V
            q.CGEventSetFlags(event, q.kCGEventFlagMaskCommand)
            q.CGEventPostToPid(target.pid, event)

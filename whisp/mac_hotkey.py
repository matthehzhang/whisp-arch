"""Fn/Globe events on Apple keyboards; never confuse Fn with F1-F12."""

import threading

FN_KEYCODE = 63
SPACE_KEYCODE = 49
ESC_KEYCODE = 53
FN_FLAG = 1 << 23


class FnGesture:
    """Pure key-state logic shared by the event tap and regression tests."""

    def __init__(self, mode="hold"):
        self.mode = mode
        self.down = False
        self.latched = False
        self.space_down = False
        self.suppress_release = False

    def fn(self, down):
        if down == self.down:
            return None
        self.down = down
        if down:
            if self.latched:
                self.latched = False
                self.suppress_release = True
                return "release"
            if self.mode == "toggle":
                self.latched = True
            return "press"
        if self.suppress_release:
            self.suppress_release = False
            return None
        return None if self.latched else "release"

    def space(self, down):
        was_down, self.space_down = self.space_down, down
        if not down or was_down or not self.down:
            return None
        self.latched = True
        return "lock"

    def cancel(self):
        self.latched = False
        self.suppress_release = self.down
        return "cancel"

    def reset_session(self):
        self.latched = False
        self.suppress_release = self.down


class FnHotkey:
    def __init__(self, post, capture_target, mode="hold"):
        self.post, self.capture_target = post, capture_target
        self.gesture = FnGesture(mode)
        self.guard = threading.Lock()
        self.stopping = threading.Event()
        self.ready = threading.Event()
        self.error = None
        self.listener = threading.Thread(target=self._run, name="whisp-fn", daemon=True)

    def start(self):
        self.listener.start()
        if not self.ready.wait(2):
            self.stopping.set()
            raise RuntimeError(
                "Keyboard listener did not start. Check Accessibility and Input Monitoring."
            )
        if self.error:
            raise RuntimeError(self.error)

    def stop(self):
        self.stopping.set()

    def reset_session(self):
        with self.guard:
            self.gesture.reset_session()

    def _run(self):
        import ApplicationServices as ax
        import CoreFoundation as cf
        import Quartz as q

        tap = None
        try:
            if not ax.AXIsProcessTrusted() or not q.CGPreflightListenEventAccess():
                raise RuntimeError("Allow Accessibility and Input Monitoring in setup.")

            def callback(proxy, event_type, event, info):
                if event_type in (q.kCGEventTapDisabledByTimeout, q.kCGEventTapDisabledByUserInput):
                    q.CGEventTapEnable(tap, True)
                    return event
                key = q.CGEventGetIntegerValueField(event, q.kCGKeyboardEventKeycode)
                action, consume = None, False
                with self.guard:
                    if event_type == q.kCGEventFlagsChanged and key == FN_KEYCODE:
                        action = self.gesture.fn(bool(q.CGEventGetFlags(event) & FN_FLAG))
                        consume = True
                    elif (
                        event_type in (q.kCGEventKeyDown, q.kCGEventKeyUp) and key == SPACE_KEYCODE
                    ):
                        consume = self.gesture.down or self.gesture.space_down
                        action = self.gesture.space(event_type == q.kCGEventKeyDown)
                    elif event_type == q.kCGEventKeyDown and key == ESC_KEYCODE:
                        action = self.gesture.cancel()
                if action:
                    target = self.capture_target() if action == "release" else None
                    self.post(action, target)
                # Consume only the dedicated Fn control and Fn+Space, never normal typing.
                return None if consume else event

            mask = sum(
                1 << kind for kind in (q.kCGEventFlagsChanged, q.kCGEventKeyDown, q.kCGEventKeyUp)
            )
            tap = q.CGEventTapCreate(
                q.kCGSessionEventTap,
                q.kCGHeadInsertEventTap,
                q.kCGEventTapOptionDefault,
                mask,
                callback,
                None,
            )
            if tap is None:
                raise RuntimeError(
                    "macOS blocked the shortcut. Allow Accessibility and Input Monitoring."
                )
            source = cf.CFMachPortCreateRunLoopSource(None, tap, 0)
            loop = cf.CFRunLoopGetCurrent()
            cf.CFRunLoopAddSource(loop, source, cf.kCFRunLoopCommonModes)
            q.CGEventTapEnable(tap, True)
            self.ready.set()
            while not self.stopping.is_set():
                cf.CFRunLoopRunInMode(cf.kCFRunLoopDefaultMode, 0.2, False)
            cf.CFRunLoopRemoveSource(loop, source, cf.kCFRunLoopCommonModes)
        except Exception as exc:
            self.error = str(exc)
            self.ready.set()
        finally:
            if tap is not None:
                q.CGEventTapEnable(tap, False)
                cf.CFMachPortInvalidate(tap)

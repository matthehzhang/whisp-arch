"""Callbacks enqueue only; no recording or transcription in the OS hook."""

import threading


class KeyHotkey:
    def __init__(self, name, post, capture_target, mode="hold"):
        from pynput import keyboard

        self.keyboard = keyboard
        self.key = getattr(keyboard.Key, name.lower())
        self.post = post
        self.capture_target = capture_target
        self.mode = mode
        self.latched = False
        self.held = False
        self.lock = threading.Lock()
        self.listener = keyboard.Listener(on_press=self._press, on_release=self._release)

    def start(self):
        if not self.listener.IS_TRUSTED:
            raise RuntimeError(
                "Allow Whisp in Accessibility and Input Monitoring, then restart Whisp."
            )
        self.listener.start()

    def stop(self):
        self.listener.stop()

    def _press(self, key):
        if key == self.keyboard.Key.esc:
            self.latched = False
            self.post("cancel", None)
        elif key == self.key:
            with self.lock:
                if self.held:
                    return
                self.held = True
            if self.mode == "toggle":
                self.latched = not self.latched
                self.post(
                    "press" if self.latched else "release",
                    None if self.latched else self.capture_target(),
                )
            else:
                self.post("press", None)

    def _release(self, key):
        if key == self.key:
            with self.lock:
                if not self.held:
                    return
                self.held = False
            if self.mode != "toggle":
                self.post("release", self.capture_target())

    def reset_session(self):
        self.latched = False


def Hotkey(name, post, capture_target, mode="hold"):
    if name == "Fn / Globe":
        from .mac_hotkey import FnHotkey

        return FnHotkey(post, capture_target, mode)
    return KeyHotkey(name, post, capture_target, mode)

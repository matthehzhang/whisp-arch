"""Callbacks enqueue only; no recording or transcription in the OS hook."""

import threading


class Hotkey:
    def __init__(self, name, post, capture_target):
        from pynput import keyboard

        self.keyboard = keyboard
        self.key = getattr(keyboard.Key, name.lower())
        self.post = post
        self.capture_target = capture_target
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
            self.post("cancel", None)
        elif key == self.key:
            with self.lock:
                if self.held:
                    return
                self.held = True
            self.post("press", None)

    def _release(self, key):
        if key == self.key:
            with self.lock:
                if not self.held:
                    return
                self.held = False
            self.post("release", self.capture_target())

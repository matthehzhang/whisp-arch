"""Serialized lifecycle; all methods/callbacks run on the UI thread.

Workers return via `work(job, done)`; `done(value, error)` is dispatched by
the UI queue. A cancelled session stays busy until its worker has stopped,
so an old capture or model result can never overwrite a new recording.
"""

import time
from dataclasses import dataclass


@dataclass
class Session:
    released: bool = False
    cancelled: bool = False
    target: object = None
    started: float = 0


class Controller:
    def __init__(self, recorder, engine, desktop, settings, work, later, changed, cue):
        self.recorder, self.engine, self.desktop, self.settings = (
            recorder,
            engine,
            desktop,
            settings,
        )
        self.work, self.later, self.changed, self.cue = work, later, changed, cue
        self.state = "setup"
        self.message = "Add your Deepgram API key to begin."
        self.session = None
        self.transcript = ""
        self.closed = False

    def update(self, state, message):
        self.state, self.message = state, message
        if not self.closed:
            self.changed()

    def prepare(self):
        if self.state not in ("setup", "ready"):
            return
        self.update("loading", "Checking API key…")
        self.work(self.engine.prepare, self._prepared)

    def _prepared(self, value, error):
        if error:
            self.update("setup", str(error))
        else:
            self.update("ready", "Ready. Hold the shortcut to speak.")

    def press(self):
        if self.closed or self.state != "ready":
            return
        self.session = Session()
        self.update("starting", "Opening microphone…")
        self.work(lambda: self.recorder.start(self.settings.microphone), self._started)

    def _started(self, value, error):
        if error:
            self.session = None
            self.update("ready", f"Microphone unavailable: {error}")
            return
        session = self.session
        session.started = time.monotonic()
        if session.cancelled or self.closed:
            self._stop()
            return
        self.cue(True)
        self.update("recording", "Listening… Release to transcribe. Esc cancels.")
        if session.released:
            self._tail()

    def release(self, target=None):
        if self.state not in ("starting", "recording") or self.session is None:
            return
        self.session.released = True
        self.session.target = target
        if self.state == "recording":
            self._tail()

    def _tail(self):
        session = self.session
        self.update("tail", "Finishing recording…")

        def finish():
            if self.session is session and self.state == "tail":
                self._stop()

        self.later(200, finish)

    def _stop(self):
        self.update("stopping", "Stopping microphone…")
        self.work(self.recorder.stop, self._stopped)

    def _stopped(self, samples, error):
        self.cue(False)
        session = self.session
        if session.cancelled or self.closed:
            self.session = None
            self.update("ready", "Cancelled.")
        elif error:
            self.session = None
            self.update("ready", f"Recording failed: {error}")
        else:
            self.update("transcribing", "Transcribing with Deepgram…")
            self.work(
                lambda: self.engine.transcribe(samples, lambda: session.cancelled or self.closed),
                self._transcribed,
            )

    def _transcribed(self, text, error):
        session, self.session = self.session, None
        if session.cancelled or self.closed:
            self.update("ready", "Cancelled.")
            return
        if error:
            self.update("ready", f"Transcription failed: {error}")
            return
        if not text:
            self.update("ready", "No speech detected. Try again closer to the microphone.")
            return
        self.transcript = text
        try:
            message = self.desktop.deliver(text, session.target, self.settings.auto_paste)
        except Exception as exc:
            message = f"Automatic delivery unavailable: {exc}. Use Copy transcript."
        self.update("ready", message)

    def cancel(self):
        if self.session is None:
            return
        self.session.cancelled = True
        if self.state in ("recording", "tail"):
            self._stop()
        else:
            self.update(self.state, "Cancelling…")

    def close(self):
        self.closed = True
        self.cancel()

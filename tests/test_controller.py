"""Exercise delayed hardware/ASR completions, cancellation, and target routing."""

from types import SimpleNamespace

import pytest

from whisp.config import Settings
from whisp.controller import Controller


class Harness:
    def __init__(self):
        self.jobs, self.timers, self.delivered, self.cues = [], [], [], []
        self.stop_count = 0
        self.start_error = False
        self.engine_error = False
        self.recorder = SimpleNamespace(start=self.start, stop=self.stop)
        self.engine = SimpleNamespace(prepare=lambda: None, transcribe=self.transcribe)
        self.desktop = SimpleNamespace(deliver=self.deliver)
        self.app = Controller(
            self.recorder,
            self.engine,
            self.desktop,
            Settings(),
            lambda job, done: self.jobs.append((job, done)),
            lambda delay, fn: self.timers.append(fn),
            lambda: None,
            self.cues.append,
        )
        self.app.state = "ready"

    def start(self, name):
        if self.start_error:
            raise RuntimeError("permission denied")

    def stop(self):
        self.stop_count += 1
        return "audio"

    def transcribe(self, samples, is_cancelled=lambda: False):
        assert samples == "audio"
        if self.engine_error:
            raise RuntimeError("ASR failed")
        return "The actual transcript."

    def deliver(self, text, target, paste):
        self.delivered.append((text, target, paste))
        return "copied"

    def finish_job(self):
        job, done = self.jobs.pop(0)
        try:
            result = job()
        except Exception as exc:
            done(None, exc)
        else:
            done(result, None)

    def begin(self):
        self.app.press()
        self.finish_job()

    def release(self, target="editor"):
        self.app.release(target)
        self.timers.pop(0)()
        self.finish_job()


def test_release_while_device_is_opening_is_not_lost():
    h = Harness()
    h.app.press()
    h.app.release("editor")
    h.finish_job()
    assert h.app.state == "tail"
    h.timers.pop(0)()
    h.finish_job()
    h.finish_job()
    assert h.delivered == [("The actual transcript.", "editor", True)]
    assert h.cues == [True, False]


@pytest.mark.parametrize("phase", ["starting", "recording", "tail", "stopping", "transcribing"])
def test_cancel_at_every_async_boundary_never_pastes(phase):
    h = Harness()
    h.app.press()
    if phase != "starting":
        h.finish_job()
    if phase in ("tail", "stopping", "transcribing"):
        h.app.release("editor")
    if phase in ("stopping", "transcribing"):
        h.timers.pop(0)()
    if phase == "transcribing":
        h.finish_job()
    h.app.cancel()
    while h.jobs:
        h.finish_job()
    while h.timers:
        h.timers.pop(0)()
    assert h.app.state == "ready"
    assert h.stop_count == 1
    assert not h.delivered
    assert not h.app.transcript


def test_stale_tail_cannot_stop_a_new_recording():
    h = Harness()
    h.begin()
    h.app.release("first")
    old_timer = h.timers.pop()
    h.app.cancel()
    h.finish_job()
    h.begin()
    old_timer()
    assert h.app.state == "recording"
    assert not h.jobs


def test_held_key_autorepeat_cannot_open_multiple_streams():
    h = Harness()
    h.app.press()
    h.app.press()
    assert len(h.jobs) == 1
    h.finish_job()
    h.app.press()
    assert not h.jobs


def test_microphone_failure_never_claims_recording_started():
    h = Harness()
    h.start_error = True
    h.begin()
    assert h.app.state == "ready"
    assert not h.cues
    assert h.app.session is None


def test_asr_error_preserves_previous_transcript():
    h = Harness()
    h.app.transcript = "previous"
    h.engine_error = True
    h.begin()
    h.release()
    h.finish_job()
    assert h.app.transcript == "previous"
    assert h.app.state == "ready"
    assert not h.delivered


def test_close_during_transcription_never_pastes():
    h = Harness()
    h.begin()
    h.release()
    h.app.close()
    h.finish_job()
    h.app.press()
    assert not h.delivered
    assert not h.jobs

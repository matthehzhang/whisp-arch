import numpy as np
import pytest

from whisp.audio import cue_samples, to_16k
from whisp.transcribe import Engine


@pytest.mark.parametrize("rate", [16000, 44100, 48000, 96000])
def test_resample_preserves_pitch_and_duration(rate):
    t = np.arange(rate) / rate
    wave = np.sin(2 * np.pi * 440 * t).astype(np.float32)
    result = to_16k(np.column_stack([wave, wave]), rate)
    assert len(result) == 16000
    assert result.dtype == np.float32
    peak = np.argmax(np.abs(np.fft.rfft(result)))
    assert peak == 440


def test_resampling_rejects_nonfinite_audio():
    with pytest.raises(ValueError):
        to_16k(np.array([np.nan]), 48000)


def test_short_clips_and_silence_never_run_asr(tmp_path):
    def fail():
        raise AssertionError("Silence must not access credentials or the API")

    engine = Engine(key_reader=fail)
    assert engine.transcribe(np.ones(100, np.float32)) == ""
    assert engine.transcribe(np.zeros(16000, np.float32)) == ""


@pytest.mark.parametrize("start", [True, False])
def test_cues_are_short_bounded_and_fade_at_edges(start):
    cue = cue_samples(start, 48000)
    assert len(cue) == 7200
    assert np.max(np.abs(cue)) <= 0.181
    assert cue[0] == cue[-1] == 0

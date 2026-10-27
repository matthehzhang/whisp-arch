"""PortAudio capture and output: WASAPI/CoreAudio, with native-rate capture."""

import logging
import queue
import threading

import numpy as np

RATE = 16_000
MAX_SECONDS = 90
log = logging.getLogger(__name__)


def devices(kind: str) -> list[tuple[str, int]]:
    import sounddevice as sd

    apis = sd.query_hostapis()
    return [
        (f"{d['name']} [{apis[d['hostapi']]['name']}]", i)
        for i, d in enumerate(sd.query_devices())
        if d[f"max_{kind}_channels"] > 0
    ]


def resolve_device(name: str, kind: str):
    if not name:
        return None
    for label, index in devices(kind):
        if label == name:
            return index
    raise RuntimeError(f"Selected {kind} device is disconnected. Choose another device.")


def to_16k(samples: np.ndarray, rate: int) -> np.ndarray:
    import soxr

    audio = np.asarray(samples, dtype=np.float32)
    if audio.ndim == 2:
        audio = audio.mean(axis=1)
    if audio.ndim != 1 or rate <= 0 or not np.isfinite(audio).all():
        raise ValueError("Invalid microphone audio")
    if not audio.size:
        return audio
    if rate != RATE:
        audio = soxr.resample(audio, rate, RATE)
    return np.ascontiguousarray(audio, dtype=np.float32)


class Recorder:
    def __init__(self):
        self.stream = None
        self.blocks = []
        self.samples = 0
        self.rate = RATE
        self.level = 0.0
        self.capped = threading.Event()
        self.error = ""

    def start(self, device_name: str = ""):
        import sounddevice as sd

        if self.stream is not None:
            raise RuntimeError("Microphone already open")
        device = resolve_device(device_name, "input")
        info = sd.query_devices(device, "input")
        self.rate = int(info["default_samplerate"])
        self.blocks = []
        self.samples = 0
        self.error = ""
        self.level = 0.0
        self.capped.clear()

        def capture(indata, frames, timing, status):
            if status.input_overflow:
                self.error = "Microphone audio was lost. Try another input device."
                self.capped.set()
            remaining = MAX_SECONDS * self.rate - self.samples
            if remaining <= 0:
                self.capped.set()
                return
            block = indata[:remaining, 0].copy()
            self.blocks.append(block)
            self.samples += len(block)
            self.level = float(np.sqrt(np.mean(block * block))) if block.size else 0.0
            if self.samples >= MAX_SECONDS * self.rate:
                self.capped.set()

        stream = sd.InputStream(
            device=device,
            samplerate=self.rate,
            channels=1,
            dtype="float32",
            callback=capture,
            blocksize=0,
        )
        try:
            stream.start()
        except Exception:
            stream.close()
            raise
        self.stream = stream

    def stop(self) -> np.ndarray:
        stream, self.stream = self.stream, None
        if stream is None:
            return np.empty(0, dtype=np.float32)
        try:
            stream.stop()
        finally:
            stream.close()
        self.level = 0.0
        blocks, self.blocks = self.blocks, []
        if self.error:
            raise RuntimeError(self.error)
        raw = np.concatenate(blocks) if blocks else np.empty(0, dtype=np.float32)
        return to_16k(raw, self.rate)


def cue_samples(start: bool, rate: int) -> np.ndarray:
    notes = (660, 880) if start else (880, 660)
    size, fade = int(rate * 0.065), max(1, int(rate * 0.008))
    t = np.arange(size)
    envelope = np.minimum(np.minimum(t, size - 1 - t) / fade, 1)
    parts = []
    for hz in notes:
        parts.extend(
            [0.18 * envelope * np.sin(2 * np.pi * hz * t / rate), np.zeros(int(rate * 0.01))]
        )
    return np.concatenate(parts).astype(np.float32)


class Cues:
    def __init__(self, on_error=lambda message: None):
        self.queue = queue.Queue(maxsize=8)
        self.on_error = on_error
        threading.Thread(target=self._run, name="whisp-sounds", daemon=True).start()

    def play(self, start: bool, device_name: str = ""):
        try:
            self.queue.put_nowait((start, device_name))
        except queue.Full:
            pass

    def _run(self):
        import sounddevice as sd

        while True:
            start, device_name = self.queue.get()
            try:
                device = resolve_device(device_name, "output")
                info = sd.query_devices(device, "output")
                rate = int(info["default_samplerate"])
                # Dedicated output stream: sd.play()/sd.stop() would interfere
                # with other convenience streams and concurrent recording.
                with sd.OutputStream(
                    device=device, samplerate=rate, channels=1, dtype="float32", latency="low"
                ) as stream:
                    stream.write(cue_samples(start, rate).reshape(-1, 1))
            except Exception as exc:
                log.warning("Cue playback failed: %s", exc)
                self.on_error("Recording sound unavailable. Check the sound output.")

"""Deepgram Nova-3 using each user's own key. No automatic billable retries."""

import io
import wave

import numpy as np
import requests

from .audio import RATE
from .credentials import read_key

ENDPOINT = "https://api.deepgram.com/v1/listen"


def wav_bytes(samples: np.ndarray) -> bytes:
    buffer = io.BytesIO()
    pcm = (np.clip(samples, -1, 1) * 32767).astype("<i2")
    with wave.open(buffer, "wb") as wav:
        wav.setnchannels(1)
        wav.setsampwidth(2)
        wav.setframerate(RATE)
        wav.writeframes(pcm.tobytes())
    return buffer.getvalue()


class Engine:
    def __init__(self, key_reader=read_key, post=requests.post):
        self.key_reader, self.post = key_reader, post

    def prepare(self):
        if not self.key_reader():
            raise RuntimeError("Add your Deepgram API key to begin.")

    def transcribe(self, samples: np.ndarray, is_cancelled=lambda: False) -> str:
        if is_cancelled():
            return ""
        if len(samples) < RATE * 0.3 or np.max(np.abs(samples), initial=0) < 0.01:
            return ""
        key = self.key_reader()
        if not key:
            raise RuntimeError("Add your Deepgram API key to begin.")
        payload = wav_bytes(samples)
        if is_cancelled():
            return ""
        try:
            response = self.post(
                ENDPOINT,
                params={"model": "nova-3", "smart_format": "true", "language": "multi"},
                headers={"Authorization": f"Token {key}", "Content-Type": "audio/wav"},
                data=payload,
                timeout=(5, 60),
                allow_redirects=False,
            )
        except requests.Timeout:
            raise RuntimeError("Deepgram timed out. Check your connection and try again.") from None
        except requests.RequestException:
            raise RuntimeError("Cannot reach Deepgram. Check your internet connection.") from None
        try:
            if response.status_code in (401, 403):
                raise RuntimeError(
                    "Deepgram rejected this API key. Replace it in API key settings."
                )
            if response.status_code == 402:
                raise RuntimeError("Your Deepgram account needs credit.")
            if response.status_code == 429:
                raise RuntimeError("Deepgram's usage limit was reached. Wait and try again.")
            if response.status_code != 200:
                raise RuntimeError(
                    f"Deepgram returned HTTP {response.status_code}. Try again later."
                )
            try:
                text = response.json()["results"]["channels"][0]["alternatives"][0]["transcript"]
                if not isinstance(text, str):
                    raise TypeError("Transcript is not text")
            except (ValueError, KeyError, IndexError, TypeError):
                raise RuntimeError(
                    "Deepgram returned an unreadable transcript. Try again."
                ) from None
            return " ".join(text.split())
        finally:
            response.close()

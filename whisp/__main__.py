"""Normal launch plus a non-recording packaged-runtime smoke check."""

import argparse
import json
import logging
import sys
from logging.handlers import RotatingFileHandler
from pathlib import Path


def self_test(report: Path):
    import tkinter as tk

    import numpy as np
    import requests
    import sounddevice
    from pynput import keyboard  # noqa: F401

    from .app import App
    from .audio import cue_samples, to_16k
    from .credentials import backend
    from .desktop import Desktop

    root = tk.Tk()
    root.withdraw()

    class SmokeApp(App):
        def _install_hotkey(self):
            pass

        def work(self, job, done):
            pass

    ui = SmokeApp(root)
    for state in ("setup", "ready", "recording", "transcribing"):
        ui.controller.update(state, "Runtime check")
        root.update_idletasks()
    root.destroy()
    native = Desktop()
    result = {
        "platform": sys.platform,
        "native_adapter": type(native.native).__name__,
        "audio_host_apis": len(sounddevice.query_hostapis()),
        "resampled_samples": len(to_16k(np.zeros(48000, np.float32), 48000)),
        "cue_samples": len(cue_samples(True, 48000)),
        "credential_backend": type(backend()).__name__,
        "https_certificate_bundle": Path(requests.certs.where()).is_file(),
    }
    assert result["resampled_samples"] == 16000
    report.write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")


def main():
    parser = argparse.ArgumentParser(description="Whisp desktop dictation")
    parser.add_argument("--self-test", type=Path, metavar="REPORT.json")
    args = parser.parse_args()
    from .config import data_dir

    directory = data_dir()
    directory.mkdir(parents=True, exist_ok=True)
    # Only diagnostics, not audio/transcripts. Bound disk use.
    handler = RotatingFileHandler(
        directory / "whisp.log", maxBytes=1_000_000, backupCount=1, encoding="utf-8"
    )
    logging.basicConfig(level=logging.WARNING, handlers=[handler])
    if args.self_test:
        self_test(args.self_test)
        return
    from .app import run

    run()


if __name__ == "__main__":
    main()

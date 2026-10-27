"""Validated settings; never store audio, transcripts, or machine-specific defaults."""

import json
import os
from dataclasses import asdict, dataclass
from pathlib import Path

HOTKEYS = ("F8", "F9", "F10")


def data_dir() -> Path:
    from platformdirs import user_data_path

    return user_data_path("Whisp", appauthor=False)


@dataclass
class Settings:
    hotkey: str = "F8"
    microphone: str = ""
    speaker: str = ""
    sounds: bool = True
    auto_paste: bool = True

    @classmethod
    def load(cls, path: Path) -> "Settings":
        if not path.exists():
            return cls()
        raw = json.loads(path.read_text(encoding="utf-8"))
        if not isinstance(raw, dict):
            raise ValueError("Settings must be a JSON object")
        settings = cls()
        for key in asdict(settings):
            if key in raw and type(raw[key]) is type(getattr(settings, key)):
                setattr(settings, key, raw[key])
        if settings.hotkey not in HOTKEYS:
            settings.hotkey = "F8"
        return settings

    def save(self, path: Path) -> None:
        path.parent.mkdir(parents=True, exist_ok=True)
        temporary = path.with_suffix(".tmp")
        temporary.write_text(json.dumps(asdict(self), indent=2) + "\n", encoding="utf-8")
        os.replace(temporary, path)

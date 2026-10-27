import json

import pytest

from whisp.config import Settings


def test_settings_round_trip(tmp_path):
    path = tmp_path / "Whisp" / "settings.json"
    settings = Settings(hotkey="F9", sounds=False, speaker="Laptop speakers")
    settings.save(path)
    assert Settings.load(path) == settings


def test_invalid_setting_types_do_not_turn_on_features(tmp_path):
    path = tmp_path / "settings.json"
    path.write_text(
        json.dumps({"model": "invalid", "hotkey": "bad", "sounds": "false", "auto_paste": 1})
    )
    assert Settings.load(path) == Settings()


def test_corrupt_settings_report_an_error(tmp_path):
    path = tmp_path / "settings.json"
    path.write_text("broken")
    with pytest.raises(ValueError):
        Settings.load(path)

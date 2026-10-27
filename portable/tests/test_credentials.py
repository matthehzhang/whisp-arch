import json

import pytest

from whisp import credentials
from whisp.config import Settings


def test_key_only_goes_to_credential_store(monkeypatch, tmp_path):
    saved = {}

    class Store:
        def set_password(self, service, account, value):
            saved[service, account] = value

        def get_password(self, service, account):
            return saved.get((service, account))

        def delete_password(self, service, account):
            saved.pop((service, account))

    monkeypatch.setattr(credentials, "backend", lambda: Store())
    credentials.save_key("  test-secret  ")
    assert credentials.read_key() == "test-secret"
    path = tmp_path / "settings.json"
    Settings().save(path)
    assert "test-secret" not in path.read_text()
    assert not any("key" in key and key != "hotkey" for key in json.loads(path.read_text()))
    credentials.delete_key()
    assert credentials.read_key() is None


def test_unavailable_keychain_does_not_fall_back_to_a_file(monkeypatch):
    def fail():
        raise RuntimeError("backend unavailable")

    monkeypatch.setattr(credentials, "backend", fail)
    with pytest.raises(RuntimeError, match="credential store"):
        credentials.save_key("test-secret")


@pytest.mark.parametrize("key", ["", " ", "Token fake", "bad\nkey"])
def test_reject_empty_or_header_style_keys(key):
    with pytest.raises(ValueError):
        credentials.save_key(key)

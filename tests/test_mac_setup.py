import sys
from pathlib import Path
from types import SimpleNamespace

import pytest

from whisp import mac_system
from whisp.config import Settings


def test_revoking_permission_stops_listener_and_capture(monkeypatch):
    from whisp.app import App

    calls = []
    allowed = {"Microphone": True, "Accessibility": True, "Input monitoring": True}
    monkeypatch.setattr(mac_system, "permission_status", lambda: {**allowed, "Microphone": False})
    app = SimpleNamespace(
        permissions=allowed,
        permission_labels={
            key: SimpleNamespace(configure=lambda **kwargs: None) for key in allowed
        },
        hotkey=SimpleNamespace(stop=lambda: calls.append("stop listener")),
        controller=SimpleNamespace(session=object(), cancel=lambda: calls.append("cancel capture")),
        settings=Settings(setup_complete=False),
        render=lambda: None,
        show_notice=lambda message: calls.append(message),
    )
    App.refresh_permissions(app)
    assert calls == ["stop listener", "cancel capture"]
    assert app.hotkey is None


def test_setup_choices_survive_restart(tmp_path):
    settings = Settings(
        mode="toggle", haptics=False, sounds=True, launch_at_login=False, setup_complete=True
    )
    path = tmp_path / "settings.json"
    settings.save(path)
    assert Settings.load(path) == settings


def test_login_registration_requires_installed_bundle(monkeypatch):
    monkeypatch.setitem(
        sys.modules,
        "ServiceManagement",
        SimpleNamespace(
            SMAppService=object(), SMAppServiceStatusEnabled=1, SMAppServiceStatusRequiresApproval=2
        ),
    )
    monkeypatch.setattr(mac_system, "installed_bundle", lambda: None)
    with pytest.raises(RuntimeError, match="Applications"):
        mac_system.set_login_enabled(True)
    # Source/dev launches can finish setup with automatic startup disabled.
    mac_system.set_login_enabled(False)


@pytest.mark.parametrize("enabled", [True, False])
def test_login_switch_calls_native_service_without_shell_scripts(monkeypatch, enabled):
    calls = []

    class Service:
        current = 0 if enabled else 1

        def status(self):
            return self.current

        def registerAndReturnError_(self, error):
            calls.append("register")
            self.current = 1
            return True, None

        def unregisterAndReturnError_(self, error):
            calls.append("unregister")
            self.current = 0
            return True, None

    service = Service()
    monkeypatch.setitem(
        sys.modules,
        "ServiceManagement",
        SimpleNamespace(
            SMAppService=SimpleNamespace(mainAppService=lambda: service),
            SMAppServiceStatusEnabled=1,
            SMAppServiceStatusRequiresApproval=2,
        ),
    )
    monkeypatch.setattr(mac_system, "installed_bundle", lambda: Path("/Applications/Whisp.app"))
    mac_system.set_login_enabled(enabled)
    assert calls == ["register" if enabled else "unregister"]


def test_login_approval_is_not_reported_as_enabled(monkeypatch):
    calls = []
    service = SimpleNamespace(status=lambda: 2, registerAndReturnError_=lambda error: (True, None))
    monkeypatch.setitem(
        sys.modules,
        "ServiceManagement",
        SimpleNamespace(
            SMAppService=SimpleNamespace(
                mainAppService=lambda: service,
                openSystemSettingsLoginItems=lambda: calls.append(True),
            ),
            SMAppServiceStatusEnabled=1,
            SMAppServiceStatusRequiresApproval=2,
        ),
    )
    monkeypatch.setattr(mac_system, "installed_bundle", lambda: Path("/Applications/Whisp.app"))
    with pytest.raises(RuntimeError, match="Approve"):
        mac_system.set_login_enabled(True)
    assert calls == [True]


def test_haptics_use_different_native_patterns(monkeypatch):
    calls = []
    performer = SimpleNamespace(
        performFeedbackPattern_performanceTime_=lambda pattern, when: calls.append((pattern, when))
    )
    monkeypatch.setitem(
        sys.modules,
        "AppKit",
        SimpleNamespace(
            NSHapticFeedbackManager=SimpleNamespace(defaultPerformer=lambda: performer),
            NSHapticFeedbackPatternAlignment=1,
            NSHapticFeedbackPatternGeneric=0,
            NSHapticFeedbackPerformanceTimeNow=0,
        ),
    )
    mac_system.haptic(True)
    mac_system.haptic(False)
    assert calls == [(1, 0), (0, 0)]

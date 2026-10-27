from types import SimpleNamespace

import pytest

from whisp.desktop import Desktop, Target


def desktop(current, modifiers=False):
    copied, pasted = [], []
    instance = Desktop.__new__(Desktop)
    instance.copy = copied.append
    instance.native = SimpleNamespace(
        capture=lambda: current,
        modifiers_down=lambda: modifiers,
        same=lambda a, b: a == b,
        paste=pasted.append,
    )
    return instance, copied, pasted


@pytest.mark.parametrize(
    "current", [None, Target(9, 20, 40), Target(9, 21, 30), Target(10, 20, 30)]
)
def test_changed_window_process_or_field_copies_without_paste(current):
    d, copied, pasted = desktop(current)
    d.deliver("recoverable", Target(9, 20, 30), True)
    assert copied == ["recoverable"]
    assert not pasted


def test_unchanged_target_pastes_once():
    target = Target(9, 20, 30)
    d, copied, pasted = desktop(target)
    d.deliver("hello", target, True)
    assert pasted == [target]


@pytest.mark.parametrize("auto_paste,modifiers", [(False, False), (True, True)])
def test_manual_mode_and_held_modifiers_only_copy(auto_paste, modifiers):
    target = Target(9, 20, 30)
    d, copied, pasted = desktop(target, modifiers)
    d.deliver("hello", target, auto_paste)
    assert copied == ["hello"]
    assert not pasted


def test_clipboard_failure_prevents_paste():
    target = Target(9, 20, 30)
    d, copied, pasted = desktop(target)

    def fail(text):
        raise RuntimeError("clipboard locked")

    d.copy = fail
    with pytest.raises(RuntimeError):
        d.deliver("hello", target, True)
    assert not pasted

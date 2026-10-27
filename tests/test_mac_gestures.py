from whisp.mac_hotkey import FnGesture


def test_hold_starts_once_and_releases_once():
    gesture = FnGesture("hold")
    assert gesture.fn(True) == "press"
    assert gesture.fn(True) is None
    assert gesture.fn(False) == "release"
    assert gesture.fn(False) is None


def test_toggle_ignores_release_and_stops_on_second_press():
    gesture = FnGesture("toggle")
    assert gesture.fn(True) == "press"
    assert gesture.fn(False) is None
    assert gesture.fn(True) == "release"
    assert gesture.fn(False) is None
    assert gesture.fn(True) == "press"


def test_fn_space_locks_recording_after_release():
    gesture = FnGesture("hold")
    assert gesture.fn(True) == "press"
    assert gesture.space(True) == "lock"
    assert gesture.space(True) is None
    assert gesture.space(False) is None
    assert gesture.fn(False) is None
    assert gesture.fn(True) == "release"
    assert gesture.fn(False) is None


def test_escape_does_not_transcribe_on_subsequent_release():
    gesture = FnGesture("hold")
    gesture.fn(True)
    assert gesture.cancel() == "cancel"
    assert gesture.fn(False) is None
    assert gesture.fn(True) == "press"


def test_watchdog_or_failed_start_resets_toggle_latch():
    gesture = FnGesture("toggle")
    gesture.fn(True)
    gesture.reset_session()
    assert gesture.fn(False) is None
    assert gesture.fn(True) == "press"


def test_plain_space_never_starts_dictation():
    gesture = FnGesture()
    assert gesture.space(True) is None
    assert gesture.space(False) is None

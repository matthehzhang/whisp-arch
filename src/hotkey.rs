//! evdev push-to-talk listener + modifier-state queries. OWNER: hotkey agent.
//!
//! Right Ctrl is the PTT key: press -> `PttDown`, release -> `PttUp`. Esc always
//! emits `Cancel` (the state machine decides whether it's relevant). We only ever
//! *listen* on the input devices (never `grab`), so normal typing is unaffected.
//!
//! `modifiers_clear()` answers "is it physically safe to inject right now?" by
//! doing an `EVIOCGKEY` key-state query (not by tracking events) so it reflects
//! true hardware state even after dropped/racy events.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use evdev::{Device, EventSummary, KeyCode};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyEvent {
    PttDown,
    PttUp,
    Cancel, // Cancel = Esc while recording
}

/// The push-to-talk key.
const PTT: KeyCode = KeyCode::KEY_RIGHTCTRL;

/// Keys whose physical "down" state must block injection. PTT (Right Ctrl) is one
/// of these, so checking this list also covers the PTT key per spec.
const MODIFIERS: [KeyCode; 8] = [
    KeyCode::KEY_LEFTCTRL,
    KeyCode::KEY_RIGHTCTRL,
    KeyCode::KEY_LEFTALT,
    KeyCode::KEY_RIGHTALT,
    KeyCode::KEY_LEFTSHIFT,
    KeyCode::KEY_RIGHTSHIFT,
    KeyCode::KEY_LEFTMETA,
    KeyCode::KEY_RIGHTMETA,
];

/// A device is a PTT candidate iff it reports the Right Ctrl key (i.e. a keyboard).
fn is_candidate(dev: &Device) -> bool {
    dev.supported_keys().is_some_and(|keys| keys.contains(PTT))
}

/// Pure classification of a raw key (code, value) into an optional `HotkeyEvent`.
/// value: 1 = press, 0 = release, 2 = autorepeat (ignored for PTT).
fn classify(code: KeyCode, value: i32) -> Option<HotkeyEvent> {
    match code {
        PTT => match value {
            1 => Some(HotkeyEvent::PttDown),
            0 => Some(HotkeyEvent::PttUp),
            _ => None, // ignore autorepeat while holding
        },
        KeyCode::KEY_ESC => match value {
            1 => Some(HotkeyEvent::Cancel),
            _ => None,
        },
        _ => None,
    }
}

/// Spawn the listener. Enumerates `/dev/input/event*`, keeps every device that
/// reports Right Ctrl, and reads each in its own thread. Dead devices are dropped;
/// a supervisor re-enumerates every 2 s to pick up hotplugged keyboards.
pub fn spawn_listener(tx: Sender<HotkeyEvent>) -> Result<()> {
    // Paths we currently have a reader thread for. A reader removes its own path
    // on exit so the supervisor re-adopts the device on the next sweep.
    let active: Arc<Mutex<HashSet<PathBuf>>> = Arc::new(Mutex::new(HashSet::new()));

    thread::Builder::new()
        .name("whisp-hotkey-supervisor".into())
        .spawn(move || loop {
            for (path, dev) in evdev::enumerate() {
                if !is_candidate(&dev) {
                    continue;
                }
                {
                    let mut guard = match active.lock() {
                        Ok(g) => g,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                    if !guard.insert(path.clone()) {
                        continue; // already have a reader for this path
                    }
                }
                let name = dev.name().unwrap_or("<unnamed>").to_string();
                eprintln!("[whisp] hotkey: listening on {} ({})", path.display(), name);
                let tx = tx.clone();
                let reader_active = Arc::clone(&active);
                let reader_path = path.clone();
                if let Err(e) = thread::Builder::new()
                    .name("whisp-hotkey-reader".into())
                    .spawn(move || {
                        read_device(dev, &reader_path, &tx);
                        if let Ok(mut g) = reader_active.lock() {
                            g.remove(&reader_path);
                        }
                    })
                {
                    eprintln!("[whisp] hotkey: failed to spawn reader: {e}");
                    if let Ok(mut g) = active.lock() {
                        g.remove(&path);
                    }
                }
            }
            thread::sleep(Duration::from_secs(2));
        })?;

    Ok(())
}

/// Blocking read loop for one device. Returns (letting the thread exit) on any
/// read error, which drops the device from the active set so it can be re-adopted.
fn read_device(mut dev: Device, path: &Path, tx: &Sender<HotkeyEvent>) {
    loop {
        let events = match dev.fetch_events() {
            Ok(events) => events,
            Err(e) => {
                eprintln!(
                    "[whisp] hotkey: {} read error: {e}; dropping",
                    path.display()
                );
                return;
            }
        };
        for ev in events {
            if let EventSummary::Key(_, code, value) = ev.destructure() {
                if let Some(hk) = classify(code, value) {
                    if tx.send(hk).is_err() {
                        // Receiver gone: the daemon is shutting down.
                        return;
                    }
                }
            }
        }
    }
}

/// True when the PTT key and every modifier (Ctrl/Alt/Shift/Super, both sides) are
/// physically up right now, per an `EVIOCGKEY` key-state query across all keyboards.
///
/// This re-enumerates and queries each candidate device directly, so it reflects
/// real hardware state rather than any cached event stream. Unreadable devices are
/// skipped; if a modifier is down on *any* readable keyboard, returns false.
pub fn modifiers_clear() -> bool {
    for (path, dev) in evdev::enumerate() {
        if !is_candidate(&dev) {
            continue;
        }
        let state = match dev.get_key_state() {
            Ok(state) => state,
            Err(e) => {
                eprintln!(
                    "[whisp] hotkey: EVIOCGKEY on {} failed: {e}; skipping",
                    path.display()
                );
                continue;
            }
        };
        if MODIFIERS.iter().any(|&k| state.contains(k)) {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ptt_press_and_release() {
        assert_eq!(classify(PTT, 1), Some(HotkeyEvent::PttDown));
        assert_eq!(classify(PTT, 0), Some(HotkeyEvent::PttUp));
    }

    #[test]
    fn ptt_autorepeat_ignored() {
        assert_eq!(classify(PTT, 2), None);
    }

    #[test]
    fn esc_press_cancels_release_ignored() {
        assert_eq!(classify(KeyCode::KEY_ESC, 1), Some(HotkeyEvent::Cancel));
        assert_eq!(classify(KeyCode::KEY_ESC, 0), None);
        assert_eq!(classify(KeyCode::KEY_ESC, 2), None);
    }

    #[test]
    fn other_keys_ignored() {
        assert_eq!(classify(KeyCode::KEY_A, 1), None);
        assert_eq!(classify(KeyCode::KEY_LEFTCTRL, 1), None);
    }

    #[test]
    fn ptt_is_in_modifier_set() {
        // Checking modifiers must also cover the PTT key (spec requirement).
        assert!(MODIFIERS.contains(&PTT));
    }

    /// Sanity check that the machine actually exposes a keyboard we can listen on.
    /// Requires read access to /dev/input (user in the `input` group). Ignored by
    /// default so CI without input access doesn't fail; run with `--ignored`.
    #[test]
    #[ignore]
    fn finds_a_keyboard() {
        let count = evdev::enumerate().filter(|(_, d)| is_candidate(d)).count();
        assert!(count >= 1, "expected at least one KEY_RIGHTCTRL device");
    }
}

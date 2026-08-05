//! evdev PTT listener. OWNER: hotkey agent. Scaffold stub — replace entirely.

pub enum HotkeyEvent {
    PttDown,
    PttUp,
    Cancel,
}

pub fn spawn_listener(_tx: std::sync::mpsc::Sender<HotkeyEvent>) -> anyhow::Result<()> {
    anyhow::bail!("stub")
}

pub fn modifiers_clear() -> bool {
    false
}

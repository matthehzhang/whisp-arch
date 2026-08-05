//! Headset capture + presence watcher. OWNER: audio agent. Scaffold stub — replace entirely.

pub struct Capture;

pub fn headset_source() -> Option<String> {
    None
}

pub fn spawn_watcher(_cb: impl Fn(Option<String>) + Send + 'static) {}

impl Capture {
    pub fn start(_source: &str) -> anyhow::Result<Capture> {
        anyhow::bail!("stub")
    }
    pub fn stop(self) -> anyhow::Result<Vec<f32>> {
        anyhow::bail!("stub")
    }
}

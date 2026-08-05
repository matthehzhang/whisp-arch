//! whisper.cpp wrapper + gating. OWNER: asr agent. Scaffold stub — replace entirely.

pub struct Engine;

impl Engine {
    pub fn load() -> anyhow::Result<Engine> {
        anyhow::bail!("stub")
    }
    pub fn transcribe(&self, _pcm: &[f32]) -> anyhow::Result<Option<String>> {
        anyhow::bail!("stub")
    }
}

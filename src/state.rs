//! Shared state types. OWNER: integration agent. Module agents: read, don't edit.

use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonState {
    HeadsetMissing,
    Disabled,
    Ready,
    Recording,
    Transcribing,
}

pub struct Inner {
    pub state: DaemonState,
    /// Bumped on cancel/toggle-off; pipeline results with a stale generation are dropped.
    pub generation: u64,
    pub enabled: bool,
    /// Pulse source name of the headset mic, when present.
    pub source: Option<String>,
}

#[derive(Clone)]
pub struct Shared(pub Arc<Mutex<Inner>>);

impl Shared {
    pub fn new() -> Self {
        Shared(Arc::new(Mutex::new(Inner {
            state: DaemonState::HeadsetMissing,
            generation: 0,
            enabled: true,
            source: None,
        })))
    }
}

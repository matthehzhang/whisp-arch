//! Status file + control socket for waybar. OWNER: status agent. Scaffold stub — replace entirely.

use crate::state::{DaemonState, Shared};

pub fn write_status(_state: &DaemonState) {}

pub fn spawn_control(_shared: Shared) -> anyhow::Result<()> {
    anyhow::bail!("stub")
}

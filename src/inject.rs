//! wtype injection with safe-to-inject gating. OWNER: inject agent.
//!
//! `inject` blocks (polling `wait_clear`) until no PTT/modifier key is
//! physically held, then types `text` (plus a trailing space, dictation
//! convention) via `wtype -`, feeding the text on stdin to dodge argv
//! length/quoting issues. Returns `Ok(false)` when the wait times out and the
//! text is dropped.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::Context;

/// Poll `wait_clear` this often while waiting for modifiers to release.
const POLL_INTERVAL: Duration = Duration::from_millis(50);
/// Give up (drop the text) after this long with modifiers still held.
const WAIT_TIMEOUT: Duration = Duration::from_secs(5);

/// Blocks until safe (modifiers clear) then types text. Ok(false) = dropped (timeout).
pub fn inject(text: &str, wait_clear: impl Fn() -> bool) -> anyhow::Result<bool> {
    inject_with(text, wait_clear, POLL_INTERVAL, WAIT_TIMEOUT)
}

/// Core with injectable timings so tests can run fast. `interval` is the poll
/// period; `timeout` is the total budget before the text is dropped.
fn inject_with(
    text: &str,
    wait_clear: impl Fn() -> bool,
    interval: Duration,
    timeout: Duration,
) -> anyhow::Result<bool> {
    let start = Instant::now();
    // Gate BEFORE spawning wtype: never type while PTT/modifiers are held.
    while !wait_clear() {
        if start.elapsed() >= timeout {
            eprintln!(
                "[whisp] inject: modifiers still held after {:?}; dropping text",
                timeout
            );
            return Ok(false);
        }
        std::thread::sleep(interval);
    }

    spawn_wtype(text)?;
    Ok(true)
}

/// Type `text` (with a trailing space) via `wtype -`, text on stdin, 1 ms
/// inter-key delay. No clipboard use.
fn spawn_wtype(text: &str) -> anyhow::Result<()> {
    let payload = format!("{text} ");

    let mut child = Command::new("wtype")
        .arg("-d")
        .arg("1")
        .arg("-")
        .stdin(Stdio::piped())
        .spawn()
        .context("failed to spawn wtype (is it installed?)")?;

    {
        let mut stdin = child.stdin.take().context("wtype stdin unavailable")?;
        stdin
            .write_all(payload.as_bytes())
            .context("failed to write text to wtype stdin")?;
    } // drop stdin -> EOF so wtype finishes reading

    let status = child.wait().context("failed to wait on wtype")?;
    if !status.success() {
        anyhow::bail!("wtype exited with failure status: {status}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn times_out_when_never_clear() {
        // wait_clear that always reports "held" -> must drop the text (Ok(false))
        // without ever spawning wtype. Tiny timings keep the test near-instant.
        let calls = AtomicUsize::new(0);
        let out = inject_with(
            "hello",
            || {
                calls.fetch_add(1, Ordering::SeqCst);
                false
            },
            Duration::from_millis(1),
            Duration::from_millis(10),
        )
        .expect("inject should not error on timeout");
        assert!(!out, "timeout must return Ok(false)");
        assert!(
            calls.load(Ordering::SeqCst) >= 1,
            "wait_clear should have been polled at least once"
        );
    }

    #[test]
    fn timeout_respects_budget() {
        // Roughly the timeout should elapse (never fires wtype). Loose bounds to
        // avoid flakiness on a loaded machine.
        let start = Instant::now();
        let out = inject_with(
            "x",
            || false,
            Duration::from_millis(5),
            Duration::from_millis(40),
        )
        .expect("timeout path must be Ok");
        assert!(!out);
        assert!(start.elapsed() >= Duration::from_millis(40));
    }
}

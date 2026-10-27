//! Status file + control socket for waybar. OWNER: status agent.
//!
//! Writes a waybar-consumable JSON status line to
//! `$XDG_RUNTIME_DIR/whisp-arch/status.json` (atomically) and serves a
//! line-oriented control socket at `$XDG_RUNTIME_DIR/whisp-arch.sock`.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use serde::Serialize;

use crate::state::{DaemonState, Shared};

/// One waybar status line. `text` is a Nerd-Font icon glyph; color is applied
/// by waybar CSS keyed on `class` (see `waybar/README-snippet.md`).
#[derive(Serialize)]
struct StatusLine {
    /// Machine-readable state name (also usable as an alt/class hook).
    state: &'static str,
    /// Icon glyph shown in the bar.
    text: &'static str,
    /// waybar CSS class: ready/recording/transcribing/disabled/missing.
    class: &'static str,
    /// Hover tooltip.
    tooltip: &'static str,
}

impl StatusLine {
    fn from_state(state: &DaemonState) -> StatusLine {
        match state {
            DaemonState::HeadsetMissing => StatusLine {
                state: "headset-missing",
                text: "\u{f02d0}", // 󰋐 headset (dim via CSS)
                class: "missing",
                tooltip: "whisp: no microphone source",
            },
            DaemonState::Disabled => StatusLine {
                state: "disabled",
                text: "\u{f036d}", // 󰍭 mic-off
                class: "disabled",
                tooltip: "whisp: disabled",
            },
            DaemonState::Ready => StatusLine {
                state: "ready",
                text: "\u{f036c}", // 󰍬 mic
                class: "ready",
                tooltip: "whisp: ready (hold Right Alt)",
            },
            DaemonState::Recording => StatusLine {
                state: "recording",
                text: "\u{f036c}", // 󰍬 mic (red via CSS)
                class: "recording",
                tooltip: "whisp: recording\u{2026}",
            },
            DaemonState::Transcribing => StatusLine {
                state: "transcribing",
                text: "\u{f0996}", // 󰦖 waveform
                class: "transcribing",
                tooltip: "whisp: transcribing\u{2026}",
            },
        }
    }

    /// Serialize to a single JSON line (no trailing newline).
    fn to_json(&self) -> String {
        // Fields are all &'static str, so serialization cannot fail; keep a
        // graceful fallback rather than unwrap so we never panic.
        serde_json::to_string(self).unwrap_or_else(|_| {
            String::from(
                r#"{"state":"error","text":"","class":"missing","tooltip":"whisp: status encode error"}"#,
            )
        })
    }
}

/// `$XDG_RUNTIME_DIR`, falling back to `/tmp` (logged) when unset.
fn runtime_base() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(d) => PathBuf::from(d),
        None => {
            eprintln!("[whisp] XDG_RUNTIME_DIR unset; falling back to /tmp");
            PathBuf::from("/tmp")
        }
    }
}

/// Directory holding the status file: `$XDG_RUNTIME_DIR/whisp-arch/`.
fn status_dir() -> PathBuf {
    runtime_base().join("whisp-arch")
}

/// Full path to the status JSON file.
fn status_path() -> PathBuf {
    status_dir().join("status.json")
}

/// Control socket path: `$XDG_RUNTIME_DIR/whisp-arch.sock`.
/// Public so the CLI client (`--toggle`/`--status`) can reach the same socket
/// the daemon binds.
pub fn socket_path() -> PathBuf {
    runtime_base().join("whisp-arch.sock")
}

/// Atomically write the status JSON for `state`. Creates the runtime dir if
/// needed. Errors are logged, never propagated (waybar polls the file, so a
/// missing/stale write is non-fatal).
pub fn write_status(state: &DaemonState) {
    let line = StatusLine::from_state(state);
    if let Err(e) = write_status_inner(&line) {
        eprintln!("[whisp] write_status failed: {e:#}");
    }
    // Also tell the overlay.
    publish(&format!(r#"{{"ev":"state","state":"{}"}}"#, line.state));
}

// ---------------------------------------------------------------------------
// Events socket: newline-delimited JSON broadcast to every connected subscriber
// (the overlay). `$XDG_RUNTIME_DIR/whisp-arch.events`.
// ---------------------------------------------------------------------------

static EVENT_CLIENTS: OnceLock<Arc<Mutex<Vec<UnixStream>>>> = OnceLock::new();

/// Events socket path.
pub fn events_path() -> PathBuf {
    runtime_base().join("whisp-arch.events")
}

/// Start accepting event subscribers on a background thread. Non-fatal on
/// failure (the overlay just gets no events).
pub fn spawn_events() -> anyhow::Result<()> {
    let path = events_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(anyhow::Error::from(e)),
    }
    let listener = UnixListener::bind(&path)?;
    let clients = EVENT_CLIENTS
        .get_or_init(|| Arc::new(Mutex::new(Vec::new())))
        .clone();
    eprintln!("[whisp] events socket listening at {}", path.display());
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            match conn {
                Ok(stream) => {
                    // Never let a slow subscriber block the owner thread.
                    let _ = stream.set_nonblocking(true);
                    if let Ok(mut v) = clients.lock() {
                        v.push(stream);
                    }
                }
                Err(e) => eprintln!("[whisp] events accept error: {e:#}"),
            }
        }
    });
    Ok(())
}

/// Broadcast one JSON event line to all subscribers; drops subscribers whose
/// socket is gone or full.
pub fn publish(json: &str) {
    let Some(clients) = EVENT_CLIENTS.get() else {
        return;
    };
    let Ok(mut v) = clients.lock() else {
        return;
    };
    if v.is_empty() {
        return;
    }
    let mut line = json.to_string();
    line.push('\n');
    v.retain_mut(|s| s.write_all(line.as_bytes()).is_ok());
}

/// JSON-encode a string (with quotes) for embedding in an event line.
pub fn json_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}

fn write_status_inner(line: &StatusLine) -> anyhow::Result<()> {
    let dir = status_dir();
    std::fs::create_dir_all(&dir)?;

    // Write to a pid-unique temp file in the same dir, then rename over the
    // target so readers never observe a partial file.
    let final_path = status_path();
    let tmp_path = dir.join(format!("status.json.tmp.{}", std::process::id()));

    let mut json = line.to_json();
    json.push('\n');

    {
        let mut f = std::fs::File::create(&tmp_path)?;
        f.write_all(json.as_bytes())?;
        f.flush()?;
    }
    std::fs::rename(&tmp_path, &final_path)?;
    Ok(())
}

/// Spawn the control socket listener on a background thread. Line-oriented
/// protocol over a Unix stream; each connection may issue one command:
/// - `toggle` — flip the enabled flag (bumps generation when disabling so any
///   in-flight pipeline result is dropped). Only has effect when the headset is
///   present. Replies with the current status JSON line.
/// - `status` — reply with the current status JSON line.
pub fn spawn_control(shared: Shared) -> anyhow::Result<()> {
    let path = socket_path();

    // Ensure the parent (runtime base) exists and clear any stale socket.
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(anyhow::Error::from(e)),
    }

    let listener = UnixListener::bind(&path)?;
    eprintln!("[whisp] control socket listening at {}", path.display());

    std::thread::spawn(move || {
        for conn in listener.incoming() {
            match conn {
                Ok(stream) => {
                    if let Err(e) = handle_conn(stream, &shared) {
                        eprintln!("[whisp] control conn error: {e:#}");
                    }
                }
                Err(e) => {
                    eprintln!("[whisp] control accept error: {e:#}");
                }
            }
        }
    });

    Ok(())
}

/// Handle a single control connection: read one command line, act, reply.
fn handle_conn(stream: UnixStream, shared: &Shared) -> anyhow::Result<()> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let n = reader.read_line(&mut line)?;
    if n == 0 {
        return Ok(()); // empty connection
    }
    let cmd = line.trim();

    let reply = match cmd {
        "toggle" => {
            do_toggle(shared);
            current_status_json(shared)
        }
        "status" => current_status_json(shared),
        other => {
            eprintln!("[whisp] control: unknown command {other:?}");
            format!(
                r#"{{"state":"error","text":"","class":"missing","tooltip":"whisp: unknown command {other}"}}"#
            )
        }
    };

    // Reply on the same stream (BufReader owns it; recover via get_mut).
    let stream = reader.get_mut();
    stream.write_all(reply.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(())
}

/// Flip the enabled flag. No-op (logged) when the headset is missing, since
/// enabling/disabling is meaningless without a source. Bumps the generation
/// counter when disabling so any in-flight transcription is discarded by the
/// pipeline's generation check.
fn do_toggle(shared: &Shared) {
    match shared.0.lock() {
        Ok(mut inner) => {
            if inner.state == DaemonState::HeadsetMissing {
                eprintln!("[whisp] toggle ignored: headset not connected");
                return;
            }
            inner.enabled = !inner.enabled;
            if !inner.enabled {
                inner.generation = inner.generation.wrapping_add(1);
            }
            eprintln!("[whisp] toggle: enabled = {}", inner.enabled);
        }
        Err(e) => eprintln!("[whisp] toggle: state lock poisoned: {e}"),
    }
}

/// Current status as a JSON line, derived from the shared state.
fn current_status_json(shared: &Shared) -> String {
    match shared.0.lock() {
        Ok(inner) => StatusLine::from_state(&inner.state).to_json(),
        Err(e) => {
            eprintln!("[whisp] status: state lock poisoned: {e}");
            StatusLine::from_state(&DaemonState::HeadsetMissing).to_json()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(state: &DaemonState) -> serde_json::Value {
        serde_json::from_str(&StatusLine::from_state(state).to_json()).expect("valid json")
    }

    #[test]
    fn every_state_maps_to_known_class() {
        let cases = [
            (DaemonState::HeadsetMissing, "missing"),
            (DaemonState::Disabled, "disabled"),
            (DaemonState::Ready, "ready"),
            (DaemonState::Recording, "recording"),
            (DaemonState::Transcribing, "transcribing"),
        ];
        for (state, class) in cases {
            let v = parse(&state);
            assert_eq!(v["class"], class, "class for {state:?}");
            // text must be a non-empty icon glyph
            assert!(
                !v["text"].as_str().unwrap().is_empty(),
                "icon for {state:?}"
            );
            assert!(
                v["tooltip"].as_str().unwrap().starts_with("whisp:"),
                "tooltip prefix for {state:?}"
            );
        }
    }

    #[test]
    fn json_is_single_line() {
        for state in [
            DaemonState::HeadsetMissing,
            DaemonState::Disabled,
            DaemonState::Ready,
            DaemonState::Recording,
            DaemonState::Transcribing,
        ] {
            let json = StatusLine::from_state(&state).to_json();
            assert!(!json.contains('\n'), "no embedded newline: {json}");
        }
    }

    #[test]
    fn recording_and_ready_share_mic_icon() {
        assert_eq!(
            parse(&DaemonState::Ready)["text"],
            parse(&DaemonState::Recording)["text"]
        );
        // but differ by class so CSS can color them differently
        assert_ne!(
            parse(&DaemonState::Ready)["class"],
            parse(&DaemonState::Recording)["class"]
        );
    }
}

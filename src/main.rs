//! whisp-arch daemon entrypoint + state-owner thread. OWNER: integration agent.
//!
//! Architecture (SPEC §main): a single state-owner loop owns the state machine.
//! Every input — PTT hotkey events, headset-presence changes from the audio
//! watcher, and transcription results from pipeline threads — arrives on one
//! mpsc channel and is handled serially, so state transitions never race.
//!
//! The control socket (`status::spawn_control`) mutates `enabled`/`generation`
//! on the shared `Inner` directly (its fixed signature only hands it `Shared`);
//! the owner loop reconciles the visible `DaemonState` (Ready/Disabled) from
//! that flag on a short `recv_timeout` tick, so a waybar click-toggle is
//! reflected within ~200 ms even with no other events flowing.
//!
//! Generation counter (design-review invariant): a session's generation is
//! captured at PttUp; Cancel, headset-loss, and toggle-off all bump it. The
//! inject decision is made *on this owner thread* immediately before typing:
//! we drop the transcript unless its generation still matches AND the daemon is
//! enabled with the headset present. This is the "check-and-inject on the
//! serialized owner thread" rule.

mod asr;
mod audio;
mod hotkey;
mod inject;
mod state;
mod status;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::hotkey::HotkeyEvent;
use crate::state::{DaemonState, Inner, Shared};

/// How often the owner loop wakes to reconcile socket-driven toggles when no
/// other event is pending.
const RECONCILE_TICK: Duration = Duration::from_millis(200);

/// Events funneled into the single state-owner loop.
enum Event {
    /// A raw PTT/Esc event from the evdev listener.
    Hotkey(HotkeyEvent),
    /// Headset presence changed (pulse source name, or `None` when absent).
    Headset(Option<String>),
    /// A pipeline thread finished; `generation` is the session it belongs to.
    Transcribed {
        generation: u64,
        result: Option<String>,
    },
    /// The whisper model finished loading in the background.
    EngineLoaded(Arc<asr::Engine>),
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--toggle") => std::process::exit(cli_toggle()),
        Some("--status") => std::process::exit(cli_status()),
        Some("--help" | "-h") => {
            println!("whisp-arch — push-to-talk dictation daemon");
            println!("  (no args)   run the daemon");
            println!("  --toggle    toggle enabled via the control socket, then exit");
            println!("  --status    print the current waybar status JSON, then exit");
            std::process::exit(0);
        }
        Some(other) => {
            eprintln!("[whisp] unknown argument {other:?}; run with --help");
            std::process::exit(2);
        }
        None => {
            if let Err(e) = run_daemon() {
                eprintln!("[whisp] fatal: {e:#}");
                std::process::exit(1);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// CLI clients (--toggle / --status): talk to the daemon's control socket.
// ---------------------------------------------------------------------------

/// Send one command over the control socket and return the daemon's reply line.
fn cli_send(cmd: &str) -> Result<String> {
    let path = status::socket_path();
    let mut stream = UnixStream::connect(&path)
        .with_context(|| format!("connecting to control socket {}", path.display()))?;
    stream.write_all(cmd.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let mut resp = String::new();
    BufReader::new(stream).read_line(&mut resp)?;
    Ok(resp.trim_end().to_string())
}

/// `--toggle`: flip enabled, exit 0 on success (waybar on-click ignores output).
fn cli_toggle() -> i32 {
    match cli_send("toggle") {
        Ok(_) => 0,
        Err(e) => {
            eprintln!("[whisp] --toggle failed: {e:#}");
            1
        }
    }
}

/// `--status`: print the current waybar JSON line. Falls back to a
/// "daemon not running" line (still exit 0) so waybar always renders something.
fn cli_status() -> i32 {
    match cli_send("status") {
        Ok(line) => {
            println!("{line}");
            0
        }
        Err(e) => {
            eprintln!("[whisp] --status: {e:#}");
            // Same fallback line as waybar/whisper.sh so the module never blanks.
            println!(
                "{{\"state\":\"down\",\"text\":\"\u{f00d0}\",\"class\":\"missing\",\"tooltip\":\"whisp: daemon not running\"}}"
            );
            0
        }
    }
}

// ---------------------------------------------------------------------------
// Daemon: wire up inputs, then run the state-owner loop.
// ---------------------------------------------------------------------------

fn run_daemon() -> Result<()> {
    let shared = Shared::new();

    // Write an initial status immediately so waybar (and the smoke test) see a
    // file right away, before the watcher's first observation lands.
    status::write_status(&DaemonState::HeadsetMissing);

    // Control socket (toggle/status). Non-fatal if it fails — the daemon can
    // still dictate; only waybar click-toggle would be unavailable.
    if let Err(e) = status::spawn_control(shared.clone()) {
        eprintln!("[whisp] control socket unavailable: {e:#}");
    }

    let (ev_tx, ev_rx) = mpsc::channel::<Event>();

    // Hotkey listener → bridge its HotkeyEvent channel into the unified Event bus.
    let (hk_tx, hk_rx) = mpsc::channel::<HotkeyEvent>();
    hotkey::spawn_listener(hk_tx).context("starting hotkey listener")?;
    {
        let ev_tx = ev_tx.clone();
        thread::spawn(move || {
            for hk in hk_rx {
                if ev_tx.send(Event::Hotkey(hk)).is_err() {
                    break;
                }
            }
        });
    }

    // Headset-presence watcher.
    {
        let ev_tx = ev_tx.clone();
        audio::spawn_watcher(move |src| {
            let _ = ev_tx.send(Event::Headset(src));
        });
    }

    // Load the whisper model off the hot path; deliver it when ready.
    {
        let ev_tx = ev_tx.clone();
        thread::spawn(move || match asr::Engine::load() {
            Ok(engine) => {
                let _ = ev_tx.send(Event::EngineLoaded(Arc::new(engine)));
            }
            Err(e) => eprintln!("[whisp] asr engine load failed: {e:#}"),
        });
    }

    eprintln!("[whisp] daemon started (hold Right Ctrl to dictate)");
    owner_loop(&shared, &ev_tx, ev_rx);
    Ok(())
}

/// The serialized state-owner loop. Holds the live `Capture` handle and the
/// loaded ASR engine; every transition happens here.
fn owner_loop(shared: &Shared, ev_tx: &mpsc::Sender<Event>, ev_rx: mpsc::Receiver<Event>) {
    let mut capture: Option<audio::Capture> = None;
    let mut engine: Option<Arc<asr::Engine>> = None;

    loop {
        match ev_rx.recv_timeout(RECONCILE_TICK) {
            Ok(Event::Hotkey(HotkeyEvent::PttDown)) => on_ptt_down(shared, &mut capture),
            Ok(Event::Hotkey(HotkeyEvent::PttUp)) => {
                on_ptt_up(shared, &mut capture, &engine, ev_tx)
            }
            Ok(Event::Hotkey(HotkeyEvent::Cancel)) => on_cancel(shared, &mut capture),
            Ok(Event::Headset(src)) => on_headset(shared, &mut capture, src),
            Ok(Event::Transcribed { generation, result }) => {
                on_transcribed(shared, generation, result)
            }
            Ok(Event::EngineLoaded(eng)) => {
                eprintln!("[whisp] asr engine ready");
                engine = Some(eng);
            }
            Err(RecvTimeoutError::Timeout) => reconcile_idle(shared),
            Err(RecvTimeoutError::Disconnected) => {
                eprintln!("[whisp] all event senders dropped; owner loop exiting");
                break;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Event handlers (owner thread only).
// ---------------------------------------------------------------------------

/// PttDown: begin capture only from the Ready state.
fn on_ptt_down(shared: &Shared, capture: &mut Option<audio::Capture>) {
    let (state, source) = with_inner(shared, |i| (i.state, i.source.clone()));
    if state != DaemonState::Ready {
        return;
    }
    let Some(src) = source else {
        return;
    };
    match audio::Capture::start(&src) {
        Ok(cap) => {
            *capture = Some(cap);
            set_state(shared, DaemonState::Recording);
        }
        Err(e) => eprintln!("[whisp] failed to start capture: {e:#}"),
    }
}

/// PttUp: stop capture, snapshot the session generation, and hand the pcm to a
/// pipeline thread that reports back via `Event::Transcribed`.
fn on_ptt_up(
    shared: &Shared,
    capture: &mut Option<audio::Capture>,
    engine: &Option<Arc<asr::Engine>>,
    ev_tx: &mpsc::Sender<Event>,
) {
    let state = with_inner(shared, |i| i.state);
    if state != DaemonState::Recording {
        return;
    }
    let Some(cap) = capture.take() else {
        return;
    };

    let pcm = match cap.stop() {
        Ok(pcm) => pcm,
        Err(e) => {
            eprintln!("[whisp] capture stop failed: {e:#}");
            go_idle(shared);
            return;
        }
    };

    let Some(engine) = engine.clone() else {
        eprintln!("[whisp] asr engine not ready yet; dropping utterance");
        go_idle(shared);
        return;
    };

    let generation = with_inner(shared, |i| i.generation);
    set_state(shared, DaemonState::Transcribing);

    let ev_tx = ev_tx.clone();
    thread::spawn(move || {
        let result = match engine.transcribe(&pcm) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[whisp] transcribe error: {e:#}");
                None
            }
        };
        let _ = ev_tx.send(Event::Transcribed { generation, result });
    });
}

/// Cancel (Esc): abort any recording, bump the generation to invalidate an
/// in-flight transcription, and return to the idle base state.
fn on_cancel(shared: &Shared, capture: &mut Option<audio::Capture>) {
    if let Some(cap) = capture.take() {
        let _ = cap.stop(); // discard audio; stop() also joins the reader thread
    }
    bump_generation(shared);
    eprintln!("[whisp] cancelled");
    go_idle(shared);
}

/// Headset presence change. Losing the headset mid-session cancels it (drop the
/// capture, bump the generation) and drops to HeadsetMissing; gaining it (or any
/// other change) just reconciles the idle base state.
fn on_headset(shared: &Shared, capture: &mut Option<audio::Capture>, src: Option<String>) {
    let present = src.is_some();
    with_inner(shared, |i| i.source = src);
    if present {
        eprintln!("[whisp] headset connected");
        reconcile_idle(shared);
    } else {
        eprintln!("[whisp] headset disconnected");
        if let Some(cap) = capture.take() {
            let _ = cap.stop();
        }
        bump_generation(shared);
        go_idle(shared); // base state is now HeadsetMissing
    }
}

/// A transcription result arrived. This is the serialized inject decision point:
/// inject only if the session is still current (generation matches) and the
/// daemon is enabled with the headset present.
fn on_transcribed(shared: &Shared, generation: u64, result: Option<String>) {
    let (cur_gen, enabled, present) =
        with_inner(shared, |i| (i.generation, i.enabled, i.source.is_some()));

    let current = generation == cur_gen && enabled && present;
    if !current {
        eprintln!("[whisp] dropping stale/invalidated transcript (gen {generation} vs {cur_gen})");
    } else if let Some(text) = result {
        // Injection gating (modifiers-clear poll) lives inside inject().
        match inject::inject(&text, hotkey::modifiers_clear) {
            Ok(true) => eprintln!("[whisp] injected: {text:?}"),
            Ok(false) => eprintln!("[whisp] injection dropped (modifiers held 5 s)"),
            Err(e) => eprintln!("[whisp] inject error: {e:#}"),
        }
    } else {
        eprintln!("[whisp] transcript gated to none");
    }
    go_idle(shared);
}

// ---------------------------------------------------------------------------
// State helpers.
// ---------------------------------------------------------------------------

/// Lock the shared inner state (recovering from poisoning) and run `f`.
fn with_inner<R>(shared: &Shared, f: impl FnOnce(&mut Inner) -> R) -> R {
    let mut guard = match shared.0.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    f(&mut guard)
}

/// The state the daemon should show when idle, derived from presence + enabled.
fn base_state(inner: &Inner) -> DaemonState {
    if inner.source.is_none() {
        DaemonState::HeadsetMissing
    } else if !inner.enabled {
        DaemonState::Disabled
    } else {
        DaemonState::Ready
    }
}

/// Force `state` and write the status file if it actually changed.
fn set_state(shared: &Shared, new: DaemonState) {
    let changed = with_inner(shared, |i| {
        let changed = i.state != new;
        i.state = new;
        changed
    });
    if changed {
        status::write_status(&new);
    }
}

/// Drop to the idle base state (Ready/Disabled/HeadsetMissing) unconditionally.
fn go_idle(shared: &Shared) {
    let (base, changed) = with_inner(shared, |i| {
        let base = base_state(i);
        let changed = i.state != base;
        i.state = base;
        (base, changed)
    });
    if changed {
        status::write_status(&base);
    }
}

/// Reconcile only when currently idle — used on the timeout tick to pick up a
/// socket-driven toggle without disturbing an active Recording/Transcribing.
fn reconcile_idle(shared: &Shared) {
    let update = with_inner(shared, |i| {
        let idle = matches!(
            i.state,
            DaemonState::Ready | DaemonState::Disabled | DaemonState::HeadsetMissing
        );
        if !idle {
            return None;
        }
        let base = base_state(i);
        if i.state != base {
            i.state = base;
            Some(base)
        } else {
            None
        }
    });
    if let Some(base) = update {
        status::write_status(&base);
    }
}

/// Bump the generation counter, invalidating any in-flight pipeline result.
fn bump_generation(shared: &Shared) {
    with_inner(shared, |i| i.generation = i.generation.wrapping_add(1));
}

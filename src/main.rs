//! whisp-arch daemon entrypoint + state-owner thread. OWNER: integration agent.
//!
//! Architecture (SPEC §main): a single state-owner loop owns the state machine.
//! Every input — PTT hotkey events, headset-presence changes from the audio
//! watcher, streamed audio levels, remote partial/final transcripts, and local
//! transcription results — arrives on one mpsc channel and is handled serially,
//! so state transitions never race.
//!
//! Dictation path (Phase E): on PttDown we open a remote session to the whisp
//! gateway on the compute box and stream 16 kHz audio while the key is held; the
//! gateway returns live partials and, after PttUp, the LLM-cleaned final text.
//! If the gateway is unreachable or fails, the buffered utterance goes through
//! the local whisper engine instead (the pre-Phase-E path).
//!
//! The control socket (`status::spawn_control`) mutates `enabled`/`generation`
//! on the shared `Inner` directly; the owner loop reconciles the visible
//! `DaemonState` from that flag on a short `recv_timeout` tick.
//!
//! Generation counter (design-review invariant): a session's generation is
//! captured at PttDown; Cancel, headset-loss, and toggle-off all bump it. The
//! delivery decision is made *on this owner thread* immediately before pasting:
//! we drop the transcript unless its generation still matches AND the daemon is
//! enabled with the headset present.

mod asr;
mod audio;
mod hotkey;
mod inject;
mod remote;
mod sounds;
mod state;
mod status;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::hotkey::{HotkeyEvent, HotkeySignal};
use crate::state::{DaemonState, Inner, Shared};

/// How often the owner loop wakes to reconcile socket-driven toggles when no
/// other event is pending.
const RECONCILE_TICK: Duration = Duration::from_millis(200);
/// Capture continues this long after the key is released. People let go while
/// the last word is still being spoken; without a tail the recognizer never
/// hears its end and drops the whole word (measured: 150 ms missing from the
/// final word loses it entirely). `WHISP_TAIL_MS` overrides, 0 disables.
const RELEASE_TAIL_MS_DEFAULT: u64 = 400;

fn release_tail() -> Duration {
    static TAIL: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *TAIL.get_or_init(|| {
        let ms = std::env::var("WHISP_TAIL_MS").ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(RELEASE_TAIL_MS_DEFAULT);
        Duration::from_millis(ms.min(2000))
    })
}
/// Minimum spacing between `level` events sent to the overlay.
const LEVEL_EVENT_MIN_GAP: Duration = Duration::from_millis(40);
/// A recording longer than this is ended as if the key were released (missed
/// release event, stuck key).
const MAX_RECORDING: Duration = Duration::from_secs(45);
/// Default gateway when `WHISP_GATEWAY` is unset: the compute box over Tailscale.
const DEFAULT_GATEWAY: &str = "ws://100.122.171.110:8082";

/// Events funneled into the single state-owner loop.
enum Event {
    /// A raw PTT/Esc event from the evdev listener.
    Hotkey(HotkeySignal),
    /// Headset presence changed (pulse source name, or `None` when absent).
    Headset(Option<String>),
    /// Streamed audio block level (RMS) + spectrum bands while recording.
    Level(f32, [f32; audio::N_BANDS]),
    /// Interim transcript from the gateway for session `generation`.
    Partial { generation: u64, text: String },
    /// The remote session for `generation` finished.
    RemoteDone {
        generation: u64,
        outcome: remote::Outcome,
    },
    /// The post-release capture tail for session `generation` has elapsed:
    /// stop the capture now and send `End`.
    ReleaseTail { generation: u64 },
    /// The capture thread finished winding down after release (full utterance,
    /// kept for the local fallback).
    CaptureDone {
        generation: u64,
        result: Result<Vec<f32>>,
    },
    /// A local (whisper) pipeline thread finished; `generation` is its session.
    Transcribed {
        generation: u64,
        result: Option<String>,
    },
    /// The whisper model finished loading in the background.
    EngineLoaded(Arc<asr::Engine>),
}

/// Runtime configuration from the environment.
struct Config {
    /// `ws://host:port` of the gateway, or `None` to run local-only.
    gateway: Option<String>,

}

impl Config {
    fn from_env() -> Config {
        let local_only = std::env::var("WHISP_LOCAL_ONLY").map(|v| v == "1").unwrap_or(false);
        let gateway = if local_only {
            None
        } else {
            Some(std::env::var("WHISP_GATEWAY").unwrap_or_else(|_| DEFAULT_GATEWAY.to_string()))
        };
        Config { gateway }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--toggle") => std::process::exit(cli_toggle()),
        Some("--status") => std::process::exit(cli_status()),
        Some("--test-sounds") => {
            if let Err(e) = sounds::preview() {
                eprintln!("[whisp] sound preview failed: {e:#}");
                std::process::exit(1);
            }
        }
        Some("--help" | "-h") => {
            println!("whisp-arch — push-to-talk dictation daemon");
            println!("  (no args)   run the daemon");
            println!("  --toggle    toggle enabled via the control socket, then exit");
            println!("  --status    print the current waybar status JSON, then exit");
            println!("  --test-sounds  play the start and stop recording cues, then exit");
            println!("env: WHISP_GATEWAY=ws://host:port (default {DEFAULT_GATEWAY}), WHISP_LOCAL_ONLY=1");
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
    let config = Config::from_env();

    // Events socket first so the overlay can attach before anything happens.
    if let Err(e) = status::spawn_events() {
        eprintln!("[whisp] events socket unavailable: {e:#}");
    }

    // Write an initial status immediately so waybar sees a file right away.
    status::write_status(&DaemonState::HeadsetMissing);

    // Control socket (toggle/status). Non-fatal if it fails.
    if let Err(e) = status::spawn_control(shared.clone()) {
        eprintln!("[whisp] control socket unavailable: {e:#}");
    }

    let (ev_tx, ev_rx) = mpsc::channel::<Event>();

    // Hotkey listener → bridge its HotkeyEvent channel into the unified Event bus.
    let (hk_tx, hk_rx) = mpsc::channel::<HotkeySignal>();
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

    // Load the local whisper model off the hot path (fallback engine).
    {
        let ev_tx = ev_tx.clone();
        thread::spawn(move || match asr::Engine::load() {
            Ok(engine) => {
                let _ = ev_tx.send(Event::EngineLoaded(Arc::new(engine)));
            }
            Err(e) => eprintln!("[whisp] asr engine load failed: {e:#}"),
        });
    }

    match &config.gateway {
        Some(g) => eprintln!("[whisp] daemon started (hold {:?} to dictate; gateway {g}; window-bound paste delivery)", hotkey::ptt_key()),
        None => eprintln!("[whisp] daemon started (hold {:?} to dictate; local-only)", hotkey::ptt_key()),
    }
    owner_loop(&shared, &config, &ev_tx, ev_rx);
    Ok(())
}

/// Everything the owner loop holds for the current utterance.
struct Live {
    /// Persistent mic (ALSA sources): open while idle, armed per utterance.
    mic: Option<audio::Mic>,
    /// True while the persistent mic is delivering chunks for this utterance.
    mic_armed: bool,
    /// Per-utterance capture (Bluetooth sources, or when the persistent mic failed).
    capture: Option<audio::Capture>,
    /// Remote session, if the gateway is in use for this utterance.
    remote: Option<remote::Session>,
    /// Session generation captured at PttDown.
    generation: u64,
    /// Whole utterance kept after PttUp for the local fallback.
    fallback_pcm: Option<Vec<f32>>,
    /// True between PttUp and CaptureDone.
    capture_pending: bool,
    /// Owner's clone of the chunk sender, used to inject the end marker.
    chunk_tx: Option<mpsc::Sender<audio::Chunk>>,
    /// True once the remote session has reported failure for this utterance.
    remote_failed: bool,
    /// When the current recording started (watchdog).
    recording_since: Option<Instant>,
    /// When the PTT key went up (for the tail timing log line).
    released_at: Option<Instant>,
    /// When the current utterance started (latency logging; kept past release).
    utterance_started: Option<Instant>,
    /// Set once the first partial of the utterance has been logged.
    first_partial_logged: bool,
    /// Spare pre-connected gateway socket keeper.
    warm: Option<remote::Warm>,
    /// Destination captured on PTT release; never replaced by later focus.
    target: Option<inject::WindowTarget>,
    /// A rare early final result waits for release instead of targeting a new window.
    pending_final: Option<Option<String>>,
}

/// The serialized state-owner loop.
fn owner_loop(shared: &Shared, config: &Config, ev_tx: &mpsc::Sender<Event>, ev_rx: mpsc::Receiver<Event>) {
    let mut engine: Option<Arc<asr::Engine>> = None;
    let mut live = Live {
        mic: None,
        mic_armed: false,
        capture: None,
        remote: None,
        generation: 0,
        fallback_pcm: None,
        capture_pending: false,
        chunk_tx: None,
        remote_failed: false,
        recording_since: None,
        released_at: None,
        utterance_started: None,
        first_partial_logged: false,
        warm: config.gateway.clone().map(remote::Warm::start),
        target: None,
        pending_final: None,
    };
    let mut last_level = Instant::now() - LEVEL_EVENT_MIN_GAP;

    loop {
        // Watchdog: a capture that ended by itself (duration cap / read error) or
        // a recording that has run past MAX_RECORDING is treated as a release.
        {
            let too_long = live.recording_since.is_some_and(|t| t.elapsed() > MAX_RECORDING);
            let cap_finished = live.capture.as_ref().is_some_and(|c| c.is_finished());
            let mic_died = live.mic_armed && live.mic.as_ref().is_some_and(|m| !m.is_alive());
            if (live.capture.is_some() || live.mic_armed) && (too_long || cap_finished || mic_died) {
                eprintln!("[whisp] recording ended by watchdog (too_long={too_long}, cap_finished={cap_finished}, mic_died={mic_died})");
                on_ptt_up(shared, &mut live, &engine, ev_tx, inject::capture_target());
            }
        }
        match ev_rx.recv_timeout(RECONCILE_TICK) {
            Ok(Event::Hotkey(HotkeySignal { event: HotkeyEvent::PttDown, .. })) => on_ptt_down(shared, config, &mut live, ev_tx),
            Ok(Event::Hotkey(HotkeySignal { event: HotkeyEvent::PttUp, target })) => on_ptt_up(shared, &mut live, &engine, ev_tx, target),
            Ok(Event::Hotkey(HotkeySignal { event: HotkeyEvent::Cancel, .. })) => on_cancel(shared, &mut live),
            Ok(Event::Headset(src)) => on_headset(shared, &mut live, src),
            Ok(Event::Level(rms, bands)) => {
                if last_level.elapsed() >= LEVEL_EVENT_MIN_GAP {
                    last_level = Instant::now();
                    let b: Vec<String> = bands.iter().map(|v| format!("{v:.3}")).collect();
                    status::publish(&format!(r#"{{"ev":"level","rms":{rms:.4},"bands":[{}]}}"#, b.join(",")));
                }
            }
            Ok(Event::Partial { generation, text }) => {
                if generation == live.generation {
                    if !live.first_partial_logged && !text.trim().is_empty() {
                        live.first_partial_logged = true;
                        if let Some(t) = live.utterance_started {
                            eprintln!("[whisp] first words {} ms after key press: {text:?}", t.elapsed().as_millis());
                        }
                    }
                    status::publish(&format!(r#"{{"ev":"partial","text":{}}}"#, status::json_str(&text)));
                }
            }
            Ok(Event::RemoteDone { generation, outcome }) => {
                on_remote_done(shared, &mut live, &engine, ev_tx, generation, outcome)
            }
            Ok(Event::ReleaseTail { generation }) => on_release_tail(shared, &mut live, &engine, ev_tx, generation),
            Ok(Event::CaptureDone { generation, result }) => {
                on_capture_done(shared, &mut live, &engine, ev_tx, generation, result)
            }
            Ok(Event::Transcribed { generation, result }) => on_transcribed(shared, &mut live, generation, result),
            Ok(Event::EngineLoaded(eng)) => {
                eprintln!("[whisp] local asr engine ready (fallback)");
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

/// PttDown: begin capture (streaming blocks) and open the remote session.
fn on_ptt_down(shared: &Shared, config: &Config, live: &mut Live, ev_tx: &mpsc::Sender<Event>) {
    let (state, source, generation) = with_inner(shared, |i| (i.state, i.source.clone(), i.generation));
    if state != DaemonState::Ready {
        return;
    }
    let Some(src) = source else {
        return;
    };

    // Remote session first so audio queued during the handshake is not lost.
    let remote = config.gateway.as_ref().map(|url| {
        let (app_class, app_title) = remote::active_window();
        let ctx = remote::StartCtx {
            app_class,
            app_title,
            dictionary: remote::load_dictionary(),
        };
        let tx_p = ev_tx.clone();
        let tx_d = ev_tx.clone();
        let warm_sock = live.warm.as_ref().and_then(|w| w.take());
        remote::Session::start(
            url.clone(),
            warm_sock,
            ctx,
            move |text| {
                let _ = tx_p.send(Event::Partial { generation, text });
            },
            move |outcome| {
                let _ = tx_d.send(Event::RemoteDone { generation, outcome });
            },
        )
    });

    // Streaming blocks → levels for the bar + audio for the remote session.
    // The forwarder sends `End` itself when the capture closes the channel, so
    // `End` can never overtake audio blocks still queued here (ordering bug
    // found on the first live test: short utterances arrived truncated).
    let (chunk_tx, chunk_rx) = mpsc::channel::<audio::Chunk>();
    {
        let ev_tx = ev_tx.clone();
        let remote_tx = remote.as_ref().map(|r| r.sender());
        let dump = std::env::var_os("WHISP_DUMP").is_some();
        thread::spawn(move || {
            let mut dumped: Vec<f32> = Vec::new();
            for chunk in chunk_rx {
                if chunk.last {
                    break; // owner released the key: everything before this is in order
                }
                let _ = ev_tx.send(Event::Level(chunk.rms, chunk.bands));
                if dump {
                    dumped.extend_from_slice(&chunk.pcm);
                }
                if let Some(r) = &remote_tx {
                    let _ = r.send(remote::Cmd::Audio(chunk.pcm));
                }
            }
            if let Some(r) = &remote_tx {
                let _ = r.send(remote::Cmd::End);
            }
            if dump {
                write_wav("/tmp/whisp-last-stream.wav", &dumped);
                // Keep the last ten utterances for post-mortems of lost words.
                let dir = std::path::Path::new("/tmp/whisp-dumps");
                if std::fs::create_dir_all(dir).is_ok() {
                    let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
                    write_wav(&format!("/tmp/whisp-dumps/{ms}.wav"), &dumped);
                    if let Ok(rd) = std::fs::read_dir(dir) {
                        let mut names: Vec<_> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
                        names.sort();
                        for old in names.iter().take(names.len().saturating_sub(10)) {
                            let _ = std::fs::remove_file(old);
                        }
                    }
                }
            }
        });
    }

    // Reopen a persistent mic that died (e.g. the source went away and came back).
    if live.mic.as_ref().is_some_and(|mic| !mic.is_alive()) {
        live.mic = None;
    }
    if live.mic.is_none() && !audio::source_is_bluetooth(&src) {
        match audio::Mic::open(&src) {
            Ok(mic) => live.mic = Some(mic),
            Err(e) => eprintln!("[whisp] persistent mic unavailable ({e:#}); using per-utterance capture"),
        }
    }

    let started = if let Some(mic) = &live.mic {
        mic.arm(chunk_tx);
        live.mic_armed = true;
        live.chunk_tx = None; // the mic thread sends the end marker itself
        true
    } else {
        let owner_chunk_tx = chunk_tx.clone();
        match audio::Capture::start(&src, Some(chunk_tx)) {
            Ok(cap) => {
                live.capture = Some(cap);
                live.chunk_tx = Some(owner_chunk_tx);
                true
            }
            Err(e) => {
                eprintln!("[whisp] failed to start capture: {e:#}");
                false
            }
        }
    };
    if started {
        live.remote = remote;
        live.generation = generation;
        live.fallback_pcm = None;
        live.capture_pending = false;
        live.target = None;
        live.pending_final = None;
        live.remote_failed = false;
        live.recording_since = Some(Instant::now());
        live.utterance_started = Some(Instant::now());
        live.first_partial_logged = false;
        set_state(shared, DaemonState::Recording);
        sounds::play(sounds::Cue::Start);
    } else if let Some(r) = remote {
        r.cancel();
    }
}

/// PttUp: move to Transcribing and start the release tail; the capture keeps
/// streaming until `ReleaseTail` fires and `finish_utterance` sends `End`.
fn on_ptt_up(shared: &Shared, live: &mut Live, engine: &Option<Arc<asr::Engine>>, ev_tx: &mpsc::Sender<Event>, target: Option<inject::WindowTarget>) {
    let state = with_inner(shared, |i| i.state);
    if state != DaemonState::Recording {
        return;
    }
    live.target = target;
    let held = live.recording_since.take().map(|t| t.elapsed().as_millis()).unwrap_or(0);
    live.released_at = Some(Instant::now());
    set_state(shared, DaemonState::Transcribing);
    let tail = release_tail();
    eprintln!("[whisp] released after {held} ms; capture tail {} ms", tail.as_millis());
    if tail.is_zero() {
        finish_utterance(live, engine, ev_tx);
        flush_pending(shared, live);
        return;
    }
    // Keep capturing (and streaming) through the tail; the timer finishes the
    // utterance. A cancel in between disarms the mic and bumps the generation,
    // so the late event is a no-op.
    let generation = live.generation;
    let tx = ev_tx.clone();
    thread::spawn(move || {
        thread::sleep(tail);
        let _ = tx.send(Event::ReleaseTail { generation });
    });
}

/// The release tail elapsed (or was skipped): stop the capture and send `End`.
fn on_release_tail(shared: &Shared, live: &mut Live, engine: &Option<Arc<asr::Engine>>, ev_tx: &mpsc::Sender<Event>, generation: u64) {
    let state = with_inner(shared, |i| i.state);
    if generation != live.generation || state != DaemonState::Transcribing || !(live.mic_armed || live.capture.is_some()) {
        return;
    }
    finish_utterance(live, engine, ev_tx);
    flush_pending(shared, live);
}

fn flush_pending(shared: &Shared, live: &mut Live) {
    if let Some(result) = live.pending_final.take() {
        deliver(shared, live, live.generation, result);
    }
}

/// Mark the end of the utterance (ordered after all queued audio) and stop the
/// capture without waiting for it. The remote session gets `End` from the
/// forwarder; the full utterance arrives later as `CaptureDone` for the local
/// fallback.
fn finish_utterance(live: &mut Live, engine: &Option<Arc<asr::Engine>>, ev_tx: &mpsc::Sender<Event>) {
    if let Some(t) = live.released_at.take() {
        eprintln!("[whisp] capture stopped {} ms after release", t.elapsed().as_millis());
    }
    let generation = live.generation;
    let tx = ev_tx.clone();
    if live.mic_armed {
        live.mic_armed = false;
        match &live.mic {
            Some(mic) => mic.disarm(move |pcm| {
                sounds::play(sounds::Cue::Stop);
                let _ = tx.send(Event::CaptureDone { generation, result: Ok(pcm) });
            }),
            None => return,
        }
    } else {
        let Some(cap) = live.capture.take() else {
            return;
        };
        if let Some(tx) = live.chunk_tx.take() {
            let _ = tx.send(audio::Chunk::end_marker());
        }
        cap.stop_async(move |result| {
            sounds::play(sounds::Cue::Stop);
            let _ = tx.send(Event::CaptureDone { generation, result });
        });
    }
    live.capture_pending = true;

    if live.remote.is_none() || live.remote_failed {
        // No remote path: the local fallback runs as soon as the audio lands.
        live.remote = None;
        let _ = engine; // used when CaptureDone arrives
    }
}

/// The capture thread delivered the full utterance.
fn on_capture_done(shared: &Shared, live: &mut Live, engine: &Option<Arc<asr::Engine>>, ev_tx: &mpsc::Sender<Event>, generation: u64, result: Result<Vec<f32>>) {
    if generation != live.generation {
        return;
    }
    live.capture_pending = false;
    match result {
        Ok(pcm) => live.fallback_pcm = Some(pcm),
        Err(e) => eprintln!("[whisp] capture stop failed: {e:#}"),
    }
    // Remote already finished (success or empty): nothing more to do.
    let state = with_inner(shared, |i| i.state);
    if state != DaemonState::Transcribing {
        live.fallback_pcm = None;
        return;
    }
    if live.remote.is_none() || live.remote_failed {
        live.remote = None;
        run_local_fallback(shared, live, engine, ev_tx);
    }
}

/// Transcribe the kept utterance with the local whisper engine.
fn run_local_fallback(shared: &Shared, live: &mut Live, engine: &Option<Arc<asr::Engine>>, ev_tx: &mpsc::Sender<Event>) {
    let Some(pcm) = live.fallback_pcm.take() else {
        if live.capture_pending {
            return; // CaptureDone will call us again
        }
        go_idle(shared);
        return;
    };
    let Some(engine) = engine.clone() else {
        eprintln!("[whisp] no gateway and local asr engine not ready; dropping utterance");
        status::publish(r#"{"ev":"error","message":"no ASR available"}"#);
        go_idle(shared);
        return;
    };
    status::publish(r#"{"ev":"fallback"}"#);
    let generation = live.generation;
    let ev_tx = ev_tx.clone();
    thread::spawn(move || {
        let result = match engine.transcribe(&pcm) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[whisp] local transcribe error: {e:#}");
                None
            }
        };
        let _ = ev_tx.send(Event::Transcribed { generation, result });
    });
}

/// The remote session reported its outcome.
fn on_remote_done(
    shared: &Shared,
    live: &mut Live,
    engine: &Option<Arc<asr::Engine>>,
    ev_tx: &mpsc::Sender<Event>,
    generation: u64,
    outcome: remote::Outcome,
) {
    if generation != live.generation {
        eprintln!("[whisp] remote: ignoring outcome for stale session {generation}");
        return;
    }
    let still_recording = live.capture.is_some() || live.mic_armed;
    match outcome {
        remote::Outcome::Final { raw, text, cleaned, asr_ms, llm_ms } => {
            eprintln!(
                "[whisp] remote: final (asr {asr_ms} ms, llm {llm_ms} ms, cleaned={cleaned}) raw={raw:?}"
            );
            status::publish(&format!(
                r#"{{"ev":"final","text":{},"raw":{},"cleaned":{cleaned},"asr_ms":{asr_ms},"llm_ms":{llm_ms}}}"#,
                status::json_str(&text),
                status::json_str(&raw)
            ));
            live.remote = None;
            live.fallback_pcm = None;
            deliver(shared, live, generation, Some(text));
        }
        remote::Outcome::Empty => {
            eprintln!("[whisp] remote: heard nothing");
            status::publish(r#"{"ev":"final","text":"","raw":"","cleaned":false,"asr_ms":0,"llm_ms":0}"#);
            live.remote = None;
            live.fallback_pcm = None;
            deliver(shared, live, generation, None);
        }
        remote::Outcome::Failed(msg) => {
            eprintln!("[whisp] remote: failed ({msg})");
            live.remote = None;
            live.remote_failed = true;
            status::publish(r#"{"ev":"fallback"}"#);
            if !still_recording && !live.capture_pending {
                run_local_fallback(shared, live, engine, ev_tx);
            }
            // else: PttUp / CaptureDone will run the fallback when the audio lands.
        }
    }
}

/// Cancel (Esc): abort any recording, bump the generation to invalidate an
/// in-flight transcription, and return to the idle base state.
fn on_cancel(shared: &Shared, live: &mut Live) {
    if let Some(cap) = live.capture.take() {
        cap.stop_async(|_| sounds::play(sounds::Cue::Stop)); // discard audio without blocking
    }
    if live.mic_armed {
        live.mic_armed = false;
        if let Some(mic) = &live.mic {
            mic.disarm(|_| sounds::play(sounds::Cue::Stop));
        }
    }
    live.chunk_tx = None;
    if let Some(r) = live.remote.take() {
        r.cancel();
    }
    live.fallback_pcm = None;
    live.capture_pending = false;
    live.target = None;
    live.pending_final = None;
    bump_generation(shared);
    eprintln!("[whisp] cancelled");
    status::publish(r#"{"ev":"cancel"}"#);
    go_idle(shared);
}

/// Headset presence change. Losing the headset mid-session cancels it.
fn on_headset(shared: &Shared, live: &mut Live, src: Option<String>) {
    let present = src.is_some();
    with_inner(shared, |i| i.source = src);
    if present {
        eprintln!("[whisp] mic source present");
        let src = with_inner(shared, |i| i.source.clone()).unwrap_or_default();
        if live.mic.is_none() && !audio::source_is_bluetooth(&src) {
            match audio::Mic::open(&src) {
                Ok(mic) => live.mic = Some(mic),
                Err(e) => eprintln!("[whisp] persistent mic unavailable ({e:#}); will use per-utterance capture"),
            }
        }
        reconcile_idle(shared);
    } else {
        eprintln!("[whisp] mic source gone");
        let was_recording = live.mic_armed || live.capture.is_some();
        if let Some(cap) = live.capture.take() {
            cap.stop_async(|_| {});
        }
        live.mic_armed = false;
        live.mic = None; // dropping stops the reader thread
        if was_recording {
            sounds::play(sounds::Cue::Stop);
        }
        live.chunk_tx = None;
        if let Some(r) = live.remote.take() {
            r.cancel();
        }
        live.fallback_pcm = None;
        live.target = None;
        live.pending_final = None;
        bump_generation(shared);
        go_idle(shared); // base state is now HeadsetMissing
    }
}

/// A local transcription result arrived.
fn on_transcribed(shared: &Shared, live: &mut Live, generation: u64, result: Option<String>) {
    if let Some(text) = &result {
        status::publish(&format!(
            r#"{{"ev":"final","text":{},"raw":{},"cleaned":false,"asr_ms":0,"llm_ms":0}}"#,
            status::json_str(text),
            status::json_str(text)
        ));
    }
    deliver(shared, live, generation, result);
}

/// Deliver only the current generation, to the release-time window identity.
fn deliver(shared: &Shared, live: &mut Live, generation: u64, result: Option<String>) {
    let (cur_gen, enabled, present, state) =
        with_inner(shared, |i| (i.generation, i.enabled, i.source.is_some(), i.state));
    if generation != cur_gen {
        // An old worker must not reset the state or destination of a newer utterance.
        eprintln!("[whisp] dropping stale transcript (gen {generation} vs {cur_gen})");
        return;
    }
    if !enabled || !present {
        eprintln!("[whisp] dropping stale/invalidated transcript (gen {generation} vs {cur_gen})");
        live.pending_final = None;
        go_idle(shared);
        return;
    }
    if state == DaemonState::Recording || live.mic_armed || live.capture.is_some() {
        live.pending_final = Some(result);
        return;
    }
    if state != DaemonState::Transcribing { return; }
    if let Some(text) = result.filter(|t| !t.trim().is_empty()) {
        match inject::inject_to(&text, live.target.as_ref(), hotkey::modifiers_clear) {
            Ok(inject::Delivery::Pasted) => eprintln!("[whisp] pasted transcript into release-time destination"),
            Ok(inject::Delivery::ClipboardOnly) => eprintln!("[whisp] transcript retained in clipboard; no window injection"),
            Err(e) => {
                eprintln!("[whisp] delivery error: {e:#}");
                status::publish(r#"{"ev":"error","message":"Paste failed; check the clipboard for the transcript"}"#);
            }
        }
    }
    live.target = None;
    live.pending_final = None;
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

/// Debug aid (`WHISP_DUMP=1`): write 16 kHz mono f32 samples as a 16-bit WAV.
fn write_wav(path: &str, pcm: &[f32]) {
    let mut bytes: Vec<u8> = Vec::with_capacity(44 + pcm.len() * 2);
    let data_len = (pcm.len() * 2) as u32;
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes()); // PCM
    bytes.extend_from_slice(&1u16.to_le_bytes()); // mono
    bytes.extend_from_slice(&16000u32.to_le_bytes());
    bytes.extend_from_slice(&32000u32.to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_len.to_le_bytes());
    for s in pcm {
        bytes.extend_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
    }
    match std::fs::write(path, bytes) {
        Ok(()) => eprintln!("[whisp] dump: wrote {} samples to {path}", pcm.len()),
        Err(e) => eprintln!("[whisp] dump failed: {e}"),
    }
}

/// Bump the generation counter, invalidating any in-flight pipeline result.
fn bump_generation(shared: &Shared) {
    with_inner(shared, |i| i.generation = i.generation.wrapping_add(1));
}

#[cfg(test)]
mod delivery_tests {
    use super::*;

    fn live() -> Live {
        Live {
            mic: None, mic_armed: false, capture: None, remote: None,
            generation: 4, fallback_pcm: None, capture_pending: false,
            chunk_tx: None, remote_failed: false, recording_since: None,
            released_at: None, utterance_started: None, first_partial_logged: false,
            warm: None, target: None, pending_final: None,
        }
    }
    fn shared(state: DaemonState) -> Shared {
        let shared = Shared::new();
        with_inner(&shared, |i| {
            i.generation = 4; i.state = state; i.source = Some("test-mic".into());
        });
        shared
    }
    #[test]
    fn early_final_waits_until_button_release() {
        let shared = shared(DaemonState::Recording); let mut live = live();
        deliver(&shared, &mut live, 4, Some("ready early".into()));
        assert_eq!(live.pending_final, Some(Some("ready early".into())));
        assert_eq!(with_inner(&shared, |i| i.state), DaemonState::Recording);
    }
    #[test]
    fn final_during_release_tail_waits_for_capture_to_stop() {
        let shared = shared(DaemonState::Transcribing); let mut live = live();
        live.mic_armed = true;
        deliver(&shared, &mut live, 4, Some("final".into()));
        assert_eq!(live.pending_final, Some(Some("final".into())));
        assert_eq!(with_inner(&shared, |i| i.state), DaemonState::Transcribing);
    }
    #[test]
    fn stale_final_cannot_reset_a_new_recording() {
        let shared = shared(DaemonState::Recording); let mut live = live();
        live.pending_final = Some(Some("new session".into()));
        deliver(&shared, &mut live, 3, Some("stale".into()));
        assert_eq!(live.pending_final, Some(Some("new session".into())));
        assert_eq!(with_inner(&shared, |i| i.state), DaemonState::Recording);
    }
}

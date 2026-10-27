//! Headset capture + presence watcher. OWNER: audio agent.
//!
//! PipeWire-pulse capture from the Sony WH-1000XM5 bluetooth mic. Source detection
//! matches a non-monitor `bluez` source carrying the headset MAC; capture happens at
//! the source's native rate/channels via libpulse-simple, then we downmix to mono and
//! linear-resample to 16 kHz on `stop()`.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{anyhow, bail, Context as _, Result};

use libpulse_binding::callbacks::ListResult;
use libpulse_binding::context::{Context, FlagSet as ContextFlagSet};
use libpulse_binding::mainloop::threaded::Mainloop;
use libpulse_binding::operation::State as OpState;
use libpulse_binding::def::BufferAttr;
use libpulse_binding::sample::{Format, Spec};
use libpulse_binding::stream::Direction;
use libpulse_simple_binding::Simple;

/// Headset MAC in canonical lowercase colon form; also matched in `_` form.
const HEADSET_MAC: &str = "58:18:62:87:81:36";
/// Target sample rate handed to the ASR stage.
const TARGET_RATE: u32 = 16_000;
/// Hard cap on captured audio (seconds); protects against a stuck PTT.
const MAX_SECONDS: usize = 90;
/// Read granularity from pulse (ms). Bounds stop() latency.
const CHUNK_MS: u32 = 20;
/// Streaming block size handed to the remote session (ms of 16 kHz audio).
const STREAM_BLOCK_MS: usize = 80;

/// Record-stream buffering: deliver audio every CHUNK_MS. Without this,
/// pipewire-pulse applies its `pulse.default.frag` of 2 s to a stream that
/// requests nothing, so audio arrived in bursts up to two seconds late:
/// bars trailed the voice and up to 2 s of the latest speech was missing
/// when the key came up (observed 2026-10-11: last words lost on release).
fn low_latency_attr(spec: &Spec) -> BufferAttr {
    let frames = (spec.rate as u64 * CHUNK_MS as u64 / 1000).max(1);
    let fragsize = (frames * spec.channels as u64 * 4) as u32;
    BufferAttr { maxlength: u32::MAX, tlength: u32::MAX, prebuf: u32::MAX, minreq: u32::MAX, fragsize }
}
/// Audio kept from before the key press by the persistent mic (ms).
const PRE_ROLL_MS: usize = 300;
/// Number of spectrum bands reported per block (for the waybar bars).
pub const N_BANDS: usize = 8;
/// Band centre frequencies (Hz), log-spaced over the speech range.
const BAND_HZ: [f32; N_BANDS] = [120.0, 190.0, 300.0, 480.0, 760.0, 1200.0, 1900.0, 3000.0];

/// One streaming block of 16 kHz mono audio emitted while recording, plus its RMS
/// level and a coarse spectrum (for the waybar bars).
pub struct Chunk {
    pub pcm: Vec<f32>,
    pub rms: f32,
    pub bands: [f32; N_BANDS],
    /// Sentinel injected by the owner on key release: "no more audio after this".
    pub last: bool,
}

impl Chunk {
    /// The end-of-utterance marker (sent by the owner through its own sender clone,
    /// so it is ordered after every block the capture thread already queued).
    pub fn end_marker() -> Chunk {
        Chunk { pcm: Vec::new(), rms: 0.0, bands: [0.0; N_BANDS], last: true }
    }
}

/// Handle to an in-progress capture. Drop-free: `stop()` consumes it and joins the
/// reader thread.
pub struct Capture {
    stop_flag: Arc<AtomicBool>,
    handle: Option<JoinHandle<Result<CaptureRaw>>>,
}

/// Raw interleaved capture plus the native spec needed to post-process it.
struct CaptureRaw {
    interleaved: Vec<f32>,
    rate: u32,
    channels: u8,
}

/// Pulse source to capture from, if present right now.
///
/// Selection (`WHISP_SOURCE`):
///   unset / "internal"  -> the laptop's built-in mic (first non-monitor `alsa_input`)
///   "bluez" / "xm5"     -> the Sony XM5 headset mic (MAC match), as in the original spec
///   anything else       -> first non-monitor source whose name contains that substring
///
/// Default is the built-in mic because on 2026-10-10 the XM5's HFP transport failed
/// every time the daemon opened it (see desktop/README.md), starving the capture.
pub fn headset_source() -> Option<String> {
    let pref = std::env::var("WHISP_SOURCE").unwrap_or_default();
    match query_source(&pref) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[whisp] audio: source enumeration failed: {e:#}");
            None
        }
    }
}

/// Pure selection rule over (name, is_monitor) pairs; see `headset_source`.
fn select_source<'a>(names: impl Iterator<Item = (&'a str, bool)>, pref: &str) -> Option<String> {
    let p = pref.trim().to_ascii_lowercase();
    for (name, is_monitor) in names {
        let lower = name.to_ascii_lowercase();
        if is_monitor || lower.ends_with(".monitor") {
            continue;
        }
        let hit = match p.as_str() {
            "" | "internal" | "laptop" => lower.starts_with("alsa_input"),
            "bluez" | "xm5" | "headset" => match_headset(name, is_monitor),
            other => lower.contains(other),
        };
        if hit {
            return Some(name.to_string());
        }
    }
    None
}

/// Poll pulse every 2 s and invoke `cb` whenever the matched headset source changes
/// (including the initial observation). Runs on a detached background thread.
pub fn spawn_watcher(cb: impl Fn(Option<String>) + Send + 'static) {
    std::thread::spawn(move || {
        let mut last: Option<Option<String>> = None;
        loop {
            let cur = headset_source();
            if last.as_ref() != Some(&cur) {
                cb(cur.clone());
                last = Some(cur);
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    });
}

impl Capture {
    /// Begin capturing from `source` (an exact pulse source name). Queries the source's
    /// native spec, then streams f32 chunks on a background thread until `stop()`.
    pub fn start(source: &str, chunk_tx: Option<mpsc::Sender<Chunk>>) -> Result<Capture> {
        let (rate, channels) = native_spec(source)
            .with_context(|| format!("querying native spec of source {source}"))?;
        if rate == 0 || channels == 0 {
            bail!("source {source} reports invalid spec {rate} Hz / {channels} ch");
        }

        let spec = Spec {
            format: Format::FLOAT32NE,
            channels,
            rate,
        };
        if !spec.is_valid() {
            bail!("invalid capture spec {rate} Hz / {channels} ch");
        }

        let source_owned = source.to_string();
        let stop_flag = Arc::new(AtomicBool::new(false));
        let thread_flag = Arc::clone(&stop_flag);

        // Hand the constructed Simple stream out of the thread's init so start() can
        // surface connection errors synchronously.
        let (init_tx, init_rx) = mpsc::channel::<Result<()>>();

        let handle = std::thread::spawn(move || -> Result<CaptureRaw> {
            let attr = low_latency_attr(&spec);
            let simple = match Simple::new(
                None,
                "whisp-arch",
                Direction::Record,
                Some(&source_owned),
                "dictation",
                &spec,
                None,
                Some(&attr),
            ) {
                Ok(s) => {
                    let _ = init_tx.send(Ok(()));
                    s
                }
                Err(e) => {
                    let _ = init_tx.send(Err(anyhow!("pulse connect: {e}")));
                    return Err(anyhow!("pulse connect: {e}"));
                }
            };

            let frames_per_chunk = ((rate as u64 * CHUNK_MS as u64) / 1000).max(1) as usize;
            let samples_per_chunk = frames_per_chunk * channels as usize;
            let max_samples = MAX_SECONDS * rate as usize * channels as usize;

            let mut byte_buf = vec![0u8; samples_per_chunk * 4];
            let mut interleaved: Vec<f32> = Vec::with_capacity(rate as usize * channels as usize);

            // Streaming path: downmix + resample each pulse read incrementally and emit
            // STREAM_BLOCK_MS blocks of 16 kHz mono as they fill.
            let mut resampler = Resampler::new(rate, TARGET_RATE);
            let mut stream_acc: Vec<f32> = Vec::with_capacity(TARGET_RATE as usize);
            let block_len = TARGET_RATE as usize * STREAM_BLOCK_MS / 1000;
            let mut chunk_tx = chunk_tx;

            while !thread_flag.load(Ordering::Relaxed) {
                if let Err(e) = simple.read(&mut byte_buf) {
                    eprintln!("[whisp] audio: read error: {e}");
                    break;
                }
                let before = interleaved.len();
                for b in byte_buf.chunks_exact(4) {
                    let s = f32::from_ne_bytes([b[0], b[1], b[2], b[3]]);
                    interleaved.push(s);
                }
                if let Some(tx) = chunk_tx.as_ref() {
                    let mono = downmix(&interleaved[before..], channels);
                    resampler.process(&mono, &mut stream_acc);
                    while stream_acc.len() >= block_len {
                        let pcm: Vec<f32> = stream_acc.drain(..block_len).collect();
                        let rms = rms_level(&pcm);
                        let bands = spectrum_bands(&pcm, TARGET_RATE);
                        if tx.send(Chunk { pcm, rms, bands, last: false }).is_err() {
                            chunk_tx = None; // receiver gone; stop streaming, keep recording
                            break;
                        }
                    }
                }
                if interleaved.len() >= max_samples {
                    eprintln!("[whisp] audio: hit {MAX_SECONDS}s cap, stopping capture");
                    interleaved.truncate(max_samples);
                    break;
                }
            }

            Ok(CaptureRaw {
                interleaved,
                rate,
                channels,
            })
        });

        match init_rx.recv() {
            Ok(Ok(())) => Ok(Capture {
                stop_flag,
                handle: Some(handle),
            }),
            Ok(Err(e)) => {
                let _ = handle.join();
                Err(e)
            }
            Err(_) => {
                // Thread panicked before reporting init state.
                let _ = handle.join();
                bail!("capture thread died during init")
            }
        }
    }

    /// True once the reader thread has exited on its own (hit the duration cap or
    /// a read error); the owner should then treat the session as released.
    pub fn is_finished(&self) -> bool {
        self.handle.as_ref().is_some_and(|h| h.is_finished())
    }

    /// Stop capture without blocking: flips the flag and joins the reader on a
    /// helper thread, handing the finished utterance to `done`. The Bluetooth
    /// HFP transport can stall a read for many seconds; the owner loop must never
    /// wait on it.
    pub fn stop_async(self, done: impl FnOnce(Result<Vec<f32>>) + Send + 'static) {
        std::thread::spawn(move || done(self.stop()));
    }

    /// Stop capture and return the whole utterance as 16 kHz mono f32.
    pub fn stop(mut self) -> Result<Vec<f32>> {
        self.stop_flag.store(true, Ordering::Relaxed);
        let handle = self
            .handle
            .take()
            .ok_or_else(|| anyhow!("capture already stopped"))?;
        let raw = handle
            .join()
            .map_err(|_| anyhow!("capture thread panicked"))??;

        let mono = downmix(&raw.interleaved, raw.channels);
        let out = resample_linear(&mono, raw.rate, TARGET_RATE);
        eprintln!(
            "[whisp] audio: captured {} native frames @ {} Hz / {} ch -> {} samples @ {} Hz",
            mono.len(),
            raw.rate,
            raw.channels,
            out.len(),
            TARGET_RATE
        );
        Ok(out)
    }
}


// ---------------------------------------------------------------------------
// Persistent mic: the stream stays open while idle so a key press costs nothing
// and the PRE_ROLL_MS before it is included. Used for ALSA sources; Bluetooth
// headsets keep the per-utterance `Capture` so their HFP profile is not held open.
// ---------------------------------------------------------------------------

struct MicState {
    /// Chunk sink while recording; `None` when idle.
    tx: Option<mpsc::Sender<Chunk>>,
    /// Whole utterance (16 kHz mono) accumulated while recording.
    utter: Vec<f32>,
    /// Called from the reader thread with the finished utterance after a stop.
    done: Option<Box<dyn FnOnce(Vec<f32>) + Send>>,
    stop_requested: bool,
}

/// Long-lived capture handle. Dropping it stops the reader thread.
pub struct Mic {
    state: Arc<Mutex<MicState>>,
    alive: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
}

impl Drop for Mic {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Mic {
    /// Open the stream on `source` and start reading on a background thread.
    pub fn open(source: &str) -> Result<Mic> {
        let (rate, channels) = native_spec(source)
            .with_context(|| format!("querying native spec of source {source}"))?;
        if rate == 0 || channels == 0 {
            bail!("source {source} reports invalid spec {rate} Hz / {channels} ch");
        }
        let spec = Spec { format: Format::FLOAT32NE, channels, rate };
        if !spec.is_valid() {
            bail!("invalid capture spec {rate} Hz / {channels} ch");
        }
        let state = Arc::new(Mutex::new(MicState {
            tx: None,
            utter: Vec::new(),
            done: None,
            stop_requested: false,
        }));
        let alive = Arc::new(AtomicBool::new(true));
        let stop = Arc::new(AtomicBool::new(false));
        let (init_tx, init_rx) = mpsc::channel::<Result<()>>();
        let source_owned = source.to_string();
        let t_state = Arc::clone(&state);
        let t_alive = Arc::clone(&alive);
        let t_stop = Arc::clone(&stop);

        std::thread::Builder::new()
            .name("whisp-mic".into())
            .spawn(move || {
                let attr = low_latency_attr(&spec);
                let simple = match Simple::new(
                    None, "whisp-arch", Direction::Record, Some(&source_owned), "dictation", &spec, None, Some(&attr),
                ) {
                    Ok(s) => {
                        let _ = init_tx.send(Ok(()));
                        s
                    }
                    Err(e) => {
                        let _ = init_tx.send(Err(anyhow!("pulse connect: {e}")));
                        t_alive.store(false, Ordering::Relaxed);
                        return;
                    }
                };
                let frames_per_chunk = ((rate as u64 * CHUNK_MS as u64) / 1000).max(1) as usize;
                let samples_per_chunk = frames_per_chunk * channels as usize;
                let mut byte_buf = vec![0u8; samples_per_chunk * 4];
                let mut native: Vec<f32> = Vec::with_capacity(samples_per_chunk);
                let mut out16k: Vec<f32> = Vec::with_capacity(TARGET_RATE as usize / 10);
                let mut resampler = Resampler::new(rate, TARGET_RATE);
                let pre_roll_len = TARGET_RATE as usize * PRE_ROLL_MS / 1000;
                let mut pre_roll: VecDeque<f32> = VecDeque::with_capacity(pre_roll_len + 1024);
                let block_len = TARGET_RATE as usize * STREAM_BLOCK_MS / 1000;
                let max_samples = MAX_SECONDS * TARGET_RATE as usize;
                let mut acc: Vec<f32> = Vec::with_capacity(block_len * 2);
                let mut recording = false;

                while !t_stop.load(Ordering::Relaxed) {
                    if let Err(e) = simple.read(&mut byte_buf) {
                        eprintln!("[whisp] mic: read error: {e}");
                        break;
                    }
                    native.clear();
                    for b in byte_buf.chunks_exact(4) {
                        native.push(f32::from_ne_bytes([b[0], b[1], b[2], b[3]]));
                    }
                    let mono = downmix(&native, channels);
                    out16k.clear();
                    resampler.process(&mono, &mut out16k);

                    // Decide what to do under the lock, act on `done` outside it.
                    let mut finished: Option<(Box<dyn FnOnce(Vec<f32>) + Send>, Vec<f32>)> = None;
                    {
                        let mut st = match t_state.lock() {
                            Ok(g) => g,
                            Err(p) => p.into_inner(),
                        };
                        if !recording && st.tx.is_some() {
                            recording = true;
                            acc.clear();
                            acc.extend(pre_roll.drain(..));
                            st.utter.clear();
                            st.utter.extend_from_slice(&acc);
                        }
                        if recording {
                            acc.extend_from_slice(&out16k);
                            st.utter.extend_from_slice(&out16k);
                            while acc.len() >= block_len {
                                let pcm: Vec<f32> = acc.drain(..block_len).collect();
                                let rms = rms_level(&pcm);
                                let bands = spectrum_bands(&pcm, TARGET_RATE);
                                let ok = st.tx.as_ref().is_some_and(|tx| {
                                    tx.send(Chunk { pcm, rms, bands, last: false }).is_ok()
                                });
                                if !ok {
                                    st.tx = None;
                                }
                            }
                            let capped = st.utter.len() >= max_samples;
                            if st.stop_requested || capped || st.tx.is_none() {
                                if capped {
                                    eprintln!("[whisp] mic: hit {MAX_SECONDS}s cap, ending utterance");
                                }
                                if let Some(tx) = st.tx.take() {
                                    if !acc.is_empty() {
                                        let pcm: Vec<f32> = acc.drain(..).collect();
                                        let rms = rms_level(&pcm);
                                        let bands = spectrum_bands(&pcm, TARGET_RATE);
                                        let _ = tx.send(Chunk { pcm, rms, bands, last: false });
                                    }
                                    let _ = tx.send(Chunk::end_marker());
                                }
                                acc.clear();
                                let utter = std::mem::take(&mut st.utter);
                                if let Some(done) = st.done.take() {
                                    finished = Some((done, utter));
                                }
                                st.stop_requested = false;
                                recording = false;
                            }
                        } else {
                            for &v in &out16k {
                                if pre_roll.len() >= pre_roll_len {
                                    pre_roll.pop_front();
                                }
                                pre_roll.push_back(v);
                            }
                        }
                    }
                    if let Some((done, utter)) = finished {
                        done(utter);
                    }
                }
                // Dying mid-recording: hand over what we have.
                let leftover = {
                    let mut st = match t_state.lock() {
                        Ok(g) => g,
                        Err(p) => p.into_inner(),
                    };
                    if let Some(tx) = st.tx.take() {
                        let _ = tx.send(Chunk::end_marker());
                    }
                    st.done.take().map(|d| (d, std::mem::take(&mut st.utter)))
                };
                if let Some((done, utter)) = leftover {
                    done(utter);
                }
                t_alive.store(false, Ordering::Relaxed);
            })
            .context("spawning mic thread")?;

        match init_rx.recv() {
            Ok(Ok(())) => {
                eprintln!("[whisp] mic: stream open on {source} ({rate} Hz / {channels} ch, pre-roll {PRE_ROLL_MS} ms)");
                Ok(Mic { state, alive, stop })
            }
            Ok(Err(e)) => Err(e),
            Err(_) => bail!("mic thread died during init"),
        }
    }

    /// Start delivering chunks (pre-roll first) to `tx`.
    pub fn arm(&self, tx: mpsc::Sender<Chunk>) {
        if let Ok(mut st) = self.state.lock() {
            st.tx = Some(tx);
            st.stop_requested = false;
        }
    }

    /// Stop delivering chunks. The reader thread sends the end marker after the
    /// last block and then calls `done` with the whole utterance. Never blocks.
    pub fn disarm(&self, done: impl FnOnce(Vec<f32>) + Send + 'static) {
        if let Ok(mut st) = self.state.lock() {
            if st.tx.is_none() {
                return; // not recording; nothing to deliver
            }
            st.done = Some(Box::new(done));
            st.stop_requested = true;
        }
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }
}

/// True for sources whose stream should not be held open while idle.
pub fn source_is_bluetooth(source: &str) -> bool {
    source.to_ascii_lowercase().contains("bluez")
}

// ---------------------------------------------------------------------------
// Pure logic (unit-tested)
// ---------------------------------------------------------------------------

/// True if `name` is the XM5 headset input: contains `bluez`, carries the headset MAC
/// (either `:`- or `_`-separated), and is not a monitor source. Case-insensitive.
fn match_headset(name: &str, is_monitor: bool) -> bool {
    let lower = name.to_ascii_lowercase();
    if is_monitor || lower.ends_with(".monitor") {
        return false;
    }
    if !lower.contains("bluez") {
        return false;
    }
    let mac_colon = HEADSET_MAC; // already lowercase
    let mac_under = HEADSET_MAC.replace(':', "_");
    lower.contains(mac_colon) || lower.contains(&mac_under)
}

/// Average all interleaved channels into a single mono track.
fn downmix(interleaved: &[f32], channels: u8) -> Vec<f32> {
    let ch = channels.max(1) as usize;
    if ch == 1 {
        return interleaved.to_vec();
    }
    let frames = interleaved.len() / ch;
    let mut out = Vec::with_capacity(frames);
    for f in 0..frames {
        let base = f * ch;
        let mut acc = 0.0f32;
        for c in 0..ch {
            acc += interleaved[base + c];
        }
        out.push(acc / ch as f32);
    }
    out
}

/// Root-mean-square level of a block (0.0 silence .. ~1.0 full scale).
fn rms_level(pcm: &[f32]) -> f32 {
    if pcm.is_empty() {
        return 0.0;
    }
    let sum: f32 = pcm.iter().map(|s| s * s).sum();
    (sum / pcm.len() as f32).sqrt()
}

/// Goertzel power at `freq` for one block, normalized to roughly 0..1 for speech.
fn goertzel(pcm: &[f32], rate: u32, freq: f32) -> f32 {
    let n = pcm.len();
    if n == 0 {
        return 0.0;
    }
    let w = 2.0 * std::f32::consts::PI * freq / rate as f32;
    let coeff = 2.0 * w.cos();
    let (mut s1, mut s2) = (0.0f32, 0.0f32);
    for (i, &x) in pcm.iter().enumerate() {
        // Hann window keeps leakage from smearing low bands into high ones.
        let win = 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / n as f32).cos();
        let s0 = x * win + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    let power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
    (power.max(0.0)).sqrt() / (n as f32 * 0.5)
}

/// Coarse log-spaced spectrum of a block, each band ~0..1 (compressed so quiet
/// speech still moves the bars).
fn spectrum_bands(pcm: &[f32], rate: u32) -> [f32; N_BANDS] {
    let mut out = [0.0f32; N_BANDS];
    for (i, &f) in BAND_HZ.iter().enumerate() {
        let a = goertzel(pcm, rate, f);
        // Measured on real speech (16 kHz, normal level): per-band amplitude p50
        // 0.0003, p90 0.005, p99 0.015. This maps p90 to ~0.65 and p99 to full.
        out[i] = (a / 0.012).powf(0.6).min(1.0);
    }
    out
}

/// Stateful linear resampler for streaming use: feed arbitrary-length mono blocks,
/// get a continuous output stream (no seams between blocks).
struct Resampler {
    ratio: f64,
    /// Read position in the virtual input [last, block...]; index 0 is `last`.
    pos: f64,
    last: f32,
    primed: bool,
}

impl Resampler {
    fn new(in_rate: u32, out_rate: u32) -> Self {
        Resampler {
            ratio: in_rate as f64 / out_rate.max(1) as f64,
            pos: 0.0,
            last: 0.0,
            primed: false,
        }
    }

    fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let n = input.len();
        if n == 0 {
            return;
        }
        if !self.primed {
            self.last = input[0];
            self.pos = 1.0; // first output sample is exactly input[0]
            self.primed = true;
        }
        let get = |i: usize| if i == 0 { self.last } else { input[i - 1] };
        // Virtual input has n+1 samples (last + block); interpolation needs i0+1 <= n.
        while self.pos < n as f64 {
            let i0 = self.pos.floor() as usize;
            let frac = (self.pos - i0 as f64) as f32;
            let a = get(i0);
            let b = get(i0 + 1);
            out.push(a + (b - a) * frac);
            self.pos += self.ratio;
        }
        self.last = input[n - 1];
        self.pos -= n as f64;
    }
}

/// Linear-interpolating resampler from `in_rate` to `out_rate` on mono f32.
fn resample_linear(input: &[f32], in_rate: u32, out_rate: u32) -> Vec<f32> {
    if input.is_empty() || in_rate == 0 || out_rate == 0 {
        return Vec::new();
    }
    if in_rate == out_rate {
        return input.to_vec();
    }
    let ratio = in_rate as f64 / out_rate as f64;
    let out_len = ((input.len() as f64) / ratio).floor() as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src = i as f64 * ratio;
        let i0 = src.floor() as usize;
        let frac = (src - i0 as f64) as f32;
        let a = input[i0];
        let b = if i0 + 1 < input.len() {
            input[i0 + 1]
        } else {
            a
        };
        out.push(a + (b - a) * frac);
    }
    out
}

// ---------------------------------------------------------------------------
// Pulse plumbing (threaded mainloop + introspection)
// ---------------------------------------------------------------------------

/// Enumerate sources and apply `select_source` with preference `pref`.
fn query_source(pref: &str) -> Result<Option<String>> {
    let pref = pref.to_string();
    with_context(|ml, ctx| {
        let names: Rc<RefCell<Vec<(String, bool)>>> = Rc::new(RefCell::new(Vec::new()));
        let names_cb = Rc::clone(&names);
        let ml_cb = Rc::clone(ml);

        let op = ctx
            .borrow()
            .introspect()
            .get_source_info_list(move |res| match res {
                ListResult::Item(info) => {
                    if let Some(name) = &info.name {
                        let is_monitor = info.monitor_of_sink.is_some();
                        names_cb.borrow_mut().push((name.to_string(), is_monitor));
                    }
                }
                ListResult::End | ListResult::Error => {
                    unsafe { (*ml_cb.as_ptr()).signal(false) };
                }
            });

        while op.get_state() == OpState::Running {
            ml.borrow_mut().wait();
        }

        let list = names.borrow();
        Ok(select_source(list.iter().map(|(n, m)| (n.as_str(), *m)), &pref))
    })
}

/// Query a source's native (rate, channels).
fn native_spec(source: &str) -> Result<(u32, u8)> {
    with_context(|ml, ctx| {
        let slot: Rc<RefCell<Option<(u32, u8)>>> = Rc::new(RefCell::new(None));
        let slot_cb = Rc::clone(&slot);
        let ml_cb = Rc::clone(ml);

        let op = ctx
            .borrow()
            .introspect()
            .get_source_info_by_name(source, move |res| match res {
                ListResult::Item(info) => {
                    *slot_cb.borrow_mut() =
                        Some((info.sample_spec.rate, info.sample_spec.channels));
                }
                ListResult::End | ListResult::Error => {
                    unsafe { (*ml_cb.as_ptr()).signal(false) };
                }
            });

        while op.get_state() == OpState::Running {
            ml.borrow_mut().wait();
        }

        let out = *slot.borrow();
        out.ok_or_else(|| anyhow!("source {source} not found"))
    })
}

/// Set up a threaded mainloop + connected context, run `f` (which drives introspection
/// operations under the mainloop lock), then tear the connection down cleanly.
fn with_context<T, F>(f: F) -> Result<T>
where
    F: FnOnce(&Rc<RefCell<Mainloop>>, &Rc<RefCell<Context>>) -> Result<T>,
{
    let mainloop = Rc::new(RefCell::new(
        Mainloop::new().ok_or_else(|| anyhow!("failed to create pulse mainloop"))?,
    ));
    let context = Rc::new(RefCell::new(
        Context::new(&*mainloop.borrow(), "whisp-arch")
            .ok_or_else(|| anyhow!("failed to create pulse context"))?,
    ));

    {
        let ml_ref = Rc::clone(&mainloop);
        let ctx_ref = Rc::clone(&context);
        context
            .borrow_mut()
            .set_state_callback(Some(Box::new(move || {
                let state = unsafe { (*ctx_ref.as_ptr()).get_state() };
                use libpulse_binding::context::State;
                if matches!(state, State::Ready | State::Failed | State::Terminated) {
                    unsafe { (*ml_ref.as_ptr()).signal(false) };
                }
            })));
    }

    context
        .borrow_mut()
        .connect(None, ContextFlagSet::NOFLAGS, None)
        .map_err(|e| anyhow!("pulse context connect: {e}"))?;

    mainloop
        .borrow_mut()
        .start()
        .map_err(|e| anyhow!("pulse mainloop start: {e}"))?;

    mainloop.borrow_mut().lock();

    // Wait for the context to settle.
    let ready = loop {
        use libpulse_binding::context::State;
        match context.borrow().get_state() {
            State::Ready => break true,
            State::Failed | State::Terminated => break false,
            _ => mainloop.borrow_mut().wait(),
        }
    };
    context.borrow_mut().set_state_callback(None);

    if !ready {
        mainloop.borrow_mut().unlock();
        mainloop.borrow_mut().stop();
        bail!("pulse context failed to become ready");
    }

    // Context ready; run the caller's locked work.
    let result = f(&mainloop, &context);

    context.borrow_mut().disconnect();
    mainloop.borrow_mut().unlock();
    mainloop.borrow_mut().stop();

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_underscore_form() {
        assert!(match_headset(
            "bluez_input.58_18_62_87_81_36.headset-head-unit",
            false
        ));
    }

    #[test]
    fn matches_colon_form() {
        assert!(match_headset(
            "bluez_input.58:18:62:87:81:36.a2dp-source",
            false
        ));
    }

    #[test]
    fn matches_case_insensitive() {
        assert!(match_headset(
            "BLUEZ_INPUT.58_18_62_87_81_36.HEADSET",
            false
        ));
    }

    #[test]
    fn rejects_monitor_flag() {
        assert!(!match_headset(
            "bluez_output.58_18_62_87_81_36.1.monitor",
            true
        ));
    }

    #[test]
    fn rejects_monitor_by_name() {
        assert!(!match_headset(
            "bluez_output.58_18_62_87_81_36.1.monitor",
            false
        ));
    }

    #[test]
    fn rejects_non_bluez() {
        assert!(!match_headset(
            "alsa_input.pci-0000_00_1f.3.analog-stereo",
            false
        ));
    }

    #[test]
    fn rejects_wrong_mac() {
        assert!(!match_headset(
            "bluez_input.aa_bb_cc_11_22_33.headset",
            false
        ));
    }

    #[test]
    fn source_selection_rules() {
        let names = [
            ("alsa_output.pci-0000_c2_00.6.analog-stereo.monitor", true),
            ("alsa_input.pci-0000_c2_00.6.analog-stereo", false),
            ("bluez_input.58:18:62:87:81:36", false),
            ("bluez_output.58_18_62_87_81_36.1.monitor", false),
        ];
        let it = || names.iter().map(|(n, m)| (*n, *m));
        assert_eq!(select_source(it(), "").as_deref(), Some("alsa_input.pci-0000_c2_00.6.analog-stereo"));
        assert_eq!(select_source(it(), "internal").as_deref(), Some("alsa_input.pci-0000_c2_00.6.analog-stereo"));
        assert_eq!(select_source(it(), "bluez").as_deref(), Some("bluez_input.58:18:62:87:81:36"));
        assert_eq!(select_source(it(), "xm5").as_deref(), Some("bluez_input.58:18:62:87:81:36"));
        assert_eq!(select_source(it(), "58_18").as_deref(), None, "monitor must never be picked");
        assert_eq!(select_source(it(), "nonexistent"), None);
    }

    #[test]
    fn downmix_stereo_averages() {
        // Two frames, stereo: [L0,R0, L1,R1]
        let inter = vec![1.0, 3.0, -2.0, 2.0];
        let mono = downmix(&inter, 2);
        assert_eq!(mono, vec![2.0, 0.0]);
    }

    #[test]
    fn downmix_mono_passthrough() {
        let inter = vec![0.1, 0.2, 0.3];
        assert_eq!(downmix(&inter, 1), inter);
    }

    #[test]
    fn resample_same_rate_passthrough() {
        let sig = vec![0.0, 0.5, 1.0, 0.5];
        assert_eq!(resample_linear(&sig, 16000, 16000), sig);
    }

    #[test]
    fn resample_sine_preserves_frequency() {
        // 440 Hz sine at 48 kHz -> 16 kHz. Check duration ratio and amplitude bound,
        // and that reconstructed samples stay close to the analytic sine at 16 kHz.
        let in_rate = 48_000u32;
        let out_rate = 16_000u32;
        let freq = 440.0f64;
        let secs = 0.1f64;
        let n_in = (in_rate as f64 * secs) as usize;
        let input: Vec<f32> = (0..n_in)
            .map(|i| {
                let t = i as f64 / in_rate as f64;
                (2.0 * std::f64::consts::PI * freq * t).sin() as f32
            })
            .collect();

        let out = resample_linear(&input, in_rate, out_rate);

        // Length should scale by 1/3 (48k -> 16k).
        let expected = n_in / 3;
        assert!(
            (out.len() as i64 - expected as i64).abs() <= 1,
            "len {} vs expected {}",
            out.len(),
            expected
        );

        // Compare against analytic sine at the output rate; linear interp of a 440 Hz
        // tone sampled at 48 kHz is very close to the true value.
        let mut max_err = 0.0f32;
        for (i, &s) in out.iter().enumerate() {
            let t = i as f64 / out_rate as f64;
            let truth = (2.0 * std::f64::consts::PI * freq * t).sin() as f32;
            max_err = max_err.max((s - truth).abs());
        }
        assert!(max_err < 0.02, "max resample error {max_err}");
    }

    #[test]
    fn streaming_resampler_matches_oneshot_on_constant() {
        let mut r = Resampler::new(48_000, 16_000);
        let mut out = Vec::new();
        for _ in 0..10 {
            r.process(&[0.5f32; 480], &mut out); // 10 ms blocks
        }
        // 4800 in -> ~1600 out, all 0.5
        assert!((out.len() as i64 - 1600).abs() <= 2, "len {}", out.len());
        assert!(out.iter().all(|&v| (v - 0.5).abs() < 1e-6));
    }

    #[test]
    fn streaming_resampler_is_seamless() {
        // A 440 Hz tone fed in 20 ms blocks must match the analytic sine at 16 kHz,
        // including across block boundaries.
        let in_rate = 48_000u32;
        let freq = 440.0f64;
        let mut r = Resampler::new(in_rate, 16_000);
        let mut out = Vec::new();
        let block = 960; // 20 ms
        let mut idx = 0usize;
        for _ in 0..25 {
            let blk: Vec<f32> = (0..block)
                .map(|j| {
                    let t = (idx + j) as f64 / in_rate as f64;
                    (2.0 * std::f64::consts::PI * freq * t).sin() as f32
                })
                .collect();
            idx += block;
            r.process(&blk, &mut out);
        }
        let mut max_err = 0.0f32;
        for (i, &s) in out.iter().enumerate() {
            let t = i as f64 / 16_000.0;
            let truth = (2.0 * std::f64::consts::PI * freq * t).sin() as f32;
            max_err = max_err.max((s - truth).abs());
        }
        assert!(max_err < 0.02, "max streaming resample error {max_err}");
    }

    #[test]
    fn spectrum_bands_track_a_tone() {
        // A 760 Hz tone must light band 4 far more than band 0 or band 7.
        let rate = 16_000u32;
        let pcm: Vec<f32> = (0..1280)
            .map(|i| 0.2 * (2.0 * std::f32::consts::PI * 760.0 * i as f32 / rate as f32).sin())
            .collect();
        let b = spectrum_bands(&pcm, rate);
        assert!(b[4] > 0.9, "band4 {}", b[4]);
        assert!(b[4] > 4.0 * b[0].max(0.01), "band0 {} vs band4 {}", b[0], b[4]);
        assert!(b[4] > 4.0 * b[7].max(0.01), "band7 {} vs band4 {}", b[7], b[4]);
        assert!(spectrum_bands(&[0.0; 1280], rate).iter().all(|&v| v == 0.0));
    }

    #[test]
    fn rms_of_silence_and_square() {
        assert_eq!(rms_level(&[]), 0.0);
        assert_eq!(rms_level(&[0.0; 16]), 0.0);
        let sq = [0.5f32, -0.5, 0.5, -0.5];
        assert!((rms_level(&sq) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn resample_empty_is_empty() {
        assert!(resample_linear(&[], 48000, 16000).is_empty());
    }
}

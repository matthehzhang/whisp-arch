//! Headset capture + presence watcher. OWNER: audio agent.
//!
//! PipeWire-pulse capture from the Sony WH-1000XM5 bluetooth mic. Source detection
//! matches a non-monitor `bluez` source carrying the headset MAC; capture happens at
//! the source's native rate/channels via libpulse-simple, then we downmix to mono and
//! linear-resample to 16 kHz on `stop()`.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{anyhow, bail, Context as _, Result};

use libpulse_binding::callbacks::ListResult;
use libpulse_binding::context::{Context, FlagSet as ContextFlagSet};
use libpulse_binding::mainloop::threaded::Mainloop;
use libpulse_binding::operation::State as OpState;
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

/// Pulse source name for the XM5 mic, if a matching source is present right now.
///
/// Returns `None` both when the headset is absent (the expected live result when the
/// XM5 is not connected) and when the pulse server cannot be reached.
pub fn headset_source() -> Option<String> {
    match query_headset_source() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[whisp] audio: source enumeration failed: {e:#}");
            None
        }
    }
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
    pub fn start(source: &str) -> Result<Capture> {
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
            let simple = match Simple::new(
                None,
                "whisp-arch",
                Direction::Record,
                Some(&source_owned),
                "dictation",
                &spec,
                None,
                None,
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

            while !thread_flag.load(Ordering::Relaxed) {
                if let Err(e) = simple.read(&mut byte_buf) {
                    eprintln!("[whisp] audio: read error: {e}");
                    break;
                }
                for b in byte_buf.chunks_exact(4) {
                    let s = f32::from_ne_bytes([b[0], b[1], b[2], b[3]]);
                    interleaved.push(s);
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

/// Enumerate sources and return the first non-monitor bluez source matching the MAC.
fn query_headset_source() -> Result<Option<String>> {
    with_context(|ml, ctx| {
        let found: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
        let found_cb = Rc::clone(&found);
        let ml_cb = Rc::clone(ml);

        let op = ctx
            .borrow()
            .introspect()
            .get_source_info_list(move |res| match res {
                ListResult::Item(info) => {
                    if found_cb.borrow().is_none() {
                        if let Some(name) = &info.name {
                            let is_monitor = info.monitor_of_sink.is_some();
                            if match_headset(name, is_monitor) {
                                *found_cb.borrow_mut() = Some(name.to_string());
                            }
                        }
                    }
                }
                ListResult::End | ListResult::Error => {
                    unsafe { (*ml_cb.as_ptr()).signal(false) };
                }
            });

        while op.get_state() == OpState::Running {
            ml.borrow_mut().wait();
        }

        let out = found.borrow().clone();
        Ok(out)
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
    fn resample_empty_is_empty() {
        assert!(resample_linear(&[], 48000, 16000).is_empty());
    }
}

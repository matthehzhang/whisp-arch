//! whisper.cpp wrapper + silence/hallucination gating + model selection.
//! OWNER: asr agent.
//!
//! Pipeline gates (SPEC §Pipeline-correctness) live here so the rest of the
//! pipeline has a single gate to call:
//!   - duration gate: discard capture < 300 ms
//!   - peak gate: discard if peak amplitude < 0.01
//!   - no_speech gate: drop ASR segments with `no_speech_prob > 0.6`
//!   - hallucination blocklist: drop whole-transcript results that are a known
//!     whisper.cpp silence hallucination when the transcript is <= 3 words.

use std::path::{Path, PathBuf};
use std::sync::Once;

use anyhow::{Context, Result};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

/// Target sample rate the whole pipeline works in (16 kHz mono f32).
const SAMPLE_RATE: usize = 16_000;
/// Minimum capture duration; shorter clips are discarded before ASR.
const MIN_DURATION_MS: usize = 300;
/// Minimum peak amplitude; quieter clips are treated as silence.
const PEAK_MIN: f32 = 0.01;
/// Per-segment `no_speech_prob` above which a segment is dropped.
const NO_SPEECH_DROP: f32 = 0.6;
/// whisper.cpp wants at least ~1 s of audio; pad shorter (but non-gated) clips.
const MIN_SAMPLES_FOR_WHISPER: usize = SAMPLE_RATE; // 1 s

/// Whole-transcript hallucination blocklist (already normalized: lowercase, no
/// punctuation, single-spaced). Applied only when the transcript is <= 3 words.
const HALLUCINATION_BLOCKLIST: &[&str] = &["thank you", "thanks for watching", "you", "bye", ""];

/// A loaded whisper model, ready to transcribe. Cheap to share across threads
/// (the underlying context is reference-counted and `Send + Sync`).
pub struct Engine {
    ctx: WhisperContext,
    n_threads: i32,
}

/// Install whisper.cpp / GGML log hooks exactly once. Without a `log`/`tracing`
/// backend feature enabled this silences whisper.cpp's stderr spam entirely.
fn silence_whisper_logs() {
    static ONCE: Once = Once::new();
    ONCE.call_once(whisper_rs::install_logging_hooks);
}

/// Resolve the model directory: `$XDG_DATA_HOME/whisp-arch/models` or
/// `~/.local/share/whisp-arch/models`.
fn model_dir() -> Result<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        if !xdg.is_empty() {
            return Ok(PathBuf::from(xdg).join("whisp-arch/models"));
        }
    }
    let home = std::env::var("HOME").context("HOME not set; cannot locate model dir")?;
    Ok(PathBuf::from(home).join(".local/share/whisp-arch/models"))
}

impl Engine {
    /// Load the ASR engine: prefer `ggml-small.en.bin`, fall back to
    /// `ggml-base.en.bin`, error clearly if neither exists. Safe to call from a
    /// background thread; whisper model load is the slow part.
    pub fn load() -> Result<Engine> {
        let dir = model_dir()?;
        let small = dir.join("ggml-small.en.bin");
        let base = dir.join("ggml-base.en.bin");
        let path = if small.is_file() {
            small
        } else if base.is_file() {
            base
        } else {
            anyhow::bail!(
                "no whisper model found in {}: expected ggml-small.en.bin or ggml-base.en.bin",
                dir.display()
            );
        };
        Self::load_from(&path)
    }

    /// Load a specific model file. Kept private; `load()` is the public entry.
    fn load_from(path: &Path) -> Result<Engine> {
        silence_whisper_logs();
        eprintln!("[whisp] asr: loading model {}", path.display());
        let ctx = WhisperContext::new_with_params(path, WhisperContextParameters::default())
            .with_context(|| format!("failed to load whisper model {}", path.display()))?;
        let n_threads = std::thread::available_parallelism()
            .map(|n| n.get() as i32)
            .unwrap_or(4)
            .clamp(1, 8);
        eprintln!("[whisp] asr: model loaded ({} threads)", n_threads);
        Ok(Engine { ctx, n_threads })
    }

    /// Transcribe a 16 kHz mono f32 utterance. Returns `Ok(None)` when the clip
    /// is gated (too short, too quiet, no speech, or a known hallucination),
    /// `Ok(Some(text))` otherwise.
    pub fn transcribe(&self, pcm: &[f32]) -> Result<Option<String>> {
        // --- pcm-level gates (duration + peak) ---
        if gated_by_duration(pcm) {
            eprintln!(
                "[whisp] asr: gated (duration {} ms < {} ms)",
                pcm.len() * 1000 / SAMPLE_RATE.max(1),
                MIN_DURATION_MS
            );
            return Ok(None);
        }
        let peak = peak_amplitude(pcm);
        if peak < PEAK_MIN {
            eprintln!("[whisp] asr: gated (peak {:.4} < {:.4})", peak, PEAK_MIN);
            return Ok(None);
        }

        // whisper.cpp mel needs a minimum length; zero-pad short clips.
        let padded: Vec<f32>;
        let audio: &[f32] = if pcm.len() < MIN_SAMPLES_FOR_WHISPER {
            let mut v = Vec::with_capacity(MIN_SAMPLES_FOR_WHISPER);
            v.extend_from_slice(pcm);
            v.resize(MIN_SAMPLES_FOR_WHISPER, 0.0);
            padded = v;
            &padded
        } else {
            pcm
        };

        // --- run whisper ---
        let mut state = self
            .ctx
            .create_state()
            .context("failed to create whisper state")?;
        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_n_threads(self.n_threads);
        params.set_language(Some("en"));
        params.set_translate(false);
        params.set_single_segment(false);
        params.set_suppress_blank(true);
        params.set_suppress_nst(true); // suppress non-speech tokens
                                       // Quiet: no realtime/progress/timestamp printing to stdout.
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);

        state
            .full(params, audio)
            .context("whisper full() inference failed")?;

        // --- collect segments, dropping high no_speech_prob ones ---
        let n = state.full_n_segments();
        let mut pieces: Vec<String> = Vec::new();
        for i in 0..n {
            let seg = match state.get_segment(i) {
                Some(s) => s,
                None => continue,
            };
            let nsp = seg.no_speech_probability();
            if nsp > NO_SPEECH_DROP {
                eprintln!(
                    "[whisp] asr: dropped segment {} (no_speech_prob {:.3} > {:.2})",
                    i, nsp, NO_SPEECH_DROP
                );
                continue;
            }
            let text = seg
                .to_str_lossy()
                .context("failed to read whisper segment text")?;
            pieces.push(text.into_owned());
        }

        let text = clean_text(&pieces.join(" "));
        if text.is_empty() {
            eprintln!("[whisp] asr: gated (empty after no_speech filtering)");
            return Ok(None);
        }
        if is_hallucination(&text) {
            eprintln!("[whisp] asr: gated (hallucination blocklist: {:?})", text);
            return Ok(None);
        }
        Ok(Some(text))
    }
}

// -------------------- pure gating logic (unit-tested) --------------------

/// True when the clip is shorter than the minimum capture duration.
fn gated_by_duration(pcm: &[f32]) -> bool {
    let ms = pcm.len() * 1000 / SAMPLE_RATE.max(1);
    ms < MIN_DURATION_MS
}

/// Peak absolute amplitude of the clip.
fn peak_amplitude(pcm: &[f32]) -> f32 {
    pcm.iter().fold(0.0_f32, |m, &s| m.max(s.abs()))
}

/// Trim outer whitespace and collapse all internal whitespace (incl. newlines)
/// to single spaces.
fn clean_text(raw: &str) -> String {
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Normalize a transcript for blocklist comparison: lowercase, strip ASCII
/// punctuation, collapse whitespace. Case- and punctuation-insensitive.
fn normalize_transcript(s: &str) -> String {
    let lowered: String = s
        .chars()
        .filter_map(|c| {
            if c.is_ascii_punctuation() {
                None
            } else if c.is_whitespace() {
                Some(' ')
            } else {
                Some(c.to_ascii_lowercase())
            }
        })
        .collect();
    lowered.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// True when the transcript is <= 3 words AND normalizes to a known
/// whisper.cpp silence hallucination.
fn is_hallucination(text: &str) -> bool {
    let normalized = normalize_transcript(text);
    let words = normalized.split_whitespace().count();
    if words > 3 {
        return false;
    }
    HALLUCINATION_BLOCKLIST.contains(&normalized.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizer_strips_case_and_punct() {
        assert_eq!(normalize_transcript("Thank you."), "thank you");
        assert_eq!(
            normalize_transcript("  THANKS,  for   watching!! "),
            "thanks for watching"
        );
        assert_eq!(normalize_transcript("You?"), "you");
        assert_eq!(normalize_transcript("...Bye..."), "bye");
        assert_eq!(normalize_transcript(""), "");
        assert_eq!(normalize_transcript("   "), "");
    }

    #[test]
    fn blocklist_drops_short_hallucinations() {
        assert!(is_hallucination("Thank you."));
        assert!(is_hallucination("thanks for watching"));
        assert!(is_hallucination("You"));
        assert!(is_hallucination("Bye!"));
        assert!(is_hallucination(""));
    }

    #[test]
    fn blocklist_keeps_real_speech() {
        // Real content that happens to contain a blocklist word.
        assert!(!is_hallucination("thank you very much for the help"));
        assert!(!is_hallucination("you should refactor this module"));
        // Not on the list, short.
        assert!(!is_hallucination("open the door"));
        assert!(!is_hallucination("hello there"));
    }

    #[test]
    fn blocklist_only_applies_when_short() {
        // 3 words, not on list -> kept.
        assert!(!is_hallucination("thanks for everything"));
        // "thanks for watching" is 3 words and on list -> dropped.
        assert!(is_hallucination("Thanks for watching"));
    }

    #[test]
    fn duration_gate() {
        // 299 ms worth of samples at 16 kHz is gated; 300 ms is not.
        let short = vec![0.0_f32; SAMPLE_RATE * 299 / 1000];
        let ok = vec![0.0_f32; SAMPLE_RATE * 300 / 1000];
        assert!(gated_by_duration(&short));
        assert!(!gated_by_duration(&ok));
    }

    #[test]
    fn peak_gate_math() {
        let quiet = vec![0.005_f32, -0.004, 0.009];
        let loud = vec![0.005_f32, -0.5, 0.009];
        assert!(peak_amplitude(&quiet) < PEAK_MIN);
        assert!(peak_amplitude(&loud) >= PEAK_MIN);
    }

    #[test]
    fn clean_collapses_newlines() {
        assert_eq!(clean_text("  hello\n  world \n"), "hello world");
    }

    /// Integration: load the real base.en model and confirm silence gates to
    /// None. Run manually: `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn base_en_silence_gates_to_none() {
        let dir = model_dir().expect("model dir");
        let base = dir.join("ggml-base.en.bin");
        let engine = Engine::load_from(&base).expect("load base.en");

        // 2 s of true silence -> peak gate -> None (whisper not even invoked).
        let silence = vec![0.0_f32; SAMPLE_RATE * 2];
        assert_eq!(engine.transcribe(&silence).expect("transcribe"), None);

        // 2 s of faint noise that clears the peak gate so whisper actually runs
        // on base.en; a silence-like clip should still gate to None via the
        // no_speech / hallucination path.
        let mut seed = 0x1234_5678_u32;
        let noise: Vec<f32> = (0..SAMPLE_RATE * 2)
            .map(|_| {
                // xorshift, scaled to about +-0.02 (above the 0.01 peak gate).
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                ((seed as f32 / u32::MAX as f32) - 0.5) * 0.04
            })
            .collect();
        let out = engine.transcribe(&noise).expect("transcribe noise");
        assert_eq!(out, None, "faint noise should gate to None, got {:?}", out);
    }
}

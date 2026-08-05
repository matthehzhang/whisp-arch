# whisp-arch — push-to-talk dictation daemon for Arch/Hyprland

Hold **Right Ctrl**, speak into the **Sony WH-1000XM5 headset mic**, release — the
transcript is typed into the focused app. Only active while the headset mic is
connected. A waybar module shows state and click-toggles the daemon.

Single Rust daemon. No GUI toolkit. MVP = hold-to-talk only (no double-tap lock,
no LLM cleanup — deterministic post-processing only).

## Module ownership (one agent per file; do NOT edit files you don't own)

| File | Owns |
|---|---|
| `src/hotkey.rs` | evdev PTT listener + modifier-state queries |
| `src/audio.rs` | PipeWire/pulse capture from headset source, headset presence detection, resample to 16k mono |
| `src/asr.rs` | whisper-rs wrapper, silence/hallucination gating, model selection |
| `src/inject.rs` | text injection via wtype, safe-to-inject gating |
| `src/status.rs` | status file + control socket; `waybar/` module files |
| `src/state.rs`, `src/main.rs`, `Cargo.toml` | integration agent ONLY |

Every module exposes the exact pub API in this spec. If you need something extra,
add a NEW pub item and document it with `///` — never change a specified signature.
`cargo check` must pass with your file + the scaffold stubs of everything else...
(stubs compile; your module must too, standalone: `cargo check` after your edit).

## Shared types (in `src/state.rs`, already scaffolded — read it, don't edit it)

```rust
pub struct SessionId(pub u64);
pub enum DaemonState { HeadsetMissing, Disabled, Ready, Recording, Transcribing }
pub struct Shared { /* Arc<Mutex<Inner>> with state, generation counter, enabled flag */ }
```

Pipeline correctness rules (from design review — MUST hold):
- **Generation counter**: every session gets a generation number; Esc or toggle-off
  bumps the counter; the pipeline re-checks the counter immediately before injection
  and aborts if stale. The check-and-inject decision happens on the state-owner
  thread (serialized), not the pipeline thread.
- **Injection gating**: never inject while the PTT key or any modifier
  (Ctrl/Alt/Shift/Super) is physically held — poll `hotkey::modifiers_clear()`
  (EVIOCGKEY key-state query, not event tracking) every 50 ms, up to 5 s; on
  timeout, drop the text and log.
- **Silence gating**: discard capture < 300 ms; discard if peak < 0.01; after ASR,
  drop segments with `no_speech_prob > 0.6`; drop whole-transcript results in the
  hallucination blocklist (case/punct-insensitive): "thank you", "thanks for
  watching", "you", "bye", "" — when total transcript ≤ 3 words.

## Module APIs

### `src/hotkey.rs`
```rust
pub enum HotkeyEvent { PttDown, PttUp, Cancel } // Cancel = Esc while recording
pub fn spawn_listener(tx: std::sync::mpsc::Sender<HotkeyEvent>) -> anyhow::Result<()>;
/// True when PTT + all modifiers are physically up (EVIOCGKEY state query).
pub fn modifiers_clear() -> bool;
```
- Enumerate `/dev/input/event*`, keep devices that report `KEY_RIGHTCTRL`.
  Listen-only (never grab). PTT = `KEY_RIGHTCTRL`. Esc emits `Cancel` always
  (state machine decides relevance). Read all candidate devices (epoll or one
  thread per device). Handle device errors by dropping that device; if all die,
  re-enumerate every 2 s (covers hotplug well enough for MVP).

### `src/audio.rs`
```rust
pub struct Capture; // handle
pub fn headset_source() -> Option<String>; // pulse source name for the XM5 mic, if present
impl Capture {
    pub fn start(source: &str) -> anyhow::Result<Capture>;
    pub fn stop(self) -> anyhow::Result<Vec<f32>>; // 16 kHz mono f32, whole utterance
}
```
- Headset detection: enumerate pulse sources (libpulse, pipewire-pulse server);
  match a **non-monitor** source whose name contains `bluez` and the MAC
  `58:18:62:87:81:36` in either `:` or `_` form (case-insensitive). Poll every 2 s
  in a `pub fn spawn_watcher(cb: impl Fn(Option<String>) + Send + 'static)`.
- Capture with libpulse-simple-binding from that exact source name. Record at the
  source's native rate, downmix to mono, linear-resample to 16 kHz on `stop()`.
  BT HFP sources are 8/16 kHz mono already — handle any rate. Cap buffer at 90 s.

### `src/asr.rs`
```rust
pub struct Engine;
impl Engine {
    pub fn load() -> anyhow::Result<Engine>; // spawn-once, background-thread friendly
    /// Returns None when gated (silence/hallucination). pcm = 16 kHz mono f32.
    pub fn transcribe(&self, pcm: &[f32]) -> anyhow::Result<Option<String>>;
}
```
- Model dir `~/.local/share/whisp-arch/models/`; prefer `ggml-small.en.bin`, fall
  back to `ggml-base.en.bin`, error with a clear message if neither exists.
- whisper-rs params: language "en", `no_speech_thold` default, single segment off,
  suppress non-speech tokens. Apply the silence/hallucination gates from above
  (peak + duration gates live here too, on the pcm, so the pipeline has one gate).
- Pad-to-1s minimum if whisper.cpp requires it. Trim leading/trailing whitespace;
  collapse internal newlines to spaces.

### `src/inject.rs`
```rust
/// Blocks until safe (modifiers clear) then types text. Ok(false) = dropped (timeout).
pub fn inject(text: &str, wait_clear: impl Fn() -> bool) -> anyhow::Result<bool>;
```
- Implementation: `wtype -` with text on stdin (avoids argv length/quoting issues),
  `-d 1` inter-key delay 1 ms. A single trailing space after the text (dictation
  convention). No clipboard use.

### `src/status.rs`
```rust
pub fn write_status(state: &DaemonState);           // atomic write of status JSON
pub fn spawn_control(shared: Shared) -> anyhow::Result<()>; // unix socket: "toggle"/"status"
```
- Status file: `$XDG_RUNTIME_DIR/whisp-arch/status.json` —
  `{"state":"ready","text":"","class":"ready","tooltip":"whisp: ready (XM5 connected)"}`
  where text is an icon: `` HeadsetMissing → "󰋐" dim, Disabled → "󰍭", Ready → "󰍬",
  Recording → "󰍬" red, Transcribing → "󰦖" ``. Written on every state change.
- Control socket `$XDG_RUNTIME_DIR/whisp-arch.sock`: line-oriented; `toggle` flips
  enabled (only meaningful when headset present), `status` returns the JSON line.
- Also own `waybar/whisper.sh` (emits the JSON for waybar, `cat` of status file
  with a fallback "daemon not running" JSON) and `waybar/README-snippet.md`
  showing the `custom/whisper` config block (interval 1, return-type json,
  on-click `whisp-arch --toggle`) and CSS classes
  (`#custom-whisper.ready/.recording/.transcribing/.disabled/.missing`).

### `src/main.rs` + `src/state.rs` (integration agent)
- State-owner thread holds the state machine, receives HotkeyEvents + watcher
  updates + control-socket commands via one mpsc channel.
- PttDown (Ready) → start capture, state Recording. PttUp (Recording) → stop,
  spawn pipeline thread {transcribe → gate → back to owner thread for
  check-generation-and-inject}, state Transcribing → Ready on completion.
  Cancel → bump generation, discard, Ready. Headset lost mid-recording → cancel.
- `--toggle` CLI flag: connect to socket, send toggle, exit (for waybar on-click).
- CLI `--status` likewise. Daemon is default mode. systemd user unit in
  `packaging/whisp-arch.service` (WantedBy graphical-session.target).

## Non-negotiables
- No `unwrap()` outside tests; `anyhow` everywhere; eprintln logging with a
  `[whisp]` prefix is fine for MVP.
- rustfmt clean; `cargo clippy` warning-free is the bar for integration.
- Unit tests where logic is pure (gating blocklist, resample, MAC matching).

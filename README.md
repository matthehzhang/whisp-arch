# Whisp for Linux

This is the **Linux / Hyprland** branch of [whisp-arch](https://github.com/matthehzhang/whisp-arch).
The [main](https://github.com/matthehzhang/whisp-arch) and
[mac](https://github.com/matthehzhang/whisp-arch/tree/mac) branches contain the Mac app;
[windows](https://github.com/matthehzhang/whisp-arch/tree/windows) contains the Windows app.

Hold the configured push-to-talk key, speak, and release to transcribe. The daemon
uses the built-in microphone by default, supports a streaming gateway with local
Whisper fallback, and plays start/stop sounds. Current settings and the Phase E
section below take precedence over the original headset-specific design in SPEC.md.

## How it works

- **Key = Right Alt** (listen-only evdev; your normal Ctrl usage is unaffected —
  the daemon never grabs the device). Press → record, release → transcribe +
  inject. **Esc** cancels an in-flight session.
- **Headset-gated**: a 2 s watcher matches the XM5's non-monitor `bluez` pulse
  source by MAC. No headset → the module shows `missing` and PTT does nothing.
- **Injection safety**: text is never typed while any modifier (Ctrl/Alt/Shift/
  Super) or the PTT key is still physically held — the daemon polls the real
  key state (EVIOCGKEY) for up to 5 s, then drops the text.
- **Silence/hallucination gating**: clips < 300 ms or peak < 0.01 are dropped;
  segments with `no_speech_prob > 0.6` are dropped; short whisper.cpp silence
  hallucinations ("thank you", "thanks for watching", "you", "bye") are dropped.

## Requirements

- Wayland compositor with virtual-keyboard support (Hyprland) and **`wtype`**.
- **PipeWire** with `pipewire-pulse`.
- Membership in the **`input`** group (to read `/dev/input/event*`).
- A whisper ggml model in `~/.local/share/whisp-arch/models/` — prefers
  `ggml-small.en.bin`, falls back to `ggml-base.en.bin`:

  ```sh
  mkdir -p ~/.local/share/whisp-arch/models
  cd ~/.local/share/whisp-arch/models
  # e.g. from the whisper.cpp model repo:
  curl -L -o ggml-small.en.bin \
    https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.en.bin
  ```


## Remote dictation (Phase E)

By default the daemon streams audio to the whisp gateway on the compute box while Right Alt is held
(`WHISP_GATEWAY`, default `ws://100.122.171.110:8082`) and types the LLM-cleaned text on release.
Live partials and audio levels go to the overlay over `$XDG_RUNTIME_DIR/whisp-arch.events`.
If the gateway is unreachable or fails, the buffered utterance goes through the local whisper model.

- Mic: the laptop's built-in mic by default. `WHISP_SOURCE=bluez` selects the Sony XM5 headset mic (the original
  spec's choice; its Bluetooth HFP transport failed on every mic open on 2026-10-10, so it is opt-in now), or any
  substring of a PipeWire source name.
- Live typing: with `WHISP_PTT_KEY=f13` and keyd remapping Right Alt to F13 (`packaging/keyd-default.conf`),
  raw words are typed into the focused window as they are recognised and replaced by the cleaned text on release.
  With Right Alt as a real modifier this is off (injected letters would become Ctrl shortcuts); `WHISP_LIVE_TYPE=0`
  disables it regardless. Other key names: rightctrl, f13, f14, f15, capslock, pause, scrolllock, menu.
- Mic gain: on the Framework 16 the built-in mic clips hard above about 15% PipeWire volume (PipeWire maps 60%+ to
  +30 dB at the codec). Keep the source around 12-20%: `pactl set-source-volume alsa_input.pci-0000_c2_00.6.analog-stereo 15%`.
  Also check the hardware mic privacy switch on the display bezel: off means exact digital silence.
- `WHISP_LOCAL_ONLY=1` forces the local path. `~/.config/whisp/dictionary.txt` is sent with each utterance
  (one term per line) and applied by the cleanup prompt. The active Hyprland window class picks the tone.
- Bar indicator: `waybar/whisper.py` renders the mic spectrum as pink block glyphs in waybar while the key is held,
  a processing wave after release, then the cleaned text with a check mark. See `waybar/README-snippet.md`.
- Optional floating pill instead: `overlay/whisp-overlay.py` (GTK4 + gtk4-layer-shell) with `packaging/whisp-overlay.service`
  and `overlay/hyprland-layerrule.conf`. Not enabled by default.
  Test either without speaking: `overlay/fake-events.py` plays a scripted dictation into the events socket.
- Server side (ASR, LLM router, gateway) lives in `desktop/` and runs on `desktop-e4jpcc6`; see `desktop/README.md`.

## Build & install

Recording plays a brief rising cue on start and a falling cue after capture stops
(including the release tail), or when cancelled. Playback runs in the background.
Set `WHISP_SOUNDS=0` to mute cues, or `WHISP_SOUND_SINK` to a PulseAudio/PipeWire
sink name (from `pactl list short sinks`) to use speakers even with headphones
connected. Otherwise cues use the default output. Preview them without recording
with `target/release/whisp-arch --test-sounds`.

```sh
cargo build --release          # -> target/release/whisp-arch
```

Run it directly to test, or install the systemd **user** unit:

```sh
install -Dm644 packaging/whisp-arch.service \
  ~/.config/systemd/user/whisp-arch.service
# ensure wtype can reach the compositor:
systemctl --user import-environment WAYLAND_DISPLAY XDG_RUNTIME_DIR
systemctl --user enable --now whisp-arch.service
# then add the waybar module (waybar/README-snippet.md) and reload waybar: pkill -SIGUSR2 -x waybar
```

The unit is `WantedBy=graphical-session.target`, so it starts with your Wayland
session. Logs: `journalctl --user -u whisp-arch -f`.

## Configure the headset

The matched MAC is currently hard-coded in `src/audio.rs`
(`58:18:62:87:81:36`). Point it at your own XM5 by editing `HEADSET_MAC` there
and rebuilding.

## waybar

The daemon writes `$XDG_RUNTIME_DIR/whisp-arch/status.json` (waybar JSON) on
every state change and serves a `toggle`/`status` control socket at
`$XDG_RUNTIME_DIR/whisp-arch.sock`. Wire it up with the exec script and config
block in **[`waybar/README-snippet.md`](waybar/README-snippet.md)** — copy
`waybar/whisper.sh`, add the `custom/whisper` module (`interval: 1`,
`return-type: json`, `on-click: whisp-arch --toggle`), and the CSS classes
(`.ready` `.recording` `.transcribing` `.disabled` `.missing`).

## CLI

```
whisp-arch            run the daemon (default)
whisp-arch --toggle   flip enabled via the control socket (waybar on-click)
whisp-arch --status   print the current waybar status JSON line
```

## Layout

| File | Responsibility |
|---|---|
| `src/main.rs`, `src/state.rs` | state-owner loop + shared types (integration) |
| `src/hotkey.rs` | evdev Right-Ctrl listener + modifier-state queries |
| `src/audio.rs` | headset detection + pulse capture → 16 kHz mono |
| `src/asr.rs` | whisper-rs wrapper + silence/hallucination gating |
| `src/inject.rs` | `wtype` injection + safe-to-inject gating |
| `src/status.rs` | status file + control socket |
| `waybar/` | waybar exec script + config snippet |
| `packaging/` | systemd user unit |

## Cloud recognizer (Deepgram) with the gateway on the laptop

The gateway (`desktop/compute/srv/whisp/gateway/gateway.py`) has two ASR backends, selected by
`WHISP_ASR_BACKEND`: `sherpa` (Nemotron on the compute box, the original layout) and `deepgram`
(Nova-3 streaming, ~200-300 ms to first words, the personal dictionary sent as `keyterm` biasing).
With `deepgram` the gateway runs on the laptop (`packaging/whisp-gateway.service`), so dictation no
longer needs the desktop at all; only the cleanup pass calls llama-swap on the compute box, with a
0.8 s connect timeout and raw text as the fallback. One spare Deepgram connection is kept warm with
KeepAlive so a key press costs no handshake. The daemon keeps capturing for 400 ms after the key is
released (`WHISP_TAIL_MS`): releasing while the last word is still sounding otherwise loses that word.

Setup: put `DEEPGRAM_API_KEY=...` in `~/.config/whisp/env` (mode 600), install the service, then
`packaging/drop-ins/gateway-local.conf` points the daemon at `ws://127.0.0.1:8082`. Cost is about
$0.46 per hour of speech. The compute-box gateway keeps working unchanged for the `sherpa` layout.

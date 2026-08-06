# whisp-arch

Push-to-talk dictation daemon for Arch + Hyprland (Wayland). **Hold Right Ctrl**,
speak into the **Sony WH-1000XM5** headset mic, release — the transcript is typed
into the focused window via `wtype`. Dictation is **gated on the headset mic**:
the daemon only arms while the XM5 is connected, so the laptop mic is never used.
A waybar module shows state and click-toggles the daemon.

Single Rust daemon, no GUI toolkit. Local whisper.cpp (via `whisper-rs`); nothing
leaves the machine. MVP is hold-to-talk only — no double-tap lock, no LLM
cleanup, deterministic silence/hallucination gating only.

## How it works

- **Key = Right Ctrl** (listen-only evdev; your normal Ctrl usage is unaffected —
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

## Build & install

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

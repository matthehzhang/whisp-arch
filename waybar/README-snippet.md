# waybar module: `custom/whisper`

Live dictation state in the bar. `waybar/whisper.py` is a continuous `exec` module: it subscribes to
the daemon's events socket and prints a JSON line whenever the display changes. While Right Alt is
held it renders the mic spectrum as block glyphs (`▁▂▃▄▅▆▇█`, 8 bands at ~25 fps, same style as the
media animation), then a processing wave, then the cleaned text with a check mark for a second.

Put it first in `modules-right` so it grows leftward when it expands:

```jsonc
"modules-right": ["custom/whisper", ...],

"custom/whisper": {
  "exec": "~/projects/mini/whisp-arch/waybar/whisper.py",
  "return-type": "json",
  "escape": true,
  "on-click": "~/projects/mini/whisp-arch/target/release/whisp-arch --toggle"
}
```

CSS (`style.css`), pink while listening, green flash on completion, amber when the local whisper
fallback was used:

```css
@define-color whisp-pink rgba(255, 121, 198, 1);
@keyframes whisp-pulse { from { color: @whisp-pink; } to { color: rgba(255, 179, 221, 1); } }
#custom-whisper { color: @dark-5; margin: 5px 14px 5px 4px; font-size: 15px; }
#custom-whisper.missing, #custom-whisper.disabled { color: @dark-6; }
#custom-whisper.recording { color: @whisp-pink; font-size: 11px; animation: whisp-pulse 1.1s ease-in-out infinite alternate; }
#custom-whisper.transcribing { color: @whisp-pink; font-size: 11px; opacity: 0.75; }
#custom-whisper.done { color: #42ff9f; font-size: 12px; }
#custom-whisper.fallback { color: rgba(255, 184, 77, 1); font-size: 11px; }
#custom-whisper.error { color: rgba(227, 81, 73, 1); }
```

Classes: `missing` (headset absent / daemon down), `disabled`, `ready`, `recording`, `transcribing`,
`done`, `fallback`, `error`. Click toggles the daemon on/off.

`whisper.sh` (polling the status file) is the older, minimal variant and still works if you prefer a
static icon.

Test without speaking: `overlay/fake-events.py` binds the events socket and plays a scripted dictation
(stop the daemon first, or it owns the socket).

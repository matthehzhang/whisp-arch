# waybar integration for whisp-arch

The daemon writes `$XDG_RUNTIME_DIR/whisp-arch/status.json` on every state
change. The module below `cat`s that file each second and click-toggles the
daemon over its control socket via `whisp-arch --toggle`.

## 1. Install the exec script

Copy `whisper.sh` somewhere on disk and make it executable:

```sh
install -Dm755 waybar/whisper.sh ~/.config/waybar/scripts/whisper.sh
```

## 2. Module config

Add to `~/.config/waybar/config` (JSONC). Put `custom/whisper` in one of your
`modules-left/center/right` arrays, then add the block:

```jsonc
"custom/whisper": {
    "exec": "~/.config/waybar/scripts/whisper.sh",
    "return-type": "json",
    "interval": 1,
    "on-click": "whisp-arch --toggle"
}
```

- `return-type: json` — the script emits `{text, class, tooltip}` per line.
- `interval: 1` — poll the status file once a second.
- `on-click` — sends `toggle` to the control socket, flipping enabled/disabled
  (only meaningful while the headset is connected).

## 3. CSS

Add to `~/.config/waybar/style.css`. The `class` field selects one of these:

```css
#custom-whisper {
    padding: 0 8px;
    margin: 0 2px;
}

/* Headset connected, dictation armed */
#custom-whisper.ready {
    color: #a6e3a1;
}

/* Actively capturing audio */
#custom-whisper.recording {
    color: #f38ba8;
}

/* Running whisper on the captured audio */
#custom-whisper.transcribing {
    color: #f9e2af;
}

/* Headset present but toggled off */
#custom-whisper.disabled {
    color: #6c7086;
}

/* No headset / daemon not running (dim) */
#custom-whisper.missing {
    color: #45475a;
}
```

## Notes

- The icons are Nerd Font glyphs (`󰍬`, `󰍭`, `󰋐`, `󰦖`). Use a Nerd-Font-patched
  bar font or they render as tofu.
- If the daemon is not running, `whisper.sh` emits a `missing`-class
  "daemon not running" line so the module never disappears.

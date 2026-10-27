#!/usr/bin/env python3
"""waybar custom/whisper module: live dictation state in the bar.

Continuous `exec` (no interval): subscribes to the daemon's events socket
($XDG_RUNTIME_DIR/whisp-arch.events) and prints one waybar JSON line whenever the
display should change. While the key is held it renders the mic spectrum as block
glyphs (same style as the media animation), ~25 fps, interpolated between the
daemon's band updates.

Classes (for style.css): missing | disabled | ready | recording | transcribing | done | fallback | error
"""
import json, math, os, select, socket, sys, time

SOCK = os.path.join(os.environ.get("XDG_RUNTIME_DIR", "/tmp"), "whisp-arch.events")
STATUS = os.path.join(os.environ.get("XDG_RUNTIME_DIR", "/tmp"), "whisp-arch", "status.json")
GLYPHS = "▁▂▃▄▅▆▇█"
N_BARS = 8           # one per spectrum band
FPS = 25
ICON = {"ready": "\U000f036c", "disabled": "\U000f036d", "headset-missing": "\U000f02d0", "down": "\U000f00d0"}
TOOLTIP = {"ready": "whisp: ready", "disabled": "whisp: disabled", "headset-missing": "whisp: headset not connected",
           "down": "whisp: daemon not running"}
PROCESSING_FRAMES = ["▂▄▆▄▂▁▂▄", "▄▆▄▂▁▂▄▆", "▆▄▂▁▂▄▆▄", "▄▂▁▂▄▆▄▂", "▂▁▂▄▆▄▂▁", "▁▂▄▆▄▂▁▂"]


def emit(text, cls, tooltip=""):
    sys.stdout.write(json.dumps({"text": text, "class": cls, "alt": cls, "tooltip": tooltip}, ensure_ascii=False) + "\n")
    sys.stdout.flush()


def idle_line(state):
    st = state if state in ICON else "ready"
    cls = {"ready": "ready", "disabled": "disabled", "headset-missing": "missing", "down": "missing"}[st]
    return ICON[st], cls, TOOLTIP[st]


def bars(levels):
    return "".join(GLYPHS[min(len(GLYPHS) - 1, int(round(max(0.0, min(1.0, v)) * (len(GLYPHS) - 1))))] for v in levels)


def main():
    idle_state = "down"
    last_emit = None

    def show(text, cls, tooltip=""):
        nonlocal last_emit
        key = (text, cls, tooltip)
        if key != last_emit:
            last_emit = key
            emit(text, cls, tooltip)

    # Seed from the status file if the daemon is already up.
    try:
        with open(STATUS) as f:
            idle_state = json.load(f).get("state", "down")
    except (OSError, ValueError):
        pass
    show(*idle_line(idle_state))

    while True:
        try:
            s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            s.connect(SOCK)
        except OSError:
            show(*idle_line("down"))
            time.sleep(1.0)
            continue

        buf = b""
        mode = "idle"            # idle | recording | transcribing | done | error
        target = [0.0] * N_BARS
        shown = [0.0] * N_BARS
        phase = [i * 0.9 for i in range(N_BARS)]
        mode_since = time.monotonic()
        peak = 0.35
        fallback = False
        final_text = ""
        frame = 0
        try:
            while True:
                r, _, _ = select.select([s], [], [], 1.0 / FPS)
                now = time.monotonic()
                if r:
                    data = s.recv(65536)
                    if not data:
                        break
                    buf += data
                    while b"\n" in buf:
                        line, buf = buf.split(b"\n", 1)
                        try:
                            ev = json.loads(line)
                        except ValueError:
                            continue
                        k = ev.get("ev")
                        if k == "state":
                            st = ev.get("state")
                            if st == "recording":
                                mode, mode_since, fallback, final_text = "recording", now, False, ""
                                target = [0.0] * N_BARS
                                shown = [0.0] * N_BARS
                            elif st == "transcribing":
                                mode, mode_since = "transcribing", now
                            else:
                                idle_state = st
                                if mode in ("recording", "transcribing"):
                                    mode = "idle"
                        elif k == "level" and mode == "recording":
                            b = ev.get("bands")
                            if isinstance(b, list) and len(b) == N_BARS:
                                target = [float(v) for v in b]
                            else:
                                rms = float(ev.get("rms", 0.0))
                                target = [min(1.0, (rms / 0.18) ** 0.6)] * N_BARS
                        elif k == "final":
                            final_text = ev.get("text", "")
                            mode, mode_since = ("done" if final_text.strip() else "idle"), now
                        elif k == "fallback":
                            fallback = True
                        elif k in ("cancel", "error"):
                            mode, mode_since = "error", now

                # ---- render ----
                if mode == "recording":
                    # auto-gain: normalise against the loudest band seen recently (decays),
                    # floored so background noise stays low and speech reaches the top.
                    peak = max(peak * 0.985, max(target), 0.35)
                    for i in range(N_BARS):
                        wob = 0.85 + 0.15 * math.sin(now * (6.0 + i * 0.7) + phase[i])
                        goal = min(1.0, target[i] / peak) * wob
                        shown[i] += (goal - shown[i]) * (0.55 if goal > shown[i] else 0.22)
                    target = [v * 0.9 for v in target]  # decay between daemon updates
                    show(bars(shown) + " \U000f036c", "fallback" if fallback else "recording",
                         "whisp: listening (local fallback)" if fallback else "whisp: listening")
                elif mode == "transcribing":
                    frame = int((now - mode_since) * 10) % len(PROCESSING_FRAMES)
                    show(PROCESSING_FRAMES[frame] + " \U000f0996", "fallback" if fallback else "transcribing",
                         "whisp: cleaning up…")
                elif mode == "done":
                    if now - mode_since > 1.2:
                        mode = "idle"
                        show(*idle_line(idle_state))
                    else:
                        snippet = final_text if len(final_text) <= 48 else "…" + final_text[-47:]
                        show("\U000f012c " + snippet, "done", final_text)   # 󰄬 check
                elif mode == "error":
                    if now - mode_since > 0.8:
                        mode = "idle"
                        show(*idle_line(idle_state))
                    else:
                        show("\U000f0156", "error", "whisp: cancelled")       # 󰅖 close
                else:
                    show(*idle_line(idle_state))
        except OSError:
            pass
        finally:
            try:
                s.close()
            except OSError:
                pass
        show(*idle_line("down"))
        time.sleep(0.5)


if __name__ == "__main__":
    main()

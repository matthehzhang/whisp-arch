#!/usr/bin/env python3
"""whisp overlay: a floating pill on Hyprland that shows the dictation state.

Subscribes to the daemon's events socket ($XDG_RUNTIME_DIR/whisp-arch.events,
newline JSON) and renders a layer-shell surface anchored bottom-center:

  recording    -> pill slides/fades in, green glow, live waveform driven by the
                  mic level, partial transcript fades in underneath the bars
  transcribing -> bars collapse into a shimmering progress line
  final        -> cleaned text with a check mark, then the pill fades out
  fallback     -> amber tint (local whisper instead of the compute box)
  cancel/error -> brief red flash, fade out

Palette follows the waybar theme: green accent #42ff9f, dark-8 rgba(39,39,42),
font JetBrainsMono Nerd Font Propo. Hyprland blur behind the pill is enabled by a
layerrule on namespace "whisp-overlay" (see overlay/hyprland-layerrule.conf).
"""
import json, math, os, random, socket, sys, time

# gtk4-layer-shell must be in the process before GTK initialises its Wayland backend. Importing the
# typelib is not enough under PyGObject (it pulls GTK in first), so preload the library globally.
import ctypes
ctypes.CDLL("libgtk4-layer-shell.so.0", mode=ctypes.RTLD_GLOBAL)
import gi
gi.require_version("Gtk4LayerShell", "1.0")
from gi.repository import Gtk4LayerShell as LayerShell  # noqa: E402
gi.require_version("Gtk", "4.0")
from gi.repository import GLib, Gtk, Gdk  # noqa: E402

GREEN = (0x42 / 255, 0xff / 255, 0x9f / 255)
AMBER = (1.0, 0.72, 0.30)
RED = (0.89, 0.32, 0.29)
TEXT = (0.93, 0.93, 0.95)
MUTED = (0.62, 0.62, 0.68)

N_BARS = 28
BAR_W = 3.0
BAR_GAP = 3.0
PILL_H = 44
PILL_MIN_W = 220
PILL_MAX_W = 720
MARGIN_BOTTOM = 42
FPS = 60

SOCK = os.path.join(os.environ.get("XDG_RUNTIME_DIR", "/tmp"), "whisp-arch.events")

CSS = b"""
window { background: transparent; }
.pill {
  background-color: rgba(39, 39, 42, 0.86);
  border: 1px solid rgba(66, 255, 159, 0.28);
  border-radius: 22px;
  box-shadow: 0 6px 24px rgba(0, 0, 0, 0.45), 0 0 0 1px rgba(0, 0, 0, 0.25) inset;
}
.pill.fallback { border-color: rgba(255, 184, 77, 0.45); }
.pill.error    { border-color: rgba(227, 81, 73, 0.6); }
label.main {
  font-family: "JetBrainsMono Nerd Font Propo", "JetBrains Mono", monospace;
  font-size: 13.5px;
  color: #ededf2;
}
label.hint {
  font-family: "JetBrainsMono Nerd Font Propo", "JetBrains Mono", monospace;
  font-size: 11px;
  color: #9e9ea8;
}
"""


def ease_out_cubic(t):
    t = max(0.0, min(1.0, t))
    return 1 - (1 - t) ** 3


class Overlay:
    def __init__(self):
        self.state = "idle"          # idle|recording|transcribing|final|error
        self.tint = GREEN
        self.levels = [0.0] * N_BARS   # smoothed bar heights 0..1
        self.target = 0.0              # latest RMS mapped to 0..1
        self.phase = [random.random() * math.tau for _ in range(N_BARS)]
        self.partial = ""
        self.final_text = ""
        self.final_at = None
        self.visible = 0.0             # 0..1 show/hide progress
        self.want_visible = False
        self.t_state = time.monotonic()
        self.fallback = False
        self.buf = b""
        self.sock = None

        self.win = Gtk.Window()
        self.win.set_title("whisp")
        LayerShell.init_for_window(self.win)
        LayerShell.set_namespace(self.win, "whisp-overlay")
        LayerShell.set_layer(self.win, LayerShell.Layer.OVERLAY)
        LayerShell.set_anchor(self.win, LayerShell.Edge.BOTTOM, True)
        LayerShell.set_margin(self.win, LayerShell.Edge.BOTTOM, MARGIN_BOTTOM)
        LayerShell.set_keyboard_mode(self.win, LayerShell.KeyboardMode.NONE)
        LayerShell.set_exclusive_zone(self.win, -1)

        css = Gtk.CssProvider()
        css.load_from_data(CSS)
        Gtk.StyleContext.add_provider_for_display(Gdk.Display.get_default(), css,
                                                  Gtk.STYLE_PROVIDER_PRIORITY_APPLICATION)

        self.area = Gtk.DrawingArea()
        self.area.set_content_width(PILL_MIN_W)
        self.area.set_content_height(PILL_H)
        self.area.set_draw_func(self.draw)
        self.area.add_css_class("pill")
        self.win.set_child(self.area)
        self.win.set_opacity(0.0)
        self.win.present()  # mapped but fully transparent while idle

        GLib.timeout_add(int(1000 / FPS), self.tick)
        GLib.timeout_add(500, self.ensure_socket)

    # ---------------- events ----------------
    def ensure_socket(self):
        if self.sock is None:
            try:
                s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                s.connect(SOCK)
                s.setblocking(False)
                self.sock = s
                GLib.io_add_watch(s.fileno(), GLib.IO_IN | GLib.IO_HUP, self.on_readable)
            except OSError:
                self.sock = None
        return True

    def on_readable(self, fd, cond):
        try:
            data = self.sock.recv(65536)
        except BlockingIOError:
            return True
        except OSError:
            data = b""
        if not data:
            try:
                self.sock.close()
            except OSError:
                pass
            self.sock = None
            self.set_state("idle")
            return False
        self.buf += data
        while b"\n" in self.buf:
            line, self.buf = self.buf.split(b"\n", 1)
            try:
                self.handle(json.loads(line))
            except json.JSONDecodeError:
                pass
        return True

    def handle(self, ev):
        k = ev.get("ev")
        if k == "state":
            st = ev.get("state")
            if st == "recording":
                self.partial = ""
                self.final_text = ""
                self.fallback = False
                self.tint = GREEN
                self.set_state("recording")
            elif st == "transcribing":
                self.set_state("transcribing")
            elif st in ("ready", "disabled", "headset-missing"):
                if self.state not in ("final", "error"):
                    self.set_state("idle")
        elif k == "level":
            # RMS of speech sits around 0.02..0.2; map with a soft curve to 0..1.
            rms = float(ev.get("rms", 0.0))
            self.target = max(0.0, min(1.0, (rms / 0.18) ** 0.6))
        elif k == "partial":
            self.partial = ev.get("text", "")
        elif k == "final":
            text = ev.get("text", "")
            if text.strip():
                self.final_text = text
                self.set_state("final")
            else:
                self.set_state("idle")
        elif k == "fallback":
            self.fallback = True
            self.tint = AMBER
        elif k in ("cancel", "error"):
            self.tint = RED
            self.set_state("error")

    def set_state(self, st):
        self.state = st
        self.t_state = time.monotonic()
        self.want_visible = st != "idle"
        if st in ("final", "error"):
            self.final_at = time.monotonic()
        self.area.remove_css_class("fallback")
        self.area.remove_css_class("error")
        if self.fallback and st != "error":
            self.area.add_css_class("fallback")
        if st == "error":
            self.area.add_css_class("error")

    # ---------------- animation ----------------
    def tick(self):
        now = time.monotonic()
        # auto-hide after showing the final text / error
        if self.state in ("final", "error") and self.final_at and now - self.final_at > (1.4 if self.state == "final" else 0.9):
            self.set_state("idle")
        # show/hide progress
        step = 1.0 / (FPS * 0.22)
        self.visible = min(1.0, self.visible + step) if self.want_visible else max(0.0, self.visible - step)
        # bars: ease toward target with per-bar motion so it feels alive
        if self.state == "recording":
            for i in range(N_BARS):
                centre = 1.0 - abs((i - (N_BARS - 1) / 2) / ((N_BARS - 1) / 2)) ** 1.6
                wobble = 0.5 + 0.5 * math.sin(now * (5.5 + 0.9 * (i % 5)) + self.phase[i])
                goal = self.target * (0.35 + 0.65 * centre) * (0.55 + 0.45 * wobble)
                self.levels[i] += (goal - self.levels[i]) * (0.35 if goal > self.levels[i] else 0.12)
        else:
            for i in range(N_BARS):
                self.levels[i] += (0.0 - self.levels[i]) * 0.18
        self.target *= 0.80  # decay if no new level arrives
        # size the pill to its text
        want_w = PILL_MIN_W
        text = self.final_text if self.state == "final" else self.partial
        if text:
            want_w = int(min(PILL_MAX_W, max(PILL_MIN_W, 120 + 7.6 * len(text[-80:]))))
        if abs(self.area.get_content_width() - want_w) > 2:
            self.area.set_content_width(want_w)
        self.win.set_opacity(ease_out_cubic(self.visible))
        self.area.queue_draw()
        return True

    # ---------------- drawing ----------------
    def draw(self, area, cr, w, h):
        v = ease_out_cubic(self.visible)
        # slide-up: translate content by (1-v)*10px
        cr.translate(0, (1 - v) * 10)
        r, g, b = self.tint
        cx, cy = w / 2, h / 2

        if self.state == "recording":
            total = N_BARS * BAR_W + (N_BARS - 1) * BAR_GAP
            x0 = cx - total / 2
            for i, lv in enumerate(self.levels):
                bh = max(3.0, lv * (h - 18))
                x = x0 + i * (BAR_W + BAR_GAP)
                a = 0.55 + 0.45 * lv
                cr.set_source_rgba(r, g, b, a)
                self.rounded_rect(cr, x, cy - bh / 2, BAR_W, bh, BAR_W / 2)
                cr.fill()
            if self.partial:
                self.text(cr, self.partial, w, h, MUTED, 11.0, y=h - 9, right_align=True, maxw=w - 28)

        elif self.state == "transcribing":
            # shimmering line
            lw = min(w - 48, 180)
            x0 = cx - lw / 2
            t = (time.monotonic() - self.t_state) * 1.4
            cr.set_line_width(3.0)
            cr.set_line_cap(1)  # round
            cr.set_source_rgba(r, g, b, 0.18)
            cr.move_to(x0, cy); cr.line_to(x0 + lw, cy); cr.stroke()
            sweep = (math.sin(t) * 0.5 + 0.5)
            hx = x0 + sweep * lw
            grad = self._radial(cr, hx, cy, 34, (r, g, b))
            cr.set_source(grad)
            cr.move_to(x0, cy); cr.line_to(x0 + lw, cy); cr.stroke()
            if self.partial:
                self.text(cr, self.partial, w, h, MUTED, 11.0, y=h - 9, right_align=True, maxw=w - 28)

        elif self.state == "final":
            # check mark + text
            cr.set_source_rgba(r, g, b, 0.95)
            cr.set_line_width(2.2)
            cr.set_line_cap(1); cr.set_line_join(1)
            cr.move_to(18, cy); cr.line_to(23, cy + 5); cr.line_to(31, cy - 6); cr.stroke()
            self.text(cr, self.final_text, w, h, TEXT, 13.0, y=cy + 5, x=42, maxw=w - 56)

        elif self.state == "error":
            cr.set_source_rgba(r, g, b, 0.9)
            cr.set_line_width(2.2); cr.set_line_cap(1)
            cr.move_to(cx - 6, cy - 6); cr.line_to(cx + 6, cy + 6)
            cr.move_to(cx + 6, cy - 6); cr.line_to(cx - 6, cy + 6); cr.stroke()

    @staticmethod
    def _radial(cr, x, y, rad, rgb):
        import cairo
        g = cairo.RadialGradient(x, y, 0, x, y, rad)
        r, gg, b = rgb
        g.add_color_stop_rgba(0, r, gg, b, 1.0)
        g.add_color_stop_rgba(1, r, gg, b, 0.0)
        return g

    @staticmethod
    def rounded_rect(cr, x, y, w, h, rad):
        rad = min(rad, w / 2, h / 2)
        cr.new_sub_path()
        cr.arc(x + w - rad, y + rad, rad, -math.pi / 2, 0)
        cr.arc(x + w - rad, y + h - rad, rad, 0, math.pi / 2)
        cr.arc(x + rad, y + h - rad, rad, math.pi / 2, math.pi)
        cr.arc(x + rad, y + rad, rad, math.pi, 3 * math.pi / 2)
        cr.close_path()

    @staticmethod
    def text(cr, s, w, h, rgb, size, y, x=None, right_align=False, maxw=None):
        cr.select_font_face("JetBrainsMono Nerd Font Propo")
        cr.set_font_size(size)
        # clip from the left so the newest words stay visible
        maxw = maxw or (w - 28)
        shown = s
        while shown and cr.text_extents(shown).x_advance > maxw:
            shown = shown[max(1, len(shown) // 8):]
        if shown != s:
            shown = "…" + shown.lstrip()
        ext = cr.text_extents(shown)
        if x is None:
            x = (w - ext.x_advance) - 14 if right_align else (w - ext.x_advance) / 2
        r, g, b = rgb
        cr.set_source_rgba(r, g, b, 0.95)
        cr.move_to(x, y)
        cr.show_text(shown)


def main():
    app = Gtk.Application(application_id="dev.whisp.overlay")
    holder = {}

    def activate(a):
        holder["ov"] = Overlay()
        a.hold()

    app.connect("activate", activate)
    app.run(sys.argv)


if __name__ == "__main__":
    main()

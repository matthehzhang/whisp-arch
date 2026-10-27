"""Small native Tk window. Every widget is touched only on the main thread."""

import logging
import queue
import sys
import threading
import time
import tkinter as tk
from tkinter import simpledialog, ttk

from .audio import MAX_SECONDS, Cues, Recorder, devices
from .config import HOTKEYS, Settings, data_dir
from .controller import Controller
from .credentials import delete_key, save_key
from .desktop import Desktop
from .hotkey import Hotkey
from .transcribe import Engine

log = logging.getLogger(__name__)

# Quiet utility window; the large recording control carries the visual emphasis.
# White workspace, slate text, blue controls, red reserved for live capture.
PALETTE = {
    "paper": "#FFFFFF",
    "surface": "#EEF2F7",
    "ink": "#24364B",
    "muted": "#53657B",
    "action": "#225DAD",
    "live": "#B02B46",
}


class App:
    def __init__(self, root):
        self.root = root
        self.events = queue.Queue()
        self.path = data_dir() / "settings.json"
        self.hotkey = None
        self.closing = False
        self.last_transcript = ""
        self.notice = ""
        self.device_labels = {"input": ["System default"], "output": ["System default"]}
        try:
            self.settings = Settings.load(self.path)
        except (OSError, ValueError) as exc:
            self.settings = Settings()
            self.notice = f"Settings could not be read; using defaults. {exc}"
        self.recorder = Recorder()
        self.desktop = Desktop()
        self.cues = Cues(lambda text: self.post(lambda: self.show_notice(text)))
        self.controller = Controller(
            self.recorder,
            Engine(),
            self.desktop,
            self.settings,
            self.work,
            root.after,
            self.render,
            self.play_cue,
        )
        self._build()
        self._install_hotkey()
        root.protocol("WM_DELETE_WINDOW", self.close)
        root.after(25, self.pump)
        self.refresh_devices()
        # Check for a stored key without sending any audio or API request.
        self.controller.prepare()

    def _build(self):
        root = self.root
        root.title("Whisp")
        root.geometry("600x760")
        root.minsize(530, 710)
        root.configure(background=PALETTE["paper"])
        family = (
            "Segoe UI"
            if sys.platform == "win32"
            else "Helvetica Neue"
            if sys.platform == "darwin"
            else "DejaVu Sans"
        )
        root.option_add("*Font", (family, 11))
        style = ttk.Style(root)
        style.theme_use("clam")
        style.configure("TFrame", background=PALETTE["paper"])
        style.configure("TLabel", background=PALETTE["paper"], foreground=PALETTE["ink"])
        style.configure("Quiet.TLabel", foreground=PALETTE["muted"])
        style.configure("TButton", padding=(12, 8))
        style.configure("TCheckbutton", background=PALETTE["paper"], foreground=PALETTE["ink"])
        style.configure(
            "Record.TButton",
            background=PALETTE["action"],
            foreground="white",
            font=(family, 15, "bold"),
            padding=(16, 17),
        )
        style.map("Record.TButton", background=[("disabled", "#8796AA"), ("active", "#194987")])
        style.configure(
            "Live.TButton",
            background=PALETTE["live"],
            foreground="white",
            font=(family, 15, "bold"),
            padding=(16, 17),
        )
        style.map("Live.TButton", background=[("active", "#8D2137")])
        style.configure(
            "Level.Horizontal.TProgressbar",
            background=PALETTE["action"],
            troughcolor=PALETTE["surface"],
            borderwidth=0,
        )

        page = ttk.Frame(root, padding=26)
        page.pack(fill="both", expand=True)
        title = ttk.Frame(page)
        title.pack(fill="x")
        ttk.Label(title, text="Whisp", font=(family, 25, "bold")).pack(side="left")
        ttk.Label(title, text="Your voice, in any app", style="Quiet.TLabel").pack(
            side="right", pady=8
        )
        self.hint = ttk.Label(page, style="Quiet.TLabel")
        self.hint.pack(anchor="w", pady=(16, 10))
        self.record = ttk.Button(
            page, text="Add an API key to begin", style="Record.TButton", command=self.toggle
        )
        self.record.pack(fill="x")
        self.level = ttk.Progressbar(page, style="Level.Horizontal.TProgressbar", maximum=1)
        self.level.pack(fill="x", pady=(8, 10))
        self.status = ttk.Label(page, wraplength=530)
        self.status.pack(fill="x")
        self.cancel_button = ttk.Button(
            page, text="Cancel recording", command=self.controller.cancel
        )
        self.cancel_button.pack(anchor="w", pady=(8, 15))

        config = ttk.Frame(page)
        config.pack(fill="x")
        config.columnconfigure(1, weight=1)
        self.shortcut = tk.StringVar(value=self.settings.hotkey)
        self.microphone = tk.StringVar(value=self.settings.microphone or "System default")
        self.speaker = tk.StringVar(value=self.settings.speaker or "System default")
        self.controls = []
        for row, label, variable, values in (
            (0, "Hold to talk", self.shortcut, HOTKEYS),
            (1, "Microphone", self.microphone, self.device_labels["input"]),
            (2, "Sound output", self.speaker, self.device_labels["output"]),
        ):
            ttk.Label(config, text=label).grid(row=row, column=0, sticky="w", padx=(0, 15), pady=5)
            control = ttk.Combobox(config, textvariable=variable, values=values, state="readonly")
            control.grid(row=row, column=1, sticky="ew", pady=5)
            control.bind("<<ComboboxSelected>>", self.save_settings)
            self.controls.append(control)
        actions = ttk.Frame(page)
        actions.pack(fill="x", pady=(6, 8))
        self.key_button = ttk.Button(actions, text="Set API key", command=self.set_key)
        self.key_button.pack(side="left")
        self.forget_button = ttk.Button(actions, text="Forget key", command=self.forget_key)
        self.forget_button.pack(side="left", padx=6)
        self.refresh = ttk.Button(actions, text="Refresh devices", command=self.refresh_devices)
        self.refresh.pack(side="right")
        ttk.Label(
            page,
            text="Audio is sent to Deepgram after recording. Uses your own API key and account credit.",
            style="Quiet.TLabel",
            wraplength=530,
        ).pack(anchor="w", pady=(0, 10))
        self.sounds = tk.BooleanVar(value=self.settings.sounds)
        self.auto_paste = tk.BooleanVar(value=self.settings.auto_paste)
        self.sound_toggle = ttk.Checkbutton(
            page,
            text="Play start and stop sounds",
            variable=self.sounds,
            command=self.save_settings,
        )
        self.sound_toggle.pack(anchor="w")
        self.paste_toggle = ttk.Checkbutton(
            page,
            text="Paste if the destination is still focused",
            variable=self.auto_paste,
            command=self.save_settings,
        )
        self.paste_toggle.pack(anchor="w", pady=(4, 14))

        transcript_header = ttk.Frame(page)
        transcript_header.pack(fill="x")
        ttk.Label(transcript_header, text="Last transcript", font=(family, 11, "bold")).pack(
            side="left"
        )
        self.copy_button = ttk.Button(transcript_header, text="Copy", command=self.copy)
        self.copy_button.pack(side="right")
        self.text = tk.Text(
            page,
            height=4,
            wrap="word",
            relief="flat",
            padx=12,
            pady=10,
            background=PALETTE["surface"],
            foreground=PALETTE["ink"],
            state="disabled",
        )
        self.text.pack(fill="both", expand=True, pady=(8, 8))
        self.notice_label = ttk.Label(page, text=self.notice, style="Quiet.TLabel", wraplength=530)
        self.notice_label.pack(fill="x")
        if sys.platform == "darwin":
            self.notice = "Mac: allow Microphone, Accessibility, and Input Monitoring in System Settings. You may need Fn with F8."
        elif sys.platform != "win32":
            self.notice = "Windows and macOS build. On Linux, use the existing native daemon for global hotkeys and paste."
        self.render()

    def post(self, callback):
        self.events.put(callback)

    def work(self, job, done):
        def run():
            try:
                value = job()
            except Exception as exc:
                log.exception("Background operation failed")
                self.post(lambda exc=exc: done(None, exc))
            else:
                self.post(lambda: done(value, None))

        threading.Thread(target=run, daemon=True).start()

    def pump(self):
        if self.closing:
            return
        for _ in range(100):
            try:
                callback = self.events.get_nowait()
            except queue.Empty:
                break
            try:
                callback()
            except Exception as exc:
                log.exception("UI event failed")
                self.show_notice(f"Operation failed: {exc}")
        if self.controller.state == "recording" and self.recorder.capped.is_set():
            self.controller.release(self.desktop.capture())
        self.level["value"] = min(1, self.recorder.level * 8)
        if self.controller.state == "recording" and self.controller.session:
            elapsed = int(time.monotonic() - self.controller.session.started)
            self.record.configure(text=f"Stop recording   {elapsed // 60}:{elapsed % 60:02d}")
            stream = self.recorder.stream
            try:
                ended = stream is None or not stream.active
            except Exception:
                ended = True
            if ended or elapsed >= MAX_SECONDS:
                self.controller.release(self.desktop.capture())
        if self.hotkey and not self.hotkey.listener.is_alive():
            self.hotkey = None
            self.show_notice(
                "Global shortcut stopped. Restart Whisp; you can still use the recording button."
            )
        self.root.after(25, self.pump)

    def _install_hotkey(self):
        if self.hotkey:
            self.hotkey.stop()
            self.hotkey = None
        try:
            hotkey = Hotkey(self.settings.hotkey, self._key_event, self.desktop.capture)
            hotkey.start()
            self.hotkey = hotkey
        except Exception as exc:
            self.show_notice(f"Global shortcut unavailable: {exc}. Use the recording button.")

    def _key_event(self, kind, target):
        def handle():
            if kind == "press":
                self.controller.press()
            elif kind == "release":
                self.controller.release(target)
            elif kind == "cancel":
                self.controller.cancel()

        self.post(handle)

    def toggle(self):
        if self.controller.state in ("starting", "recording"):
            # Clicking the UI means delivery is clipboard-only.
            self.controller.release()
        else:
            self.controller.press()

    def play_cue(self, start):
        if self.settings.sounds:
            self.cues.play(start, self.settings.speaker)

    def show_notice(self, text):
        self.notice = text
        if not self.closing:
            self.notice_label.configure(text=text)

    def refresh_devices(self):
        def discover():
            return {kind: [label for label, _ in devices(kind)] for kind in ("input", "output")}

        def ready(result, error):
            if error:
                self.show_notice(f"Cannot list audio devices: {error}")
                return
            for index, kind in ((1, "input"), (2, "output")):
                self.controls[index]["values"] = ["System default", *result[kind]]

        self.work(discover, ready)

    def save_settings(self, event=None):
        old_hotkey = self.settings.hotkey
        self.settings.hotkey = self.shortcut.get()
        self.settings.microphone = (
            "" if self.microphone.get() == "System default" else self.microphone.get()
        )
        self.settings.speaker = "" if self.speaker.get() == "System default" else self.speaker.get()
        self.settings.sounds, self.settings.auto_paste = self.sounds.get(), self.auto_paste.get()
        try:
            self.settings.save(self.path)
        except OSError as exc:
            self.show_notice(f"Could not save settings: {exc}")
        if old_hotkey != self.settings.hotkey:
            self._install_hotkey()
        self.render()

    def set_key(self):
        previous = self.controller.state, self.controller.message
        if previous[0] not in ("setup", "ready"):
            return
        self.controller.update("loading", "Enter your API key…")
        value = simpledialog.askstring(
            "Deepgram API key",
            "Paste your own Deepgram API key. It will be saved in your system credential store.",
            show="*",
            parent=self.root,
        )
        if value is None:
            self.controller.update(*previous)
            return
        self.controller.update("loading", "Saving API key…")

        def done(result, error):
            if error:
                self.controller.update("setup", str(error))
            else:
                self.controller.update("setup", "Key saved.")
                self.controller.prepare()

        self.work(lambda: save_key(value), done)

    def forget_key(self):
        self.controller.update("loading", "Removing API key…")

        def done(result, error):
            self.controller.update(
                "setup", str(error) if error else "API key removed. Add a key to begin."
            )

        self.work(delete_key, done)

    def copy(self):
        if not self.controller.transcript:
            return
        try:
            self.desktop.copy(self.controller.transcript)
            self.show_notice("Transcript copied.")
        except Exception as exc:
            self.show_notice(f"Clipboard unavailable: {exc}. Select and copy text below.")

    def render(self):
        if self.closing:
            return
        state = self.controller.state
        idle = state in ("setup", "ready")
        recording = state == "recording"
        self.hint.configure(text=f"Hold {self.settings.hotkey} in any app, or click to record.")
        self.status.configure(text=self.controller.message)
        self.record.configure(
            style="Live.TButton" if recording else "Record.TButton",
            text={
                "ready": "Record",
                "recording": "Stop recording",
                "starting": "Opening microphone…",
                "setup": "Add an API key to begin",
                "loading": "Checking API key…",
                "transcribing": "Transcribing…",
                "tail": "Finishing recording…",
                "stopping": "Stopping microphone…",
            }[state],
        )
        self.record.state(
            ["!disabled"] if state in ("ready", "starting", "recording") else ["disabled"]
        )
        self.cancel_button.state(["!disabled"] if self.controller.session else ["disabled"])
        self.key_button.state(["!disabled"] if idle else ["disabled"])
        self.forget_button.state(["!disabled"] if idle else ["disabled"])
        self.refresh.state(["!disabled"] if idle else ["disabled"])
        for control in self.controls:
            control.configure(state="readonly" if idle else "disabled")
        self.copy_button.state(["!disabled"] if self.controller.transcript else ["disabled"])
        if self.last_transcript != self.controller.transcript:
            self.last_transcript = self.controller.transcript
            self.text.configure(state="normal")
            self.text.delete("1.0", "end")
            self.text.insert("1.0", self.last_transcript)
            self.text.configure(state="disabled")
        self.notice_label.configure(text=self.notice)

    def close(self):
        self.closing = True
        self.controller.close()
        if self.hotkey:
            self.hotkey.stop()
        # Daemon workers cannot inject after close; no Tk callbacks are run.
        self.root.destroy()


def run():
    root = tk.Tk()
    App(root)
    root.mainloop()

# Whisp for Windows

This is the **Windows** branch. The default [main](https://github.com/matthehzhang/whisp-arch)
branch contains the Mac app; [linux](https://github.com/matthehzhang/whisp-arch/tree/linux)
contains the Linux / Hyprland daemon.

Hold **F8**, speak, and release to transcribe with your own **Deepgram API key**.
Whisp plays start/stop sounds, copies the result, and pastes when the destination
still has focus. **Esc** cancels. Record / Stop buttons are also available.

## Install the Windows preview

Download the `Whisp-windows-x64` artifact from a successful
[Windows desktop build](https://github.com/matthehzhang/whisp-arch/actions/workflows/windows.yml).
Extract the artifact and its enclosed ZIP. Keep the whole **Whisp** folder
together, then open **Whisp.exe**. No Python installation is needed. This preview
is unsigned, so Windows may show an unknown-publisher notice.

Choose **Set API key** and enter your own key from the
[Deepgram console](https://console.deepgram.com/). The key is saved in Windows
Credential Manager, not in the repository or settings file. Audio is uploaded to
Deepgram over HTTPS after recording; internet and credit in your account are
required. There is no Matthew-owned server, local model download, or shared key.

## Controls

- Pick your microphone and speaker output in Settings. Enable microphone access
  for desktop apps in Windows privacy settings.
- Choose F8, F9, or F10. The shortcut is listened to, not suppressed, so choose
  a key that does not already trigger another action in your applications.
- Keep the destination focused until transcription finishes. If the window or
  native focused field changed, use the clipboard copy instead of automatic paste.
- Ctrl+V is used for paste. Administrator apps can block injected keystrokes;
  terminals may require a different shortcut. Use manual paste there.
- Recording stops after 90 seconds, with a 200 ms tail after normal release.
  Cancellation discards the result but cannot recall audio already uploaded.
- Minimize to keep the shortcut active. Closing this Windows preview quits it.
- **Forget key** removes the stored credential. Settings and diagnostic logs are
  under `%LOCALAPPDATA%\Whisp`. Audio and transcript history are not saved.

## Develop

Use Python 3.11 or 3.12 with Tk included, or uv:

```sh
uv sync --locked --python 3.12
uv run pytest -q
uv run python -m whisp
uv run pyinstaller --noconfirm Whisp.spec
uv run python package.py
```

Build on Windows x64. CI checks tests and the frozen application's UI/imports;
real microphone, permission, paid API, and paste checks still need a Windows PC.
The Mac branch has its own Fn/Globe, haptic, menu-bar, and login-startup integration.

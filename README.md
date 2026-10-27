# Whisp for Mac

Hold the bottom-left **Fn / Globe** key, speak, and release to insert your words.
Prefer a toggle? Choose **Press to start / stop** during setup. Whisp lives in the
menu bar, shows a small recording indicator, and can open automatically at login.
Quiet start/stop sounds and optional trackpad haptics tell you when it is listening.

**[Download the Mac preview](https://github.com/matthehzhang/whisp-arch/releases/tag/mac-v0.2.0-preview)**

This repository has separate platform branches:

| Branch | Contents |
| --- | --- |
| [`main`](https://github.com/matthehzhang/whisp-arch) | Current Mac version; default checkout |
| [`mac`](https://github.com/matthehzhang/whisp-arch/tree/mac) | Mac development and release branch |
| [`windows`](https://github.com/matthehzhang/whisp-arch/tree/windows) | Windows app |
| [`linux`](https://github.com/matthehzhang/whisp-arch/tree/linux) | Linux / Hyprland daemon |

## Install

Requires **macOS 13 or later**. No Python installation or Terminal commands needed.

1. Download `Whisp-Mac-AppleSilicon.dmg` for M1/M2/M3/M4 or newer Macs, or
   `Whisp-Mac-Intel.dmg` for Intel. Apple menu → About This Mac identifies your chip.
2. Open the DMG and drag **Whisp** onto **Applications**.
3. Open Whisp from Applications. Eject the installer when finished.
4. This is an **unsigned preview**. If macOS blocks it, open System Settings →
   Privacy & Security → **Open Anyway** for Whisp, then confirm. Do not disable
   Gatekeeper or other system protections.
5. In setup, choose hold or toggle, and set your own
   [Deepgram API key](https://console.deepgram.com/).
6. Use the **Allow / settings** buttons for Microphone, Accessibility, and Input
   Monitoring. Allow Whisp in each panel. Setup notices permission changes without
   restarting. If macOS requires a restart after Input Monitoring, quit from the
   menu bar and reopen Whisp.
7. Leave **Open at login** checked to enable automatic startup. Approve Whisp in
   Login Items if macOS asks, then click **Finish setup**.

The key is saved in **macOS Keychain**, not in a settings file. Each recording is
sent to Deepgram over HTTPS after you stop and billed to your own account. No
Matthew-owned server, shared account, or local speech model is involved.

## Everyday use

- **Hold mode:** hold Fn / Globe, speak, release. Fn+Space locks the current
  recording hands-free; press Fn again to stop.
- **Toggle mode:** press Fn to start; press it again to stop. Releasing Fn keeps
  recording in this mode.
- **Escape:** cancel a recording or discard a pending result. Audio already sent
  to the API cannot be recalled; cancelling prevents its result from being pasted.
- **Menu bar:** start/stop recording, cancel, open Settings, or quit Whisp.
  Closing Settings keeps Whisp running. Reopening the app shows the existing
  settings window instead of starting another listener.
- **Feedback:** sounds and haptics have separate switches. Trackpad haptics require
  supported Force Touch hardware; they do not vibrate an ordinary mouse or every Mac.
- **Destination:** keep your text field focused while transcription finishes.
  If the destination changed or cannot be verified, the result remains on the
  clipboard. The last transcript is also available in Settings.
- **External keyboards:** choose F8, F9, or F10 if your keyboard has no Apple Fn
  key. These function-key fallbacks are listened to, not suppressed.
- **Globe conflicts:** if Globe opens emoji or Apple's Dictation, use the Keyboard
  settings button and set “Press Globe key to” to **Do Nothing**. Whisp does not
  silently change your system keyboard preferences.

There is a 90-second recording limit and a 200 ms tail after release. A new
recording waits for the previous transcription. Whisp does not save audio or a
transcript history to disk, and does not yet add a separate LLM rewriting step.
Some applications reuse the same accessibility element for several internal text
fields; disable automatic paste in those applications and use the clipboard.

## Disable startup or uninstall

Uncheck **Open at login** in Settings, or disable Whisp in System Settings →
General → Login Items. Use **Forget key** to delete your API key from Keychain,
quit from the menu bar, and remove Whisp.app from Applications. Settings and
bounded diagnostic logs are in `~/Library/Application Support/Whisp`.

## Build and validate

```sh
uv sync --locked --python 3.12
uv run pytest -q
uv run python -m whisp
uv run pyinstaller --noconfirm Whisp.spec
uv run python package.py
```

Build on a Mac of the matching architecture. GitHub Actions builds both Mac
variants, runs lifecycle/shortcut/login/haptic/HTTP regression tests, and checks
the frozen app's UI, menu bar, indicator, native frameworks, and credential/TLS
libraries. It then creates the drag-to-Applications DMG and a SHA-256 checksum.
The smoke check does not record, read stored keys, send audio, grant permissions,
or register a login item.

Real-device checks are still required for Fn events, OS permission prompts,
Microphone input, physical haptics, login after reboot, and paste into your apps.
Unsigned builds may require permissions again after replacing the app. A signed,
notarized release requires an Apple Developer ID; this preview deliberately does
not claim that status.

The interaction follows familiar dictation conventions, including those in
[Wispr Flow's Mac guide](https://docs.wisprflow.ai/articles/7682075140-how-to-install-wispr-flow-on-mac).
Whisp is an independent application with its own code and interface.

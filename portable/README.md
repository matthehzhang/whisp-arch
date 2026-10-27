# Whisp Desktop — Windows and macOS preview

Hold **F8**, speak, and release to transcribe using **your own Deepgram API key**.
A rising sound marks recording start; a falling sound marks recording stop.
Whisp copies the result and sends a paste shortcut if the destination still has
focus. **Esc** cancels. You can also click Record / Stop and copy manually.

This is a portable client in the existing repository. It uses Deepgram Nova-3
with smart formatting and multilingual recognition, without Matthew's private
server, Tailscale, or a local speech-model download. It does not yet include
streaming text or the Linux setup's additional LLM cleanup.

## Status

Source and a native build workflow are provided. A successful build does not
verify real microphone capture, OS permission prompts, or paste in another app;
complete the manual checks below before treating a package as a tested release.
Preview packages are not Developer ID signed/notarized or Windows publisher signed.

## Install a preview package

Get your computer's artifact from a successful **Portable desktop builds** run in
this repository's GitHub Actions. Extract the artifact, then the platform ZIP.

- **Windows x64:** keep the entire `Whisp` folder together and open `Whisp.exe`.
  Windows may show an unknown-publisher notice for this unsigned preview.
- **Apple Silicon Mac:** use the Apple Silicon artifact; move `Whisp.app` to Applications.
- **Intel Mac:** use the Intel artifact; move `Whisp.app` to Applications.
  macOS may block this unsigned preview. For a build you trust, use **Open Anyway**
  in System Settings → Privacy & Security after opening it. Signing/notarization
  is needed for smooth public distribution.

Choose **Set API key** and paste your own key from the
[Deepgram console](https://console.deepgram.com/). The key is saved in **Windows
Credential Manager** or **macOS Keychain**, never in the repository or settings
file. **Forget key** removes it. Startup checks that a key is stored; the first
transcription verifies it against the service. Internet and Deepgram account
credit are required; requests are billed to your account. Whisp does not retry
billable requests automatically.

Audio is uploaded to Deepgram over HTTPS after you stop recording. Cancel before
upload to discard it; cancelling during a request prevents pasting but cannot
recall audio already sent or reverse provider charges. Provider handling of the
audio is governed by your Deepgram account and its terms. No key or account is
shared with Matthew.

## Permissions and controls

On macOS, allow **Microphone**, **Accessibility**, and **Input Monitoring** for
Whisp in System Settings → Privacy & Security, then restart. From source, the
Terminal or Python launcher may need these permissions instead. Function keys
may require **Fn** or enabling standard F1–F12 keys in keyboard settings.

On Windows, enable microphone access for desktop apps in privacy settings.
Paste into administrator apps may be blocked; use Copy and paste manually.

- Select the microphone and sound output. Pick your speakers if you want people
  nearby to hear the cues while you wear headphones.
- F8/F9/F10 are available. The shortcut is listened to, not suppressed: choose a
  key that does not trigger another action in the app you use.
- Keep the destination focused until transcription finishes. If focus changed,
  Whisp keeps the transcript on the clipboard instead of switching applications.
  Apps that do not expose focus information use clipboard-only delivery.
- Paste uses Ctrl+V or Command+V. Some terminals require a different shortcut;
  paste manually there. An OS keystroke API cannot confirm the app accepted paste.
- Recording is capped at 90 seconds. Release keeps a 200 ms tail to retain the
  end of the last word. A new recording waits for the previous request to finish.
- Minimize to keep the shortcut active; closing quits Whisp.
- The last transcript stays in memory until quit and on the clipboard until
  replaced. Whisp does not save audio or transcript history to disk.

## Run from source

Use Python **3.11 or 3.12**, with Tk included (the python.org installer includes it
on Windows and macOS). Windows ARM is not a native build target in this preview.

```sh
cd portable
python -m venv .venv
# Windows:
.venv\Scripts\python -m pip install .
.venv\Scripts\python -m whisp
# macOS:
.venv/bin/python -m pip install .
.venv/bin/python -m whisp
```

For reproducible development/builds, install uv and run:

```sh
uv sync --locked --python 3.12
uv run pytest -q
uv run python -m whisp
uv run pyinstaller --noconfirm Whisp.spec
uv run python package.py
```

Build on the target OS and CPU architecture; PyInstaller does not cross-compile
these apps. The workflow builds Windows x64, macOS arm64, and macOS x64. Frozen
runtime checks construct the actual UI in a hidden window, verify native imports,
credential-backend imports, certificate bundles, and resampling. They do not
record, type, read or write credentials, or make a paid API call.

Settings and bounded diagnostic logs live in `%LOCALAPPDATA%\Whisp` on Windows or
`~/Library/Application Support/Whisp` on Mac. **Forget key** before removing the app
to delete the stored credential. Delete that directory to reset other settings.
Audio, transcripts, and keys are excluded from logs.

## Manual release checks

Run on a Windows PC and each supported Mac architecture:

1. Open a fresh package without Python installed; save your key and grant permissions.
2. Hold/release in a text editor. Confirm both sounds, full final word, and pasted text.
3. Tap rapidly, hold through repeats, and press Esc during capture and transcription.
   Cancelled results must never paste later.
4. Switch windows or text fields before the request ends. Confirm clipboard recovery.
   Some apps reuse the same native/accessibility field for internal editors; use
   clipboard-only mode where focus detection cannot distinguish them.
5. Deny/revoke microphone and keyboard permissions; verify recovery messages.
6. Disconnect/reconnect a selected mic, refresh, and choose an available input.
7. Test speakers/headphones, including cue volume and speech recognition.
8. Test an invalid key, no internet, service failure, and key removal/replacement.
9. Test sleep/wake and the 90-second recording limit.

## Implementation references

- [Deepgram prerecorded transcription](https://developers.deepgram.com/docs/pre-recorded-audio).
- [Native credential stores](https://keyring.readthedocs.io/en/stable/).
- [PortAudio/sounddevice](https://python-sounddevice.readthedocs.io/en/latest/installation.html).
- [pynput permissions](https://pynput.readthedocs.io/en/latest/limitations.html).
- [Windows SendInput restrictions](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-sendinput).
- [PyInstaller packaging](https://pyinstaller.org/en/stable/feature-notes.html).

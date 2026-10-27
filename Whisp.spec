# Build on the target OS/architecture; no local model or private credentials.
import sys
import os
from PyInstaller.utils.hooks import collect_all, collect_submodules

datas, binaries, hiddenimports = [], [], []
for package in ("soxr", "keyring", "certifi"):
    data, libs, modules = collect_all(package)
    datas += data
    binaries += libs
    hiddenimports += modules
backend = "win32" if sys.platform == "win32" else "darwin" if sys.platform == "darwin" else "xorg"
hiddenimports += [f"pynput.keyboard._{backend}", f"pynput.mouse._{backend}", f"pynput._util.{backend}"]
if sys.platform == "darwin":
    hiddenimports += collect_submodules("ApplicationServices")

a = Analysis(["launcher.py"], pathex=[SPECPATH], binaries=binaries, datas=datas,
             hiddenimports=hiddenimports, hookspath=[], runtime_hooks=[],
             excludes=["torch", "tensorflow", "pytest"], noarchive=False)
pyz = PYZ(a.pure)
exe = EXE(pyz, a.scripts, [], exclude_binaries=True, name="Whisp", debug=False,
          strip=False, upx=False, console=False, disable_windowed_traceback=False,
          target_arch=None, codesign_identity=os.environ.get("WHISP_SIGN_IDENTITY"), entitlements_file=None)
bundle = COLLECT(exe, a.binaries, a.datas, strip=False, upx=False, name="Whisp")
if sys.platform == "darwin":
    app = BUNDLE(bundle, name="Whisp.app", bundle_identifier="io.github.matthehzhang.whisp",
                 icon="assets/Whisp.icns",
                 info_plist={
                     "CFBundleShortVersionString": "0.2.0",
                     "CFBundleVersion": "2",
                     "LSUIElement": True,
                     "LSMinimumSystemVersion": "13.0",
                     "NSMicrophoneUsageDescription": "Whisp records while you dictate and sends audio to Deepgram using your own API key.",
                     "NSHighResolutionCapable": True,
                 })

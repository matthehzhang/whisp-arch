"""Build a drag-to-Applications disk image. No scripts run on the user's Mac."""

import hashlib
import platform
import sys
from pathlib import Path


def build():
    if sys.platform != "darwin":
        raise SystemExit("Build the Mac installer on macOS.")
    import dmgbuild

    root = Path(__file__).resolve().parent
    artifacts = root / "artifacts"
    artifacts.mkdir(exist_ok=True)
    cpu = "AppleSilicon" if platform.machine() == "arm64" else "Intel"
    dmg = artifacts / f"Whisp-Mac-{cpu}.dmg"
    dmgbuild.build_dmg(
        str(dmg),
        "Install Whisp",
        settings={
            "files": [str(root / "dist" / "Whisp.app")],
            "symlinks": {"Applications": "/Applications"},
            "icon_locations": {"Whisp.app": (160, 155), "Applications": (480, 155)},
            "window_rect": ((100, 100), (640, 340)),
            "icon_size": 96,
            "text_size": 14,
            "default_view": "icon-view",
            "show_status_bar": False,
            "show_tab_view": False,
            "show_toolbar": False,
            "show_pathbar": False,
            "show_sidebar": False,
            "background": str(root / "assets" / "installer.png"),
        },
    )
    digest = hashlib.file_digest(dmg.open("rb"), "sha256").hexdigest()
    (artifacts / f"{dmg.name}.sha256").write_text(f"{digest}  {dmg.name}\n")
    print(dmg)


if __name__ == "__main__":
    build()

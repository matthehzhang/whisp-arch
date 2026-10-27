"""Zip the native build with user-facing setup notes and dependency notices."""

import platform
import shutil
import subprocess
import sys
from pathlib import Path

root = Path(__file__).resolve().parent
artifacts = root / "artifacts"
artifacts.mkdir(exist_ok=True)
tag = f"Whisp-{sys.platform}-{platform.machine()}"
if sys.platform == "darwin":
    # ditto preserves application bundle symlinks, permissions, and metadata.
    subprocess.run(
        [
            "ditto",
            "-c",
            "-k",
            "--sequesterRsrc",
            "--keepParent",
            str(root / "dist" / "Whisp.app"),
            str(artifacts / f"{tag}.zip"),
        ],
        check=True,
    )
elif sys.platform == "win32":
    shutil.copy2(root / "README.md", root / "dist" / "Whisp" / "README.md")
    shutil.make_archive(str(artifacts / tag), "zip", root / "dist", "Whisp")
else:
    raise SystemExit("Release packages are built on Windows or macOS.")

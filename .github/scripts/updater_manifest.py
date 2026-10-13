#!/usr/bin/env python3
"""Writes the desktop app's update manifest (latest.json) for one release.

Usage: updater_manifest.py <release-files-dir> <tag> <repository> <notes-file>

Each update package is listed under the targets the app looks for (`{os}-{arch}-{installer}`), so an install only
ever updates from its own kind of package: a Debian install from the .deb, never from the AppImage. A package
without its `.sig` is left out, and the app on that platform sees no update. The signature is the `.sig` file's
contents, which the app checks against the public key built into it before installing anything.
"""

import datetime
import json
import pathlib
import sys

PACKAGES = {
    "Culprit-SQL-Studio-macOS.app.tar.gz": ["darwin-aarch64-app", "darwin-x86_64-app"],
    "Culprit-SQL-Studio-Windows-x64-setup.exe": ["windows-x86_64-nsis"],
    "Culprit-SQL-Studio-Linux-x86_64.AppImage": ["linux-x86_64-appimage"],
    "Culprit-SQL-Studio-Linux-amd64.deb": ["linux-x86_64-deb"],
}


def main() -> int:
    if len(sys.argv) != 5:
        print(__doc__, file=sys.stderr)
        return 2
    files, tag, repository, notes = pathlib.Path(sys.argv[1]), sys.argv[2], sys.argv[3], pathlib.Path(sys.argv[4])
    platforms = {}
    for name, targets in PACKAGES.items():
        package, signature = files / name, files / f"{name}.sig"
        if not package.exists():
            print(f"::warning::{name} isn't in this release, so its platforms get no update")
            continue
        if not signature.exists():
            print(f"::warning::{name} has no signature, so its platforms get no update")
            continue
        entry = {
            "signature": signature.read_text().strip(),
            "url": f"https://github.com/{repository}/releases/download/{tag}/{name}",
        }
        for target in targets:
            platforms[target] = entry
    if not platforms:
        print("::warning::no signed update package: this release has no latest.json, and installed apps won't update to it")
        return 0
    manifest = {
        "version": tag.removeprefix("v"),
        "notes": notes.read_text().strip(),
        "pub_date": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "platforms": platforms,
    }
    (files / "latest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print(f"latest.json: {', '.join(sorted(platforms))}")
    return 0


if __name__ == "__main__":
    sys.exit(main())

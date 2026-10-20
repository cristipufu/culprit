#!/usr/bin/env python3
"""Finish the copied static site: current facts, content-versioned assets and an enforced page CSP.

GitHub Pages controls HTTP cache headers. Query versions prevent stale asset reuse when content changes while
keeping stable paths valid for cached HTML; they do not change the host's ten-minute cache policy.
"""

import base64
import hashlib
import html
import json
from pathlib import Path
import re
import tomllib


def build(root: Path) -> None:
    site = root / "_site"
    version = tomllib.loads((root / "Cargo.toml").read_text())["workspace"]["package"]["version"]
    checks = len(json.loads((root / "fixtures/checks.sample.json").read_text()))
    assets = {}

    def remember(path: Path) -> None:
        url = "/" + path.relative_to(site).as_posix()
        digest = hashlib.sha256(path.read_bytes()).hexdigest()[:16]
        assets[url] = f"{url}?v={digest}"

    def references(text: str) -> str:
        for source, target in sorted(assets.items(), key=lambda pair: -len(pair[0])):
            # A complete path only: avoid changing a longer filename or already-versioned URL.
            text = re.sub(re.escape(source) + r"(?=[\s\"'<>),]|$)", lambda _: target, text)
        return text

    for path in sorted(site.rglob("*")):
        if path.suffix in {".webp", ".woff2", ".js", ".svg", ".ico", ".png", ".jpg"}:
            remember(path)
    for path in sorted(site.rglob("*.css")):
        path.write_text(references(path.read_text()))
        remember(path)
    for path in sorted(site.glob("*.html")):
        text = references(path.read_text().replace("__VERSION__", version).replace("__CHECK_COUNT__", str(checks)))
        inline = re.findall(r'<script type="application/ld\+json">(.*?)</script>', text, flags=re.DOTALL)
        hashes = " ".join("'sha256-" + base64.b64encode(hashlib.sha256(script.encode()).digest()).decode() + "'" for script in inline)
        policy = (
            "default-src 'none'; "
            f"script-src 'self' https://plausible.io/js/pa-r5EkaWqkhpILeZH2bizcF.js {hashes}; "
            "style-src 'self'; font-src 'self'; img-src 'self' data:; "
            "connect-src 'self' https://api.github.com/repos/cristipufu/culprit-sql-studio https://plausible.io/api/event; "
            "object-src 'none'; base-uri 'none'; form-action 'none'; frame-src 'none'"
        )
        path.write_text(text.replace("__CONTENT_SECURITY_POLICY__", html.escape(policy, quote=True)))
    path = site / "llms.txt"
    path.write_text(path.read_text().replace("__CHECK_COUNT__", str(checks)))


if __name__ == "__main__":
    build(Path(__file__).resolve().parent.parent)

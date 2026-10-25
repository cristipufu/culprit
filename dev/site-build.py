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
from urllib.parse import unquote, urlsplit, urlunsplit


def documentation(root: Path, site: Path) -> str:
    """Publish the common docs and every provider linked by the parity catalog."""
    parity = root / "docs/parity.md"
    parity_text = parity.read_text()
    providers = re.findall(r"^\| \[([^\]]+)\]\((providers/[a-z-]+\.md)\)", parity_text, re.MULTILINE)
    if not providers or len({path for _, path in providers}) != len(providers):
        raise ValueError("Engine parity must list each provider once")

    sources = ["README.md", "docs/guide.md", "docs/agents.md", "docs/parity.md", "docs/ssh.md"]
    provider_sources = ["docs/" + path for _, path in providers]
    sections = ["Connecting", "Permissions and setup", "Workload and plans", "Live activity and waits",
                "Schema and indexes", "Limits and evidence", "Verification"]
    for (label, _), source in zip(providers, provider_sources):
        text = (root / source).read_text()
        if text.splitlines()[0] != f"# {label}" or re.findall(r"^## ([^\n]+)$", text, re.MULTILINE) != sections:
            raise ValueError(f"Provider page {source} must use the common title and seven-section structure")
    sources += provider_sources + ["docs/providers/coverage.md", "docs/ARCHITECTURE.md", "docs/DESIGN.md", "docs/sqlite.md"]
    destinations = {str((root / source).resolve()): source.lower() for source in sources}
    destinations[str((root / "README.md").resolve())] = "docs/readme.md"

    def publish_links(text: str, source: Path) -> str:
        def target(url: str) -> str:
            parts = urlsplit(url)
            if parts.scheme or parts.netloc or not parts.path:
                return url
            path = (source.parent / unquote(parts.path)).resolve()
            relative = path.relative_to(root).as_posix()
            if not path.exists():
                raise ValueError(f"Broken documentation link in {source.relative_to(root)}: {url}")
            destination = destinations.get(str(path))
            if destination:
                base = "/" + destination
            elif path.suffix.lower() in {".png", ".svg", ".jpg", ".webp"}:
                base = "https://raw.githubusercontent.com/cristipufu/culprit-sql-studio/main/" + relative
            else:
                kind = "tree" if path.is_dir() else "blob"
                base = f"https://github.com/cristipufu/culprit-sql-studio/{kind}/main/{relative}"
            parsed = urlsplit(base)
            return urlunsplit((parsed.scheme, parsed.netloc, parsed.path, parts.query, parts.fragment))

        def replace(match: re.Match) -> str:
            return match[1] + target(match[2]) + match[3]

        text = re.sub(r'(!?\[[^\]\n]*\]\()([^\s)]+)(\))', replace, text)
        return re.sub(r'''(\b(?:src|href)=["'])([^"']+)(["'])''', replace, text)

    published = {}
    for source in sources:
        path = root / source
        text = publish_links(path.read_text(), path)
        destination = site / destinations[str(path.resolve())]
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_text(text)
        published[source] = text
    full_sources = ["README.md", "docs/guide.md", "docs/parity.md"] + provider_sources + ["docs/ssh.md", "docs/agents.md", "docs/providers/coverage.md"]
    full = "# Culprit SQL Studio: full documentation\n\nSource: https://github.com/cristipufu/culprit-sql-studio · Website: https://culpritsql.com\n"
    for source in full_sources:
        full += "\n---\n\n" + published[source]
    (site / "llms-full.txt").write_text(full)

    logos = site / "images/providers"
    logos.mkdir(parents=True, exist_ok=True)
    cards = []
    for label, provider in providers:
        slug = Path(provider).stem
        logo = root / "assets/providers" / f"{slug}.svg"
        (logos / logo.name).write_bytes(logo.read_bytes())
        row = next(line for line in parity_text.splitlines() if line.startswith(f"| [{label}]({provider})"))
        summary = row.split("|")[3].strip().replace("`", "")
        cards.append(
            f'<article class="engine" data-provider="{slug}"><span class="provider-logo" aria-hidden="true">'
            f'<img src="/images/providers/{slug}.svg" alt="" width="40" height="40" loading="lazy" decoding="async"></span>'
            f'<h3>{html.escape(label)}</h3><p>{html.escape(summary)}.</p><p>'
            f'<a href="https://github.com/cristipufu/culprit-sql-studio/blob/main/docs/{provider}">Coverage and setup →</a></p></article>'
        )
    return "\n          ".join(cards)


def build(root: Path) -> None:
    site = root / "_site"
    provider_cards = documentation(root, site)
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
        text = references(path.read_text().replace("__VERSION__", version).replace("__CHECK_COUNT__", str(checks)).replace("__PROVIDER_CARDS__", provider_cards))
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

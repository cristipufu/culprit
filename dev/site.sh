#!/usr/bin/env bash
# Assemble culpritsql.com into _site/: the page and its assets, the docs as markdown for llms.txt, llms-full.txt, and the
# sitemap dated by the last commit that touched the site or the docs.
#
#   dev/site.sh          # build _site/
#   dev/site.sh serve    # build and serve on http://localhost:4321
set -euo pipefail
cd "$(dirname "$0")/.."
rm -rf _site
mkdir -p _site/docs
cp -R site/. _site/

# The docs, as markdown, at stable lowercase paths (llms.txt links to them).
cp README.md _site/docs/readme.md
cp docs/guide.md _site/docs/guide.md
cp docs/agents.md _site/docs/agents.md
cp docs/parity.md _site/docs/parity.md
cp docs/DESIGN.md _site/docs/design.md
{
  echo "# Culprit SQL Studio: full documentation"
  echo
  echo "Source: https://github.com/cristipufu/culprit-sql-studio · Website: https://culpritsql.com"
  for f in README.md docs/guide.md docs/agents.md docs/parity.md; do
    echo
    echo "---"
    echo
    cat "$f"
  done
} > _site/llms-full.txt

lastmod=$(git log -1 --format=%cs -- site docs README.md 2>/dev/null || date -u +%F)
sed "s/__LASTMOD__/${lastmod:-$(date -u +%F)}/" site/sitemap.xml > _site/sitemap.xml

echo "built _site/ ($(du -sh _site | cut -f1))"
if [[ "${1:-}" == "serve" ]]; then
  python3 -m http.server 4321 --directory _site
fi

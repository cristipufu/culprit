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

lastmod=$(git log -1 --format=%cs -- site docs README.md 2>/dev/null || date -u +%F)
sed "s/__LASTMOD__/${lastmod:-$(date -u +%F)}/" site/sitemap.xml > _site/sitemap.xml

# Publish the parity catalog's provider docs, rebase Markdown links, and finish assets/CSP (also runs in Pages CI).
python3 dev/site-build.py

echo "built _site/ ($(du -sh _site | cut -f1))"
if [[ "${1:-}" == "serve" ]]; then
  python3 -m http.server 4321 --directory _site
fi

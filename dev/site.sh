#!/usr/bin/env bash
# Assemble the culpritsql.com site into _site/ (the page plus the README's screenshots) and, with `serve`, preview it.
#
#   dev/site.sh          # build _site/
#   dev/site.sh serve    # build and serve on http://localhost:4321
set -euo pipefail
cd "$(dirname "$0")/.."
rm -rf _site
mkdir -p _site/images
cp -R site/. _site/
cp docs/images/*.png _site/images/
echo "built _site/ ($(du -sh _site | cut -f1))"
if [[ "${1:-}" == "serve" ]]; then
  python3 -m http.server 4321 --directory _site
fi

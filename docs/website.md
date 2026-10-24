# Presentation website

`site/` is a static website. `bash dev/site.sh` builds `_site/`; add `serve` to preview on port 4321.
The Website workflow publishes that directory to GitHub Pages. HTTPS enforcement is enabled for culpritsql.com.

The build adds an enforced Content Security Policy to each HTML page, with hashes for its structured data and
explicit origins for the GitHub star count and Plausible analytics. Executable scripts and styles are external;
inline handlers and arbitrary third-party scripts are not allowed. It also versions asset URLs by their contents,
including the font referenced by both CSS and its preload, while keeping stable paths available to cached pages.
The version and check count come from the workspace metadata and exported checks.

The provider catalog in `docs/parity.md` is the documentation entry point. Its ordered provider links drive the
website cards, the published `/docs/providers/` pages and inclusion in `llms-full.txt`. All provider pages use the
same seven sections. The build rebases links between published Markdown pages and sends source/fixture links to
the repository; it rejects missing local documentation targets. The README and `llms.txt` link to the common index.

Screenshots keep the original WebP files and their full recorded quality, including light, dark and high-density
variants. Their declared sizes follow the layout; below-the-fold images are lazy, while the hero has high fetch priority.
The font is already preloaded. `llms.txt` and `llms-full.txt` are plain-text resources; an HTML language test on
the browser's generated text viewer does not establish a missing language attribute in those files.

## Hosting limits

GitHub Pages controls HTTP response headers and currently serves these assets with `Cache-Control: max-age=600`.
An HTML meta tag or a `_headers` file cannot turn that into a different response header on this host. Content
versions prevent stale asset reuse after changes; they do not provide a longer cache lifetime.

To finish the header recommendations, configure an edge proxy or a host with response-header support:

- Send `X-Content-Type-Options: nosniff` and `X-Frame-Options: DENY`.
- Send `Content-Security-Policy: frame-ancestors 'none'` to prevent framing. This directive is ignored in meta CSP;
  the page policy restricts resource loading but does not claim to prevent another site from framing it.
- Roll out HSTS only after verifying HTTPS on every affected hostname. `includeSubDomains` commits all subdomains
  to HTTPS; adding `preload` alone does not submit the domain to a browser preload list. Do not enable either
  blindly to satisfy a score.
- For year-long immutable caching, first serve assets at immutable content-hashed paths and retain old versions
  for cached HTML. Keep HTML short-lived. The current stable asset paths with query versions are not a promise
  of immutable bytes and should not receive a blanket year-long caching rule.

Verify the actual deployed response after any hosting change, rather than treating a configuration file as proof:

```sh
curl -I https://culpritsql.com/
curl -I https://culpritsql.com/llms.txt
curl -I https://culpritsql.com/styles.css
```

References: [GitHub Pages HTTPS](https://docs.github.com/en/pages/getting-started-with-github-pages/securing-your-github-pages-site-with-https),
[CSP delivery and limits](https://developer.mozilla.org/en-US/docs/Web/HTTP/Guides/CSP),
[frame-ancestors](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Content-Security-Policy/frame-ancestors),
[HSTS](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Strict-Transport-Security),
and [WCAG target size](https://www.w3.org/WAI/WCAG22/Understanding/target-size-minimum).

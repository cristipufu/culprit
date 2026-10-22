// Culprit SQL Studio — culpritsql.com. Points the main download button at the visitor's computer, shows the star
// count, switches the agent setup tabs and wires the copy buttons. The page works without it.

(() => {
  // Deferred scripts run in document order: queue initialization before Plausible loads, without inline JavaScript.
  window.plausible = window.plausible || function () { (window.plausible.q = window.plausible.q || []).push(arguments); };
  window.plausible.init = window.plausible.init || function (options) { window.plausible.o = options || {}; };
  window.plausible.init();

  const RELEASE = "https://github.com/cristipufu/culprit-sql-studio/releases/latest/download/";
  // Fixed icons, defined here; nothing from the network or the URL is ever inserted as markup.
  const PLATFORMS = {
    mac: {
      label: "Download for macOS",
      file: "Culprit-SQL-Studio-macOS.dmg",
      icon: '<svg viewBox="0 0 24 24"><path fill="currentColor" d="M16.4 12.6c0-2.3 1.9-3.4 2-3.5-1.1-1.6-2.8-1.8-3.4-1.8-1.4-.2-2.8.8-3.5.8s-1.8-.8-3-.8C7 7.3 5.6 8.2 4.8 9.6c-1.7 2.9-.4 7.2 1.2 9.6.8 1.2 1.8 2.5 3 2.4 1.2 0 1.7-.8 3.1-.8s1.9.8 3.1.8c1.3 0 2.1-1.2 2.9-2.3.9-1.3 1.3-2.6 1.3-2.7-.1 0-2.5-1-2.5-3.9ZM14.1 5.8c.6-.8 1.1-1.9 1-3-.9 0-2.1.6-2.7 1.4-.6.7-1.1 1.8-1 2.9 1 .1 2.1-.5 2.7-1.3Z"/></svg>',
      meta: 'macOS 11+ (Apple silicon &amp; Intel · signed &amp; notarized) · <a href="#download">Windows, Linux &amp; CLI ↓</a> · <a href="#answer-title">Explore sample diagnoses ↓</a>',
    },
    windows: {
      label: "Download for Windows",
      file: "Culprit-SQL-Studio-Windows-x64-setup.exe",
      icon: '<svg viewBox="0 0 24 24"><path fill="currentColor" d="M3 5.5 10.5 4.4v7.2H3V5.5Zm0 13 7.5 1.1v-7.1H3v6Zm8.4 1.2L21 21v-8.5h-9.6v7.2Zm0-15.4v7.3H21V3l-9.6 1.3Z"/></svg>',
      meta: 'Windows 10 &amp; 11 (x64 · not code-signed yet: SmartScreen may ask once) · <a href="#download">macOS, Linux &amp; CLI ↓</a> · <a href="#answer-title">Explore sample diagnoses ↓</a>',
    },
    linux: {
      label: "Download for Linux",
      file: "Culprit-SQL-Studio-Linux-x86_64.AppImage",
      icon: '<svg viewBox="0 0 24 24"><path fill="currentColor" d="M4 5h16a2 2 0 0 1 2 2v10a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V7a2 2 0 0 1 2-2Zm2.3 4.3L9.6 12l-3.3 2.7 1.3 1.5L12.7 12 7.6 7.8 6.3 9.3ZM12 15h6v-2h-6v2Z"/></svg>',
      meta: 'Linux x86_64 (AppImage, or <a href="#download">.deb below</a>) · <a href="#download">macOS, Windows &amp; CLI ↓</a> · <a href="#answer-title">Explore sample diagnoses ↓</a>',
    },
  };

  // Phones and tablets can't run a desktop app (an iPad reports itself as a Mac with a touch screen), so they keep the
  // generic button that scrolls to every platform.
  const ua = navigator.userAgent || "";
  const platform = (navigator.userAgentData && navigator.userAgentData.platform) || navigator.platform || "";
  const mobile =
    (navigator.userAgentData && navigator.userAgentData.mobile) ||
    /iphone|ipad|ipod|android|mobile/i.test(ua) ||
    (/mac/i.test(platform) && navigator.maxTouchPoints > 1);
  const os = mobile ? null : /mac/i.test(platform) ? "mac" : /win/i.test(platform) ? "windows" : /linux|x11/i.test(platform) ? "linux" : null;

  if (os) {
    const p = PLATFORMS[os];
    const button = document.querySelector("[data-os-download]");
    if (button) {
      button.href = RELEASE + p.file;
      button.querySelector("[data-os-label]").textContent = p.label;
      button.querySelector(".os-icon").innerHTML = p.icon;
    }
    const meta = document.querySelector("[data-os-meta]");
    if (meta) meta.innerHTML = p.meta;
    document.querySelector(`.dl[data-os="${os}"]`)?.classList.add("current");
    document.querySelector("[data-other-os]")?.removeAttribute("hidden");
  }

  // The star count, when GitHub answers; nothing is shown otherwise.
  const stars = document.querySelector("[data-stars]");
  if (stars) {
    fetch("https://api.github.com/repos/cristipufu/culprit-sql-studio", { headers: { Accept: "application/vnd.github+json" } })
      .then((r) => (r.ok ? r.json() : null))
      .then((repo) => {
        const n = repo && Number(repo.stargazers_count);
        // A handful of stars reads as "nobody uses this": show the count once it says something.
        if (!Number.isFinite(n) || n < 25) return;
        stars.textContent = `★ ${n >= 1000 ? (n / 1000).toFixed(1) + "k" : n}`;
        stars.hidden = false;
      })
      .catch(() => {});
  }

  // Accessible tablists (sample diagnoses and agent setup).
  const wireTabs = (selector) => {
    const tabs = [...document.querySelectorAll(selector)];
    const select = (tab) => {
      for (const t of tabs) {
        const on = t === tab;
        t.setAttribute("aria-selected", String(on));
        t.tabIndex = on ? 0 : -1;
        document.getElementById(t.getAttribute("aria-controls")).hidden = !on;
      }
    };
    for (const tab of tabs) {
      tab.addEventListener("click", () => select(tab));
      tab.addEventListener("keydown", (e) => {
        const i = tabs.indexOf(tab);
        const next = e.key === "ArrowRight" ? tabs[(i + 1) % tabs.length] : e.key === "ArrowLeft" ? tabs[(i - 1 + tabs.length) % tabs.length] : null;
        if (next) {
          e.preventDefault();
          select(next);
          next.focus();
        }
      });
    }
  };
  wireTabs('[role="tab"][data-sample]');
  wireTabs('[role="tab"][data-tab]');

  // Copy buttons copy the visible commands; the result is announced for screen readers.
  const announce = document.querySelector("[data-announce]");
  const source = (target) =>
    target === "setup" ? document.querySelector("pre[data-setup]:not([hidden])") : document.querySelector("pre[data-cli]");
  const commands = (pre) =>
    pre.textContent
      .split("\n")
      .map((line) => line.replace(/^\$ /, "").replace(/\s+#.*$/, ""))
      .filter((line) => line.trim() && !line.trim().startsWith("#") && !line.trim().startsWith("//"))
      .join("\n");
  for (const button of document.querySelectorAll("[data-copy-target]")) {
    button.addEventListener("click", async () => {
      const pre = source(button.dataset.copyTarget);
      if (!pre) return;
      try {
        await navigator.clipboard.writeText(commands(pre));
        button.textContent = "Copied";
        if (announce) announce.textContent = "Copied to the clipboard";
      } catch {
        // No clipboard access: select the text so ⌘C / Ctrl+C works.
        const range = document.createRange();
        range.selectNodeContents(pre);
        const selection = getSelection();
        selection.removeAllRanges();
        selection.addRange(range);
        button.textContent = "Selected";
      }
      setTimeout(() => (button.textContent = "Copy"), 1600);
    });
  }
})();

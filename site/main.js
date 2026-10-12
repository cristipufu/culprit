// Culprit SQL Studio — culpritsql.com. Points the main download button at the visitor's platform and wires the copy
// buttons. Everything works without it: the button falls back to the download section.

(() => {
  const RELEASE = "https://github.com/cristipufu/culprit-sql-studio/releases/latest/download/";
  const PLATFORMS = {
    mac: {
      label: "Download for macOS",
      file: "Culprit-SQL-Studio-macOS.dmg",
      icon: '<svg viewBox="0 0 24 24"><path fill="currentColor" d="M16.4 12.6c0-2.3 1.9-3.4 2-3.5-1.1-1.6-2.8-1.8-3.4-1.8-1.4-.2-2.8.8-3.5.8s-1.8-.8-3-.8C7 7.3 5.6 8.2 4.8 9.6c-1.7 2.9-.4 7.2 1.2 9.6.8 1.2 1.8 2.5 3 2.4 1.2 0 1.7-.8 3.1-.8s1.9.8 3.1.8c1.3 0 2.1-1.2 2.9-2.3.9-1.3 1.3-2.6 1.3-2.7-.1 0-2.5-1-2.5-3.9ZM14.1 5.8c.6-.8 1.1-1.9 1-3-.9 0-2.1.6-2.7 1.4-.6.7-1.1 1.8-1 2.9 1 .1 2.1-.5 2.7-1.3Z"/></svg>',
    },
    windows: {
      label: "Download for Windows",
      file: "Culprit-SQL-Studio-Windows-x64-setup.exe",
      icon: '<svg viewBox="0 0 24 24"><path fill="currentColor" d="M3 5.5 10.5 4.4v7.2H3V5.5Zm0 13 7.5 1.1v-7.1H3v6Zm8.4 1.2L21 21v-8.5h-9.6v7.2Zm0-15.4v7.3H21V3l-9.6 1.3Z"/></svg>',
    },
    linux: {
      label: "Download for Linux",
      file: "Culprit-SQL-Studio-Linux-x86_64.AppImage",
      icon: "",
    },
  };

  const ua = (navigator.userAgentData && navigator.userAgentData.platform) || navigator.platform || navigator.userAgent || "";
  const os = /mac|iphone|ipad/i.test(ua) ? "mac" : /win/i.test(ua) ? "windows" : /linux|x11/i.test(ua) ? "linux" : null;

  if (os) {
    const p = PLATFORMS[os];
    const button = document.querySelector("[data-os-download]");
    if (button) {
      button.href = RELEASE + p.file;
      button.querySelector("[data-os-label]").textContent = p.label;
      button.querySelector(".os-icon").innerHTML = p.icon;
    }
    const card = document.querySelector(`.dl[data-os="${os}"]`);
    if (card) card.classList.add("current");
  }

  for (const button of document.querySelectorAll("[data-copy]")) {
    button.addEventListener("click", async () => {
      try {
        await navigator.clipboard.writeText(button.dataset.copy);
        button.textContent = "Copied";
      } catch {
        button.textContent = "Select and copy";
      }
      setTimeout(() => (button.textContent = "Copy"), 1600);
    });
  }
})();

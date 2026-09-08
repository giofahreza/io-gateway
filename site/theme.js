(() => {
  const storageKey = "io-gateway.theme";
  const root = document.documentElement;
  const prefersDark = window.matchMedia?.("(prefers-color-scheme: dark)");

  function readStoredTheme() {
    try {
      const value = window.localStorage.getItem(storageKey);
      return value === "light" || value === "dark" ? value : null;
    } catch {
      return null;
    }
  }

  function writeStoredTheme(theme) {
    try {
      window.localStorage.setItem(storageKey, theme);
    } catch {
      // Theme persistence is optional when browser storage is unavailable.
    }
  }

  function resolvedTheme() {
    if (root.dataset.theme === "light" || root.dataset.theme === "dark") {
      return root.dataset.theme;
    }

    return prefersDark?.matches ? "dark" : "light";
  }

  function updateThemeColor() {
    const meta = document.querySelector('meta[name="theme-color"]');
    if (meta) {
      const light = meta.dataset.themeColorLight || "#ebe8df";
      const dark = meta.dataset.themeColorDark || "#1b211c";
      meta.setAttribute("content", resolvedTheme() === "dark" ? dark : light);
    }
  }

  function updateControls() {
    const activeTheme = resolvedTheme();
    const nextTheme = activeTheme === "dark" ? "light" : "dark";

    document.querySelectorAll("[data-theme-toggle]").forEach((button) => {
      button.setAttribute("aria-pressed", String(activeTheme === "dark"));
      button.setAttribute("aria-label", `Switch to ${nextTheme} theme`);

      const label = button.querySelector("[data-theme-label]");
      if (label) label.textContent = activeTheme === "dark" ? "Dark" : "Light";
    });
  }

  function refresh() {
    updateThemeColor();
    updateControls();
  }

  function installControls() {
    document.querySelectorAll("[data-theme-toggle]").forEach((button) => {
      if (button.dataset.themeBound === "true") return;

      button.dataset.themeBound = "true";
      button.addEventListener("click", () => {
        const nextTheme = resolvedTheme() === "dark" ? "light" : "dark";
        root.dataset.theme = nextTheme;
        writeStoredTheme(nextTheme);
        refresh();
      });
    });

    refresh();
  }

  const savedTheme = readStoredTheme();
  if (savedTheme) root.dataset.theme = savedTheme;
  updateThemeColor();

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", installControls, { once: true });
  } else {
    installControls();
  }

  const handleSystemThemeChange = () => {
    if (!root.dataset.theme) refresh();
  };

  if (prefersDark?.addEventListener) {
    prefersDark.addEventListener("change", handleSystemThemeChange);
  } else if (prefersDark?.addListener) {
    prefersDark.addListener(handleSystemThemeChange);
  }
})();

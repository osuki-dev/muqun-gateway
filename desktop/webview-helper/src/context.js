// Executed at document start in the plugin origin's top-level frame.
(origin, value) => {
  if (self !== top || location.origin !== origin) return;
  const theme = Object.freeze({ ...value.theme, colors: Object.freeze({ ...value.theme.colors }) });
  const context = Object.freeze({
    ...value,
    theme,
    ...(value.workspace ? { workspace: Object.freeze({ ...value.workspace }) } : {}),
  });
  const updating = Object.hasOwn(window, "muqunDesktop");
  Object.defineProperty(window, "muqunDesktop", {
    value: Object.freeze({ context, ...(window.__muqunAppendToDraft ? {appendToDraft: window.__muqunAppendToDraft} : {}) }),
    configurable: true,
  });
  function updateStyles() {
    const root = document.documentElement;
    if (!root) return;
    const current = window.muqunDesktop.context;
    root.lang = current.locale;
    root.style.colorScheme = current.theme.colorScheme;
    for (const [name, color] of Object.entries(current.theme.colors)) {
      root.style.setProperty("--muqun-" + name.replace(/[A-Z]/g, c => "-" + c.toLowerCase()), color);
    }
  }
  if (document.documentElement) updateStyles();
  else document.addEventListener("DOMContentLoaded", updateStyles, { once: true });
  if (updating) window.dispatchEvent(new CustomEvent("muqun:contextchange", { detail: context }));
}

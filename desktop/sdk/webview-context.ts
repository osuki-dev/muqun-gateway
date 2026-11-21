import type { PluginContext, PluginThemeContext } from "./index.ts";

export type WebViewThemeColors = Readonly<Record<
  "background" | "surface" | "surfaceRaised" | "border" | "text" | "textMuted" | "accent" | "onAccent" | "danger",
  string
>>;

export type WebViewContext = Readonly<Pick<PluginContext, "locale" | "platform" | "desktopVersion" | "workspace"> & {
  theme: PluginThemeContext & Readonly<{ colors: WebViewThemeColors }>;
}>;

declare global {
  interface Window {
    readonly muqunDesktop?: Readonly<{ context: WebViewContext; appendToDraft?: (text: string) => Promise<void> }>;
  }
}

function hostWindow(): Window | undefined {
  if (typeof window === "undefined") return undefined;
  if (window.muqunDesktop) return window;
  // Same-origin subframes can consume the owning page's context.
  try { return window.top?.muqunDesktop ? window.top : undefined; }
  catch { return undefined; }
}

/** The current snapshot in a Muqun plugin page, or undefined in a standalone browser. */
export function getWebViewContext(): WebViewContext | undefined {
  return hostWindow()?.muqunDesktop?.context;
}

/** Subscribe to subsequent Desktop theme, locale, and workspace changes. */
export function onWebViewContextChange(listener: (value: WebViewContext) => void): () => void {
  const owner = hostWindow();
  if (!owner) return () => {};
  const notify = () => {
    const context = getWebViewContext();
    if (context) listener(context);
  };
  owner.addEventListener("muqun:contextchange", notify);
  return () => owner.removeEventListener("muqun:contextchange", notify);
}


/** Append to the opening conversation after a user gesture. Only granted in an owned service. */
export async function appendToDraft(text: string): Promise<void> {
  const append = hostWindow()?.muqunDesktop?.appendToDraft;
  if (!append) throw new Error("Desktop did not grant access to the conversation draft.");
  await append(text);
}

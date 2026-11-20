export { appendToDraft, getWebViewContext, onWebViewContextChange } from "./webview-context.ts";
export type { WebViewContext, WebViewThemeColors } from "./webview-context.ts";

/** Native view descriptions for the local one-shot Bun protocol. */
export type Tone = "muted" | "accent" | "info" | "success" | "warning";
export type LayoutProps = { key?: string; width?: number; height?: number | "fill"; gap?: number; wrap?: boolean; align?: "start" | "center" | "end"; justify?: "start" | "center" | "end"; surface?: "subtle"; tone?: Tone; paddingX?: number; paddingY?: number; grow?: boolean };
export type ContainerProps = LayoutProps & { children: readonly ViewNode[] };
export type TextProps = { text: string; font?: "body" | "ui" | "small"; tone?: Tone; width?: number; height?: number; rotate?: number; truncate?: boolean; grow?: boolean };
export type ButtonProps = { label: string; action: string; variant?: "outline" | "ghost"; size?: "regular" | "small"; ariaLabel?: string; width?: number; height?: number };

export const Column = (props: ContainerProps) => ({ ...props, type: "column" as const });
export const Row = (props: ContainerProps) => ({ ...props, type: "row" as const });
export type ScrollAreaProps = { key: string; axis: "horizontal" | "vertical"; height?: number | "fill"; children: readonly ViewNode[] };
export const ScrollArea = (props: ScrollAreaProps) => ({ ...props, type: "scrollarea" as const });
export type CardProps = { key: string; label: string; action?: string; variant?: "raised" | "elevated" | "ghost"; tone?: Tone; children: readonly ViewNode[] };
export const Card = (props: CardProps) => ({ ...props, type: "card" as const });
/** Inline PNG/JPEG/WebP thumbnail; URLs are never fetched by Desktop. */
export type AvatarProps = { name: string; image?: string; size?: number };
export const Avatar = (props: AvatarProps) => ({ ...props, type: "avatar" as const });
/** Original aspect ratio is preserved. Pass a media:filename written for this view
 * in MUQUN_PLUGIN_MEDIA_DIR, or an inline PNG/JPEG/WebP data URI. */
export type ImageProps = { src: string; alt: string; width?: number; height?: number };
export const Image = (props: ImageProps) => ({ ...props, type: "image" as const });
export type BadgeProps = { text: string; tone?: Tone };
export const Badge = (props: BadgeProps) => ({ ...props, type: "badge" as const });
export const Text = (props: TextProps) => ({ ...props, type: "text" as const });
export const Heading = ({ text }: TextProps) => ({ type: "heading" as const, text });
export type MarkdownProps = { text: string; anchor?: string };
export const Markdown = (props: MarkdownProps) => ({ ...props, type: "markdown" as const });
export const Button = (props: ButtonProps) => ({ ...props, type: "button" as const });

export type SelectProps = { key: string; label: string; options: readonly { label: string; action: string; selected?: boolean }[] };
export const Select = (props: SelectProps) => ({ ...props, type: "select" as const });

export type TextFieldProps = { name: string; label: string; value?: string; multiline?: boolean; placeholder?: string };
export const TextField = (props: TextFieldProps) => ({ ...props, type: "textfield" as const });
export type TerminalButtonProps = { label: string; entry: string };
export const TerminalButton = (props: TerminalButtonProps) => ({ ...props, type: "terminalbutton" as const });
export type FormProps = { action: string; submitLabel: string; inline?: boolean; showLabels?: boolean; children: readonly ReturnType<typeof TextField>[] };
export const Form = (props: FormProps) => ({ ...props, type: "form" as const });

/** A root view backed by an instance-owned loopback service. */
export type WebViewProps = { entry: string };
export const WebView = ({ entry }: WebViewProps) => ({ type: "webview" as const, entry });

export type ViewNode =
  | ReturnType<typeof Select>
  | ReturnType<typeof TerminalButton>
  | ReturnType<typeof Form>
  | ({ type: "scrollarea" } & ScrollAreaProps)
  | ({ type: "card" } & CardProps)
  | ReturnType<typeof Avatar>
  | ReturnType<typeof Image>
  | ReturnType<typeof Badge>
  | ({ type: "column" | "row" } & ContainerProps)
  | ReturnType<typeof Markdown>
  | ({ type: "text" | "heading" } & TextProps)
  | ({ type: "button" } & ButtonProps);
export type ViewResponse = { version: 1; view: ViewNode | ReturnType<typeof WebView> };

/** Desktop settings for this invocation; native views inherit colors from the host. */
export type PluginThemeContext = Readonly<{
  id: string;
  colorScheme: "light" | "dark";
  preference: "system" | "light" | "dark";
  reducedMotion: boolean;
  reducedTransparency: boolean;
  textScale: number;
}>;
export type PluginContext = Readonly<{
  locale: string;
  workspace?: Readonly<{ path: string }>;
  capabilities: Readonly<{ webviewEntry: boolean; nativeLayout: boolean; nativePresentation: boolean; nativeImages: boolean; fillLayout: boolean; markdownAnchors: boolean; nativeSelect: boolean }>;
  theme: PluginThemeContext;
  form: Readonly<Record<string, string>>;
  platform: "linux" | "macos" | "windows";
  desktopVersion: string;
}>;
export type Plugin = (action?: string, context?: Omit<PluginContext, "form" | "capabilities"> & Partial<Pick<PluginContext, "form" | "capabilities">>) => Promise<ViewResponse>;

export function definePlugin(render: (action: string, context: PluginContext) => ViewResponse["view"] | Promise<ViewResponse["view"]>): Plugin {
  return async (action = "open", context) => ({ version: 1, view: await render(action, snapshot(context)) });
}

/** Run once from the Bun entry point; keep render diagnostics out of the response. */
export async function runPlugin(plugin: Plugin): Promise<void> {
  const { log, info, debug } = console;
  const diagnostic = console.error.bind(console);
  console.log = console.info = console.debug = diagnostic;
  try {
    let context: unknown;
    try { context = JSON.parse(process.env.MUQUN_PLUGIN_CONTEXT || "null"); }
    catch { context = undefined; }
    const response = await plugin(process.env.MUQUN_PLUGIN_ACTION, snapshot(context, process.env.MUQUN_PLUGIN_LOCALE));
    process.stdout.write(JSON.stringify(response) + "\n");
  } finally {
    console.log = log;
    console.info = info;
    console.debug = debug;
  }
}

/** A service entry runs once per mounted view. Register owned resources before awaiting startup. */
export interface WebViewServiceContext {
  readonly signal: AbortSignal;
  onClose(cleanup: () => void | Promise<void>): void;
}

export class WebViewServiceError extends Error {
  constructor(readonly code: "runtime-unavailable" | "backend-unavailable" | "invalid-endpoint") {
    super(code);
  }
}

export async function runWebViewService(
  start: (context: WebViewServiceContext) => URL | string | Promise<URL | string>,
): Promise<void> {
  const lifetime = new AbortController();
  const cleanups: Array<() => void | Promise<void>> = [];
  const { stdin, stdout } = process;
  const close = () => lifetime.abort();
  const closed = new Promise<void>((resolve) => {
    lifetime.signal.addEventListener("abort", () => resolve(), { once: true });
  });
  const { log, info, debug } = console;
  console.log = console.info = console.debug = console.error.bind(console);
  stdin.once("end", close).once("close", close).once("error", close);
  stdin.resume();
  if (stdin.readableEnded || stdin.destroyed) close();
  try {
    if (lifetime.signal.aborted) return;
    const endpoint = await Promise.race([closed, start({
      signal: lifetime.signal,
      onClose(cleanup) {
        if (lifetime.signal.aborted) void Promise.resolve().then(cleanup).catch(() => {});
        else cleanups.push(cleanup);
      },
    })]);
    if (lifetime.signal.aborted) return;
    let url: URL;
    try {
      url = new URL(String(endpoint));
      if (url.protocol !== "http:" || url.hostname !== "127.0.0.1" || !url.port ||
          url.username || url.password || Buffer.byteLength(url.href) + 1 > 8192) throw new Error();
    } catch {
      throw new WebViewServiceError("invalid-endpoint");
    }
    stdout.write(url.href + "\n");
    await closed;
  } catch (error) {
    if (!lifetime.signal.aborted) {
      const allowed = ["runtime-unavailable", "backend-unavailable", "invalid-endpoint"];
      const code = error instanceof WebViewServiceError && allowed.includes(error.code) ? error.code : "startup-failed";
      stdout.write("!muqun:webview:error:" + code + "\n");
      process.exitCode = 1;
    }
  } finally {
    close();
    stdin.off("end", close).off("close", close).off("error", close);
    stdin.pause();
    for (const cleanup of cleanups.reverse()) {
      try { await cleanup(); } catch { process.exitCode = 1; }
    }
    Object.assign(console, { log, info, debug });
  }
}

function record(value: unknown): Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value) ? value as Record<string, unknown> : {};
}

function text(value: unknown, fallback: string): string {
  return typeof value === "string" && value.trim().length > 0 && value.length <= 128 ? value : fallback;
}

function snapshot(value: unknown, fallbackLocale = "en"): PluginContext {
  const context = record(value);
  const theme = record(context.theme);
  const path = record(context.workspace).path;
  const platform = process.platform === "darwin" ? "macos" : process.platform === "win32" ? "windows" : "linux";
  return Object.freeze({
    locale: text(context.locale, text(fallbackLocale, "en")),
    ...(typeof path === "string" && path.length > 0 ? { workspace: Object.freeze({ path }) } : {}),
    capabilities: Object.freeze({ webviewEntry: record(context.capabilities).webviewEntry === true, nativeLayout: record(context.capabilities).nativeLayout === true, nativePresentation: record(context.capabilities).nativePresentation === true, nativeImages: record(context.capabilities).nativeImages === true, fillLayout: record(context.capabilities).fillLayout === true, markdownAnchors: record(context.capabilities).markdownAnchors === true, nativeSelect: record(context.capabilities).nativeSelect === true }),
    form: Object.freeze(Object.fromEntries(Object.entries(record(context.form)).filter(([, value]) => typeof value === "string"))) as Readonly<Record<string, string>>,
    theme: Object.freeze({
      id: text(theme.id, "default"),
      colorScheme: theme.colorScheme === "light" ? "light" : "dark",
      preference: theme.preference === "light" || theme.preference === "dark" ? theme.preference : "system",
      reducedMotion: theme.reducedMotion === true,
      reducedTransparency: theme.reducedTransparency === true,
      textScale: typeof theme.textScale === "number" && Number.isFinite(theme.textScale) && theme.textScale > 0 ? theme.textScale : 1,
    }),
    platform: context.platform === "linux" || context.platform === "macos" || context.platform === "windows" ? context.platform : platform,
    desktopVersion: text(context.desktopVersion, "unknown"),
  });
}

export type PickPathOptions = Readonly<{
  kind: "file" | "directory";
  startDirectory?: string;
  title?: string;
}>;

/** Open the native picker from a user action. Cancellation resolves to null. */
export async function pickPath({ kind, startDirectory, title }: PickPathOptions): Promise<string | null> {
  const executable = process.env.MUQUN_DESKTOP_EXECUTABLE;
  if (!executable) throw new Error("The Desktop file picker is unavailable.");
  const args = [executable, "--plugin-pick-path", kind];
  if (startDirectory !== undefined) args.push("--start-directory", startDirectory);
  if (title !== undefined) args.push("--title", title);
  const child = Bun.spawn(args, { stdin: "ignore", stdout: "pipe", stderr: "ignore" });
  const [status, raw] = await Promise.all([child.exited, new Response(child.stdout).text()]);
  if (status !== 0) throw new Error("The native file picker could not open.");
  const result: unknown = JSON.parse(raw);
  if (result !== null && (typeof result !== "string" || !result.length)) {
    throw new Error("The native file picker returned an invalid selection.");
  }
  return result;
}

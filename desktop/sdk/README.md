# Muqun Desktop SDK · local preview

Build a native plugin in TypeScript. Import the components and plugin API directly from `@muqun/desktop`. Desktop renders the tree with Rune and supplies the current theme.

## Link a local plugin

Run `bun link` in `desktop/sdk`, then `bun link @muqun/desktop` in your plugin directory. Configure the plugin in its root `package.json`. Desktop uses `name` for identity, `version` for its version, and `main` for the Bun entry point. The `muqun` object opts into discovery and can specify `displayName`, `icon`, and `surfaces`.

```json
{
  "name": "hello-native",
  "version": "0.1.0",
  "main": "main.ts",
  "description": "A native greeting panel.",
  "keywords": ["greeting", "example"],
  "files": ["main.ts"],
  "muqun": { "displayName": "Hello Native" },
  "peerDependencies": { "@muqun/desktop": ">=0.0.2-dev <0.1.0" },
  "devDependencies": { "@muqun/desktop": "link:@muqun/desktop" }
}
```

In `main.ts`:

```ts
import { Button, Column, Heading, definePlugin, runPlugin } from "@muqun/desktop";

const render = definePlugin(action => Column({ children: [
  Heading({ text: action === "hello" ? "Hello!" : "Welcome" }),
  Button({ label: "Say hello", action: "hello" }),
] }));

await runPlugin(render);
```

Link the plugin directory into a Desktop plugin search directory. For example, on Linux run the following in your plugin directory:

```sh
mkdir -p ~/.local/share/muqun-desktop/plugins
ln -s "$PWD" ~/.local/share/muqun-desktop/plugins/hello-native
```

Restart Desktop for discovery and open the plugin. Opening a new pane invokes its initial view. Selecting an existing dock tab preserves its view and draft. Each button invokes the entry point again, so changes to linked TypeScript take effect on the next action.

## Package and install

### Background notifications

Packages can set `muqun.notificationEntry` to a package-relative Bun script.
After the plugin first opens, Desktop runs this entry from the package directory
every 30 seconds while notifications are enabled, including after the pane
closes. The script prints one JSON object with a `notices` array. Each item has
a `title`, `body`, and plugin `action` string. Selecting the notification
opens the plugin and dispatches that action. Return an empty array for a quiet
poll and track delivered events across restarts to avoid duplicate alerts.

### Metadata in Settings

Use standard npm `description`, `keywords` and `repository` fields. Settings
displays the description and keywords as tags, and includes them in search.
Authors only need `keywords` for tags; do not repeat them under `muqun`.

Set `repository` to the actual source repository, as a URL string or an object
with `type`, `url` and optional `directory` for a package inside a monorepo.
Desktop accepts HTTPS repository URLs (including self-hosted Gitea and GitLab),
`git+https` URLs and GitHub `owner/repo` or `github:owner/repo` shorthand. The
Source opens the package directory for a GitHub monorepo, using
`tree/HEAD/<directory>`. Other forges open the repository root and retain the
directory as context. Credentials and non-HTTPS schemes are not opened.
Missing or unsupported optional metadata leaves the plugin available and omits
the Source action. Desktop never guesses a repository from a package name.

Plugins may optionally choose an existing Settings category with
`muqun.category`: `core`, `tools`, `diagnostics` or `extensions`. The default is
`extensions`. Legacy `plugin.json` keeps its existing top-level `category` and
can also provide `keywords` and `repository`. Built-in panels retain their
existing categories. See the [npm package metadata reference](https://docs.npmjs.com/cli/v11/configuring-npm/package-json/).

List the entry point, its imports and runtime assets in `files`. Bun includes the package manifest automatically. Ship TypeScript directly, or point `main` at a JavaScript bundle and include its output directory in `files`. Desktop provides the SDK peer to managed plugins. Keep build tools in `devDependencies`.

In **Settings → Plugins**, enter a single package source accepted by Bun and choose **Install**. Desktop passes the source to Bun and checks the resolved package for a valid plugin manifest before activating it. To share a local build, run `bun pm pack` and select the resulting archive with the file picker. If Bun is missing, click **Install Bun**, wait for the runtime to become available, then install the plugin. Opening Settings only checks for Bun.

Installed packages show their version, **Update** and **Uninstall**. Registry packages update within the installed dependency range. A local archive updates from its original path. To install an archive at a new path, choose it under **Install plugin**. A failed replacement keeps the previous version active.

## Type checking

Run `bun install`, `bun run typecheck` and `bun run test` in the SDK directory to check its source. In a plugin, add `typescript` and `@types/bun` as dev dependencies, enable `strict` and `types: ["bun"]` in your TypeScript configuration, and run `tsc --noEmit` during development. Bun can execute linked TypeScript directly. Static checks run separately.

Import `ViewNode` when writing a reusable component and `ButtonProps`, `TextProps` or `ContainerProps` when extending one of the built-in components. The renderer applies the active Desktop theme. The preview exposes content, composition and button actions. Its layout spacing and control variants are set by Desktop.

### Native layout capability

When `context.capabilities.nativeLayout === true`, the host also supports
`ScrollArea`, `Card`, `Avatar`, and `Badge`. `Row` and `Column` accept optional
`key`, `width`, `height`, `gap`, `wrap` (rows wrap by default), and
`align: "start" | "center" | "end"` for cross-axis child alignment. Omit
`align` to preserve existing defaults, including vertically centered rows. Dimensions
are points: width/height 1–4096, gap 0–64. Stable keys must be unique within
the view and contain 1–128 bytes. Existing descriptions remain protocol v1.

`ScrollArea({ key, axis: "horizontal" | "vertical", height, children })`
creates an independent native viewport, with height 32–2048 (default 520).
For a horizontal rail, give child columns fixed widths and use a `Row` with
`wrap: false`. Nested vertical areas scroll independently.
`Card({ key, label, action?, children })` uses a themed raised surface; an
action makes its entire surface a keyboard-accessible button using the existing
one-shot action protocol. `Badge({ text, tone? })` accepts `muted` (default),
`accent`, `success`, or `warning`.

`Avatar({ name, image?, size? })` shows initials until an image is ready or if
it cannot load. Size is 16–96 points (default 25). `image` is an inline
PNG/JPEG/WebP base64 data URL, at most 32 KiB encoded and 512×512 pixels.
Desktop never fetches a URL from this prop. Plugins should supply small
thumbnails and stay within the existing response size limit. Views are bounded
to 4096 nodes; form fields and keys retain their existing instance scope.

Older hosts omit this capability. To support older SDKs too, import the SDK
as a namespace, check both the capability and the component export, and return
a basic `Column`/`Text`/`Button` fallback when unavailable. Do not send new node
types to an older host. The `0.0.6-dev` preview adds these exports;
no new required manifest feature or protocol version is introduced.

SDK `0.0.7-dev` adds optional presentation fields behind
`context.capabilities.nativePresentation`. Defaults remain unchanged. Containers
can use `justify: "start" | "center" | "end"`, `grow`, `surface: "subtle"`,
`tone`, and `paddingX`/`paddingY` (0–64 points). Text accepts `font: "body" |
"ui" | "small"`, semantic `tone`, bounded width/height, `grow`, `truncate`, and
`rotate` in degrees (−360–360). A rotated label remains one readable string.
Cards offer `variant: "raised" | "elevated" | "ghost"` and semantic `tone`.
Buttons offer `variant: "outline" | "ghost"`, `size: "regular" | "small"`,
`ariaLabel`, and bounded width/height. Tone also accepts `info`.
`Form({ inline: true, showLabels: false, ... })` keeps fields and its explicit
submit action in one row; field labels remain accessible. TextField can supply
an optional `placeholder`. No input automatically submits a form.

## Runtime contract

- Exported components are `Row`, `Column`, `Heading`, `Text`, `Markdown` `Button`, `Form` and `TextField`. Use the exported `ViewNode` type for your own components.
- Call `runPlugin` once from the entry point with the function returned by `definePlugin`. It reads `MUQUN_PLUGIN_ACTION` (default `open`) and writes a single JSON response to stdout. Each response contains `version: 1` and a `view`.
- While rendering, `console.log`, `console.info` and `console.debug` go to stderr; the SDK restores these methods after rendering. Use `console.error` or `console.warn` for diagnostics outside the render callback. Direct stdout writes and logs before `runPlugin` still corrupt the response. Rendering failures reject without emitting a response.
- Keep entry-point execution behind `if (import.meta.main)` when exporting a renderer for reuse or tests. Importing the SDK or a guarded entry point does not run a plugin.
- Desktop sends `open` for the initial view. A button sends its opaque `action` string unchanged; the callback receives that string through the existing action API. There are no startup, shutdown or background callbacks.
- Stderr is private diagnostic output, not a view or a user-facing notification. Avoid logging credentials or private response bodies.
- Timeout or cancellation does not automatically replay an invocation. Treat a started action as potentially executed if its response is lost.
- The host retains the last successful view for the receiving pane. Each action starts a fresh Bun process. Store navigation identifiers in button actions or persist application state separately.
- The local preview uses the existing Bun command limits of 15 seconds and 64 KiB of stdout. Link trusted plugin code only. Bun runs with the current user's access.

## Forms and compatibility

A Form has an action, a submitLabel, and TextField children. Each field has a unique name within the form, a label, an optional initial value and an optional multiline flag. The callback receives the submitted strings in context.form. The host keeps drafts in the originating view until a successful response replaces it or the user closes it. Raw field values are excluded from the diagnostics journal.

Declare required native features in the package's muqun.requires array. This host recognizes "native-view", "forms", "terminal-entry" and (on Linux and macOS) "webview-entry". For forms, require SDK >=0.0.3-dev and list "forms". Discovery rejects an unsupported required capability and ignores unknown optional manifest metadata.

### Embed a local web application

A root `WebView` accepts a single package-relative Bun `entry`. The `runWebViewService` helper requires SDK `>=0.0.5-dev`. Desktop displays the service in the existing plugin tab, tile or page. A separate browser tab can navigate unrelated HTTP(S) websites without receiving the plugin service's private endpoint.

```ts
import { Text, WebView, definePlugin, runPlugin } from "@muqun/desktop";

await runPlugin(definePlugin((_action, context) =>
  context.capabilities.webviewEntry
    ? WebView({ entry: "panel.ts" })
    : Text({ text: "Device preview requires Desktop's embedded WebView." })
));
```

Include `panel.ts` in the package's `files`. A plugin that requires embedding can list `"webview-entry"` in `muqun.requires`. Plugins with native alternatives can check `context.capabilities.webviewEntry`, which is true when a Linux WebView helper is available.

In the service entry, return the private URL and register cleanup before awaiting backend startup:

```ts
import { runWebViewService } from "@muqun/desktop";

await runWebViewService(({ onClose }) => {
  const path = "/" + crypto.randomUUID();
  const server = Bun.serve({
    hostname: "127.0.0.1", port: 0,
    fetch: request => new URL(request.url).pathname === path
      ? new Response("Hello from this view")
      : new Response(null, { status: 404 }),
  });
  onClose(() => server.stop(true));
  return new URL(path, server.url);
});
```

The startup callback also receives an `AbortSignal` for cancellable requests. Registered cleanup runs on close and on startup failure. The helper reserves stdout for the host handshake. To give the user an actionable startup error, throw `WebViewServiceError("runtime-unavailable")` or `WebViewServiceError("backend-unavailable")`. Other exceptions produce a generic startup error.

Desktop starts one service per mounted view instance. The Bun entry writes its session URL as the first newline-terminated line on stdout, within 45 seconds and 8 KiB. Bind to `127.0.0.1` on a dynamically assigned port and return an HTTP URL with that origin. Use an unguessable session path for private content. Desktop passes the URL to the renderer over a private pipe. Service stdout, stderr and session URLs are excluded from diagnostics.

Re-rendering or switching away from a tab preserves its service and navigation. A separately opened instance owns its own service. Closing the view, disabling the plugin or shutting down Desktop closes the service's stdin. Handle EOF by stopping your HTTP listener and any child processes. The host allows two seconds for shutdown before terminating the service.

TerminalButton takes a label and a package-relative Bun entry. A user click opens an owned terminal tab for long-running interactive tools. The entry resolves inside the installed package. The terminal owns its lifetime and input.

## Invocation context

The render callback may read its second parameter: `definePlugin((action, { locale }) => Text({ text: locale === "zh-CN" ? "你好" : "Hello" }))`. Existing callbacks that only take `action` continue to work. Import `PluginContext` and `PluginThemeContext` to type reusable code.

`runPlugin` reads `MUQUN_PLUGIN_CONTEXT` JSON and passes a readonly, frozen snapshot for that invocation:

```ts
{
  locale: "zh-CN",
  capabilities: { webviewEntry: true },
  theme: {
    id: "default",
    colorScheme: "light",
    preference: "system",
    reducedMotion: false,
    reducedTransparency: false,
    textScale: 1,
  },
  platform: "linux",
  desktopVersion: "1.2.3",
}
```

`locale` is the language selected in Desktop, not the operating-system locale. Desktop currently supplies `en`, `zh-CN`, `zh-TW`, `ja`, `ko`, `de`, `fr`, `es`, `pt`, `ru`, `vi` or `th`. The string type permits future locales; plugins should provide their own fallback for unsupported languages. Translate plugin controls and use the locale explicitly with `Intl` for dates and numbers. Preserve user-authored names and content.

`theme.colorScheme` is the effective `light` or `dark` appearance; `theme.preference` is the user's `system`, `light` or `dark` choice. The theme ID identifies the selected Desktop theme. Native components already inherit Desktop's colors and text styling. Respect reduced motion and transparency when generating custom content; `textScale` is a positive multiplier, with `1` as the baseline. `platform` is `linux`, `macos` or `windows`; `desktopVersion` identifies the host build.

Missing or invalid fields use English, the default dark theme with system preference, disabled motion/transparency reductions, text scale `1`, the current runtime platform, and an `unknown` Desktop version. Malformed JSON does not prevent rendering. Standalone invocations can use `MUQUN_PLUGIN_LOCALE` as a locale fallback. Direct `render("open")` calls use defaults; an optional second argument supplies a full context. Each action receives Desktop's current snapshot; the existing view updates on the next invocation.


## WebView appearance and locale

A plugin page receives its initial context before its scripts run. Desktop applies the selected language to the document and sets `color-scheme`. For custom styles, use `--muqun-background`, `--muqun-surface`, `--muqun-surface-raised`, `--muqun-border`, `--muqun-text`, `--muqun-text-muted`, `--muqun-accent`, `--muqun-on-accent` and `--muqun-danger`. These values update when the theme changes.

```ts
import { getWebViewContext, onWebViewContextChange } from "@muqun/desktop";

render(getWebViewContext()?.locale ?? navigator.language);
const unsubscribe = onWebViewContextChange(context => render(context.locale));
// Call unsubscribe() when the view unmounts.
```

An owned local WebView service and native Bun workers also receive `workspace.path` for the current workspace. A plugin page opened by URL receives presentation settings only. Standalone browser tabs have no Desktop context. The same snapshot is available to scripts through `window.muqunDesktop?.context`.

## Desktop diagnostics

Settings → Diagnostics shows local runtime, package and plugin events. A view invocation ends when Desktop accepts the v1 response, rejects the response, or cancels the invocation. Each mounted view accepts one invocation at a time. Independent views can load concurrently. Closing a view discards its drafts, cancels its worker and removes its cached response. Disabling the plugin cancels its worker and discards its cached views. The last successful view stays visible if a later response fails validation.

Each retained event records its ID, completion time, elapsed time, operation, outcome and available version information. Desktop stores up to 200 records locally. Copied reports use stable snake_case action and outcome codes across display languages. Desktop warnings and errors include a code location. Raw stdout, stderr, action payloads and tracing fields are excluded from the journal. A successful worker can report that it wrote diagnostics to stderr without retaining that text.

### Select a local file or directory

Call `pickPath` from a Bun handler when the user chooses Browse. It opens the native picker, resolves to a path on selection or `null` on cancellation, and rejects if the picker cannot start. It requires a Desktop host that provides the picker and SDK 0.0.5-dev or later.

```ts
import { pickPath } from "@muqun/desktop";

const project = await pickPath({ kind: "directory", title: "Choose a project" });
if (project !== null) await openProject(project);
```


## Add text to a conversation draft

A package can opt in with `"muqun": { "permissions": ["composer.draft.append"] }`. SDK 0.0.6-dev exports `appendToDraft(text): Promise<void>` for its owned WebView service. Call it directly from a click or keyboard activation. The same method is available on the top document's `window.muqunDesktop` for same-origin panels. Standalone browser tabs and URL-only views have no draft bridge.

Desktop appends up to 8 KiB of UTF-8 text to the unsent draft of the conversation that opened the view. Switching conversations or moving the view preserves the original target. Empty text, a combined draft over 256 KiB, a missing original conversation or a different active computer produces a rejected promise. Show the result to the user and retain their annotation when adding fails. A successful call leaves submission under the user's control.

### Attachment images

When the action context advertises `capabilities.nativeImages` (SDK
`0.0.8-dev`), write PNG, JPEG or WebP bytes to the directory supplied in
`MUQUN_PLUGIN_MEDIA_DIR`. Return `Image({ src: "media:photo.png", alt: caption })`.
Optional width and height set the display area in points; the picture keeps
its aspect ratio. For small images, `src` also accepts a base64 data URI.

Use a plain ASCII filename up to 128 characters using letters, digits, dots,
hyphens and underscores, starting with a character other than a dot. Desktop
reads referenced files when the action exits, then removes the invocation
directory. The next action gets its own directory. Each image can use up to
8 MiB encoded and 12 megapixels, with 16 MiB and 32 megapixels across the
view. For larger galleries, page the images. Desktop shows alt text when a
picture is missing, unsupported or exceeds the limit.

With `capabilities.fillLayout`, a root Column, Row or ScrollArea can set
`height: "fill"` to use its plugin pane's available height. Nested fill-height
children divide the remaining vertical space in a Column and stretch to the
row's height in a Row. Use a fill-height horizontal ScrollArea containing a
fill-height Row and lane Columns to keep lane headers in place while each
lane's fill-height vertical ScrollArea scrolls independently.

### Markdown table of contents

When `capabilities.markdownAnchors` is enabled, a Markdown node can link to
another Markdown node in the same plugin pane. For example, return
`Markdown({ text: "[Setup](#install)" })` and
`Markdown({ anchor: "install", text: "## Setup\n\n…" })`. A click places the
destination at the top of its nearest vertical scroller. Each anchor identifies
one block in the current view; use up to 128 ASCII letters, digits, hyphens or
underscores. Give separate comments their own anchor prefixes when they have
overlapping section names.

### Native dropdowns

When the host advertises `capabilities.nativeSelect`, use
`Select({ key, label, options })` to place a native picker in a view.
Give each option a display `label` and its plugin `action`. Set
`selected: true` on the current option. Choosing an item invokes its action
in the same plugin instance. The picker label names the control for assistive
technology and appears as the placeholder when no option is selected.

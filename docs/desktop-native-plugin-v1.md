# Muqun Desktop native plugin SDK v1

**Status: design proposal.** The [SDK preview guide](../desktop/sdk/README.md) is the reference for the implemented package format, components and one-shot action API. Desktop supports Bun-managed npm packages and local tarballs, typed appearance and locale snapshots, and local diagnostics. The mount API, subscriptions and persistent worker protocol below are proposals.

## Decisions before a stable SDK

- **Identity and contributions.** Keep the npm package name as the durable plugin ID. Scope command and view IDs to that package. Discover views, commands and focused shortcuts from package metadata without executing code. Derive activation from the contribution that the user invokes. VS Code can infer activation for contributed views and commands. [Extension manifest](https://code.visualstudio.com/api/references/extension-manifest) · [Activation events](https://code.visualstudio.com/api/references/activation-events)
- **Compatibility.** Keep the plugin release version in package.json. Distinguish the SDK peer range from the runtime protocol version and optional host capabilities. The managed installer checks a declared peer; discovery rejects unsupported required capabilities and ignores unknown optional metadata. Keep this behavior when extending the schema, and report required upgrades with actionable errors. Apply the same rules to linked and installed packages. Keep experimental exports explicitly opt-in until their semantics are stable. [VS Code proposed API](https://code.visualstudio.com/api/advanced-topics/using-proposed-api)
- **Instances and lifecycle.** Route actions and cancellations to a mounted view. A slow request in one tab must allow interaction in a second tab. Closing or reopening a tab must invalidate late results. Keep process lifetime internal to Desktop so the SDK can reuse workers later. Measure cold open, warm navigation and idle memory separately before choosing a worker retention policy.
- **State and trust.** Separate per-view drafts, persistent plugin settings, disposable caches and credentials. Define ownership across update, disable and uninstall. Show package identity, source and the local-code trust model before first execution. Validate compatibility from metadata during updates. Running a candidate can have external side effects even when package installation scripts are disabled. [VS Code common capabilities](https://code.visualstudio.com/api/extension-capabilities/common-capabilities)
- **Compatibility fixtures.** Exercise an archived preview package on the new host and a newer plugin with an unsupported required feature on the older supported host. Verify two independent views, a discarded late response, scoped shortcuts during text composition, and preservation of the previous working installation after a rejected update.

## Writing a view

The official SDK will use the Muqun npm organization (`@muqun`). Its package name is `@muqun/desktop`. The package root exports both lifecycle functions and native components.

Import lifecycle functions and named components from `@muqun/desktop`. Components are TypeScript functions that return native view descriptions. Authors can compose them into their own functions and publish reusable component libraries. For example, a board plugin can build a `BoardColumn` from a `Column`, a `Heading` and a `VirtualList`. Column rules and card actions belong to that plugin.

| Purpose | Public component names | Usage |
| --- | --- | --- |
| Layout | `Row`, `Column`, `Stack`, `ScrollArea`, `VirtualList` | Row and Column arrange children on one axis. Stack overlays them. Use stable item keys in a VirtualList so the host can preserve identity and request visible rows plus overscan. |
| Content | `Text`, `Heading`, `Image`, `Markdown` | Render content with the host's typography and theme. Give meaningful images accessible descriptions. |
| Controls | `Button`, `TextField`, `TextArea` | Use the control that matches the interaction. Desktop supplies keyboard navigation, focus rings and native text composition. |
| Composite controls | `Dialog`, `DialogTrigger`, `DialogContent`, `DialogTitle`, `DialogDescription`, `DialogClose`; `DropdownMenu`, `DropdownMenuTrigger`, `DropdownMenuContent`, `DropdownMenuItem` | Compose named parts under one root. The root coordinates focus, dismissal and keyboard handling. The title supplies the dialog's accessible name. |

Use PascalCase nouns for components and prefix each part of a composite with the root name so it is easy to find in autocomplete. Use camelCase for props and callbacks, such as `children`, `variant`, `disabled`, `onPress` and `onValueChange`. A Button takes a short label and inherits its theme and focus behavior by default. A component's `key` identifies it across updates within an instance. A plugin can expose its own vocabulary to callers while rendering standard components underneath. The named-part convention follows the composition pattern in [shadcn/ui's Dialog](https://ui.shadcn.com/docs/components/dialog).

### Example plugin

The plugin uses its root `package.json` for discovery and versioning. The [SDK guide](../desktop/sdk/README.md) documents the supported fields.

```json
{
  "name": "studio.notes",
  "version": "1.0.0",
  "main": "main.ts",
  "muqun": { "displayName": "Notes" }
}
```

The author-defined `NoteActions` below composes SDK components. It can be reused in a tile or drawer without inspecting which surface contains it.

```ts
import { Button, Column, Heading, definePlugin } from "@muqun/desktop";

function NoteActions(props: { pinned: boolean; onToggle: () => Promise<void> }) {
  return Column({
    background: "surface.base",
    children: [
      Heading({ text: props.pinned ? "Pinned note" : "Note" }),
      Button({
        key: "toggle-pin",
        label: props.pinned ? "Unpin note" : "Pin note",
        variant: "secondary",
        onPress: props.onToggle,
      }),
    ],
  });
}

export default definePlugin({
  async mount(ctx) {
    let pinned = (await ctx.storage.get<boolean>("pinned")) ?? false;
    return () => NoteActions({
      pinned,
      async onToggle() {
        const next = !pinned;
        await ctx.storage.set("pinned", next);
        pinned = next;
        ctx.invalidate();
      },
    });
  },
});
```

The SDK ships version-matched type declarations and examples. Development mode links a local directory, reloads the plugin and exposes layout, event and log inspection. Installed packages use the same protocol and renderer.

## Runtime and rendering

Desktop provides Bun. Authors ship JavaScript or Bun-compatible TypeScript with their dependencies and assets. Packages must load their own code offline. Installation skips package lifecycle scripts. The existing loader continues to handle static plugins such as Markdown panels.

Desktop starts one Bun process per enabled dynamic plugin on demand. A process can serve multiple panes. Each mounted instance has its own ID, surface, focus and request lifetime. Store routes, stable list keys and unfinished edits per instance. Share account data explicitly. Rune draws the elements and handles scrolling, hit testing, text input and animation frames.

The host and Bun exchange versioned, bidirectional JSON Lines messages on stdin and stdout. Logs go to stderr. At startup they negotiate an API major version and capability set. On mount the host supplies the instance ID, surface, locale and theme. The SDK sends keyed view trees and keeps callbacks in Bun. The host reconciles nodes by key to preserve input, selection and scroll position. Events return to the originating instance with its ID, node key and action ID.

Pair requests with responses by request ID. Cancel outstanding instance requests on unmount. Assign a fresh generation on reload and discard late results from earlier generations. Bound message size, tree size and pending queues. If a process crashes, offer to reload that plugin while other plugins continue running.

## Embedded web applications

The Linux Desktop preview supports a root `WebView({ entry })` from `@muqun/desktop`. The [SDK guide](../desktop/sdk/README.md#embed-a-local-web-application) defines the package entry, capability check, loopback startup handshake and per-instance lifetime. Each mounted view retains its service across tab switches. Unrelated websites use the separate browser tab.

## Host capabilities

Core components are part of API v1. Plugins declare optional services in the manifest. The host reports available capabilities during the handshake and checks grants on each host call.

| Service | Plugin use | Host behavior |
| --- | --- | --- |
| Theme and localization | Pass semantic tokens to components and load plugin-owned translations. Preserve user content such as custom column names. | Notify visible instances when appearance or language changes. Supply current values when a hidden instance resumes. |
| Surfaces and keys | Declare tile, page or drawer support. Bind actions such as N to open an inbox in the focused instance. | Manage maximize, restore, pop-out, close and focus. Text editing and IME composition take precedence over single-key actions. The topmost overlay handles Escape first. |
| Storage | Save preferences, drafts and cache entries. Key account and document data separately. | Namespace storage by plugin ID and preserve it across unmounts. |
| Network and credentials | Declare network access, bind a user-configured origin and issue requests with a credential handle. | Scope host-managed credentials by plugin and origin. Strip the original authorization on cross-origin redirects. Apply the same rules to protected images. |
| Notifications | Provide a deduplication key, summary and target action for an in-app notification. Route clicks to the target document. | Scope events by plugin and account and prefer an existing target instance. System notifications require a separate grant. |
| Files, clipboard and workspace | Obtain handles from a file picker, read or write the clipboard, or request workspace context under the declared grant. | Associate selections with the requesting instance, release handles when it closes and expose only the declared workspace context. |

Plugins run as trusted local code with the user's OS permissions. The installer must explain that execution model. Capability grants govern the APIs exposed by Desktop. Enforcing restrictions on a Bun script's own filesystem, network or process access would require a separately implemented and verified cross-platform sandbox.

## Theme contract

Desktop maps the system appearance, Omarchy and custom themes to semantic tokens. The base palette includes `surface.base`, `surface.raised`, `text.primary`, `text.muted`, `accent`, `border`, `focus` and semantic success, warning and error colors. The host also supplies typography, spacing, corner radii, density and motion preferences.

Controls, selection, input focus and panel backgrounds use semantic styles by default. Markdown uses Desktop's styles for body text, headings, links, tables and code blocks. Directional controls use the active theme's artwork and direction metadata, with a host fallback. Avatars, brand assets and meaningful data colors can retain their original colors. A theme switch preserves the open document, draft, selection and scroll position. Desktop applies reduced motion and reduced transparency and renders animations locally from declared presets and parameters.

## Package lifecycle

1. **Prepare Bun.** When installing or enabling the first plugin that needs Bun, Desktop checks configured, system and managed copies for SDK compatibility. When a compatible runtime is missing, the user explicitly chooses **Install Bun** before Desktop downloads and verifies a managed runtime in the user's directory. Show progress, cancellation and retry, share one installation among concurrent requests, and resume activation when ready. Preserve the user's system installation. Hide the Bun management row in ordinary plugin settings when no enabled plugin requires it and no installation is running.
2. **Install.** Accept a single package source resolved by Bun and validate the resolved plugin manifest. Record the source and package digest. Display the source, version and requested capabilities. Validate compatibility, archive size, path boundaries, reserved IDs and source ownership in staging before activation. Packages may include icons and locale resources.
3. **Update.** Stage each version in its own directory. Check protocol and capability requirements using package metadata before activation. Confirm additional grants before activation. Switch the active version pointer atomically after validation. On failure retain the previous version. Serialize installation, update and removal for each plugin. Use a transaction or recoverable backup for data migrations so the previous code can reopen its data after rollback.
4. **Disable or uninstall.** Disabling unmounts instances, unregisters keys and notification subscriptions, stops timers and polling, and terminates the plugin process. Uninstalling also removes the package, registration and update source. Let the user retain or erase plugin data. The runtime manager retains ownership of the shared Bun installation.

## Migration and acceptance

Implement runtime provisioning, package management and the handshake while preserving existing directory plugins and script commands. Validate installation, interrupted updates and rollback with a small independently installed plugin. Then deliver native components, themes and instance-scoped events and migrate a complex private plugin out of the Desktop source tree. Ship new host features as general capabilities.

Use the same public API for a bundled plugin and an independently installed one. The host assigns namespaces for commands, panels and stored keys and loads manifests before entry points. OpenCode documents a similar boundary between its window and background components. Muqun uses the Rune renderer and its own entry-point protocol. [OpenCode GUI plugin guide](https://github.com/anomalyco/opencode/blob/dev/packages/gui-plugins/README.md)

On Linux and macOS, record the Desktop binary digest before and after plugin installation and update. Verify user-initiated Bun setup, retry after an offline failure, independent edits in two panes, theme switching with a draft open, shortcut handling during text input, rollback after a failed update and termination of background requests after uninstall. The Desktop binary digest must match on each machine before and after both operations.

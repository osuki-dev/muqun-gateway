# Rune patches for Muqun Desktop

`desktop/Cargo.toml` pins
[`ryuhzk/rune@5c35ba4`](https://github.com/ryuhzk/rune/tree/muqun-desktop-apis)
(`muqun-desktop-apis`), which is `anomalyco/rune@50d5c6d` plus:

1. `TextArea` / `TextInput` `on_input_paste` (Ctrl/Cmd+V files and images)
2. `SplitView::sidebar_leading` (brand mark in the title-bar band)
3. `register_system_cjk`: register CJK fallbacks from `.ttc` files already loaded by `load_installed`

The `0001` / `0002` patches here are the same commits, kept for review and for
rebasing onto a newer upstream tip. Prefer the git pin in CI; do not require a
local `../rune` checkout.

Native Wayland file drops are pinned at `b345658c61254c377e034a6a1d7c45c349abafb2` on the fork branch
`fix/wayland-file-drop`, based on `9bb3e8e`. Patch `0004` adds the missing
Wayland data-device receiver and surface coordinates used by Composer drop targets.
It preserves clipboard ownership and bounds URI transfers; no extra runtime dependency
is required. The fork also keeps `vendor/winit.patch` replayable against winit 0.30.13.

Validation used a real GTK file drag source and a native Rune window inside an
isolated headless Wayland compositor: Copy was delivered once to the right drop
zone, leaving the target cancelled the hover, and Move-only offers were rejected.
This does not replace an installed Desktop multi-pane acceptance check.

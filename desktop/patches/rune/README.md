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

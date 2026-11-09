# Rune patches for Muqun Desktop

`desktop/Cargo.toml` pins
[`ryuhzk/rune@030e916`](https://github.com/ryuhzk/rune/tree/muqun-desktop-apis)
(`muqun-desktop-apis`), which is `anomalyco/rune@a6a59e6` plus:

1. `TextArea` / `TextInput` `on_input_paste` (Ctrl/Cmd+V files and images)
2. `SplitView::sidebar_leading` (brand mark in the title-bar band)

The `0001` / `0002` patches here are the same commits, kept for review and for
rebasing onto a newer upstream tip. Prefer the git pin in CI; do not require a
local `../rune` checkout.

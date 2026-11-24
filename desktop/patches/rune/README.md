# Rune patches for Muqun Desktop

`desktop/Cargo.toml` pins
[`ryuhzk/rune@6a56722`](https://github.com/ryuhzk/rune/tree/muqun-sync-20261124)
(`muqun-sync-20261124`), which is `anomalyco/rune@46f334ef` plus two remaining
`rune-kit` APIs not yet upstream:

1. `TextArea` / `TextInput` `on_input_paste` (Ctrl/Cmd+V files and images)
2. `SplitView::sidebar_leading` (brand mark in the title-bar band)

The `0001` / `0002` patches here are kept for review and for rebasing onto a
newer upstream tip. Prefer the git pin in CI; do not require a local `../rune`
checkout.

Previously carried fork patches that have since landed upstream in
`anomalyco/rune` (dropped during the `2026-11-24` sync):

- `0003`: `register_system_cjk` registering CJK fallbacks from already-loaded `.ttc` files
- `0004`: Native Wayland `wl_data_device` file drops (`#1808`, lazy binding in `#1872`)
- `0005`: Early `wl_data_offer.action` and `DndAction` handling (`#1866`)

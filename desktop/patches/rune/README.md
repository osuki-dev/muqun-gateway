# Rune patches for Muqun Desktop

`desktop/Cargo.toml` pins
[`ryuhzk/rune@4be7043`](https://github.com/ryuhzk/rune/tree/muqun-sync-20261128)
(`muqun-sync-20261128`), which is `anomalyco/rune@dd15d6a7` plus one remaining
`rune-kit` API not yet upstream:

1. `SplitView::sidebar_leading` (brand mark in the title-bar band)

The `0002` patch here is kept for review and for rebasing onto a newer upstream
tip. Prefer the git pin in CI; do not require a local `../rune` checkout.

Previously carried fork patches that have since landed upstream in
`anomalyco/rune`:

- `0001`: `TextArea` / `TextInput` `on_input_paste` (dropped during the `2026-11-28` sync, landed in `#1921`)
- `0003`: `register_system_cjk` registering CJK fallbacks from already-loaded `.ttc` files
- `0004`: Native Wayland `wl_data_device` file drops (`#1808`, lazy binding in `#1872`)
- `0005`: Early `wl_data_offer.action` and `DndAction` handling (`#1866`)

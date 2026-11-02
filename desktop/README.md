# Muqun Desktop

## Icons

The App's glyphs (`app/assets/glyphs`) are the single icon source. After they
change, run `desktop/scripts/sync-glyphs.sh` (set `MUQUN_APP_DIR` if the App is
not the sibling `app/` checkout). It refreshes `assets/glyphs/`, adds
`brand-opencode.svg`, and regenerates `src/ui/glyphs.rs`. Use
`ui::glyphs::icon(Glyph::X, size).tint(color)`, and `agent_mark(kind)` for brands.

# Built-in themes

`builtin/*.muqun-theme` are compiled into the binary (`src/theme/store.rs`
`BUILTIN`). Each is a regular `.muqun-theme` package, rebuilt from the
muqun-themes sources by `build-builtin.py`:

```sh
python3 desktop/assets/themes/build-builtin.py /path/to/muqun/themes
```

Only CC0-1.0 sources are packed (the script refuses anything else). The packs
are trimmed for the desktop: they keep the manifest's colours, terminal
palette and metadata unchanged, but only the images the desktop draws -- the
gallery `preview` (768×480) and the Home `home.artwork` / `home.wallpaper`
slots (at most 1280 px) -- re-encoded as WebP with their `sha256` updated.
Icons and the App's other decoration slots are dropped. The full packs are
one gallery install away.

| id | source version | licence |
|---|---|---|
| cover-courier | 1.0.12 | CC0-1.0 |
| akiba-overdrive | 1.0.10 | CC0-1.0 |
| jade-tempest | 1.0.5 | CC0-1.0 |
| eclipse-archivist | 1.0.5 | CC0-1.0 |
| vanguard-drifter | 1.0.9 | CC0-1.0 |
| scene-voyager | 1.0.8 | CC0-1.0 |

`cover-courier.json` is the App's Cover Courier manifest, kept as the
reference the token-mapping tests read.

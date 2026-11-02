#!/usr/bin/env bash
# No colour literals outside the theme (architecture §5): views and styles.css
# name colours as `var(--token)`. The only files allowed to hold literals are
#   desktop/src/theme/tokens.rs   (the .muqun-theme -> Blinc token mapping)
#   desktop/src/theme/agents.rs   (the ten agents' brand colours)
#   desktop/src/theme/pack.rs     (the pack reader; its tests carry colour strings)
# Exit 1, listing every hit, when anything else has one.
set -euo pipefail
cd "$(dirname "$0")/../.."

pattern='Color::rgb|#[0-9a-fA-F]{6}|rgba?\('
allowed=(desktop/src/theme/tokens.rs desktop/src/theme/agents.rs desktop/src/theme/pack.rs)

hits=$(grep -rnE "$pattern" desktop/src --include='*.rs' --include='*.css' || true)
for file in "${allowed[@]}"; do
  hits=$(grep -v "^$file:" <<<"$hits" || true)
done

if [[ -n "$hits" ]]; then
  echo "Colour literals outside the theme files (use var(--token), or put brand colours in theme/agents.rs):" >&2
  echo "$hits" >&2
  exit 1
fi
echo "lint-theme-tokens: ok"

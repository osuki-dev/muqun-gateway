#!/usr/bin/env sh
# One-command install, update, and first-run setup for Muqun Gateway.
#
#   curl -fsSL https://raw.githubusercontent.com/osuki-dev/muqun-gateway/main/install.sh | sh
#
# tmux is the primary backend and needs nothing beyond tmux itself. Herdr is
# also supported: when Herdr is on PATH, this installs Muqun Gateway as a
# Herdr plugin instead, and setup/start/the pairing QR are driven through
# plugin actions inside a herdr session, same as before.
#
# On a first install it also configures, starts, and opens the pairing QR for
# you. Re-running updates the binary, runs idempotent setup, and reloads it
# while preserving the server identity, devices, and backend list.
# macOS and Linux only.
set -eu

REPO="osuki-dev/muqun-gateway"
PLUGIN_ID="herdr.gateway"
MIN_HERDR_VERSION="0.7.5"
# Where the standalone (non-Herdr) binary lands. Overridable for anyone who
# does not want it on their PATH by default.
INSTALL_DIR="${MUQUN_GATEWAY_INSTALL_DIR:-$HOME/.local/bin}"

green() { printf '\033[1;32m%s\033[0m\n' "$1"; }
info()  { printf '\033[1;36m==>\033[0m %s\n' "$1"; }
warn()  { printf '\033[1;33m!\033[0m  %s\n' "$1"; }
die()   { printf '\033[1;31mError:\033[0m %s\n' "$1" >&2; exit 1; }

version_at_least() {
  awk -v current="$1" -v required="$2" '
    BEGIN {
      sub(/^v/, "", current)
      sub(/^v/, "", required)
      split(current, current_parts, ".")
      split(required, required_parts, ".")

      for (i = 1; i <= 3; i++) {
        current_number = current_parts[i] + 0
        required_number = required_parts[i] + 0
        if (current_number > required_number) exit 0
        if (current_number < required_number) exit 1
      }

      # A prerelease of the required stable version does not satisfy it.
      if (current ~ /-/ && required !~ /-/) exit 1
      exit 0
    }
  '
}

# 1. Operating system -- Windows is not supported yet.
case "$(uname -s)" in
  Darwin) info "Detected macOS" ;;
  Linux)  info "Detected Linux" ;;
  *)      die "Unsupported OS '$(uname -s)'. macOS and Linux only for now." ;;
esac

# 2. Herdr is one of two backends, not a requirement. When it is on PATH,
#    install as a Herdr plugin (the path this script has always taken).
#    Otherwise install the standalone binary and configure the tmux backend
#    directly -- tmux is primary and needs no other terminal manager running.
if command -v herdr >/dev/null 2>&1; then
  # 2a. Older Herdr versions parse the plugin manifest before checking its
  #     declared minimum version. Check here first so users get an actionable
  #     upgrade message instead of a TOML error for newer manifest fields.
  herdr_version="$(herdr --version 2>/dev/null | awk 'NR == 1 && $1 == "herdr" { print $2 }')"
  [ -n "$herdr_version" ] \
    || die "Could not determine the installed Herdr version. Run 'herdr --version' and update Herdr before retrying."

  if ! version_at_least "$herdr_version" "$MIN_HERDR_VERSION"; then
    warn "Herdr $herdr_version is too old. Muqun Gateway requires Herdr $MIN_HERDR_VERSION or newer."
    echo
    echo "Update Herdr first:"
    echo "  herdr update --handoff"
    echo
    echo "Then run this installer again."
    exit 1
  fi
  info "Detected Herdr $herdr_version"

  # 2b. Install downloads a prebuilt, statically linked binary, so Rust is
  #     optional -- only needed as a fallback when no release binary matches
  #     this OS/arch.
  if ! command -v cargo >/dev/null 2>&1; then
    warn "Rust (cargo) not found. That is fine -- a prebuilt binary will be used."
    echo "   (If none matches your platform, install Rust from https://rustup.rs and retry.)"
  fi

  # 2c. Install or update. Reinstalling a GitHub-managed plugin replaces its
  #     checkout in place -- no uninstall needed. A local dev link is the one
  #     case Herdr refuses to install over, so detect it and explain instead
  #     of failing. `existing` (captured before install) also tells
  #     first-install from update.
  existing="$(herdr plugin list 2>/dev/null | grep "$PLUGIN_ID" || true)"
  if printf '%s' "$existing" | grep -q '\[local:'; then
    warn "Muqun Gateway is installed as a local dev link, not a GitHub plugin."
    echo "   Update that checkout in place:"
    echo "     git -C <your-checkout> pull && cargo build --release"
    echo "   Or switch to the GitHub-managed version:"
    echo "     herdr plugin unlink $PLUGIN_ID && herdr plugin install $REPO --yes"
    exit 0
  elif [ -n "$existing" ]; then
    info "Muqun Gateway is already installed -- updating to the latest..."
  else
    info "Installing Muqun Gateway..."
  fi
  herdr plugin install "$REPO" --yes

  # 2d. Configure, (re)load, and show the pairing QR. setup is idempotent --
  #     it keeps an existing server id, token, and URL, so running it every
  #     time is safe (paired devices survive) and also repairs an install
  #     whose earlier setup never completed. stop+start then reloads the
  #     freshly downloaded binary. All of this goes through herdr plugin
  #     actions and needs a live herdr session, so if that fails we print the
  #     manual commands instead of leaving a half-finished install.
  auto_done=0
  info "Configuring and starting the gateway..."
  if herdr plugin action invoke "$PLUGIN_ID.setup" >/dev/null 2>&1; then
    sleep 2   # setup runs in a herdr pane; let it write the config first
    herdr plugin action invoke "$PLUGIN_ID.stop"  >/dev/null 2>&1 || true
    sleep 1
    herdr plugin action invoke "$PLUGIN_ID.start" >/dev/null 2>&1 || true
    sleep 1
    herdr plugin pane open --plugin "$PLUGIN_ID" --entrypoint manage >/dev/null 2>&1 || true
    auto_done=1
    if [ -z "$existing" ]; then
      green "Muqun Gateway is configured, running, and showing the pairing QR."
    else
      green "Muqun Gateway updated, reloaded, and showing the pairing QR (pairings kept)."
    fi
  else
    warn "Couldn't reach a herdr session to configure the gateway."
  fi

  echo
  if [ "$auto_done" = "1" ]; then
    echo "The pairing QR is open in the herdr 'Gateway Manager' pane."
    echo "Scan it from the Muqun app on a device on the same Tailscale network."
    echo
    echo "Re-open the QR any time with:"
    echo "  herdr plugin pane open --plugin $PLUGIN_ID --entrypoint manage"
  else
    warn "Run these from INSIDE herdr to finish:"
    echo "  herdr plugin action invoke $PLUGIN_ID.setup"
    echo "  herdr plugin action invoke $PLUGIN_ID.start"
    echo "  herdr plugin pane open --plugin $PLUGIN_ID --entrypoint manage"
  fi
  exit 0
fi

# 3. Standalone path: no Herdr, so no plugin manager to install into or run
#    actions through. tmux is what `setup` configures here, so it has to
#    actually be there -- unlike Herdr this script cannot install it for you.
info "Herdr not found; installing standalone with the tmux backend."
command -v tmux >/dev/null 2>&1 \
  || die "Neither Herdr (https://herdr.dev) nor tmux was found. Install one of them, then retry."

os="$(uname -s)"
arch="$(uname -m)"
case "$os/$arch" in
  Darwin/arm64)              target="aarch64-apple-darwin" ;;
  Darwin/x86_64)              target="x86_64-apple-darwin" ;;
  Linux/x86_64)               target="x86_64-unknown-linux-musl" ;;
  Linux/aarch64|Linux/arm64)  target="aarch64-unknown-linux-musl" ;;
  *)                          target="" ;;
esac

command -v curl >/dev/null 2>&1 || die "curl is required to download the binary."
mkdir -p "$INSTALL_DIR"
binary="$INSTALL_DIR/muqun-gateway"

download_url="https://github.com/$REPO/releases/latest/download/muqun-gateway-$target"
if [ -n "$target" ] && curl -fsSL "$download_url" -o "$binary.new" 2>/dev/null; then
  chmod +x "$binary.new"
  mv "$binary.new" "$binary"
  info "Installed the latest prebuilt binary for $target to $binary"
else
  rm -f "$binary.new"
  die "No prebuilt binary found for this platform ($os/$arch). Build from source instead:
  git clone https://github.com/$REPO.git
  cd $(basename "$REPO")
  cargo build --release
  ./target/release/muqun-gateway setup --backend tmux
  ./target/release/muqun-gateway start
  ./target/release/muqun-gateway manage"
fi

case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) warn "$INSTALL_DIR is not on your PATH. Add it, or call $binary directly." ;;
esac

# setup is idempotent -- it keeps an existing server id, token, and URL, so
# running it on every install/update is safe and also repairs an install
# whose earlier setup never completed.
info "Configuring the tmux backend..."
"$binary" setup --backend tmux
"$binary" stop  >/dev/null 2>&1 || true
"$binary" start

echo
green "Muqun Gateway is configured and running with the tmux backend."
echo "Open the pairing QR any time with:"
echo "  $binary manage"
echo
echo "This path -- installer-driven standalone tmux setup -- is new with this"
echo "release and has not yet been exercised on a machine that never had Herdr"
echo "installed; if something looks wrong, run the three commands above by hand"
echo "and compare their output."

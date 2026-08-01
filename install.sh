#!/usr/bin/env sh
# One-command install, update, and first-run setup for the Herdr Gateway plugin.
#
#   curl -fsSL https://raw.githubusercontent.com/osuki-dev/herdr-gateway/main/install.sh | sh
#
# Herdr 0.7.5 installs plugins globally, so installation may run from any shell.
# Setup/start and the pairing QR are driven through plugin actions and still
# need a running herdr session to attach to.
#
# On a first install it also configures, starts, and opens the pairing QR for
# you. Re-running updates the plugin, runs idempotent setup, and reloads the
# binary while preserving the server identity, devices, and backend list.
# macOS and Linux only.
set -eu

REPO="osuki-dev/herdr-gateway"
PLUGIN_ID="herdr.gateway"
MIN_HERDR_VERSION="0.7.5"

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

# 2. Herdr itself must be installed -- it hosts the plugin and runs its actions.
command -v herdr >/dev/null 2>&1 \
  || die "Herdr is not installed. Get it from https://herdr.dev first."

# 3. Older Herdr versions parse the plugin manifest before checking its declared
#    minimum version. Check here first so users get an actionable upgrade message
#    instead of a TOML error for newer manifest fields such as `popup`.
herdr_version="$(herdr --version 2>/dev/null | awk 'NR == 1 && $1 == "herdr" { print $2 }')"
[ -n "$herdr_version" ] \
  || die "Could not determine the installed Herdr version. Run 'herdr --version' and update Herdr before retrying."

if ! version_at_least "$herdr_version" "$MIN_HERDR_VERSION"; then
  warn "Herdr $herdr_version is too old. Herdr Gateway requires Herdr $MIN_HERDR_VERSION or newer."
  echo
  echo "Update Herdr first:"
  echo "  herdr update --handoff"
  echo
  echo "Then run this installer again."
  exit 1
fi
info "Detected Herdr $herdr_version"

# 4. Install downloads a prebuilt, statically linked binary, so Rust is optional
#    -- only needed as a fallback when no release binary matches this OS/arch.
if ! command -v cargo >/dev/null 2>&1; then
  warn "Rust (cargo) not found. That is fine -- a prebuilt binary will be used."
  echo "   (If none matches your platform, install Rust from https://rustup.rs and retry.)"
fi

# 5. Install or update. Reinstalling a GitHub-managed plugin replaces its
#    checkout in place -- no uninstall needed. A local dev link is the one case
#    Herdr refuses to install over, so detect it and explain instead of failing.
#    `existing` (captured before install) also tells first-install from update.
existing="$(herdr plugin list 2>/dev/null | grep "$PLUGIN_ID" || true)"
if printf '%s' "$existing" | grep -q '\[local:'; then
  warn "Herdr Gateway is installed as a local dev link, not a GitHub plugin."
  echo "   Update that checkout in place:"
  echo "     git -C <your-checkout> pull && cargo build --release"
  echo "   Or switch to the GitHub-managed version:"
  echo "     herdr plugin unlink $PLUGIN_ID && herdr plugin install $REPO --yes"
  exit 0
elif [ -n "$existing" ]; then
  info "Herdr Gateway is already installed -- updating to the latest..."
else
  info "Installing Herdr Gateway..."
fi
herdr plugin install "$REPO" --yes

# 6. Configure, (re)load, and show the pairing QR. setup is idempotent -- it
#    keeps an existing server id, token, and URL, so running it every time is
#    safe (paired devices survive) and also repairs an install whose earlier
#    setup never completed. stop+start then reloads the freshly downloaded
#    binary. All of this goes through herdr plugin actions and needs a live
#    herdr session, so if that fails we print the manual commands instead of
#    leaving a half-finished install.
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
    green "Herdr Gateway is configured, running, and showing the pairing QR."
  else
    green "Herdr Gateway updated, reloaded, and showing the pairing QR (pairings kept)."
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

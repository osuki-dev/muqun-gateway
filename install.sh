#!/usr/bin/env sh
# One-command install, update, and first-run setup for Muqun Gateway.
#
#   curl -fsSL https://muqun.dev/gateway.sh | sh
#
# tmux is the primary backend and needs nothing beyond tmux itself. Herdr is
# also supported. The two are not an either/or choice -- the gateway happily
# drives both backends from one config at once (see `manage`'s "Terminal
# backends" list) -- so this installer configures whichever of them is
# actually present, independently:
#
# Every detected backend is configured in one standalone gateway, so optional
# backend startup does not depend on Herdr already running. tmux is the default
# on a fresh install when present; existing defaults and pairings are retained.
#
# On a first install it also configures, starts, and opens the pairing QR for
# you. Re-running updates the binary, runs idempotent setup, and reloads it
# while preserving the server identity, devices, and backend list -- and
# never flips a default an earlier install already chose.
# macOS and Linux only.
set -eu

REPO="osuki-dev/muqun-gateway"
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

# 2. Detect each backend independently -- this used to be a single if/else
#    gate on Herdr alone, which meant Herdr won whenever it was installed,
#    regardless of whether tmux was there too or which one the reader
#    actually wanted. The two are not mutually exclusive, so neither is this
#    check.
have_herdr=0
command -v herdr >/dev/null 2>&1 && have_herdr=1
have_tmux=0
command -v tmux >/dev/null 2>&1 && have_tmux=1

if [ "$have_herdr" = 0 ] && [ "$have_tmux" = 0 ]; then
  die "Neither Herdr (https://herdr.dev) nor tmux was found. Install one of them, then retry."
fi

# All detected backends use one standalone gateway, including Herdr-only hosts.
# A plugin-owned gateway cannot start Herdr after reboot: it depends on Herdr
# already running. Existing plugin pairings are imported below.
if [ "$have_tmux" = 1 ]; then
  setup_backend=tmux
else
  setup_backend=herdr
  herdr_version="$(herdr --version 2>/dev/null | awk 'NR == 1 && $1 == "herdr" { print $2 }')"
  if [ -z "$herdr_version" ] || ! version_at_least "$herdr_version" "$MIN_HERDR_VERSION"; then
    die "Herdr $MIN_HERDR_VERSION or newer is required. Update Herdr, then retry."
  fi
fi
info "Installing standalone with every detected terminal backend."

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
  ./target/release/muqun-gateway setup --backend $setup_backend
  ./target/release/muqun-gateway start
  ./target/release/muqun-gateway manage"
fi

case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) warn "$INSTALL_DIR is not on your PATH. Add it, or call $binary directly." ;;
esac

# MUQUN_GATEWAY_PORT and MUQUN_GATEWAY_TMUX_SOCKET are not needed for a
# normal install -- setup's own defaults (the default port, and the tmux
# backend pointed at the ambient default tmux server) are the whole point --
# but let this script be exercised end to end (alongside
# XDG_CONFIG_HOME/XDG_DATA_HOME/INSTALL_DIR) without touching a gateway
# already bound to the default port, or a tmux backend that would otherwise
# poll whatever the ambient default tmux server happens to be.

# Remember who owns an existing gateway before changing its configuration.
# A supervised process must be reloaded through the same supervisor: calling
# `muqun-gateway stop` is intentionally refused for it, and following that with
# `muqun-gateway start` only tries to create a second process beside the first.
# Capture this once and reuse it below so an update cannot observe two different
# service states halfway through its own work.
service_state="$("$binary" service status 2>/dev/null | head -n 1 || true)"
case "$service_state" in
  "service: installed"*) already_installed=1 ;;
  *) already_installed=0 ;;
esac

if [ "$have_herdr" = 1 ]; then
  # A machine that went through step 3 on an earlier run (Herdr-only, back
  # then) may have real paired devices sitting in that Herdr-plugin config.
  # Adopt it into the standalone install below instead of leaving it behind.
  # A no-op, not an error, when there is nothing to adopt -- a fresh machine,
  # or one already adopted on an earlier run of this script.
  "$binary" import-herdr-plugin --if-present
fi

# Snapshot what this install already has -- before adding anything -- so an
# already-chosen default is restored below rather than silently flipped by
# adding a new backend, and so "fresh install" vs. "update" is judged on the
# state that actually existed when this run started.
config_existed=0
previous_default=""
if previous_backends="$("$binary" backend list 2>/dev/null)" && [ -n "$previous_backends" ]; then
  config_existed=1
  previous_default="$(printf '%s\n' "$previous_backends" | head -n 1 | cut -f2)"
fi

# setup is idempotent -- it keeps an existing server id, token, and URL, so
# running it on every install/update is safe and also repairs an install
# whose earlier setup never completed.
info "Configuring the $setup_backend backend..."
# POSIX positional parameters preserve exact argument boundaries. Never build a
# shell command string: socket paths may contain spaces, quotes or glob syntax.
set -- setup --backend "$setup_backend"
if [ -n "${MUQUN_GATEWAY_PORT:-}" ]; then
  set -- "$@" --port "$MUQUN_GATEWAY_PORT"
fi
if [ "$have_tmux" = 1 ] && [ -n "${MUQUN_GATEWAY_TMUX_SOCKET:-}" ]; then
  set -- "$@" --socket-path "$MUQUN_GATEWAY_TMUX_SOCKET"
fi
"$binary" "$@"

if [ "$have_herdr" = 1 ] && [ "$have_tmux" = 1 ]; then
  info "Configuring the Herdr backend..."
  "$binary" backend add herdr >/dev/null
fi

# tmux is the default whenever it is configured -- the one remaining role of
# "which backend is primary" -- except an update must never flip a default an
# earlier install already chose. A fresh install (nothing configured before
# this run) always gets tmux as the default; an update keeps whatever was
# already the default.
if [ "$config_existed" = 1 ]; then
  if [ -n "$previous_default" ] && [ "$previous_default" != "tmux" ]; then
    restore_id="$("$binary" backend list | awk -F'\t' -v want="$previous_default" '$2 == want { print $1; exit }')"
    [ -n "$restore_id" ] && "$binary" backend default "$restore_id" >/dev/null
  fi
elif [ "$have_tmux" = 1 ]; then
  tmux_id="$("$binary" backend list | awk -F'\t' '$2 == "tmux" { print $1; exit }')"
  [ -n "$tmux_id" ] && "$binary" backend default "$tmux_id" >/dev/null
fi

# Backend startup is separate from registering the gateway with the OS. An
# empty answer or a noninteractive install preserves the existing preference.
echo
echo "Start configured terminal backends when the gateway starts?"
echo "  Existing tmux/Herdr sessions are reused, never replaced."
echo "  If stopped, tmux opens a detached shell and Herdr starts headless."
echo "  Herdr may restore its saved workspace; no agent prompts or trust"
echo "  approvals are submitted by the gateway."
echo "  Default: keep your current settings (new installs: off)."
if (exec 3< /dev/tty) 2>/dev/null; then
  printf "Enable backend startup? [y/N] "
  backend_answer=n
  if read -r backend_answer < /dev/tty 2>/dev/null; then :; else backend_answer=n; fi
  case "$backend_answer" in
    y|Y|yes|YES|Yes)
      "$binary" backend list | cut -f1 | while IFS= read -r backend_id; do
        if ! "$binary" backend autostart "$backend_id" on; then
          warn "Could not enable startup for $backend_id; it will need manual startup."
        fi
      done
      ;;
    *) info "Backend startup settings unchanged." ;;
  esac
else
  info "No terminal to ask on; backend startup settings unchanged."
fi
echo "  Change later: $binary backend autostart <backend-id> on|off"

if [ "$already_installed" = 1 ]; then
  # Refresh the unit as well as the binary, including child-process lifetime
  # rules that keep persistent terminals alive across gateway restarts.
  "$binary" service install
  if [ "$os" = "Linux" ]; then
    # enable --now does not restart an already active systemd service.
    systemctl --user restart dev.osuki.muqun-gateway.service
  fi
else
  "$binary" stop  >/dev/null 2>&1 || true
  "$binary" start
fi

# Autostart, asked rather than assumed.
#
# On an unmanaged install, `start` above spawns a detached child. It outlives
# this terminal, and that is all it was ever asked to do -- it does not outlive
# a reboot. So without this step the phone stops reaching the machine the next
# time it restarts, with no signal on either side saying why. A supervised
# update skips this prompt because it kept the service that was already there.
#
# Asking is deliberate. This writes a file into the reader's home directory and
# leaves something running on their computer indefinitely; that is not a default
# to take in silence, however convenient. The explanation comes before the
# question, because "install a service?" with no answer to "what for, and what
# does it touch?" is not a question anybody can answer.
#
# stdin is the script itself on the documented `curl ... | sh` path, so the
# prompt has to read the terminal directly. When there is no terminal at all --
# CI, a provisioning script -- the answer is no, and the command to do it later
# is printed instead. An unattended run must not quietly install a service.
# Anchored at the front, not a substring match: "not installed" contains
# "installed", so `*installed*` answers yes to both states. It did, and the
# prompt this whole block exists for never appeared.
if [ "$already_installed" = 1 ]; then
  info "Autostart is already set up; leaving it alone."
elif (exec 3< /dev/tty) 2>/dev/null; then
  echo
  echo "Start the gateway automatically?"
  echo
  echo "  The Muqun app on your phone talks to this gateway. As things stand it"
  echo "  runs until this computer restarts -- after a reboot your phone cannot"
  echo "  reach this machine until you come back and start it by hand."
  echo
  if [ "$(uname -s)" = "Darwin" ]; then
    echo "  Saying yes writes a LaunchAgent to ~/Library/LaunchAgents. It runs as"
    echo "  you, starts when you log in, and comes back if it stops."
  else
    echo "  Saying yes writes a systemd user unit to ~/.config/systemd/user and"
    echo "  enables lingering, so it runs as you, starts when the machine boots,"
    echo "  and comes back if it stops."
  fi
  echo "  No administrator password, nothing outside your home directory, and"
  echo "  nothing that runs as root."
  echo
  echo "  Undo it any time with:  $binary service uninstall"
  echo
  printf "Set it up now? [Y/n] "
  # A read that fails means no, never yes. /dev/tty can be openable and still
  # unreadable -- a detached CI shell hands back exactly that -- and the earlier
  # version of this line turned that failure into an empty answer, which fell
  # through to the default and installed a service on a machine that was never
  # asked. Enter still means yes; only a real answer can.
  if read -r answer < /dev/tty 2>/dev/null; then
    :
  else
    answer=n
    echo
    warn "Could not read an answer from the terminal, so autostart was not set up."
  fi
  case "$answer" in
    [Nn]*) warn "Skipped. The gateway stops when this computer restarts." ;;
    *) "$binary" service install ;;
  esac
else
  warn "No terminal to ask on, so autostart was not set up."
  echo "  The gateway stops when this computer restarts. To make it come back:"
  echo "    $binary service install"
fi

echo
if [ "$config_existed" = 1 ]; then
  green "Muqun Gateway is updated and running (pairings kept)."
else
  green "Muqun Gateway is configured and running."
fi
echo "Backends configured:"
"$binary" backend list | awk -F'\t' '{ printf "  %s %s (%s)\n", (NR == 1 ? "*" : " "), $1, $2 }'
echo "(* default)"
echo
echo "Open the pairing QR any time with:"
echo "  $binary manage"

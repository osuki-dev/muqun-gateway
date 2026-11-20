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
# you. Re-running delegates to the installed protected updater, preserving
# running state, server identity, devices, and backend defaults. Older installs
# without that updater require a manual migration; they are never overwritten.
# macOS and Linux only.
set -eu

REPO="osuki-dev/muqun-gateway"
channel=stable
bootstrap=0
desktop_terminal=0
while [ "$#" -gt 0 ]; do
  case "$1" in
    --channel) [ "$#" -ge 2 ] || { echo 'Missing channel.' >&2; exit 1; }; channel="$2"; shift 2 ;;
    --desktop-terminal) desktop_terminal=1; shift ;;
    *) echo 'Usage: install.sh [--channel stable|preview] [--desktop-terminal]' >&2; exit 1 ;;
  esac
done
case "$channel" in stable|preview) ;; *) echo 'Channel must be stable or preview.' >&2; exit 1 ;; esac
MIN_HERDR_VERSION="0.7.5"
# Where the standalone (non-Herdr) binary lands. Overridable for anyone who
# does not want it on their PATH by default.
INSTALL_DIR="${MUQUN_GATEWAY_INSTALL_DIR:-$HOME/.local/bin}"

color=0
if [ -t 1 ] && [ "${TERM:-dumb}" != dumb ] && [ "${NO_COLOR+x}" != x ]; then
  color=1
  printf '\033[1;36mMuqun Gateway\033[0m\n'
fi
green() { if [ "$color" = 1 ]; then printf '\033[1;32m%s\033[0m\n' "$1"; else printf '%s\n' "$1"; fi; }
info()  { if [ "$color" = 1 ]; then printf '\033[1;36m==>\033[0m %s\n' "$1"; else printf '==> %s\n' "$1"; fi; }
warn()  { if [ "$color" = 1 ]; then printf '\033[1;33m!\033[0m %s\n' "$1"; else printf '! %s\n' "$1"; fi; }
stage_number=0
failure_reported=0
staging=""
probe_pid=""
watchdog_pid=""
stage() { stage_number=$((stage_number + 1)); printf '\n[%s] %s\n' "$stage_number" "$1"; }
stage_ok() { green "[$stage_number] OK"; }
die() {
  failure_reported=1
  printf '[%s] FAILED\nError: %s\n' "$stage_number" "$1" >&2
  printf 'Summary: stopped at stage %s.\n' "$stage_number" >&2
  exit 1
}
finish() {
  stop_probe
  if [ "$1" -ne 0 ] && [ "$failure_reported" = 0 ]; then
    printf '[%s] FAILED\nInstallation stopped; see the error above.\n' "$stage_number" >&2
    printf 'Summary: stopped at stage %s.\n' "$stage_number" >&2
  fi
  if [ -n "$staging" ]; then rm -rf "$staging"; fi
}
stop_probe() {
  if [ -n "$watchdog_pid" ]; then
    kill "$watchdog_pid" 2>/dev/null || true
    wait "$watchdog_pid" 2>/dev/null || true
    watchdog_pid=""
  fi
  if [ -n "$probe_pid" ]; then
    kill -KILL "$probe_pid" 2>/dev/null || true
    wait "$probe_pid" 2>/dev/null || true
    probe_pid=""
  fi
}
trap 'finish $?' 0
trap 'exit 1' HUP INT TERM

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
stage "Check platform and backends"
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

if [ "$have_herdr" = 0 ] && [ "$have_tmux" = 0 ] && [ "$desktop_terminal" = 0 ]; then
  die "Neither Herdr (https://herdr.dev) nor tmux was found. Install one of them, then retry."
fi

# All detected backends use one standalone gateway, including Herdr-only hosts.
# A plugin-owned gateway cannot start Herdr after reboot: it depends on Herdr
# already running. Existing plugin pairings are imported below.
if [ "$have_herdr" = 0 ] && [ "$have_tmux" = 0 ]; then
  setup_backend=desktop
elif [ "$have_tmux" = 1 ]; then
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

stage_ok
stage "Check install destination"
mkdir -p "$INSTALL_DIR" || die "Cannot create install directory: $INSTALL_DIR"
binary="$INSTALL_DIR/muqun-gateway"

if [ -L "$binary" ] || { [ -e "$binary" ] && [ ! -f "$binary" ]; }; then
  die "Refusing symlink or nonregular destination: $binary"
fi
if [ -e "$binary" ]; then
  stage_ok
  stage "Check protected updater"
  # An exit-zero unknown command is not capability evidence. Match the actual
  # protected updater's help, not just any old CLI accepting 'update --help'.
  if [ -x "$binary" ] && update_help="$("$binary" update --help 2>/dev/null)" &&
     printf '%s\n' "$update_help" | grep -Fq 'Usage: muqun-gateway update [OPTIONS]' &&
     printf '%s\n' "$update_help" | grep -Fq 'newer stable official release (never downgrades)' &&
     printf '%s\n' "$update_help" | grep -Fq -- '--check'; then
    if [ "$channel" = preview ] && ! printf '%s\n' "$update_help" | grep -Fq -- '--channel'; then
      bootstrap=1
      info "This installed updater predates channels. Downloading a verified preview helper for the protected transition."
    else
      stage_ok
      stage "Run protected update"
      info "Using the installed verified updater (with lifecycle rollback)."
      if [ "$channel" = preview ]; then
        "$binary" update --channel preview || die "Protected preview update failed; no shell replacement or restart was attempted."
      else
        "$binary" update || die "Protected update failed; no shell replacement or restart was attempted."
      fi
      stage_ok
      green "Summary: protected updater finished."
      echo "Running state and setup left to the updater."
      echo "Open the pairing QR any time with: $binary manage"
      exit 0
    fi
  else
  die "This install lacks the protected updater; the existing binary was retained.
  Back up your Gateway config and state before a manual migration. Build a
  reviewed release from source, or verify its tagged binary against the matching
  .sha256 asset, then stop the Gateway through its existing service owner before
  replacing it. Preserve config/state and keep the old image for manual recovery.
  Do not delete the old binary to bypass this check."
  fi
fi

if [ -z "$target" ]; then
  die "No prebuilt binary found for this platform ($os/$arch). Build from source instead:
  git clone https://github.com/$REPO.git
  cd $(basename "$REPO")
  cargo build --release
  ./target/release/muqun-gateway setup --backend $setup_backend
  ./target/release/muqun-gateway start
  ./target/release/muqun-gateway manage"
fi
stage_ok

stage "Resolve $channel release"
command -v curl >/dev/null 2>&1 || die "curl is required to download the binary."
if command -v sha256sum >/dev/null 2>&1; then
  hash_command=sha256sum
elif command -v shasum >/dev/null 2>&1; then
  hash_command=shasum
else
  die "sha256sum or shasum is required to verify the binary."
fi
if [ "$os" = Darwin ]; then
  command -v perl >/dev/null 2>&1 || die "System Perl is required for atomic no-clobber publication on macOS; nothing installed."
fi

# Private, unique staging on the destination filesystem. Never touch a shared
# .new name or clean any path not created by this invocation.
umask 077
staging="$(mktemp -d "$INSTALL_DIR/.muqun-gateway-install.XXXXXX")" || die "Cannot create private staging directory."

# GUI bootstrap reads these fixed records from stderr; stage labels and
# diagnostics on the normal CLI remain independent of the machine protocol.
gui_progress() {
  if [ "${MUQUN_INSTALL_PROGRESS:-}" = 1 ]; then
    printf 'MUQUN_PROGRESS %s\n' "$1" >&2
  fi
}

download_sample() {
  received=0
  if [ -f "$output" ]; then received=$(wc -c < "$output"); fi
  # curl may follow redirects. Only the final successful response can name
  # the download size. Missing/chunked lengths are reported as zero.
  total=$(LC_ALL=C awk -v cap="$max_bytes" '
    /^HTTP\// {ok=($2 == 200); size=0; chunked=0}
    ok && tolower($1) == "content-length:" {
      sub(/\r$/, "", $2)
      if ($2 ~ /^[0-9]+$/ && $2+0 <= cap) size=$2+0
    }
    ok && tolower($1) == "transfer-encoding:" && tolower($0) ~ /chunked/ {chunked=1}
    END {printf "%.0f", chunked ? 0 : size+0}
  ' "$headers")
  sample="download $((received + 0)) $total"
  if [ "$sample" != "$last_sample" ]; then
    gui_progress "$sample"
    last_sample=$sample
  fi
}

download() (
  # curl versions before 8.4 only enforce --max-filesize for known lengths.
  # The file-size limit also bounds chunked writes. Its unit is the shell's:
  # 512-byte blocks in dash (Debian/Ubuntu /bin/sh), 1024 in bash. Size it for
  # 512 so the backstop is never tighter than the cap; the exact cap is still
  # --max-filesize plus the size check below.
  ulimit -c 0 || exit 1
  ulimit -f "$((($3 + 511) / 512))" || exit 1
  url="$1" output="$2" max_bytes="$3"
  report=${4:-}
  set -- -fsSL
  if [ "$color" = 1 ] && [ -t 2 ]; then
    # curl reports measured bytes/known Content-Length, never a simulated %.
    # Let curl size its own progress display to the terminal.
    set -- -fSL --progress-bar
  fi
  set -- "$@" --proto '=https' --proto-redir '=https' --connect-timeout 10 \
    --max-time 120 --max-filesize "$max_bytes" "$url" -o "$output"
  if [ "$report" = binary ] && [ "${MUQUN_INSTALL_PROGRESS:-}" = 1 ]; then
    headers="$staging/download-headers"
    : > "$headers"
    last_sample=
    download_sample
    curl --dump-header "$headers" "$@" &
    transfer=$!
    trap 'kill "$transfer" 2>/dev/null || :; wait "$transfer" 2>/dev/null || :; exit 1' HUP INT TERM
    while kill -0 "$transfer" 2>/dev/null; do
      download_sample
      sleep 0.25
    done
    wait "$transfer" || exit 1
    trap - HUP INT TERM
    download_sample
  else
    curl "$@" || exit 1
  fi
  [ "$(wc -c < "$output")" -le "$max_bytes" ]
)

# Resolve once, then fetch both objects from this immutable version. The
# text endpoint exposes the same channel pointer as the JSON update API.
api="https://muqun.dev/api/releases/gateway"
download "$api/$channel/version" "$staging/channel-version" 256 || die "Cannot resolve the $channel Gateway release; nothing installed."
version="$(LC_ALL=C awk -v channel="$channel" '
  NR == 1 {
    version = $0
    number = "(0|[1-9][0-9]*)"
    pattern = "^" number "[.]" number "[.]" number
    if (channel == "preview") pattern = pattern "-preview[.][1-9][0-9]*"
    if ($0 !~ (pattern "$")) exit 1
    count = split($0, parts, /[.]|-preview[.]/)
    for (i = 1; i <= count; i++) {
      if (length(parts[i]) > 20 ||
          (length(parts[i]) == 20 && ("v" parts[i]) > "v18446744073709551615")) exit 1
    }
  }
  END { if (NR != 1) exit 1; print version }
' "$staging/channel-version")" || die "Invalid $channel Gateway version; nothing installed."
tag="v$version"
stage_ok
stage "Download binary and checksum"
asset="muqun-gateway-$target"
download_url="$api/$tag/$asset"
download "$download_url" "$staging/$asset" 268435456 binary || die "Binary download failed; nothing installed."
gui_progress verify
download "$download_url.sha256" "$staging/checksum" 1024 || die "Checksum download failed; nothing installed."
stage_ok
stage "Verify release identity"
expected_hash="$(awk -v name="$asset" '
  NR != 1 { bad = 1 }
  NR == 1 {
    hash = substr($0, 1, 64)
    if (length(hash) != 64 || hash ~ /[^0-9a-f]/ || substr($0, 65) != "  " name) bad = 1
  }
  END { if (NR != 1 || bad) exit 1; print hash }
' "$staging/checksum")" || die "Malformed checksum or wrong asset name; nothing installed."
if [ "$hash_command" = sha256sum ]; then
  actual_hash="$(sha256sum "$staging/$asset")" || die "Cannot hash downloaded binary."
else
  actual_hash="$(shasum -a 256 "$staging/$asset")" || die "Cannot hash downloaded binary."
fi
actual_hash="${actual_hash%% *}"
[ "$actual_hash" = "$expected_hash" ] || die "$tag $asset SHA-256 mismatch (expected $expected_hash, got $actual_hash); nothing installed."
info "$tag  $asset  SHA-256 verified: $actual_hash"
# Mirror platform/update.rs: only native 64-bit little-endian ELF/Mach-O with
# the expected CPU may reach --version. Read exactly the bounded header, not
# file(1)'s descriptive output or an executable shell script.
header="$(od -An -v -tx1 -N20 "$staging/$asset")" || die "Cannot inspect executable header."
printf '%s\n' "$header" | awk -v os="$os" -v arch="$arch" '
  { for (i = 1; i <= NF; i++) bytes[++count] = $i }
  END {
    if (count != 20) exit 1
    if (os == "Linux") {
      magic = bytes[1] bytes[2] bytes[3] bytes[4] bytes[5] bytes[6] bytes[7]
      cpu = bytes[19] bytes[20]
      exit !(magic == "7f454c46020101" && cpu == (arch == "x86_64" ? "3e00" : "b700"))
    }
    magic = bytes[1] bytes[2] bytes[3] bytes[4]
    cpu = bytes[5] bytes[6] bytes[7] bytes[8]
    exit !(os == "Darwin" && magic == "cffaedfe" && cpu == (arch == "x86_64" ? "07000001" : "0c000001"))
  }
' || die "Release binary does not match this native platform; not executed or installed."
chmod 755 "$staging/$asset"
# Only this staged native image is probed, with no input, bounded output and
# a five-second wall deadline. Reap both the probe and its watchdog on every
# exit; never print untrusted version output or fall back to publication.
probe_blocks=1
probe_staged() {
(
  ulimit -c 0 && ulimit -f "$probe_blocks" || exit 1
  exec "$staging/$asset" "$@"
) </dev/null > "$staging/version" 2>/dev/null &
probe_pid=$!
(
  timer=""
  trap 'if [ -n "$timer" ]; then kill "$timer" 2>/dev/null || true; wait "$timer" 2>/dev/null || true; fi; exit 0' HUP INT TERM
  sleep 5 &
  timer=$!
  wait "$timer" || exit 0
  kill -KILL "$probe_pid" 2>/dev/null || true
) >/dev/null 2>&1 &
watchdog_pid=$!
probe_status=0
wait "$probe_pid" || probe_status=$?
probe_pid=""
stop_probe
[ "$probe_status" = 0 ]
}
probe_staged --version || die "Staged binary version probe failed or timed out; nothing installed."
[ "$(wc -c < "$staging/version")" -le 256 ] || die "Staged binary version output exceeded its bound; nothing installed."
awk -v expected="muqun-gateway ${tag#v}" '
  NR != 1 || $0 != expected { bad = 1 }
  END { exit (NR != 1 || bad) }
' "$staging/version" || die "Staged binary version does not match $tag; nothing installed."
if [ "$setup_backend" = desktop ] && [ "$bootstrap" = 0 ]; then
  probe_blocks=16
  probe_staged setup --help &&
    grep -Eq '^[[:space:]]*--desktop-terminal([[:space:]]|$)' "$staging/version" ||
    die "Gateway $tag predates built-in Desktop terminal support. Install a newer Gateway release; nothing installed."
fi
stage_ok
if [ "$bootstrap" = 1 ]; then
  stage "Run protected preview bootstrap"
  probe_staged bootstrap-preview --help &&
    grep -Fxq 'Usage: muqun-gateway bootstrap-preview' "$staging/version" || die "This preview predates protected bootstrap support. A newer preview is needed; the existing installation was retained."
  # The verified helper has a fixed, validated standalone destination. It owns
  # update/lifecycle locks, replacement and rollback. Never publish it here or
  # enter the fresh-install setup path for an existing installation.
  "$staging/$asset" bootstrap-preview || die "Protected preview bootstrap failed; follow its diagnostic above. No shell replacement, setup or restart was attempted."
  stage_ok
  green "Summary: protected preview bootstrap finished."
  echo "Open the pairing QR any time with: $binary manage"
  exit 0
fi
stage "Publish verified binary"
gui_progress install
# A concurrent installer must not clobber an image published since our check.
# Hard-link publication refuses existing files; also refuse directory/symlink
# destinations rather than letting ln interpret them as a target directory.
[ ! -e "$binary" ] && [ ! -L "$binary" ] || die "Destination appeared during installation; nothing replaced."
if [ "$os" = Linux ]; then
  ln -T "$staging/$asset" "$binary" || die "Cannot publish binary; nothing replaced."
else
  # Apple's ln AND link utility route directory targets through linkit().
  # System Perl (also used by shasum) calls link(2) directly: an unexpected
  # directory, symlink or file is atomically EEXIST, never a target directory.
  perl -e 'link($ARGV[0], $ARGV[1]) or die "Atomic publication failed: $!\n"' \
    -- "$staging/$asset" "$binary" || die "Cannot publish binary; nothing replaced."
fi
info "Installed verified $tag binary for $target to $binary (SHA256 integrity, not signing)."
stage_ok
stage "Configure first-run setup"

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
  # or one already adopted on an earlier run of this script -- and a notice,
  # not an import, when the standalone install already has devices paired to
  # its own identity: adopting a leftover plugin pairing would re-point them.
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
if [ "$setup_backend" = desktop ]; then
  set -- setup --desktop-terminal
else
  set -- setup --backend "$setup_backend"
fi
if [ -n "${MUQUN_GATEWAY_PORT:-}" ]; then
  set -- "$@" --port "$MUQUN_GATEWAY_PORT"
fi
if [ "$have_tmux" = 1 ] && [ -n "${MUQUN_GATEWAY_TMUX_SOCKET:-}" ]; then
  set -- "$@" --socket-path "$MUQUN_GATEWAY_TMUX_SOCKET"
fi
"$binary" "$@"

# Cache the published command suggestions once. A network outage must not
# break pairing or service setup; suggestions remain empty until retry.
if ! "$binary" commands update --if-missing; then
  warn "Command catalog unavailable. Retry later with: $binary commands update"
fi

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

if [ "$setup_backend" = desktop ]; then
  info "Muqun Desktop starts its built-in terminal daemon. Gateway pairing and agent sessions are available independently."
else
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
fi

stage_ok
stage "Start Gateway and ask autostart"
gui_progress start
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

stage_ok
stage "Check Gateway readiness"
# Confirm it actually came up. A service manager accepting the job, or a child
# being spawned, is not a gateway answering on its port: on v0.12.1 a launchd
# reload could fail halfway and this script still went on to call the result
# running. Give it a moment to bind, then say plainly if it did not. Only an
# explicit "not running" is a failure: a host that cannot answer at all (no
# `ss` on a minimal Linux, say) gets a warning rather than a failed install.
waited=0
gateway_state=unknown
while [ "$waited" -lt 20 ]; do
  case "$("$binary" service status 2>/dev/null)" in
    *"gateway: running"*) gateway_state=running; break ;;
    *"gateway: not running"*) gateway_state=stopped ;;
    *) gateway_state=unknown ;;
  esac
  waited=$((waited + 1))
  sleep 1
done
case "$gateway_state" in
  running) ;;
  stopped)
    die "The gateway did not start. Its log is in the gateway's state directory
  (gateway.log). Check with: $binary service status
  Then start it with: $binary service install   (or: $binary start)" ;;
  *) warn "Could not confirm that the gateway is running. Check with: $binary service status" ;;
esac
if [ "$gateway_state" = running ]; then
  stage_ok
else
  warn "[$stage_number] UNCONFIRMED"
fi

echo
if [ "$config_existed" = 1 ]; then
  green "Summary: Muqun Gateway is updated (pairings kept)."
else
  green "Summary: Muqun Gateway is configured."
fi
if [ "$gateway_state" = running ]; then
  echo "Gateway readiness: running."
else
  echo "Gateway readiness: unconfirmed; check service status."
fi
if [ "$setup_backend" = desktop ]; then
  echo "Terminal backend: Muqun Terminal (provided by Desktop)."
else
  echo "Backends configured:"
  "$binary" backend list | awk -F'\t' '{ printf "  %s %s (%s)\n", (NR == 1 ? "*" : " "), $1, $2 }'
  echo "(* default)"
fi
echo
echo "Open the pairing QR any time with:"
echo "  $binary manage"

# A fresh install has one thing left to do: pair the phone. Hand the terminal
# straight to `manage`, which opens on the QR when nothing is paired yet, so
# the reader is not left copying a command out of a summary. stdin is the
# script itself on the `curl ... | sh` path, so `manage` reads /dev/tty. An
# update keeps its pairings and gets the hint only; so does a run with no
# terminal, or one whose gateway did not confirm it is up.
if [ "$config_existed" = 0 ] && [ "$gateway_state" = running ] && [ -t 1 ] &&
   (exec 3< /dev/tty) 2>/dev/null; then
  echo
  info "Opening the pairing QR now. Scan it with the Muqun app; press q to leave."
  "$binary" manage < /dev/tty || warn "Could not open the pairing screen. Run: $binary manage"
fi

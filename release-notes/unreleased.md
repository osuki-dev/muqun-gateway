# Unreleased

## Changes and fixes

- Herdr terminal input preserves literal spaces by translating single-space
  key events to Herdr's named space key without changing event order
- Installation can explicitly opt configured tmux/Herdr backends into starting
  with the gateway. Enter, no, invalid input, and noninteractive installs keep
  existing settings; new installations default to off
- Running terminal servers are reused. A stopped tmux backend starts a detached
  shell, and Herdr starts headless. Startup is one-shot and does not block HTTP
  availability, repeatedly spawn processes, or repeatedly log failures
- Service definitions preserve terminal child processes across gateway restarts
- Linux service definitions quote paths and environment values safely, including
  spaces, quotes, backslashes, dollar signs, and systemd percent specifiers
- The installer preserves custom tmux socket paths as one argument, including
  whitespace, quotes, and wildcard characters
- Herdr-only installations use a standalone gateway, with guarded migration of
  existing plugin pairings. Live or inconsistent source installations require
  operator attention instead of silently replacing pairing identity
- Herdr defaults and plugin migration use Herdr's XDG paths on macOS as well as
  Linux
- Session responses include `connected`, so Apps can exclude stopped backends
  from the switcher without deleting their configuration
- Explicit read-only file previews can resolve files under platform temporary
  and cache directories. Workspace file browsing remains workspace-scoped;
  preview lookups do not expose directory listings or grant write access

## Compatibility and operation

- Historical local headless-start/reuse checks passed against installed Herdr
  and tmux. These checks do not verify operating-system reboot behavior
- Existing gateway configurations do not opt in automatically. Configure a
  backend with `muqun-gateway backend autostart <id> on|off`
- Standard default/named Herdr sessions are supported for autostart; custom
  socket paths whose persistence location is unknown remain manual
- Existing Apps can ignore the additive session field. The updated App also
  reads older gateways' per-session health information
- Reinstall the service after upgrading to apply process-lifetime settings.
  macOS starts at user login; Linux boot startup requires user-service lingering
- No system reboot, production deployment, tag, or release was performed during
  local validation. Linux boot and iOS device validation remain to be performed

Before release, move these notes to the chosen `vX.Y.Z.md`, add the other changes
included in that release, and update the validation notes with final evidence

# Agent permission controls

Audited on 2026-11-15 against installed CLI help, discovery, and the Gateway adapters. This is a capability audit, not a claim that permission changes were exercised in every live account. No user's session permission was changed during the audit.

| Agent | Native control | When the Gateway applies it | Composer presentation |
| --- | --- | --- | --- |
| Claude | `bypassPermissions` | Native session mode | One mode icon; no duplicate bypass toggle |
| Qoder | ACP `yolo` / `bypassPermissions` | Native session mode; CLI uses different snake_case spelling | Deduplicate either offered ACP alias |
| Codex | Approval policy and sandbox override | Next turn and subsequent turns | Separate permission icon; next-turn explanation |
| OpenCode | Session permission rules | Session rule update | Keep separate from Build/Plan/persona selector |
| T3 | `full-access` runtime mode | Thread runtime-mode command | Separate permission icon |
| FX | `FX_PERMISSION_MODE=full-access` | Relaunch idle session child | Disable toggle while running; explain restart |
| Cursor | `--force` | Relaunch idle session child | Disable toggle while running; explain restart |
| Antigravity | `--dangerously-skip-permissions` | Relaunch idle session child | Disable toggle while running; explain restart |
| Pi | Muqun extension `MUQUN_PI_APPROVALS` | Process startup only | No live session toggle |
| DeepSeek | Per-request decisions | No bypass control | No bypass toggle |

CLI evidence: Claude, Codex, OpenCode, Qoder, Cursor, Antigravity, FX, Pi and T3 help were read locally without starting a model turn. FX's environment policy and Pi's extension gate were checked in the adapters; they are not advertised as CLI flags. DeepSeek is the Gateway service adapter, not a separate installed CLI. T3's server `--mode` (web/desktop) is unrelated to its thread permission mode.

Discovery retains `features.autoApprove.scope` and `native` for existing Apps, and adds `applies` (`immediate`, `next_turn`, `restart_when_idle`, `next_launch`) and `modeIds`. Desktop only offers a separate toggle when discovery explicitly declares session support and no offered mode already represents that control. Missing capability information does not imply support. Native agent policies, explicit denies, and sandbox constraints remain authoritative; the UI does not promise that every request is automatically approved.

This change does not modify saved defaults, switch any existing session into bypass, or change the Gateway's permission enforcement. Existing backend busy checks remain in force even if an older client offers the control while a turn is running.

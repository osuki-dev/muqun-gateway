# Gateway preview

## Installation and updates

Gateway now resolves the stable and preview channels on muqun.dev. Each version has a manifest for all four Linux and macOS binaries. The installer and self-updater verify the download against its SHA-256 before running it and display the verified digest. Use `muqun-gateway update --channel preview --check` to check for a preview, and omit `--check` to install it. Installation retains the existing protection against downgrades.

On a fresh computer with Muqun Desktop, Gateway can pair before Desktop opens and discover the bundled terminal when it starts. Existing tmux and Herdr configurations are preserved.

## Agents, messages and history

- **Model-led Multi-Agent Orchestration (MAO) and enhanced MCP tools**: Orchestration now defaults to Lead-Model orchestration (`auto_review: false`, advertised via `agent_orchestration_model_led_v1`, alongside `agent_orchestration_pool_v1` and `agent_orchestration_router_v1`) so Gateway does not force a cold Reviewer pass after every turn while still offering optional automatic code review, on-demand Ask Advisor, dynamic agent/model pools, and automatic quota failover. MCP adds batch `tasks.wait` (`task_ids`, up to 16 tasks, `all`/`any`), warm child session reuse (`tasks.delegate(reuse_task_id)`), rich `fleet.describe` with available models/modes and live 400 ms quota health, and automatic output reduction and secret redaction.
- **Instant cold-open session previews across all 11 agents**: Native tail readers for Claude Code, Antigravity, Cursor, Qoder, and GitHub Copilot CLI plus a universal encrypted SQLite `session_preview_cache` serve recent conversation history immediately on cold open after restarts or idle eviction. ACP recovery snapshots run asynchronously off the foreground open path, and Claude Code reuses cached `claude-agent-acp` binaries directly.
- **Orchestration task titles and attachments**: Orchestration tasks preserve real session titles across Desktop and mobile App Home views (dropping the legacy `"Orchestration"` placeholder from both `TaskView.label` and `TaskView.title`). Markdown inline `[label](file://...)` references are no longer promoted as broken `message_assets`. Remote WebSocket sync recovers stalled connections via ping/pong timeouts, and active ACP terminal followers extend their deadline cleanly.
- GitHub Copilot CLI is available as an opt-in local ACP agent. It uses the CLI's existing sign-in and exposes the session modes, models, permissions and usage supported by that CLI. Selecting Autopilot requires the owner's consent.
- Opening a large session can start with a bounded window of retained messages. Earlier items remain available through pagination. Claude usage is recovered from large transcripts, Pi reads its active branch using indexed native records, and Qoder can index large recovery journals on disk.
- Agent timeline delivery preserves attachments and the origin of a prompt. A queued message appears in conversation history when the provider delivers it. OpenCode receipts reconcile with the provider's own user message, including intentional repeated sends.
- Paired clients can recover pending approvals and questions on reconnect. Uploads support legacy Word, Excel and PowerPoint attachments identified by their file formats.

## Compatibility and upgrade

Linux and macOS packages are available for x86-64 and ARM64. Herdr 0.7.5 or later remains supported. Herdr collaboration features require 0.9.0 or later. Existing Apps can continue using their current endpoints. Optional capabilities such as retained history windows and pending-request recovery require client support.

Back up the Gateway store before upgrading, particularly from an older stable release. Finish any active agent turns and restart Gateway to load the new binary. Startup applies pending store migrations. If rollback crosses a schema change, restore the database backup that matches the earlier binary. Existing transport-encrypted pairings carry over. Older unencrypted devices must pair again on a Gateway configured to require encrypted transport.

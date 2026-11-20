# Gateway preview

## Installation and updates

Gateway now resolves the stable and preview channels on muqun.dev. Each version has a manifest for all four Linux and macOS binaries. The installer and self-updater verify the download against its SHA-256 before running it and display the verified digest. Use `muqun-gateway update --channel preview --check` to check for a preview, and omit `--check` to install it. Installation retains the existing protection against downgrades.

On a fresh computer with Muqun Desktop, Gateway can pair before Desktop opens and discover the bundled terminal when it starts. Existing tmux and Herdr configurations are preserved.

## Agents, messages and history

- GitHub Copilot CLI is available as an opt-in local ACP agent. It uses the CLI's existing sign-in and exposes the session modes, models, permissions and usage supported by that CLI. Selecting Autopilot requires the owner's consent.
- Opening a large session can start with a bounded window of retained messages. Earlier items remain available through pagination. Claude usage is recovered from large transcripts, Pi reads its active branch using indexed native records, and Qoder can index large recovery journals on disk.
- Agent timeline delivery preserves attachments and the origin of a prompt. A queued message appears in conversation history when the provider delivers it. OpenCode receipts reconcile with the provider's own user message, including intentional repeated sends.
- Paired clients can recover pending approvals and questions on reconnect. Uploads support legacy Word, Excel and PowerPoint attachments identified by their file formats.

## Compatibility and upgrade

Linux and macOS packages are available for x86-64 and ARM64. Herdr 0.7.5 or later remains supported. Herdr collaboration features require 0.9.0 or later. Existing Apps can continue using their current endpoints. Optional capabilities such as retained history windows and pending-request recovery require client support.

Back up the Gateway store before upgrading, particularly from an older stable release. Finish any active agent turns and restart Gateway to load the new binary. Startup applies pending store migrations. If rollback crosses a schema change, restore the database backup that matches the earlier binary. Existing transport-encrypted pairings carry over. Older unencrypted devices must pair again on a Gateway configured to require encrypted transport.

# Desktop / Gateway acceptance — 2026-11-14

Work stays on `feat/desktop`. No merge to `main`, push, tag, release or installed
Gateway replacement is part of this pass.

## Scope and existing work

The starting worktree contained 25 modified files and the new
`desktop/src/ui/settings/plugins.rs`. That work is retained: plugin enablement
and runtime settings; Dwindle, scrolling and floating layouts; shortcut routing;
composer selection, queue controls and optimistic message reconciliation; and
their locale entries. It is not treated as already published.

This acceptance pass additionally fixes the shared connection path:

- Agent and task forwarding survive broadcast lag, request a fresh projection,
  and continue delivering subsequent events.
- A subscriber to connection status follows computer switches instead of
  remaining attached to the previous computer. Initial status is also delivered.
- Every successful socket connection requests catch-up, including the first
  connection after startup reads failed or preceded subscription.
- An idle WebSocket no longer holds the old computer's hub alive after switching
  away. Handshake and encrypted hello reads are bounded.
- Startup restores the configured remote computer before issuing session reads,
  instead of showing its name while connecting to the local Gateway.

## Evidence and limits

The local run uses a separate Gateway config, SQLite store, port and credentials,
a named Herdr session `muqun-desktop-qa-20261114`, a new native Codex conversation,
and separate Desktop config/data directories. The installed Gateway and the
main Herdr session are not restarted or used for experiments. Folder trust is off;
no trust/approval prompt is answered by the harness.

| Path | Evidence | Limit |
| --- | --- | --- |
| Shared sessions | The actual release Desktop connects over the local-client credential, opens the isolated Gateway's native Codex session, and renders its response in the real Rune element tree. | This is Desktop/Gateway evidence, not paired mobile App acceptance. |
| Send and follow-up | A real native Codex session answers an initial prompt and a contextual follow-up. The actual Desktop composer then sends another message through scripted paste/Enter; both Gateway timeline and Desktop tree contain the response. | No tool execution or approval flow is exercised. |
| Terminal restore and focus | The actual release Desktop is closed and reopened: shell PID is unchanged and old output renders again. With a command running in its terminal, focus is moved to the composer and Ctrl+C is pressed; the command still completes. The daemon also preserves an exported variable across client reattachment. A separate real Herdr background-input request preserves the original focused pane. | Desktop restart is not a machine-reboot persistence claim; mobile acceptance is separate. |
| Reconnect | A TCP proxy disconnects only the isolated Desktop link for two seconds. The actual Desktop reconnects, emits resync, rereads the timeline and exits successfully with the response still rendered. Regression tests also exercise switching an existing status listener to another socket, first-connect catch-up, and releasing abandoned hubs. | Local fault injection does not establish behavior on every network or a paired encrypted mobile connection. |

The final composer check cleared only the isolated QA draft before pasting.
The Gateway recorded the complete prompt verbatim and answered `QA_CLEAN_ORCHID`;
the actual Desktop tree rendered that same answer. No ambiguous prompt was retried.
A second fault-injection run restored the saved `qa-remote` profile at startup,
then disconnected and recovered it; its log confirms `hub: restored remote computer`.

## Mobile follow-up (outside this Desktop result)

Per the user's sequencing instruction, finish Desktop before continuing mobile
integration. No mobile acceptance pass is claimed here. The initial Mac probe
found the installed App and could read its UI, but XCTest input failed because
running QA simulators referenced deleted `testmanagerd` Unix sockets. Both
agent-device sessions opened for that probe were closed; no simulator was
restarted and no pairing or agent prompt was submitted from the App.

A later paired-App check must verify readiness before sending once, the actual
answer, a follow-up, and unchanged original terminal focus against an isolated
Herdr session. Never approve trust prompts or retry an ambiguous submission
automatically. Run `proxy_on` in a new terminal before starting agents on the Mac.

## Artifacts

Local raw evidence is under `/tmp/muqun-desktop-acceptance-20261114/`:
`turn-results.json`, `terminal-results.json`, `focus-results.json`,
`reconnect-results.json`, `remote-reconnect-results.json`,
`after-desktop-verified.json`, `desktop-terminal-results.json`, Desktop logs
and Rune element trees. `desktop-clean-tree.json` records the verified exact-copy
composer response; `desktop-terminal-restored-tree.json` records terminal output
after restarting Desktop.
Credentials in that directory are private temporary test data and must never
be committed. Regression tests live alongside `hub/dynamic.rs` and
`hub/remote.rs`; no one-off acceptance script is added to the repository.

## Local checks

- `cargo fmt --all -- --check`: passed.
- `cargo clippy --all-targets -- -D warnings`: passed.
- `cargo clippy -p muqun-desktop --all-targets --locked -- -D warnings`: passed.
- `cargo test --locked`: 2,291 passed, zero failed, 55 ignored.
- `cargo test -p muqun-desktop --locked`: 304 passed, zero failed, one ignored.
- `cargo build --release --locked -p muqun-desktop -p muqun-gateway`: passed;
  Desktop rebuilt again after the connection fixes.
- Theme-token lint and `git diff --check`: passed.

The ignored environment-dependent tests are not counted as acceptance passes.

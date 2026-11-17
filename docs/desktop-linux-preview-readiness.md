# Desktop Linux preview closeout — 2026-11-14

Status: local candidate validated; **not cleared for external publication**.
Work remains on `feat/desktop`; no main merge, tag, push or release was performed.

## Completed in this closeout

- Reproduced a real terminal input stall: when a raw connection stopped draining
  output, the daemon stopped polling its keyboard input. The regression timed
  out after two seconds before the fix. Independent connection-scoped input and
  output futures now let the same test finish in about 0.12 seconds, with output
  still unread. This establishes one cause, not resolution of every possible
  interactive delay under a live agent workload.
- Moved platform-specific plugin paths behind the platform module, satisfying
  the Desktop CI architecture boundary.
- Added a Linux archive containing the binary, launcher, icons, installer,
  uninstaller and review-center plugin. CI collects the archive and SHA-256.
  Package-owned plugins have a separate directory; user plugins take precedence.
- Installed from the extracted archive outside the source checkout with fresh
  config/data directories. The installed UI discovered review-center; its
  screenshot was inspected. A prefix containing spaces and an ampersand passed
  launcher validation. Reinstall succeeded; uninstall preserved custom plugins
  and extra user files in the bundled plugin directory.

## Verification

| Check | Actual result |
| --- | --- |
| `cargo fmt --all -- --check` | Passed |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | Passed |
| `cargo test --locked` | 2,274 library + 16 integration + 1 TLS passed; 55 ignored |
| `cargo test -p muqun-desktop --locked -- --test-threads=1` | 313 passed; 1 ignored |
| Bun review-center response test | Passed |
| Theme tokens, platform boundary, diff whitespace | Passed |
| Locked Desktop release build | Passed |
| Extracted archive install, plugin discovery, reinstall and uninstall | Passed locally |

The ignored tests are not counted as acceptance passes. Logs and screenshots
live under `/tmp/desktop-closeout-*`, `/tmp/desktop-input-backpressure-*` and
`/tmp/muqun-closeout-*`; these are local evidence, not distribution assets.
The candidate archive is under `target/desktop-package/`. Its SOURCE file marks
the dirty working tree, so its recorded HEAD is not a complete source identity.

## Remaining release gates

1. Finish the real paired App checks for shared sessions, readiness before send,
   actual response, contextual follow-up, preserved Herdr focus and encrypted
   disconnect/reconnect. Desktop/Gateway evidence exists in
   [desktop-acceptance.md](desktop-acceptance.md); the existing iOS QA build has
   only passed the smoke checks recorded in
   [desktop-ui-acceptance.md](desktop-ui-acceptance.md).
2. Confirm live agent input latency in the installed UI with the updated daemon.
   The running user daemon was deliberately preserved. Replacing the executable
   or restarting only the UI does not activate this daemon fix; finish its live
   terminal tasks before restarting the daemon. Do not force-kill it to test.
3. Review and organize the remaining pre-existing Desktop worktree changes,
   produce an identifiable candidate from the reviewed commits, and run CI.
   Local checks do not establish a successful hosted CI matrix.
4. Validate the declared Linux distribution baseline on a clean target system.
   This Arch-built local package does not prove older glibc compatibility.
   macOS signing/notarization and Windows distribution remain outside this
   Linux preview scope.
5. Final visual acceptance of the startup/sidebar transition and custom themes.
   Captured frames have been inspected, but Desktop's native transition is not
   App shader parity. Do not describe it as identical to the App.

Plugin installation marketplace, additional PR providers and external-plugin
file associations remain deferred features, not claims of this preview. Release
publication requires a separate explicit user request and reviewed release notes.

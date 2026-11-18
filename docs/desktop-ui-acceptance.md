# Desktop workspace and input UI review

Scope: the existing `feat/desktop` worktree, without merging, committing,
publishing or changing the phone App. This supplements `desktop-acceptance.md`;
the earlier transport acceptance is not a visual-design acceptance.

## Changes

- Scrolling uses readable columns with the focused column brought into view.
  Directional navigation and drag targets use the displayed layout, including
  floating positions, instead of always using the Dwindle tree.
- Floating panes paint above their siblings when focused. Drag and resize track
  the pointer directly; fitted geometry keeps controls within the workspace.
  Layout changes settle on a 240 ms non-bouncing spring. Matched transitions crop
  content instead of enlarging text; reduced motion disables the spatial motion.
- Composer and terminal keyboard routing preserve workspace layout and zoom
  chords. The terminal still receives plain shell editing chords.
- Terminal tabs use shared numbered labels, a restrained active underline,
  directory basenames with full-title tooltips, and a horizontal viewport.
  New-tab and directory actions remain outside the scrollable labels.
- Composer has a quiet surface, focus border, and a separator above its controls.
  Narrow panes put model/mode controls on their own row; empty drafts disable
  Send. Attachments, queueing, interruption and submission use existing handlers.

## UI acceptance method

Run the built Desktop with Rune's GPU headless renderer and scripted pointer/key
input against existing scenes, using isolated config/data directories. These are
the application's real view functions and input handlers with fixture content,
not live agent or paired-phone acceptance. No approval control in a fixture was
clicked and no agent prompt was submitted.

Artifacts and scripts are local to `/tmp/muqun-desktop-ui-review/`. They are not
committed as one-off tests. Screenshots include 1440 px workspaces, narrow split
panes, terminal light/dark appearance and reduced-motion mode.

The first pointer replay exposed swallowed workspace shortcuts after the composer
took focus; the final implementation includes a Runtime regression for composer
and terminal focus. A visual review also exposed crowded composer controls, which
now occupy a separate row in narrow panes.

Final UI replay confirms:

- Clicking an exposed background titlebar brings that pane above the others.
- A pointer drag moves it by exactly 140 × 80 points; the resize gesture adds
  120 × 70 points. Maximize fills the workspace; restore returns to the exact
  saved floating rectangle. Switching back returns it to Dwindle.
- An intermediate frame 80 ms after maximize has geometry between the starting
  and final rectangles: the transition visibly moves rather than cutting.
- The same layout/zoom chords work with the terminal sink focused.
- Shrinking the window to 820 × 620 fits the floating pane inside its 552 × 604
  workspace, retaining its titlebar and bottom-right resize control.
- Terminal tab switching and closing run through their real pointer handlers.
  Light/dark tab screenshots and typed, narrow composer screenshots were reviewed.
- The reduced-motion scene reports reduced motion enabled and reaches the same
  maximized geometry.

Evidence: `acceptance-results.json`, `composer-final.png`, `terminal-final.png`,
`terminal-light.png`, `restored-final.png`, `float-small-final.png`,
`zoom-in-flight.png`, `zoom-settled.png`, and `workspace-motion.mp4` in the artifact
directory. Scene text is fixture content; terminal test-result text in screenshots
is not this run's test report.

## Checks

Gateway: `cargo test --locked` passed (2,291 passed, 55 ignored), and
`cargo clippy --all-targets -- -D warnings` passed.

An initial parallel Desktop run encountered an existing file-lock test failure
in `platform::coexist`; it passed in isolation and the complete serial run.
Ignored tests do not count as acceptance evidence.

Final Desktop serial tests: 306 passed, zero failed, one ignored.
Desktop all-target clippy, formatting, theme-token lint and diff whitespace
checks passed. `cargo build --release --locked -p muqun-desktop` passed on the
final source. One earlier build process was terminated with exit 143 without
a compiler error; the standalone rerun completed successfully in 2m 09s.
Build results are recorded alongside the local artifacts.

## Unified windows, files and extensions (2026-11-14)

The source remains on `feat/desktop`; no merge, publication, or tag was performed.
Window contract and extension examples: `docs/desktop-window-plugins.md`.

Observed through the actual Rune renderer (isolated config/data directories):

- The sidebar Plugins entry opens the installed list. A file-based Workbench Notes
  extension opens as a page and right dock using host controls.
- The built-in file preview opens as a floating plugin window. The underlying
  session fills the workspace instead of retaining an empty popped-tile slot.
- Dragging and resizing change the actual geometry. Maximize → Restore returns
  to the previous geometry (subpixel spring settling is allowed in image traces).
- A narrow 1000×700 window keeps the dock alongside its composer and wraps file
  actions. The file code viewport scrolls rather than widening the dock.
- The review-center plugin ran its Bun command in the background and displayed
  the actual osuki-dev/muqun-gateway result: 3 open PRs and 0 open issues at the
  time of this check. The isolated renderer used the owner's existing gh config
  directory without copying credentials. No repository writes were performed.
- Akiba, Jade, Courier light and the external Sea Glass theme were rendered.
  Custom wallpaper/launch artwork and palette colors are respected.

Artifacts in `/tmp/muqun-desktop-neon-review/` include `review-live-verified.png`,
`file-ready.png`, `file-dragged.png`, `file-restored-final.png`, theme screenshots,
JSON layout traces and videos. These are local QA artifacts, not release assets.

Input checks:

- A real raw PTY receives four arrow escape sequences followed by exactly one
  byte `0x02` after two Ctrl+B Leader presses, through the Desktop Rune Runtime.
- A separate real nvim PTY baseline edited a temporary filename containing spaces
  and measured 60 alternating arrow responses: median ~0.23 ms, p95 ~0.49 ms on
  this machine. This excludes the Desktop compositor and is not an end-to-end
  Desktop latency claim.
- A temporary diagnostic parsed 4 MiB of representative terminal output in
  ~8.8 ms; the diagnostic was removed after measurement. This does not establish
  the cause of the user's intermittent stall.
- The identified synchronous Bun-plugin execution path was moved off the UI
  thread. A regression exercises stdout and stderr beyond pipe capacity.

Limits: the user's intermittent stall under their full real agent workload has
not been reproduced conclusively. No live Claude prompt or paired mobile App
acceptance was performed for these UI changes. Installation marketplace,
Gitea/GitLab PR providers, external-plugin file associations and multiple
simultaneous file previews are not claimed by this implementation.

Final local checks for this iteration:

- Gateway: `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test --locked` passed (2,274 library + 16 integration + 1 TLS tests;
  55 existing ignored tests).
- Desktop: formatting and clippy with `-D warnings` passed; full serial suite
  passed 312 tests, 0 failed, 1 existing ignored. The window lifecycle regression
  additionally checks disabling a plugin and rejecting a subsequent open.
- The Bun PR/Issue response test passed, including partial failure and escaping.
- Final videos were decoded/inspected: file-window interactions are 270 frames
  (4.5 seconds), startup is 132 frames (2.2 seconds), both 1440×900 H.264.
- A release-mode long-timeline fixture replayed 120 arrow-repeat events with the
  terminal panel open: CPU frame p50 ~1.07 ms and p95 ~1.86 ms. This is a frontend
  fixture workload, not a live agent end-to-end latency measurement.

Startup continuity correction (2026-11-14): removed the extra hero frame and
loading subtitle, and replaced the small-image fade with travel into Home's
measured artwork rectangle. The destination is frozen before travel begins and
accounts for Home's visual toolbar lift (layout reads exclude that transform).
The wallpaper also converges on the actual Home backdrop crop. Inspected early,
hold, exit and completed frames from `startup-aligned.mp4`, plus the exit with
the external Sea Glass Reverie theme. This supersedes the earlier startup video:
it had a size jump and a 48-point vertical mismatch. Desktop still uses a native
iris and crossfade; App's silhouette shader and snapshot dust dissolve are not
implemented by this correction.

Sidebar reveal follow-up: retain a full-window theme backdrop beneath the
converging main-card backdrop until the shell crossfade completes. Previously,
the main-card crop exposed a plain dark strip before the sidebar controls
appeared. Feather the converging backdrop edges so the retained theme layer
does not introduce a hard rectangular seam. Visual evidence is
`/tmp/muqun-desktop-neon-review/startup-sidebar-fixed.mp4`.

Installed Desktop verification (2026-11-14): built the locked release candidate,
backed up and atomically replaced the local executable, verified its SHA-256
matches the build, then gracefully restarted only the UI. The original terminal
daemon stayed alive. Actual installed-window capture shows the user's theme,
composer and restored session sidebar; startup logs contain no WARN/ERROR/panic.
The release startup and file drag/resize/maximize/restore scene replay exited
successfully. This installation does not update the Gateway or publish a release.

App follow-up on the Mac QA iOS simulator: repaired a missing testmanagerd
socket by restarting only that simulator, preserving its data. The installed
development app then exposed a stale Metro server whose source checkout no
longer existed. Replaced that broken server with the existing QA source and
verified the unpaired Home screen, Settings, Appearance, theme sheet, built-in
themes and loaded gallery through real taps. Demo terminal/composer accepts text;
the empty/nonempty send-button state changes, and the QA text was cleared.
Screenshots were inspected. No real agent message or pairing was submitted.
These are existing-QA-build smoke checks, not a current-source native build,
full device suite, launch-animation parity, or paired delivery/reconnect pass.

## Delivery verification (2026-11-15)

The latest local candidate passed 339 Desktop tests (one existing ignored test),
2,272 Gateway library tests, 16 lifecycle integration tests and one TLS test.
Formatting and all-target Clippy passed with Rust 1.99.1. Active commands,
background-marked calls and child-session activities remain individually visible;
ordinary completed activity can still be grouped.

The macOS candidate was signed, packaged and copied to the existing user-level
application location. Its installed SHA-256 matched the packaged binary, and the
running process was verified at that location. This is installation evidence,
not complete macOS visual acceptance or public notarization evidence.

Hosted Rust 1.100 builds exposed an async-stream never-type inference error in
the existing tmux activity stream. A semicolon-only change did not resolve it;
that change was reverted. Desktop candidate CI now explicitly uses Rust 1.99.1,
matching both validated local platforms. Compiler-upgrade compatibility remains
separate work. The signing/notarization job is gated on successful builds and
smoke checks; a dispatched job is not a successful notarized release.

A Linux compositor-rule experiment was reverted after a wallpaper display
failure. The wallpaper file and original selection remained intact; restarting
the desktop shell restored rendering. No compositor workaround is part of the
Desktop package. Further visual fixes must be validated inside the application.

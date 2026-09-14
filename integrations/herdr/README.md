# Herdr integration snapshot

This versioned patch publishes the native dependency of Gateway PR #27 and App
PR #81 for review. It has not been merged or released upstream. Keep the companion
PRs in draft until the remaining acceptance checks pass.

- Upstream repository: https://github.com/herdrdev/herdr
- Exact upstream base (0.9.0): `b99002ac99b09e00b4ca692436cb15a6b0d676f1`
- Local implementation commit: `e42a1a1d4a5b249033085f02b1fbcf872785b5ab`
- Patch: `0001-immutable-agent-lifecycle-and-scoped-reporting.patch`
- Patch SHA-256: `72f136f1fb0461c34f60f613713f3d283848d6904286c3a63231e894d248166e`

## Apply and validate

Use an isolated clone of the upstream repository, with no local changes:

```sh
git switch --detach b99002ac99b09e00b4ca692436cb15a6b0d676f1
git switch -c feat/muqun-managed-workflow
git apply --check /absolute/path/to/0001-immutable-agent-lifecycle-and-scoped-reporting.patch
git am /absolute/path/to/0001-immutable-agent-lifecycle-and-scoped-reporting.patch
just check
cargo build --locked
```

Do not overwrite a running user's Herdr binary or session. Run the resulting
binary with an isolated configuration, socket and session for paired QA.

## Contract and maintenance

The patch adds immutable launch/owner identities, bound startup and delivery,
interruption receipts, scoped reporting, and managed agent eligibility metadata.
The Codex reporting adapter grants only context, result submission and receipt
lookup through a per-invocation MCP server; it preserves unrelated approval and
sandbox settings. Gateway capability detection is authoritative. An unpatched
0.9.0 backend does not gain these contracts from its version number alone.

Keep this patch tied to its exact base until upstream integration is agreed.
When rebasing, regenerate the patch and checksum, run native and Gateway checks,
and repeat real paired startup, response, follow-up and reporting checks. Do not
silently add new tool permissions or claim ordinary terminal support establishes
managed task support. Remove the vendored patch only after a released native
capability provides the same tested contracts.

## Evidence and remaining acceptance

The native snapshot passed 3,297 tests with two skipped, its maintenance and
architecture suites, full `just check`, and a debug build. Gateway passed 59
library and 639 binary tests with 20 ignored, formatting, strict all-target Clippy,
and debug/release builds. Real paired Android testing verified Codex startup,
echo, follow-up, scoped result submission, exact-result review and interruption.

The newly added automatic MCP reporting path still needs a real model-driven
paired check. App managed-agent eligibility integration and final Android device
acceptance remain pending. iOS runtime testing is deferred to the owner's
available environment. These are draft implementation snapshots, not release
acceptance claims.

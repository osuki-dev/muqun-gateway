# Released V2 contract fixtures

These are synthetic, trimmed contract fixtures, **not live captures**. They
contain no provider execution or credentials. Field names and operations are
pinned to `pingdotgg/t3code` stable release `v0.1.0` (2026-10-15), commit
`81493efb71e8e8244f95d3e9f71d8369654ab151` (MIT). They were written against
`v0.0.46-nightly.20261013.3042` (`5d6e4192`); the stable release's diff
against that nightly is contract-additive only (no identifier the adapter
uses changed), so the fixtures stand as they are:

- `packages/contracts/src/orchestrationV2.ts`: commands, native projection,
  visible positions, runtime requests, outer stream sequence, opaque paging.
- `packages/contracts/src/environmentHttp.ts`: protocol header and routes.
- `packages/contracts/src/project.ts`: project mutation HTTP payload.
- `apps/server/src/ws.ts`: RPC names and subscriptions.
- `apps/server/src/orchestration-v2/Orchestrator.ts`: serialized dispatch,
  async question response, provider switching and rollback planning.
- `RuntimeRequestService.ts`, `CheckpointRollbackService.ts`: live identity
  and actual provider conversation/file restoration boundaries.
- `threadHistoryPaging.ts`: page/window positions reset to zero; cursors carry
  the older-page context, not a global position namespace.
- `apps/web/src/pendingUserInput.ts`: omitted custom-answer flags default true.

V1 reference: `v0.0.45`, commit
`6c8fed35dded9ff71c5b46807125457acbb76be6`,
`packages/contracts/src/orchestration.ts` and `environment.ts`. Existing V1
fixtures remain the redacted 0.0.42 captures and run as regressions.

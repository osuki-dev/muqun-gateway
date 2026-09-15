# Scoped reporting through MCP

The pinned Gateway executable can run `work reporting-mcp` as a stdio MCP server named `muqun_reporting`. A managed launch passes only the existing private `MUQUN_WORK_CONTEXT_FILE` environment reference. The adapter reads the protected context file itself and forwards requests to the existing same-UID Unix reporting service. Tokens are not arguments, tool inputs, tool outputs, or logs. Same-UID authority remains an application boundary, not a hostile-process sandbox.

The finite tools are:

| Tool | Arguments | Effect |
| --- | --- | --- |
| `context` | `{}` | Read this assigned task, attempt, and current task revision. |
| `submit_result` | `{request_key, expected_revision, input: {attempt_id, summary, artifacts, evidence}}` | Use the existing immutable result capture and scoped submission transaction. |
| `result_receipt` | `{request_key}` | Read the original result for the exact authenticated attempt and request key. |

A result receipt is an immutable `ResultSubmission`; submission returns the existing mutation envelope. The adapter cannot answer approvals, accept results, publish, execute commands, read arbitrary paths, delegate, or change the recipient. Artifact capture retains the existing repository scope and digest checks. A successful submission registers a result; it does not establish human acceptance or task completion.

Use one stable request key for the intended submission. A missing receipt is not proof that an in-flight request had no effect. The adapter never retries or turns a lookup into a mutation. Revoked or expired context stays invalid; launching MCP does not issue or renew authority. Per-launch client configuration may allow these exact reporting tools according to the user's authorization; it must not weaken unrelated tools or global agent permissions.

The transport follows [MCP stdio framing](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports), [initialization](https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle), and [tool messages](https://modelcontextprotocol.io/specification/2025-11-25/server/tools). It accepts newline-delimited UTF-8 JSON-RPC, caps each input frame at 256 KiB and output at 6 MiB, and processes one tool call at a time. Initialization must precede tool use. Notifications cannot invoke actions. Only protocol messages reach stdout; local transport failures are sanitized. No subscriptions, resources, prompts, sampling, elicitation, or generic forwarding are advertised.

Protocol tests cover initialization, the finite tool list, uninitialized and notification no-effect behavior, oversized frames, and sanitized errors. Existing scoped service tests cover immutable receipt recovery and foreign-attempt rejection. These tests do not establish real provider integration; the managed agent launch and paired App workflow require separate end-to-end proof.

## Managed startup eligibility

Selected-session health exposes `backends[].bound_agent_kinds` (also on the
primary backend object). An array is the native runtime's current canonical
profile admission observation; an empty array means no eligible profiles.
Absent or null means compatibility unknown. Gateway treats malformed, duplicated
or oversized native lists as unknown. The App intersects known kinds with its
existing configured catalog only for managed Tasks; ordinary terminal choices
remain unchanged. Connected `work_execution_v1` is still required, and managed
Codex additionally requires `work_reporting_mcp_codex_v1`. Profile discovery does
not prove readiness and never replaces dispatch-time executable validation.

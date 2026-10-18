# Optional agent account limits

The Gateway capability `agent_account_limits_v1` declares the authenticated
`GET /api/agents/{agent_id}/account-limits` endpoint. Each supported agent also
declares `features.accountLimits: true` in discovery. Both gates are required:
unsupported agents and older Gateways should have no account-limit UI. This is
an optional account read, independent of session context occupancy, token
activity and the ability to send a prompt. Reported credits are not converted
to currency or estimated prices.

The endpoint uses the ordinary paired-App authentication and sealed transport.
Peer Gateway credentials are refused. Agent selection is exact; it never falls
back to another provider. An unavailable runtime returns the existing
`503 agent_unavailable`; an attached unsupported adapter returns
`501 feature_unsupported`. Responses use `Cache-Control: no-store`.

```json
{
  "status": "available",
  "reason": null,
  "observed_at_ms": 1800000000000,
  "ordinary_usage_allowed": null,
  "reset_credits": null,
  "limits": [
    {
      "id": "codex",
      "name": null,
      "plan_type": null,
      "model_id": null,
      "credits": null,
      "individual_limit": null,
      "spend_control_reached": null,
      "limit_reached_reason": null,
      "windows": [
        {
          "id": "primary",
          "label": null,
          "unit": "percent",
          "used": 25.0,
          "limit": 100.0,
          "remaining": 75.0,
          "window_duration_minutes": 300,
          "resets_at_ms": 1800003600000
        }
      ]
    }
  ]
}
```

`limits` and `windows` are arrays, not fixed plan slots. Their IDs are opaque
strings. Window `label` and bucket `name` are optional display names; use the
reported duration or a generic label when absent. Never infer a five-hour or
weekly window from a slot ID, a provider name or a subscription plan. One
window, multiple windows, no reported windows and unknown durations are valid.

Window `unit` is an extensible string (for example `percent`, `tokens`,
`requests`, `credits`, `seconds`). `used`, `limit` and `remaining` are nullable
finite numbers in that same unit. Unknown units can be displayed verbatim;
they are not currencies. Each adapter must supply its actual provider evidence
and document any arithmetic derivation. Missing numbers are unknown, never zero
or unlimited. The optional duration is a positive number of minutes; reset is
a Unix timestamp in milliseconds, or null. A past reset is not a promise that
usage has recovered. `observed_at_ms` records when the Gateway read the result,
not a provider-generated observation timestamp.

`ordinary_usage_allowed` is true, false or null. It reports only the provider's
permission for ordinary included usage. False does not claim that all paid or
alternative usage is blocked. Neither percentages nor reset times determine
this field. No permission field is used to block prompt submission.

A supporting adapter with no usable snapshot returns `status: "unavailable"`,
`reason: "not_authenticated"` or `"temporarily_unavailable"`, null permission
and an empty limits array. The first reason requires explicit authentication
evidence; network errors and unknown account states use the second. A valid
snapshot can have empty windows or unknown measurements and remains available.

## Official optional account details

Each bucket can also contain these nullable fields. An absent provider field
stays null; a credits-only or monthly-limit-only bucket is still displayable.
Clients render these common structures without checking the agent ID.

| Field | Shape and meaning |
| --- | --- |
| `plan_type` | Nullable provider-reported plan identifier. It does not imply any fixed quota. |
| `model_id` | Nullable ordinary model identifier associated with this quota bucket. |
| `credits` | Null or `{has_credits: boolean|null, unlimited: boolean|null, balance: string|null}`. |
| `individual_limit` | Null or `{used: string|null, limit: string|null, remaining_percent: number|null, resets_at_ms: number|null}`. An effective monthly credit limit for Codex. |
| `spend_control_reached` | Nullable explicit provider-reported spend-control state. |
| `limit_reached_reason` | Nullable provider reason identifier; unfamiliar future values stay strings. |

`credits.unlimited: true` means unlimited credits. Otherwise,
`has_credits: true` with unknown balance means credits are available with an
undisclosed balance, not zero. False and null remain different states.
Balance and individual used/limit amounts are bounded decimal strings preserved
without floating-point conversion or rounding. Nondecimal values remain null.
Their unit is credits; there is no USD/currency inference.

For Codex's `individual_limit`, show the official monthly-credit-limit meaning,
the reported used/limit and remaining percentage, and its reset time. Do not
invent a 30-day duration: calendar months and reported reset timestamps are not
equivalent to a fixed minute window. Following the official CLI, its reported
remaining percentage is clamped to 0–100; it is never recalculated from the
credit amounts. Unknown amounts do not erase a known percentage or reset.

Explicit reached-state fields are descriptive. Do not turn them, a zero balance,
a percentage or a passed reset timestamp into a general ability-to-send rule.
For example, ordinary included usage can be exhausted while another paid path
still exists. There is no new automatic purchasing, redemption or fallback.

The top-level `reset_credits` is null or:

```json
{
  "available_count": 2,
  "credits": [
    {
      "status": "available",
      "reset_type": "codexRateLimits",
      "expires_at_ms": 1800000000000,
      "title": "Rate-limit reset"
    }
  ]
}
```

Count is nullable and authoritative: a provider may cap the detail list.
`credits: null` means details are unknown; `credits: []` means details were
fetched with no rows. Each detail's status, type, expiry and title is nullable.
Only a known expiry is shown; null must not be presented as an invented date.
An unfamiliar status/type does not imply an available recovery opportunity.
Details do not expose consumption IDs and do not authorize redeeming anything.

## Codex mapping and freshness

The adapter calls the stable `account/rateLimits/read` method on its existing
Codex app-server connection. The locally generated nonexperimental 0.163.0
schema matches the repository's minimum 0.162.1
[response schema](https://github.com/openai/codex/blob/092d3acd6bec3e3a14bdc7e7a2810ab628ab759d/codex-rs/app-server-protocol/schema/typescript/v2/GetAccountRateLimitsResponse.ts)
and [window schema](https://github.com/openai/codex/blob/092d3acd6bec3e3a14bdc7e7a2810ab628ab759d/codex-rs/app-server-protocol/schema/typescript/v2/RateLimitWindow.ts).
The [official App Server guide](https://developers.openai.com/codex/app-server/)
documents the multi-bucket view and nullable windows. The pinned
[protocol definitions](https://github.com/openai/codex/blob/092d3acd6bec3e3a14bdc7e7a2810ab628ab759d/codex-rs/protocol/src/protocol.rs)
define reset timestamps in seconds, converted here with checked multiplication.

Optional details come from the same pinned
[RateLimitSnapshot](https://github.com/openai/codex/blob/092d3acd6bec3e3a14bdc7e7a2810ab628ab759d/codex-rs/app-server-protocol/schema/typescript/v2/RateLimitSnapshot.ts):
`credits`, `individualLimit`, `planType`, `normalModelSlug`,
`spendControlReached` and `rateLimitReachedType` map directly to the fields above.
[CreditsSnapshot](https://github.com/openai/codex/blob/092d3acd6bec3e3a14bdc7e7a2810ab628ab759d/codex-rs/app-server-protocol/schema/typescript/v2/CreditsSnapshot.ts)
and [SpendControlLimitSnapshot](https://github.com/openai/codex/blob/092d3acd6bec3e3a14bdc7e7a2810ab628ab759d/codex-rs/app-server-protocol/schema/typescript/v2/SpendControlLimitSnapshot.ts)
define the string credit amounts. The response's `rateLimitResetCredits` maps
to `reset_credits` using
[RateLimitResetCreditsSummary](https://github.com/openai/codex/blob/092d3acd6bec3e3a14bdc7e7a2810ab628ab759d/codex-rs/app-server-protocol/schema/typescript/v2/RateLimitResetCreditsSummary.ts)
and [RateLimitResetCredit](https://github.com/openai/codex/blob/092d3acd6bec3e3a14bdc7e7a2810ab628ab759d/codex-rs/app-server-protocol/schema/typescript/v2/RateLimitResetCredit.ts).
The official pinned
[CLI status implementation](https://github.com/openai/codex/blob/092d3acd6bec3e3a14bdc7e7a2810ab628ab759d/codex-rs/tui/src/status/rate_limits.rs)
shows remaining percentages, resets, Unlimited/Available/credit balance and
monthly credit usage. These fields already exist in the minimum supported
0.162.1; they are not features inferred from a newer version. During this
implementation, all eight related generated schema files were byte-identical
across that commit, local 0.163.0, and upstream main; the pinned and main CLI
status files also matched.

The multi-bucket map takes precedence over the legacy single-bucket view;
the latter is used only when no nonempty map is supplied. The adapter projects
only primary/secondary windows that actually exist. `usedPercent` becomes
`unit: "percent"` and `used`; `limit: 100` is only the percentage scale, not a
token/request allocation. `remaining` is arithmetic `clamp(100 - used, 0, 100)`
when used is known. Nonfinite or negative usage remains null. Over-100 upstream
values are preserved as used, with zero remaining; they never imply a reason
for enforcement. Duration and resets come directly from the corresponding
window. `ordinaryUsageAllowed` is copied without inference.

Reads are on demand, serialized and cached in memory for at most 30 seconds,
including failures. There is no background polling. A disconnected client
does not serve cached success. `account/updated` invalidates the cache and fences
in-flight reads; sparse `account/rateLimits/updated` notifications do not replace
the authoritative snapshot or imply recovery. Open/refresh the existing context
panel to read again; a refresh within 30 seconds may reuse the same observation.

On a failed limit read, `account/read` with `refreshToken: false` distinguishes
an explicit null account from an unknown failure. No login, reset-credit
redemption, credential refresh, usage-history read or new agent is initiated.
The endpoint never persists or returns account IDs, emails, tokens, raw errors,
reset consumption IDs or provider banners. Reported plan/credit metadata is
read-only and is not inferred from account identity. At most 64 buckets, 64 reset
details, and 256 characters per text field or decimal amount are projected.
Reset counts outside JavaScript's safe integer range remain unknown. New adapters implement the optional
`AgentPort::account_limits` operation and advertise the same feature; the App
can render their reported windows without provider-specific branches.

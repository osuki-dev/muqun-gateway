# GitHub Copilot CLI adapter

The opt-in `copilot` agent uses GitHub Copilot CLI's official ACP interface.
The implementation shares the bounded private-stdio driver in `adapters/acp/`;
`adapters/copilot/` retains the native launch and configuration profile.

## Official transport

Copilot CLI officially supports ACP in public preview. `copilot --acp --stdio`
uses NDJSON over the child process's stdin/stdout; stdout is reserved for the
protocol. Closing stdin shuts down the stdio server. TCP is a different,
longer-lived ownership model and is unnecessary for a private Gateway child.
The official example initializes ACP, creates a session with `cwd` and
`mcpServers`, receives text updates and awaits the prompt's stop reason.
[GitHub ACP reference](https://docs.github.com/en/copilot/reference/copilot-cli-reference/acp-server)

The server's tool availability and initial effort are process-wide launch
settings, not parameters of `session/new`. Saved sandbox policy also applies to
created and resumed sessions. Do not infer that creating a session resets those
settings. [GitHub ACP reference](https://docs.github.com/en/copilot/reference/copilot-cli-reference/acp-server)

Only invoke a built-in after it appears in `available_commands_update`.
`/context` and `/usage` are documented informational commands that return
without invoking the model. Send the exact command as one text content block.
`/help` is explicitly unsupported over ACP; unadvertised commands are forwarded
to the model. Do not use `/help` or assume `/version` is a free probe.
[GitHub ACP slash commands](https://docs.github.com/en/copilot/reference/copilot-cli-reference/acp-server#using-slash-commands)

`--no-auto-update` prevents the CLI from updating itself or running a newer
cached version, making an installed-version protocol check repeatable.
`--model` selects an initial model; `--mode` accepts `interactive`, `plan` and
`autopilot`. These CLI options alone do not prove that a particular ACP version
advertises corresponding live selectors.
[CLI reference](https://docs.github.com/en/copilot/reference/copilot-cli-reference/cli-command-reference)

## Authentication and discovery

Normal Copilot requests require existing authentication. A configured BYOK
provider can operate without GitHub authentication. Credentials may come from
environment variables, the CLI's credential store, or authenticated `gh` as a
fallback; therefore merely omitting a login command does not isolate an account.
`COPILOT_OFFLINE=true` suppresses GitHub authentication/network traffic and
telemetry, but still permits requests to the configured BYOK provider. It is
not a guarantee that inference stays local or costs nothing.
[Authentication reference](https://docs.github.com/en/copilot/how-tos/copilot-cli/set-up-copilot-cli/authenticate-copilot-cli)

Recommendation: discovery should find the installed executable without login,
installation, session creation or model calls. An explicit owner-initiated
session may reuse that CLI's existing authentication. Do not copy credentials
into Gateway storage or display token-bearing configuration.

## Advertised session capabilities

ACP requires initialization before `session/new`; authentication can still be
required when creating a session. Call `session/load` only when `loadSession`
is advertised. Loading replays history through session updates, so the adapter
must distinguish replay from a newly submitted user turn.
[ACP session setup](https://agentclientprotocol.com/protocol/v1/session-setup)

Treat session listing as optional and respect its cursor rather than inventing
a complete history from one page.
[ACP session list](https://agentclientprotocol.com/protocol/v1/session-list)
The official Copilot issue tracker records disk-backed cross-process
list/load support as implemented in v1.0.19, with sequential resume rather than
simultaneous editing of one live session. This is historical maintainer
evidence, not verification of the installed version.
[Copilot issue #1767](https://github.com/github/copilot-cli/issues/1767)

Prefer advertised `configOptions` over legacy `modes`; preserve current values
and show only returned choices. `session/set_config_option` returns the
authoritative option set. Unknown option types can be ignored without changing
the agent's current behavior. Do not fabricate model IDs, effort values or
permission modes from generic ACP examples.
[ACP configuration options](https://agentclientprotocol.com/protocol/v1/session-config-options)

## Permissions and truthful modes

Tool availability filters and permission grants are separate controls.
`--available-tools` removes tools from the model's choices; `--allow-tool`
preapproves selected operations. Deny rules take precedence over allow rules
and saved approvals. Existing location/domain approvals can suppress future
permission prompts, so manual mode cannot be advertised as “every write asks.”
[GitHub tool permissions](https://docs.github.com/en/copilot/how-tos/copilot-cli/use-copilot-cli/allowing-tools)

Do not launch with `--allow-all`, `--allow-all-tools`, `--allow-all-paths` or
`--allow-all-urls` merely to make headless integration work. Those options grant
different kinds of broad access; managed policy can restrict them. Plan mode,
autopilot and permission bypass are distinct concepts, and none should be
presented as an OS sandbox.
[CLI reference](https://docs.github.com/en/copilot/reference/copilot-cli-reference/cli-command-reference)

Forward `session/request_permission` with its actual option IDs. Approval must
select an offered ID; cancellation uses the `cancelled` outcome. The agent owns
remembered approvals. Tool titles and option names are display text, not an
authorization policy. A change that reduces oversight needs user action.
[ACP tool permissions](https://agentclientprotocol.com/protocol/v1/tool-calls)

## Gateway configuration and ownership

Copilot defaults to disabled. An owner with an installed, authenticated CLI can
opt in through Desktop Settings or the Gateway's `config.json`:

```json
{
  "copilot": {
    "enabled": true,
    "binary": "/absolute/path/to/copilot"
  }
}
```

`binary` is optional; without it, Gateway resolves `copilot` on its runtime
`PATH`. Overrides must name an absolute executable path. `agent setup copilot`
only reports the existing setup; Gateway never installs or authenticates it.
Discovery runs only `--no-auto-update --version` with a bounded timeout and
output budget. `MISE_AUTO_INSTALL=false` prevents a mise shim from installing
missing software. A reachable discovery result means the executable was
verified; authentication and ACP readiness are checked when a session opens.

Each native session has a private `copilot --acp --stdio --no-auto-update`
child, also with `MISE_AUTO_INSTALL=false`. Existing CLI authentication and
configuration remain owned by Copilot; Gateway neither reads credentials nor
copies them into its store. Normal CLI behavior, including remote repository
indexing, can still occur. A working directory is not an OS sandbox. Gateway
does not adopt a terminal's live Copilot process. Shutdown closes only the
children it owns.

Model, effort, mode and command choices come from observed ACP metadata after
an explicit session opens. Discovery itself does not start a catalog session.
The CLI/account default model applies until the user chooses an advertised
model. Unsupported optional features stay absent or false. The generic App
agent list receives the additive `copilot` provider with its own negotiated
feature metadata; other providers' contracts are unchanged.

The installed CLI advertised Agent, Plan and Autopilot using full ACP mode
URIs. Autopilot also enables broad permissions, so selecting its URI requires
`allow_remote_auto_approve` and it is never remembered as the default for a new
session. The separate `allow_all` configuration is not exposed as the App's
auto-approve switch. Agent and Plan do not promise that every operation asks:
saved CLI permission policy still applies. Gateway forwards offered ACP
permission choices through its existing approval flow without granting them.

## Local verification

Verified on Linux with the installed Copilot CLI 1.0.114 on 2026-11-19. Read-only
initialize/new probes observed protocol version 1, native session list/load
capabilities, model/configuration metadata and the independent `allow_all`
option. These observations do not establish cross-process resume behavior;
that remains subject to the shared driver's advertised-capability checks.

A separate Gateway paired with the App on a dedicated Android emulator and an
isolated Herdr 0.9.5 session (protocol 22). After the composer was ready, one
initial prompt and one follow-up each produced their exact requested assistant
response using the CLI/account default model. Both the original main terminal
and the isolated terminal retained their workspace/tab/pane focus. No login,
trust prompt or permission request was answered. The temporary Gateway, Herdr
session and owned emulator were stopped after verification; the live Gateway
was not replaced.

Regression coverage exercises disabled/version-only discovery, rejection of
remote Autopilot creation and switching without owner permission, and omission
of Autopilot from remembered session defaults. The existing ACP driver retains
its protocol, cancellation, replay and permission coverage. Formatting, strict
clippy and the full locked Gateway test suite passed (2,283 library tests,
16 lifecycle tests, one update test; 55 existing ignored tests).

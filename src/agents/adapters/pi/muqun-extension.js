// Muqun bridge for Pi, loaded with `pi --mode rpc -e <this file>`.
//
// Pi has no built-in approval gate, so this asks the gateway before the
// mutating built-in tools run. The question goes out as an RPC extension UI
// `confirm` with a fixed title the gateway recognises; the message carries the
// call as JSON so the gateway can render a real permission card instead of
// parsing prose. No timeout is passed: Pi must wait for the user, never
// auto-resolve. The turn's abort signal is passed so Stop dismisses the
// dialog (Pi resolves it `false`, so the call is blocked); Pi tells the
// client nothing when that happens, so the gateway drops the card itself when
// the run settles. `MUQUN_PI_APPROVALS=never` turns the gate off.
//
// `tool_call` event fields (Pi 1.4.0): `toolName`, `toolCallId`, `input`, and
// `parentToolCallId` on calls a tool makes itself.
export default function (pi) {
  const ask = (process.env.MUQUN_PI_APPROVALS ?? "ask") !== "never";
  const url = process.env.MUQUN_MCP_URL;
  const token = process.env.MUQUN_MCP_TOKEN;
  let bridgeReady = true;
  if (url && token) {
    try {
      pi.registerMcpServer("muqun", { url, headers: { Authorization: `Bearer ${token}` } });
    } catch {
      // Keep the approval handler installed even when registration fails.
      bridgeReady = false;
    }
  }

  pi.on("tool_call", async (event, ctx) => {
    if (!ask) return undefined;
    if (!["bash", "edit", "write"].includes(event.toolName)) return undefined;
    // RPC has UI through extension_ui_request/response. Missing UI or a
    // broken bridge must never grant permission to a mutating tool.
    if (!bridgeReady || !ctx.hasUI) return { block: true, reason: "Muqun approval bridge unavailable" };
    try {
      const ok = await ctx.ui.confirm(
        "muqun.permission",
        JSON.stringify({ tool: event.toolName, toolCallId: event.toolCallId, input: event.input }),
        { signal: ctx.signal },
      );
      if (ok === true && !ctx.signal?.aborted) return undefined;
    } catch {
      return { block: true, reason: "Muqun approval bridge unavailable" };
    }
    return { block: true, reason: "Denied from Muqun" };
  });
}

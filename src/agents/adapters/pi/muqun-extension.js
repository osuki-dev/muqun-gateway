// Muqun bridge for Pi, loaded with `pi --mode rpc -e <this file>`.
//
// Pi has no built-in approval gate, so this asks the gateway before the
// mutating built-in tools run. The question goes out as an RPC extension UI
// `confirm` with a fixed title the gateway recognises; the message carries the
// call as JSON so the gateway can render a real permission card instead of
// parsing prose. No timeout is passed: Pi must wait for the user, never
// auto-resolve. `MUQUN_PI_APPROVALS=never` turns the gate off.
//
// `tool_call` event fields (Pi 1.4.0): `toolName`, `toolCallId`, `input`, and
// `parentToolCallId` on calls a tool makes itself.
export default function (pi) {
  const ask = (process.env.MUQUN_PI_APPROVALS ?? "ask") !== "never";
  const url = process.env.MUQUN_MCP_URL;
  const token = process.env.MUQUN_MCP_TOKEN;
  if (url && token) {
    pi.registerMcpServer("muqun", { url, headers: { Authorization: `Bearer ${token}` } });
  }

  pi.on("tool_call", async (event, ctx) => {
    if (!ask || !ctx.hasUI) return undefined;
    if (!["bash", "edit", "write"].includes(event.toolName)) return undefined;
    const ok = await ctx.ui.confirm(
      "muqun.permission",
      JSON.stringify({ tool: event.toolName, toolCallId: event.toolCallId, input: event.input }),
    );
    if (!ok) return { block: true, reason: "Denied from Muqun" };
    return undefined;
  });
}

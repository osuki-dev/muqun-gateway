import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const source = await readFile(new URL("./muqun-extension.js", import.meta.url), "utf8");
const { default: extension } = await import(`data:text/javascript;base64,${Buffer.from(source).toString("base64")}`);

test("MCP registration failure preserves a closed tool gate", async () => {
  const original = Object.fromEntries(["MUQUN_PI_APPROVALS", "MUQUN_MCP_URL", "MUQUN_MCP_TOKEN"].map(key => [key, process.env[key]]));
  try {
    process.env.MUQUN_PI_APPROVALS = "ask";
    process.env.MUQUN_MCP_URL = "http://127.0.0.1/isolated";
    process.env.MUQUN_MCP_TOKEN = "isolated-token";
    let handler;
    extension({ registerMcpServer() { throw new Error("private registration failure"); }, on(name, fn) { if (name === "tool_call") handler = fn; } });
    assert.equal((await handler({ toolName: "write" }, { hasUI: true })).block, true);
  } finally {
    for (const [key, value] of Object.entries(original)) {
      if (value === undefined) delete process.env[key];
      else process.env[key] = value;
    }
  }
});

test("mutating tools need an explicit live approval and a working bridge", async () => {
  const original = process.env.MUQUN_PI_APPROVALS;
  try {
    for (const policy of [undefined, "ask", "unknown", "never"]) {
      if (policy === undefined) delete process.env.MUQUN_PI_APPROVALS;
      else process.env.MUQUN_PI_APPROVALS = policy;
      let handler;
      extension({ registerMcpServer() {}, on(name, fn) { if (name === "tool_call") handler = fn; } });
      for (const toolName of ["bash", "edit", "write"]) {
        for (const scenario of ["allow", "deny", "undefined", "truthy", "no-ui", "throw", "aborted"]) {
          const controller = new AbortController();
          const replies = [];
          const result = await handler({ toolName, toolCallId: "isolated-call", input: { path: "example" } }, {
            hasUI: scenario !== "no-ui",
            signal: controller.signal,
            ui: { async confirm(title, message, options) {
              replies.push({ title, message, options });
              if (scenario === "throw") throw new Error("private failure text");
              if (scenario === "aborted") controller.abort();
              return { allow: true, deny: false, truthy: "true", aborted: true }[scenario];
            } },
          });
          const allowed = policy === "never" || scenario === "allow";
          assert.equal(result?.block === true, !allowed, `${policy}/${toolName}/${scenario}`);
          for (const reply of replies) {
            assert.equal(reply.title, "muqun.permission");
            assert.equal(JSON.parse(reply.message).toolCallId, "isolated-call");
            assert.equal(reply.options.signal, controller.signal);
            assert.equal(reply.options.timeout, undefined);
          }
        }
      }
      assert.equal(await handler({ toolName: "read" }, { hasUI: false }), undefined);
    }
  } finally {
    if (original === undefined) delete process.env.MUQUN_PI_APPROVALS;
    else process.env.MUQUN_PI_APPROVALS = original;
  }
});

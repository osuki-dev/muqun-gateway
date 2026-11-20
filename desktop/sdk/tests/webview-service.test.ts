import { expect, test } from "bun:test";

const sdk = new URL("../index.ts", import.meta.url).href;
function panel(source: string) {
  return Bun.spawn([process.execPath, "--eval", `import { runWebViewService, WebViewServiceError } from ${JSON.stringify(sdk)}; ${source}`], {
    stdin: "pipe", stdout: "pipe", stderr: "pipe",
  });
}

test("the private listener serves until Desktop closes stdin", async () => {
  const child = panel(`
    await runWebViewService(({ onClose, signal }) => {
      console.log("diagnostic");
      const server = Bun.serve({ hostname: "127.0.0.1", port: 0, fetch: () => new Response("ok") });
      onClose(async () => { await server.stop(true); console.error("closed", signal.aborted); });
      return new URL("/private", server.url);
    });
  `);
  try {
    const reader = child.stdout.getReader();
    const { value } = await reader.read();
    const url = new URL(new TextDecoder().decode(value).trim());
    expect(await (await fetch(url)).text()).toBe("ok");
    child.stdin.end();
    expect(await child.exited).toBe(0);
    expect(await new Response(child.stderr).text()).toBe("diagnostic\nclosed true\n");
    expect(await reader.read()).toMatchObject({ done: true });
  } finally { child.kill(); }
});

test("startup errors clean up and keep exception details out of the handshake", async () => {
  const child = panel(`
    await runWebViewService(({ onClose }) => {
      onClose(() => console.error("closed"));
      throw new Error("http://127.0.0.1:9911/private-token");
    });
  `);
  try {
    expect(await child.exited).toBe(1);
    expect(await new Response(child.stdout).text()).toBe("!muqun:webview:error:startup-failed\n");
    expect(await new Response(child.stderr).text()).toBe("closed\n");
  } finally { child.kill(); }
});

test("closing a still-starting service aborts the operation and cleans up", async () => {
  const child = panel(`
    await runWebViewService(({ signal, onClose }) => {
      onClose(() => console.error("closed",signal.aborted));
      console.error("starting");
      return new Promise(() => {});
    });
  `);
  try {
    const reader = child.stderr.getReader();
    await reader.read();
    child.stdin.end();
    expect(await child.exited).toBe(0);
    expect(await new Response(child.stdout).text()).toBe("");
    expect(new TextDecoder().decode((await reader.read()).value)).toBe("closed true\n");
  } finally { child.kill(); }
});

import { expect, test } from "bun:test";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

const sdk = new URL("../index.ts", import.meta.url).href;

async function invoke(source: string, action?: string, locale?: string, context?: string) {
  const directory = await mkdtemp(join(tmpdir(), "muqun-sdk-"));
  try {
    const entry = join(directory, "main.ts");
    await writeFile(entry, `import { Text, definePlugin, runPlugin } from ${JSON.stringify(sdk)};\n${source}`);
    const env = { ...process.env };
    delete env.MUQUN_PLUGIN_ACTION;
    delete env.MUQUN_PLUGIN_LOCALE;
    delete env.MUQUN_PLUGIN_CONTEXT;
    if (context !== undefined) env.MUQUN_PLUGIN_CONTEXT = context;
    if (locale !== undefined) env.MUQUN_PLUGIN_LOCALE = locale;
    if (action !== undefined) env.MUQUN_PLUGIN_ACTION = action;
    const child = Bun.spawn([process.execPath, entry], { env, stdout: "pipe", stderr: "pipe" });
    const [stdout, stderr, status] = await Promise.all([
      new Response(child.stdout).text(),
      new Response(child.stderr).text(),
      child.exited,
    ]);
    return { stdout, stderr, status };
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
}

test("runs a default open and preserves opaque button actions", async () => {
  for (const action of [undefined, "board:abc / 雪?&=\n", ""]) {
    const result = await invoke(`await runPlugin(definePlugin(action => Text({ text: action })));`, action);
    expect(result.status).toBe(0);
    expect(result.stderr).toBe("");
    expect(JSON.parse(result.stdout)).toEqual({ version: 1, view: { type: "text", text: action ?? "open" } });
  }
});

test("keeps synchronous and asynchronous console diagnostics off stdout and restores the console", async () => {
  const result = await invoke(`
const original = [console.log, console.info, console.debug];
await runPlugin(definePlugin(async action => {
  console.log("loading", { action });
  await Promise.resolve();
  console.info("information");
  console.debug("details");
  console.warn("warning");
  console.error("error detail");
  return Text({ text: action });
}));
if ([console.log, console.info, console.debug].some((method, i) => method !== original[i])) throw new Error("Console was not restored");
`);
  expect(result.status).toBe(0);
  expect(JSON.parse(result.stdout)).toEqual({ version: 1, view: { type: "text", text: "open" } });
  for (const message of ["loading", "information", "details", "warning", "error detail"]) expect(result.stderr).toContain(message);
});

test("rejects failed renders without a response or a leaked console override", async () => {
  const result = await invoke(`
const original = console.log;
try {
  await runPlugin(definePlugin(async () => {
    console.log("before failure");
    throw new Error("render failed");
  }));
} catch (error) {
  if (console.log !== original) throw new Error("Console was not restored");
  throw error;
}
`);
  expect(result.status).not.toBe(0);
  expect(result.stdout).toBe("");
  expect(result.stderr).toContain("render failed");
  expect(result.stderr).toContain("before failure");
  expect(result.stderr).not.toContain("Console was not restored");
});

test("importing and defining a plugin does not invoke it", async () => {
  const result = await invoke(`definePlugin(() => { throw new Error("Unexpected execution"); });`);
  expect(result).toEqual({ stdout: "", stderr: "", status: 0 });
});


test("delivers Desktop locale independently of OS language and falls back to English", async () => {
  for (const locale of [undefined, "", "en", "zh-CN", "zh-TW", "ja"]) {
    const result = await invoke(`
process.env.LANG = "de_DE.UTF-8";
await runPlugin(definePlugin((action, context) => Text({ text: action + ":" + context.locale })));
`, "open", locale);
    expect(result.status).toBe(0);
    expect(result.stderr).toBe("");
    expect(JSON.parse(result.stdout)).toEqual({ version: 1, view: { type: "text", text: "open:" + (locale || "en") } });
  }
});

test("direct render calls default to English and accept an explicit locale context", async () => {
  const result = await invoke(`
const render = definePlugin((action, { locale }) => Text({ text: action + ":" + locale }));
const first = await render();
const second = await render("follow-up", { locale: "zh-TW", theme: { id: "custom", colorScheme: "light", preference: "system", reducedMotion: false, reducedTransparency: false, textScale: 1 }, platform: "linux", desktopVersion: "test" });
if (first.view.type !== "text" || first.view.text !== "open:en") throw new Error("Missing default context");
if (second.view.type !== "text" || second.view.text !== "follow-up:zh-TW") throw new Error("Lost explicit context");
`);
  expect(result).toEqual({ stdout: "", stderr: "", status: 0 });
});


test("delivers and freezes the Desktop context independently of OS settings", async () => {
  const context = { locale: "zh-TW", capabilities: { webviewEntry: true, nativeLayout: true, nativePresentation: true, nativeImages: true, fillLayout: true, markdownAnchors: true, nativeSelect: true }, theme: { id: "paper", colorScheme: "light", preference: "system", reducedMotion: true, reducedTransparency: true, textScale: 1.25 }, platform: "macos", desktopVersion: "1.2.3-preview.4" };
  const result = await invoke(`
await runPlugin(definePlugin((action, context) => {
  if (!Object.isFrozen(context) || !Object.isFrozen(context.theme) || !Object.isFrozen(context.capabilities)) throw new Error("Mutable snapshot");
  return Text({ text: JSON.stringify(context) });
}));
`, "open", "de", JSON.stringify(context));
  expect(result.status).toBe(0);
  expect(result.stderr).toBe("");
  expect(JSON.parse(JSON.parse(result.stdout).view.text)).toEqual({ ...context, form: {} });
});

test("missing, malformed and invalid context fields use safe defaults", async () => {
  for (const context of [undefined, "{", "null", "[]", JSON.stringify({ locale: 7, theme: { colorScheme: "auto", preference: true, reducedMotion: "false", reducedTransparency: 1, textScale: -1 }, platform: "other", desktopVersion: [] })]) {
    const result = await invoke(`await runPlugin(definePlugin((action, context) => Text({ text: JSON.stringify(context) })));`, "open", undefined, context);
    expect(result.status).toBe(0);
    expect(result.stderr).toBe("");
    expect(JSON.parse(JSON.parse(result.stdout).view.text)).toEqual({ form: {}, capabilities: { webviewEntry: false, nativeLayout: false, nativePresentation: false, nativeImages: false, fillLayout: false, markdownAnchors: false, nativeSelect: false }, locale: "en", theme: { id: "default", colorScheme: "dark", preference: "system", reducedMotion: false, reducedTransparency: false, textScale: 1 }, platform: process.platform === "darwin" ? "macos" : process.platform === "win32" ? "windows" : "linux", desktopVersion: "unknown" });
  }
});

// A file-based extension: no Desktop Rust code is needed to add this view.
export type Run = (args: string[]) => Promise<any>;
const escape = (value: unknown) => String(value ?? "").replace(/[\r\n]+/g, " ").replace(/[\\`*_{}\[\]<>|()]/g, "\\$&");
const link = (title: string, url: string) => {
  try {
    const parsed = new URL(url);
    if (parsed.protocol === "https:" && !/[\s()]/.test(url)) return `[${escape(title)}](${url})`;
  } catch {}
  return escape(title);
};
export async function board(run: Run): Promise<string> {
  const repo = await run(["repo", "view", "--json", "nameWithOwner,url"]);
  const fields = "number,title,url,author,labels,updatedAt";
  const results = await Promise.allSettled([
    run(["pr", "list", "--repo", repo.nameWithOwner, "--state", "open", "--limit", "30", "--json", `${fields},isDraft,reviewDecision`]),
    run(["issue", "list", "--repo", repo.nameWithOwner, "--state", "open", "--limit", "30", "--json", fields]),
  ]);
  const sections = results.map((result, index) => {
    const title = index === 0 ? "Pull requests" : "Issues";
    if (result.status === "rejected") return `## ${title}\n\nCould not load this section. ${escape(result.reason instanceof Error ? result.reason.message : result.reason)}`;
    if (!Array.isArray(result.value)) throw new Error("Unexpected repository response");
    const items = result.value.map((item: any) => {
      const status = index === 0 ? (item.isDraft ? "Draft" : item.reviewDecision || "Review pending") : "Open";
      const labels = (item.labels ?? []).map((label: any) => escape(label.name)).join(", ");
      return `### ${link(`#${item.number} ${item.title}`, item.url)}\n\n${escape(status)} · @${escape(item.author?.login ?? "unknown")} ${labels ? `· ${labels}` : ""}`;
    });
    return `## ${title} (${items.length}${items.length === 30 ? "+" : ""})\n\n${items.join("\n\n") || "No open items."}`;
  });
  return `# ${link(repo.nameWithOwner, repo.url)}\n\nOpen items · latest 30 per section · refreshed ${new Date().toISOString()}\n\n${sections.join("\n\n---\n\n")}`;
}
if (import.meta.main) {
  const cwd = process.env.MUQUN_WORKSPACE_CWD;
  const run: Run = async (args) => {
    const child = Bun.spawn(["gh", ...args], { cwd, stdin: "ignore", stdout: "pipe", stderr: "pipe", env: {...process.env, GH_PROMPT_DISABLED: "1"} });
    const [stdout, stderr, code] = await Promise.all([new Response(child.stdout).text(), new Response(child.stderr).text(), child.exited]);
    if (code !== 0) throw new Error(stderr.trim() || "GitHub CLI failed");
    return JSON.parse(stdout);
  };
  try { console.log(JSON.stringify({markdown: await board(run)})); }
  catch (error) {
    console.log(JSON.stringify({toast:{title:"Repository unavailable",level:"warning"},markdown: `# Pull Requests & Issues\n\nUnable to load this repository. Check the workspace folder and your existing gh login.\n\n${escape(error instanceof Error ? error.message : error)}`}));
  }
}

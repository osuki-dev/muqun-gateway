// A file-based extension: no Desktop Rust code is needed to add this view.
export type Run = (args: string[]) => Promise<any>;

const escape = (value: unknown) =>
  String(value ?? "")
    .replace(/[\r\n]+/g, " ")
    .replace(/[\\`*_{}\[\]<>|()]/g, "\\$&");

const link = (title: string, url: string) => {
  try {
    const parsed = new URL(url);
    if (parsed.protocol === "https:" && !/[\s()]/.test(url)) {
      return `[${escape(title)}](${url})`;
    }
  } catch {}
  return escape(title);
};

export function formatReviewStatus(item: { isDraft?: boolean; reviewDecision?: string | null }): string {
  if (item.isDraft) return "Draft";
  switch (String(item.reviewDecision ?? "").toUpperCase()) {
    case "APPROVED":
      return "Approved";
    case "CHANGES_REQUESTED":
      return "Changes requested";
    case "REVIEW_REQUIRED":
      return "Review required";
    default:
      return "Review pending";
  }
}

export function formatRelativeTime(iso?: string, nowMs = Date.now()): string {
  if (!iso) return "";
  const parsed = Date.parse(iso);
  if (!Number.isFinite(parsed)) return "";
  const diffSec = Math.max(0, Math.floor((nowMs - parsed) / 1000));
  if (diffSec < 60) return "updated just now";
  const diffMin = Math.floor(diffSec / 60);
  if (diffMin < 60) return `updated ${diffMin}m ago`;
  const diffHr = Math.floor(diffMin / 60);
  if (diffHr < 24) return `updated ${diffHr}h ago`;
  const diffDay = Math.floor(diffHr / 24);
  if (diffDay < 30) return `updated ${diffDay}d ago`;
  return `updated ${iso.slice(0, 10)}`;
}

function formatUtcTimestamp(date: Date): string {
  return date.toISOString().replace("T", " ").slice(0, 16) + " UTC";
}

export function formatErrorMarkdown(error: unknown, cwd?: string): { title: string; markdown: string } {
  const message = error instanceof Error ? error.message : String(error ?? "Unknown error");
  const lower = message.toLowerCase();
  const folderNote = cwd ? `\n\nWorkspace: \`${escape(cwd)}\`` : "";

  if (lower.includes("enoent") || lower.includes("command not found") || lower.includes("not found in path") || lower.includes("executable not found")) {
    return {
      title: "GitHub CLI not installed",
      markdown: `# Pull Requests & Issues\n\nGitHub CLI (\`gh\`) was not found on your \`PATH\`.\n\n1. Install \`gh\` from [cli.github.com](https://cli.github.com).\n2. Sign in with \`gh auth login\`.\n3. Click **Refresh PRs & issues**.${folderNote}`,
    };
  }

  if (lower.includes("not logged into") || lower.includes("gh auth login") || lower.includes("authentication") || lower.includes("http 401")) {
    return {
      title: "GitHub sign-in required",
      markdown: `# Pull Requests & Issues\n\nYour GitHub CLI session is not authenticated.\n\n1. Open the terminal and run \`gh auth login\`.\n2. Click **Refresh PRs & issues**.${folderNote}\n\n> ${escape(message)}`,
    };
  }

  if (lower.includes("not a git repository") || lower.includes("none of the git remotes") || lower.includes("could not resolve to a repository") || lower.includes("no git remotes found")) {
    return {
      title: "Not a GitHub repository",
      markdown: `# Pull Requests & Issues\n\nThe active workspace does not point to a GitHub repository.${folderNote}\n\nOpen a session in a GitHub checkout and click **Refresh PRs & issues**.\n\n> ${escape(message)}`,
    };
  }

  return {
    title: "Repository unavailable",
    markdown: `# Pull Requests & Issues\n\nUnable to load this repository. Check the workspace folder and your existing \`gh\` login.${folderNote}\n\n> ${escape(message)}`,
  };
}

export async function board(run: Run, now = new Date()): Promise<string> {
  const repo = await run(["repo", "view", "--json", "nameWithOwner,url"]);
  if (!repo || typeof repo.nameWithOwner !== "string" || !repo.nameWithOwner) {
    throw new Error("Unexpected repository response");
  }
  const fields = "number,title,url,author,labels,updatedAt";
  const results = await Promise.allSettled([
    run(["pr", "list", "--repo", repo.nameWithOwner, "--state", "open", "--limit", "30", "--json", `${fields},isDraft,reviewDecision,headRefName`]),
    run(["issue", "list", "--repo", repo.nameWithOwner, "--state", "open", "--limit", "30", "--json", fields]),
  ]);

  const counts = results.map((result) => (result.status === "fulfilled" && Array.isArray(result.value) ? result.value.length : null));
  const summaryParts = [
    counts[0] !== null ? `${counts[0]}${counts[0] === 30 ? "+" : ""} open PRs` : "PRs unavailable",
    counts[1] !== null ? `${counts[1]}${counts[1] === 30 ? "+" : ""} open issues` : "Issues unavailable",
    `refreshed ${formatUtcTimestamp(now)}`,
  ];

  const nowMs = now.getTime();
  const sections = results.map((result, index) => {
    const title = index === 0 ? "Pull requests" : "Issues";
    if (result.status === "rejected") {
      return `## ${title}\n\nCould not load this section. ${escape(result.reason instanceof Error ? result.reason.message : result.reason)}`;
    }
    if (!Array.isArray(result.value)) throw new Error("Unexpected repository response");
    const items = result.value.map((item: any) => {
      const status = index === 0 ? formatReviewStatus(item) : "Open";
      const author = `@${escape(item.author?.login ?? "unknown")}`;
      const branch = index === 0 && item.headRefName ? `\`${escape(item.headRefName)}\`` : "";
      const labels = (item.labels ?? [])
        .map((label: any) => label?.name)
        .filter(Boolean)
        .map((name: string) => `\`${escape(name)}\``)
        .join(" ");
      const updated = formatRelativeTime(item.updatedAt, nowMs);
      const meta = [status, author, branch, labels, updated].filter(Boolean).join(" · ");
      return `### ${link(`#${item.number} ${item.title}`, item.url)}\n\n${meta}`;
    });
    return `## ${title} (${items.length}${items.length === 30 ? "+" : ""})\n\n${items.join("\n\n") || "No open items."}`;
  });

  return `# ${link(repo.nameWithOwner, repo.url)}\n\n${summaryParts.join(" · ")}\n\n${sections.join("\n\n---\n\n")}`;
}

if (import.meta.main) {
  const cwd = process.env.MUQUN_WORKSPACE_CWD || process.cwd();
  const run: Run = async (args) => {
    const child = Bun.spawn(["gh", ...args], {
      cwd,
      stdin: "ignore",
      stdout: "pipe",
      stderr: "pipe",
      env: { ...process.env, GH_PROMPT_DISABLED: "1" },
    });
    const [stdout, stderr, code] = await Promise.all([
      new Response(child.stdout).text(),
      new Response(child.stderr).text(),
      child.exited,
    ]);
    if (code !== 0) throw new Error(stderr.trim() || "GitHub CLI failed");
    return JSON.parse(stdout);
  };
  try {
    console.log(JSON.stringify({ markdown: await board(run) }));
  } catch (error) {
    const formatted = formatErrorMarkdown(error, cwd);
    console.log(
      JSON.stringify({
        toast: { title: formatted.title, level: "warning" },
        markdown: formatted.markdown,
      }),
    );
  }
}

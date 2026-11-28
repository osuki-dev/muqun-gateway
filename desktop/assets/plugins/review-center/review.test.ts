import { expect, test } from "bun:test";
import { board, formatErrorMarkdown, formatRelativeTime, formatReviewStatus } from "./review";

test("PR failure leaves issues visible and repository text cannot inject markup", async () => {
  const markdown = await board(
    async (args) => {
      if (args[0] === "repo") return { nameWithOwner: "owner/repo", url: "https://github.com/owner/repo" };
      if (args[0] === "pr") throw new Error("permission denied");
      return [{ number: 7, title: "[untrusted](javascript:bad)", url: "javascript:bad", author: { login: "dev" }, labels: [] }];
    },
    new Date("2026-11-25T10:00:00Z"),
  );
  expect(markdown).toContain("Could not load this section");
  expect(markdown).toContain("## Issues (1)");
  expect(markdown).toContain("\\[untrusted\\]");
  expect(markdown).not.toContain("](javascript:");
});

test("formats review decisions, branches, labels, and relative timestamps cleanly", async () => {
  expect(formatReviewStatus({ isDraft: true, reviewDecision: "APPROVED" })).toBe("Draft");
  expect(formatReviewStatus({ reviewDecision: "APPROVED" })).toBe("Approved");
  expect(formatReviewStatus({ reviewDecision: "CHANGES_REQUESTED" })).toBe("Changes requested");
  expect(formatReviewStatus({ reviewDecision: "REVIEW_REQUIRED" })).toBe("Review required");
  expect(formatReviewStatus({})).toBe("Review pending");

  const now = new Date("2026-11-25T10:00:00Z");
  expect(formatRelativeTime("2026-11-25T08:00:00Z", now.getTime())).toBe("updated 2h ago");
  expect(formatRelativeTime("2026-11-22T10:00:00Z", now.getTime())).toBe("updated 3d ago");

  const markdown = await board(
    async (args) => {
      if (args[0] === "repo") return { nameWithOwner: "owner/repo", url: "https://github.com/owner/repo" };
      if (args[0] === "pr") {
        return [
          {
            number: 42,
            title: "Ship plugin overhaul",
            url: "https://github.com/owner/repo/pull/42",
            author: { login: "alice" },
            labels: [{ name: "desktop" }],
            headRefName: "feat/plugin-overhaul",
            isDraft: false,
            reviewDecision: "APPROVED",
            updatedAt: "2026-11-25T08:00:00Z",
          },
        ];
      }
      return [];
    },
    now,
  );
  expect(markdown).toContain("1 open PRs · 0 open issues · refreshed 2026-11-25 10:00 UTC");
  expect(markdown).toContain("Approved · @alice · `feat/plugin-overhaul` · `desktop` · updated 2h ago");
});

test("classifies missing gh, unauthenticated gh, and non-GitHub workspaces cleanly", () => {
  const missing = formatErrorMarkdown(new Error("Executable not found in PATH: gh"), "/tmp/repo");
  expect(missing.title).toBe("GitHub CLI not installed");
  expect(missing.markdown).toContain("cli.github.com");

  const unauth = formatErrorMarkdown(new Error("You are not logged into any GitHub hosts. Run gh auth login."), "/tmp/repo");
  expect(unauth.title).toBe("GitHub sign-in required");
  expect(unauth.markdown).toContain("gh auth login");

  const nonRepo = formatErrorMarkdown(new Error("not a git repository (or any of the parent directories)"), "/tmp/plain");
  expect(nonRepo.title).toBe("Not a GitHub repository");
  expect(nonRepo.markdown).toContain("/tmp/plain");
});

import { expect, test } from "bun:test";
import { board } from "./review";
test("PR failure leaves issues visible and repository text cannot inject markup", async () => {
  const markdown = await board(async (args) => {
    if (args[0] === "repo") return {nameWithOwner:"owner/repo",url:"https://github.com/owner/repo"};
    if (args[0] === "pr") throw new Error("permission denied");
    return [{number:7,title:"[untrusted](javascript:bad)",url:"javascript:bad",author:{login:"dev"},labels:[]}];
  });
  expect(markdown).toContain("Could not load this section");
  expect(markdown).toContain("## Issues (1)");
  expect(markdown).toContain("\\[untrusted\\]");
  expect(markdown).not.toContain("](javascript:");
});

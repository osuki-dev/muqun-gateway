# macOS release signing

Preview and stable publication use the same `sign-macos.yml` reusable workflow.
It signs the two verified Mach-O artifacts with an existing local Developer ID
Application identity, verifies the Apple anchor, Developer ID extension,
expected Team ID, fixed `dev.osuki.muqun-gateway` identifier and timestamp, then
creates checksums over the signed bytes. Artifacts are never executed on the
signer. Publication combines only the signed Mac bundle and the Linux bundle;
input/output hashes and public signature metadata form a separate audit.

## Required security boundary

This repository is public. A label or repository-level runner does not protect
a personal login keychain from a PR that changes its own workflow. Do not
register this signer in an unrestricted group. The GitHub server must enforce
**Selected workflows**, allowing only:

```text
osuki-dev/muqun-gateway/.github/workflows/sign-macos.yml@refs/heads/main
```

Workflow-level access is documented for GitHub Enterprise Cloud and Enterprise
Server. The ordinary Free-plan documentation covers repository-level groups
but omits workflow-level access. Group creation or an admin token alone is
insufficient. Verify the account entitlement before requesting credentials or
registering a runner. Until that boundary is available, keep these workflow
changes unmerged: missing configuration deliberately prevents publication.

Sources: [workflow access policy](https://docs.github.com/en/enterprise-cloud@latest/actions/how-tos/manage-runners/self-hosted-runners/manage-access#changing-which-workflows-can-access-a-runner-group),
[ordinary group access](https://docs.github.com/en/actions/how-tos/manage-runners/self-hosted-runners/manage-access),
[documentation version condition](https://github.com/github/docs/blob/main/content/actions/how-tos/manage-runners/self-hosted-runners/manage-access.md).

## Organization-owner setup

1. Confirm **Selected workflows** is available. In organization **Settings →
   Actions → Runner groups**, create a dedicated group. Permit only
   `osuki-dev/muqun-gateway`, explicitly allow that public repository, and set
   **Workflow access → Selected workflows** to the exact reference above.
   If the control is unavailable, stop; do not substitute a public repo runner.
2. Verify the group API reports `visibility: selected`,
   `allows_public_repositories: true`, `restricted_to_workflows: true`, exactly
   the selected workflow above, and only the canonical repository selected.
   API setup needs organization-owner authority and fine-grained organization
   **Self-hosted runners: write** permission, or classic `admin:org`.
   Read-only policy verification needs the corresponding read permission.
3. Register a separate GitHub organization runner directly into that group,
   labeled `muqun-gateway-signer`, with a dedicated work directory. Run it as
   the existing macOS user whose keychain owns the Developer ID identity.
   Do not reuse or stop an unrelated Gitea runner. Do not export, regenerate,
   rotate, or upload a private key/P12, or broaden key access to bypass a prompt.
4. Set the public-metadata repository Actions variables
   `MACOS_SIGNING_RUNNER_GROUP`, `MACOS_SIGNING_IDENTITY_SHA1`, and
   `MACOS_SIGNING_TEAM_ID` from that existing valid identity. Both channels
   use these same values; the private key stays in the local keychain.
5. Confirm protected main and the server-side group policy, then run an
   explicitly authorized preview and inspect its signing audit and both
   published Mac signatures/checksums. Setup does not authorize a stable tag.

The hosted policy job rejects forks, PR events, non-main manual runs,
source/version substitution, missing variables and source outside protected
main before scheduling the signer. These checks complement the server-side
boundary; they cannot replace it.

Without selected-workflow entitlement, a separately reviewed private signing
control repository or constrained signing broker is required, including an
authenticated, source-bound artifact handoff. Changing a runner label in this
public repository does not provide that isolation.

## Verification

```sh
python3 -m unittest discover -s scripts/tests -v
```

The script uses the [Apple requirement language](https://developer.apple.com/documentation/technotes/tn3127-inside-code-signing-requirements).
Inputs must contain exactly two Mac binaries and their original checksums.
Output and audit paths must be new. Existing releases/assets are never
re-signed or overwritten in place.

# Read-only linked checkout browsing

`git_worktrees` is an additive build capability for the terminal pane Changes
view. Existing clients keep their current-checkout routes and response shapes.

`GET /api/sessions/{sessionId}/panes/{paneId}/git/worktrees` requires a paired
device and resolves the pane's checkout through the existing directory fence.
It returns the content envelope with `data.current` (canonical checkout path or
null), `data.worktrees` (`path`, `branch`, `head`, `detached`), `data.truncated`
and `data.total`. A pane outside a repository returns an empty inventory.

```json
{
  "current": "/home/me/app",
  "worktrees": [
    { "path": "/home/me/app", "branch": "main", "head": "1f2e...", "detached": false },
    { "path": "/home/me/app-fix", "branch": null, "head": "9a8b...", "detached": true }
  ],
  "truncated": false,
  "total": 2
}
```

The inventory comes from `git worktree list --porcelain -z`, rather than Herdr
or an agent runtime, and keeps its order (the main checkout first). Bare,
missing and replaced directories are omitted. Each available entry must be a
checkout root of the original repository's canonical Git common directory,
checked on disk without running git per entry: the main checkout's `.git` is
the common directory, and a linked checkout's `.git` file names an
administrative directory under `<common>/worktrees/` whose `gitdir` file points
back at it. One `git rev-parse` and one `git worktree list` answer the whole
inventory, under Git's existing environment scrubbing, output cap and timeout.

A large inventory is cut, never refused: at most 512 checkouts are listed.
`truncated` is true when the list stopped there with entries left unchecked, or
when Git's output reached its size cap; `total` counts the entries Git reported
before verification. Both fields are additive; a client that predates them
reads the list as before.

The existing `git/status` and `git/diff` reads accept an optional `worktree`
query parameter containing an exact canonical checkout path. Every read
revalidates it with the same on-disk check, so a checkout beyond the listed 512
can still be selected; directory aliases, subdirectories and unrelated
repositories return `403 invalid_worktree`. A pane without an authorized repository cannot
use a selected path to discover one. Authentication precedes filesystem access.

These read routes never check out a branch, move a pane or remove a worktree.
Discard and agent-session VCS routes retain their existing pane/session scope.
The App uses paged patches for alternate checkouts and suppresses mutation controls.

`pane_lazygit_worktree` adds the same optional canonical selector to both
`GET` and `POST` on the pane's `lazygit` route. The Gateway validates the path
as a checkout of the source repository, by the same check, before creating a new,
unfocused terminal tab. The selected directory is passed as the tab's native
working directory, never interpolated into a shell command. Omitting the
selector retains the original checkout behavior. The App disables alternate
checkout launches with an explanation when this capability is absent.

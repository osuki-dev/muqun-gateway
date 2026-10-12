# Read-only linked checkout browsing

`git_worktrees` is an additive build capability for the terminal pane Changes
view. Existing clients keep their current-checkout routes and response shapes.

`GET /api/sessions/{sessionId}/panes/{paneId}/git/worktrees` requires a paired
device and resolves the pane's checkout through the existing directory fence.
It returns the content envelope with `data.current` (canonical checkout path or
null) and `data.worktrees` (`path`, `branch`, `head`, `detached`). A pane outside
a repository returns an empty inventory.

The inventory comes from `git worktree list --porcelain -z`, rather than Herdr
or an agent runtime. Bare, missing and replaced directories are omitted. Each
available entry must resolve to a checkout root sharing the original repository's
canonical Git common directory. The operation retains Git's existing environment
scrubbing and output cap, limits the inventory to 128 checkouts, and has a
five-second overall timeout. Oversized inventories fail rather than returning
an apparently complete partial list.

The existing `git/status` and `git/diff` reads accept an optional `worktree`
query parameter containing an exact canonical inventory path. Every read
revalidates it; directory aliases, subdirectories and unrelated repositories
return `403 invalid_worktree`. A pane without an authorized repository cannot
use a selected path to discover one. Authentication precedes filesystem access.

These routes never check out a branch, move a pane or remove a worktree. Discard,
Lazygit and agent-session VCS routes retain their existing pane/session scope.
The App uses paged patches for alternate checkouts and suppresses mutation controls.

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

These read routes never check out a branch, move a pane or remove a worktree.
Discard and agent-session VCS routes retain their existing pane/session scope.
The App uses paged patches for alternate checkouts and suppresses mutation controls.

`pane_lazygit_worktree` adds the same optional canonical selector to both
`GET` and `POST` on the pane's `lazygit` route. The Gateway validates the path
against the source repository's current Git inventory before creating a new,
unfocused terminal tab. The selected directory is passed as the tab's native
working directory, never interpolated into a shell command. Omitting the
selector retains the original checkout behavior. The App disables alternate
checkout launches with an explanation when this capability is absent.

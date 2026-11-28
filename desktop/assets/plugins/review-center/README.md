# Pull Requests & Issues

Automatically loads open GitHub pull requests and issues for the current workspace when opened. Use **Refresh PRs & issues** at any time to fetch the latest state.

Uses your existing `gh` authentication. This plugin only reads repository data; it does not create, comment on, merge, or close anything.

Open pull requests and issues appear with review status, author, branch, labels, relative update time, and direct links to GitHub. The latest 30 of each are shown.

Requires Bun and GitHub CLI (`gh`). For a repository on another forge or without a GitHub remote, it reports clear setup guidance.

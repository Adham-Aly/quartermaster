Find files by glob pattern. Files ignored by .gitignore and hidden files are skipped. Returns paths newest first, at most 100.
- pattern: the glob. A pattern without a slash matches file names anywhere, e.g. "*.rs"; one with a slash matches paths under the folder, e.g. "src/**/*.rs".
- path: the folder to search (optional; defaults to your worktree).

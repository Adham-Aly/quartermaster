> [!IMPORTANT]
> **Install Quartermaster with this one command.** Paste it into Terminal (Apple silicon or Intel):
>
> ```sh
> curl -fsSL https://github.com/Adham-Aly/quartermaster/releases/latest/download/quartermaster-installer.sh | sh
> ```
>
> Then run `quartermaster`. To update later, run the same command again.

> [!CAUTION]
> **Don't click the files under "Assets" below.** Downloading them in a browser installs nothing:
> you get a loose file that your terminal can't find and that macOS blocks from opening. They are
> only there for the install command above to fetch.

## What's new in 0.3.0

Changes since [0.2.0](https://github.com/Adham-Aly/quartermaster/releases/tag/v0.2.0):

- **Interactive HTML plans.** Plan tasks can open a browser page with questions, diagrams and
  embedded images. Copy replies back into Quartermaster, then explicitly approve the plan before
  its build starts in normal mode. In auto mode, the planner resolves open questions and the build
  starts automatically.
- **Normal and auto autonomy.** Project autonomy is now named **normal** / **auto** instead of
  off / on. Change it from the Projects card's right-click menu or Settings ▸ Projects. Existing
  project modes migrate automatically.
- **A plain personal folder.** Personal tasks work directly in your chosen folder, without git
  branches or worktrees. Set it in Settings ▸ Projects; one code-writing task runs there at a time.
  Folder tasks now also appear correctly in the crew list and prior-task briefs.
- **In-memory session state.** Conversations and transient session state stay in memory; only kept
  state is mirrored to disk. Compaction and clear forget conversation details while preserving
  queued input, pending approvals and minimal unread outcomes. Upgrade and scratch-folder handling
  protect pending work and preserve legacy files when paths conflict.
- **Private provider scratchpads.** Provider crewmates and their helpers get a private temporary
  folder, removed when the task closes. Claude crewmates keep their existing scratch behavior.
- **A tidier terminal UI.** Crewmate replies render Markdown. Settings is a centred, compact popup;
  the top-bar menu now contains Settings, Update and Help. Server controls remain available through
  the `quartermaster server` CLI, and model levels and routing remain in Settings.
- **Routing guidance.** The new [routing guide](https://github.com/Adham-Aly/quartermaster/blob/v0.3.0/docs/routing.md)
  explains task rules, model levels, effort choices and availability fallbacks.

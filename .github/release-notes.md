> [!IMPORTANT]
> **Install Quartermaster with this one command.** Paste it into Terminal (Apple silicon or Intel):
>
> ```sh
> curl -fsSL https://github.com/Adham-Aly/quartermaster/releases/latest/download/quartermaster-installer.sh | sh
> ```
>
> Then run `quartermaster`. To update later, run the same command again.

## What's new in 0.6.0

Changes since [0.5.0](https://github.com/Adham-Aly/quartermaster/releases/tag/v0.5.0):

- **Review changes file by file.** Approval cards now show a tree of changed files beside a
  side-by-side Before/After view with line numbers. Pick a file with the mouse or keys, page and
  scroll each pane, and pan sideways through long lines. Additions stay readable in light and dark
  terminals.
- **Reports and plans you can open.** Lookup, research, reproduce and teach tasks now finish with
  a written HTML report; open it from the task with **Open report** or `r`. Plans come as two
  documents: a full technical plan for the crew, and a readable page for you with the same
  questions, replies and review controls as before.
- **Account quotas in Settings.** **Settings ▸ Accounts** shows the remaining quota for connected
  ChatGPT, GitHub Copilot and OpenRouter accounts, refreshed in the background and when Settings
  opens.
- **Ask for a model, a level or an effort, and get it.** Ask for a particular model, for a level
  such as Medium, or for an effort, and the Quartermaster sticks to it, even for crewmates started
  later or after a restart. A model you name that isn't available waits instead of being swapped
  for another. A `?` beside **Settings ▸ Routing ▸ Task kinds** explains each kind of task.
- **Start work later.** The Quartermaster and secondmates can schedule a crewmate to start when
  another task finishes, lands or reaches a set time, and can drop a dependency that no longer
  applies instead of leaving work stuck behind it.
- **Tasks hold several crewmates.** Crewmates are now named `C-n` and grouped in tasks `T-n`; a
  task can hold any number of them and goes away when its last one finishes. Folders can be
  anywhere, the Quartermaster can create a new one when you ask, and plain folders are worked in
  place without git.
- **Steering arrives sooner.** Messages to a running crewmate or secondmate now reach it between
  tool calls, instead of waiting for its turn to end.
- **Fixes.** Settings' Task kinds grid moves with the arrow keys in the order you see it and long
  notes wrap instead of being cut off; a scheduled start no longer fails after its chat message
  was cleared.

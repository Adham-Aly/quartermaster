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

## What's new in 0.4.0

Changes since [0.3.0](https://github.com/Adham-Aly/quartermaster/releases/tag/v0.3.0):

- **Send plan replies from the page.** Plan pages now have a **Send to Quartermaster** button that
  delivers your answers straight to the planner, with a one-line note in the chat. **Copy reply**
  still works as a fallback. If the app can't be reached, your answers stay saved in the page and
  it says so; each round accepts one reply, whichever way it arrives.
- **An Actions inbox.** A new list in the left column, below Projects and Crew, collects everything
  waiting on you: approvals, decisions, plan questions and approvals, agent questions, notices and
  setup prompts. Press Enter to open one and answer it; nothing is approved from the list itself.
- **Live context meter.** A strip above the input shows how full the current agent's context is,
  measured against the point where it compacts, along with the latest turn's tokens and cost.
  Task cards show a context row and token totals, and the chat shows when context is being
  compacted and how it went.
- **Release tags from the app.** A crewmate can propose a release tag for a GitHub project. Once
  you approve its card, the app pushes exactly that tag at that commit and never overwrites an
  existing one. It then watches GitHub and reports the tag, the release and its workflow runs as
  separate facts.
- **Chats survive restarts.** For agents on the app's own provider engine, the current chat and
  model context now survive a quit or a crash. A turn cut off by the app quitting says so, with
  **Ask again** or **Leave it**. Compaction and `/clear` trim what is kept; chats with Claude agents
  are still not saved to disk.
- **Durable working files.** Plans, evidence, pasted images, drafts and provider transcripts now
  live in your Quartermaster data folder instead of a temporary folder, and are removed once nothing
  needs them. Files left by older versions are moved over or cleaned up automatically.
- **Set up model levels first.** New installations start with empty model levels, and connecting
  an account no longer fills them for you. Add at least one model in **Settings ▸ Model levels**
  before messaging the Quartermaster; existing levels are kept. The
  [routing guide](https://github.com/Adham-Aly/quartermaster/blob/v0.4.0/docs/routing.md) explains
  how.

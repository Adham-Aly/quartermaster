> [!IMPORTANT]
> **Install Quartermaster with this one command.** Paste it into Terminal (Apple silicon or Intel):
>
> ```sh
> curl -fsSL https://github.com/Adham-Aly/quartermaster/releases/latest/download/quartermaster-installer.sh | sh
> ```
>
> Then run `quartermaster`. To update later, run the same command again.

## What's new in 0.7.0

Changes since [0.6.0](https://github.com/Adham-Aly/quartermaster/releases/tag/v0.6.0):

- **Reviews wait for your say-so.** A finished review now writes a report you can read and puts a
  **Review ready** card in Actions with its verdict, risk and number of findings. Open the report
  with **Open** (`o`), and nothing is done about the findings until you press **Go ahead** (`g`)
  or say so in chat. Then the Quartermaster does only what you asked for when you requested the
  review. Projects on auto autonomy skip the wait.
- **Routing follows your quotas.** When a tier holds several accounts, routing now picks the one
  with the most quota left, and skips accounts that have run out until they have quota again. Give
  an account a reserve mark (**Settings ▸ Accounts**, `m`) to keep it in reserve once it reaches
  that percentage used; each account's **Routing** line shows where it stands. A model, account
  or tier you ask for still wins.
- **Prepaid balances count as quota.** DeepSeek and Kimi (Moonshot) API keys now report their
  prepaid balance, measured since the last top-up, so they take part in quota-aware routing
  instead of always showing "quota unknown".
- **Move a crewmate while it works.** The Quartermaster and secondmates can move a running
  crewmate to another project or folder, or into and out of a worktree. It keeps its name, its
  task and its conversation. A worktree with uncommitted changes is never left behind: the move
  waits until the crewmate commits or discards them.
- **Crew and Projects show tasks.** The Crew box is now a tree of supervisors, their tasks and the
  crewmates in each task, and Projects lists each project's open tasks. Projects also grows into
  any space Actions doesn't need.
- **Updates show up sooner.** The app checks for a new release every hour instead of every six,
  so a new version appears in the top bar and the menu within the hour.

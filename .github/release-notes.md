> [!IMPORTANT]
> **Install Quartermaster with this one command.** Paste it into Terminal (Apple silicon or Intel):
>
> ```sh
> curl -fsSL https://github.com/Adham-Aly/quartermaster/releases/latest/download/quartermaster-installer.sh | sh
> ```
>
> Then run `quartermaster`. To update later, run the same command again.

## What's new in 0.5.0

Changes since [0.4.0](https://github.com/Adham-Aly/quartermaster/releases/tag/v0.4.0):

- **Choose a model level for each kind of task.** **Settings ▸ Routing** has a new **Task kinds**
  section: set build, fix, plan, research and the other kinds to Premium, High, Medium or Low.
  Changes apply at once and survive restarts; `r` resets a row and `R` resets them all. The
  [routing guide](https://github.com/Adham-Aly/quartermaster/blob/v0.5.0/docs/routing.md) explains
  the defaults.
- **Model levels hold model and effort pairs.** Each entry in **Settings ▸ Model levels** is now a
  fixed pairing such as `model (high)`, chosen from a searchable list, and the same model can sit
  in several levels at different efforts. Levels you saved before are kept and converted for you.
- **Premium means premium.** Asking for premium, or naming a premium model, now always tries the
  premium models first. If none can serve, the task falls back to High, then Medium, then Low,
  instead of waiting or quietly running a lower model, and the task card says why.
- **Cards open from Actions.** Approvals, decisions and questions no longer appear inside the chat.
  Each one is listed in **Actions**; click it or press Enter to answer it in a popup with the same
  buttons and keys.
- **Projects are folders.** A folder becomes a project when work starts in it, and you can ask the
  Quartermaster to clone a repository. The separate personal workspace is gone. Crewmates can use
  your GitHub login, and merges or tags made on GitHub outside the app are recorded.
- **Branches named after the work.** A task titled `fix(upload): retry on slow tmp` now works on
  the branch `fix/upload-retry-on-slow-tmp`, and its pull request reads like an ordinary one.
- **Desktop notifications removed.** Quartermaster no longer sends macOS notifications or rings the
  terminal bell; everything waiting on you is in **Actions**.
- **Fixes.** Helper agents started by a Claude crewmate are no longer stopped when another one
  finishes, and the quit dialog now reads correctly for one crewmate and for several.

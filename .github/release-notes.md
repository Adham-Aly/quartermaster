> [!IMPORTANT]
> **Install Quartermaster with this one command.** Paste it into Terminal (Apple silicon or Intel):
>
> ```sh
> curl -fsSL https://github.com/Adham-Aly/quartermaster/releases/latest/download/quartermaster-installer.sh | sh
> ```
>
> Then run `quartermaster`. To update later, run the same command again.

## What's new in 0.8.0

Changes since [0.7.1](https://github.com/Adham-Aly/quartermaster/releases/tag/v0.7.1):

- **A new look in teal.** The terminal UI is restyled around a teal accent, using 24-bit colour
  where your terminal supports it and the 16-colour palette otherwise. The brand in the top bar is
  now the menu button, sidebar sections sit under headings instead of in boxes, and each run of
  tool calls folds into one row (`✓ Read 4  Bash 3   7 calls · 14s`). Every chat opens with a
  header showing the version, the model and effort, and where the agent works. Terminals with
  kitty graphics (Ghostty, kitty, WezTerm) draw the Quartermaster logo.
- **Notifications.** A macOS banner appears for each request waiting on you (approvals,
  decisions, plans, questions and finished work to review), and goes away once it's answered.
  Banners are held back while you're looking at the app. A menu bar item shows how many are
  waiting; clicking a banner or a menu entry opens that card and brings your terminal tab
  forward. Turn them off in **Settings** (`notifications.enabled`).
- **What needs you comes first.** The left column now reads Projects, Needs you, Crew. Needs you
  draws your eye while something in it is new, and stops once you look. Crew groups tasks as
  Working, Waiting and Done, and Projects lists one line per project, busiest first; Enter opens
  a project's details.
- **Tasks have names.** The Quartermaster gives each new task a short name, shown wherever the
  task is listed.
- **Answers show up in Actions.** A finished lookup, research, reproduce or teach task puts a
  **Report ready** card in Actions. Open it with **Open**, or **Dismiss** it.
- **Briefs carry the right request.** When your request was a message or two back, or your latest
  message also covers other work, the Quartermaster can copy the right message into a crewmate's
  brief and leave out whole sentences about other work. Everything kept is your own words, and the
  brief says when something was left out.
- **Follow-ups stay together.** A plan and its build, a reproduction and its fix, a review or a
  retry now join the task of the work they follow on, instead of opening a new one.
- **Quotas.** Kimi Code and Grok subscriptions now show their quota and take part in quota-aware
  routing. Accounts without a quota check show a plain note instead of a false error, a ChatGPT
  login is no longer refreshed every five minutes, and the reserve mark is now a **soft limit**
  you can set from a button on each account (`s`).
- **Smaller fixes.** Closing finished work removes its row from Crew and its report files once
  you've read the report; the chat says "Closed" rather than "Stopped" when work is closed; and
  the Quartermaster knows crewmates can read files anywhere on your Mac, so it starts them where
  their changes belong.

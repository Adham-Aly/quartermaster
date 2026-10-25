> [!IMPORTANT]
> **Install Quartermaster with this one command.** Paste it into Terminal (Apple silicon or Intel):
>
> ```sh
> curl -fsSL https://github.com/Adham-Aly/quartermaster/releases/latest/download/quartermaster-installer.sh | sh
> ```
>
> Then run `quartermaster`. To update later, run the same command again.

## What's new in 0.7.1

Changes since [0.7.0](https://github.com/Adham-Aly/quartermaster/releases/tag/v0.7.0):

- **Fix: Quartermaster opens on an existing install again.** Some existing installs exited at
  launch with "can't open the database: database error: CHECK constraint failed". This happened
  when a crewmate whose task had closed still had messages it never received. Launching now
  drops that finished crewmate and its unreceived messages, and your other saved state opens as
  before. Nothing needs doing on your side: update and run `quartermaster`.

For everything new in 0.7, see the
[0.7.0 release notes](https://github.com/Adham-Aly/quartermaster/releases/tag/v0.7.0).

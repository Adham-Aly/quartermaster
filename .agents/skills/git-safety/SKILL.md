---
name: git-safety
description: Rules for any code that runs git or talks to GitHub - the app's guarded pushes and network commands, the crewmate push fence, repo-local config that could run programs, merge-gate invariants (approved head only). Use when touching qm-git, qm-github, qm-core delivery or settling, the crewmate environment, or anything that pushes, fetches, merges or calls the GitHub API.
---

# Git and GitHub safety

A task's worktree shares the repository's `.git/config` with the user's checkout, and a crewmate
runs arbitrary commands in its worktree. So any crewmate could change how git behaves for the app.
Merging or pushing the wrong thing is the worst failure the product can have. These rules keep the
app's own git honest; keep every one.

## The app's network git: `crates/qm-git/src/network.rs`

Every fetch, `ls-remote` and push the app runs is built by `Repo::network_command`, and nowhere
else (only a command carrying its `Transport` gets the longer network time limit). It overrides
what git lets it override (its own ssh command and variant, no askpass, protocol limits, TLS
verified) and refuses, with `GitError::RepoTransportConfig` naming the setting and how to remove
it, what git gives no override for (a repo-local proxy or certificate authority). Never run a
network git command another way.

## The app's local git: `crates/qm-git/src/git.rs`

Local commands (status, diff, merge, log) pass `APP_CONFIG` so repo-local config can't run a
program: no `core.fsmonitor`, content filters, merge or diff drivers (submodules included),
signing, editor, credential helper, external diff, textconv, lazy partial-clone fetch or
`core.alternateRefsCommand`. A new kind of git call starts from the same runner; a new
config key that can name a program gets pinned off there.

## App pushes: `crates/qm-core/src/delivery/push_guard.rs`

`push` is the only way the app pushes. It reads exactly where git would push (after `pushurl`,
`insteadOf`, `pushInsteadOf`) and refuses unless that is one URL naming the project's repository
(`RepoRef::from_remote`); pushes only there, re-checking under the repository's write lock; and
counts the push only once GitHub's API shows the pushed commit on the branch. Never record
`pushed_head` from git's word alone when the project has a GitHub repository.

## Release tags: `crates/qm-core/src/delivery/release.rs`

A crewmate's `propose_release_tag` binds project, repo id and name, tag and commit to a
`release_tag` card that is held under every autonomy and approved only from the UI. On approval
the app re-checks every binding, passes `releases::begin_push` (the one gate; a launch is never
sent twice), then `Repo::push_tag` (`qm-git` `tag.rs`): create-only, a lease that needs the tag
absent, through `push_guard`'s checked URL and the app's credentials. Published is claimed only
when GitHub's `tag_ref` shows exactly our object; otherwise it is unconfirmed and `reconcile`
reads it back for ten minutes, also after a restart (`release::recover`).

After publication `delivery/observe.rs` watches the tag, its GitHub release and the runs the tag
started for up to three hours (stored on the launch, re-armed at startup), then wakes the
supervisor once; `release_status` reads it meanwhile. Reads go only through
`qm_github::release_meta`'s closed `ReleaseEndpoint`s, capped at 1 MiB, keeping metadata and no
URL: never follow a URL from an answer, download an asset, or trigger a workflow. A refused or
failed read is "unavailable", never "none"; "no workflows" only when GitHub counts none.

## Crewmates can't push: `crates/qm-github/src/fence.rs`

`fence::apply` sets up a crewmate's environment and `fence::check` holds it to it, in independent
layers: no GitHub login (`GH_CONFIG_DIR` empty, no token variables), every push URL rewritten by
scheme (`pushInsteadOf` from environment-only config), `remote.origin.pushurl` at the fence, and
an ssh wrapper that refuses `git-receive-pack` on any host (`GIT_SSH_VARIANT=ssh` keeps fetches
with a port working). Fetches keep working. A crewmate's work leaves only through `pr_open`,
which the app pushes after its checks. Never pass a credential or token variable into a
crewmate's environment.

## The merge gate

- An approval applies only at the exact head that was approved (`MergePayload::head`). Right
  before merging, `qm-core` `settling/land_pr.rs` reads every fact again from GitHub, checks it
  with `qm_policy::merge_gate` (`check_approval`, `check_mergeable`) and sends the SHA as the
  merge's precondition; nothing counts until GitHub confirms. New commits after an approval or a Careful review need a new card or
  re-review. A commit after a Careful pass may be pushed (`pr_open`), but settling or approval
  needs the re-review first (`passed_for`).
- **Re-report:** `reported → reported` is allowed and carries the new head SHA. It wakes the
  supervisor, voids pending settle and approval cards for the old head, and restarts the check
  wait. It is refused mid-merge or mid-push, with what to do instead. A refused report always
  names the in-band path: a crewmate has no other channel to its supervisor.
- Checks seen on a head that later vanish block settle and approval; they never read as "no
  checks". A 404 from branch protection means "unknown", not "none required".
- A crash after asking GitHub to merge is recovered at startup from GitHub's state, never by
  offering the card again.
- Every PR merge is a squash merge (`qm_github::squash_request`), whatever the project's
  merge-method setting: the commit takes the PR's title, checked again as a commit name right
  before, and its body. A repository or ruleset that doesn't allow squash stops it
  (`check_squash_allowed`).
- Commit names are checked only on the task's own commits: `Repo::own_commits` runs
  `git log --no-merges <merge-base>..head` against a freshly fetched `origin/<branch>` (the local
  branch only when that ref is missing), never from the stored `base_commit`.
- After a merge the remote branch is deleted; side exits (stop, discard, retry) close the PR.
- A retired task's worktree and branch are removed (`qm-git` `retire.rs`), with their uncommitted
  changes and unmerged commits; git collects the commits later. No kept refs are written; startup
  `reconcile` deletes the `refs/quartermaster/kept/*` refs earlier versions wrote. A plain-folder
  task has no worktree or branch, and the app never runs git in its folder (`qm-git` `retire.rs`).

## Tokens

Git never prompts: the app's git runs with stdin closed and `GIT_TERMINAL_PROMPT=0`, and the
crewmate fence sets it too (a prompt would hang an agent's turn).
The GitHub API token comes from `gh auth token` at use, and `gh`'s path is read at each use
(`qm_github::GhProgram`), so a `gh` the setup check installs works without a restart. Never
log a token, a URL carrying userinfo (`qm_github::redact_url`) or an `Authorization` header.

## Verify

`cargo xtask check`, `cargo build`, then reproduce the flow against a private sandbox repository
of the user's (never another repository) in a throwaway run (skill `run-by-hand`). Try the
redirect you are guarding against (a `pushurl`, an `insteadOf`) and confirm the app refuses.

Propose a release tag for your project, when its release process needs one. You never create or push a tag yourself: this holds a card that only the user can approve, under every autonomy level. On approval the app creates exactly this tag at exactly this commit in your project's GitHub repository, never over an existing tag, and reports whether GitHub shows it. Tag the commit that is already on GitHub (normally the merged one on the default branch), after the release preparation has landed through a pull request.
- tag: the bare tag name, as the project's own release process names it (e.g. what its docs or workflow expect).
- commit: the full 40-digit SHA to tag (`git rev-parse <rev>`).
- branch: the branch the commit is on (optional; defaults to the project's default branch).
- kind: annotated (default) or lightweight.
- message: the annotated tag's message (optional; defaults to the tag name).
- summary: what this release is and why this commit, for the user's card.

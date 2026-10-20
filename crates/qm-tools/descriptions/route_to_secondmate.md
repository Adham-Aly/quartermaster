Send a request into one of your secondmates' scope, instead of starting the work yourself. The app copies the user's words, stamps a correlation ID and wakes the secondmate; its answer comes back to you as an app event with the same ID.
- secondmate: its ID, e.g. SM-02.
- request: what to do, in plain words.
- instructions: one-time instructions for this request only.
- base_branch, model, effort, premium, later_steps: only when the user's own message on this turn asked for them; refused on turns started by an app event.

Run a shell command in your worktree. Output (stdout and stderr together) comes back with the exit code; long output is cut in the middle.
A command still running after about 3 seconds becomes a background job (J3, J4, ...): you get its job ID and log file at once, and you're woken with its exit code and last lines when it exits. Meanwhile, read its output with job_output and stop it with job_kill. Use this for builds, test suites and servers; don't wait on them with sleep.
- command: the command.
- timeout: milliseconds before a foreground command is stopped (optional; only under 3,000 has an effect, since longer commands become jobs).
- description: what it does, in five to ten words (optional).
- run_in_background: start it as a background job at once (optional).
The command runs in its own process group with a clean environment and no input.

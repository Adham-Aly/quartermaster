Start a helper agent: a fresh agent on your model that works in your worktree with read, write, edit, grep, find, ls and bash (background jobs included), and returns its final message as this tool's result. It has no agent tool and none of your task tools, and it sees nothing of your conversation, so the prompt must carry everything it needs.
Use helpers to explore broadly (how is this done across the codebase?) and for independent searches or checks that can run side by side: several agent calls in one message run at the same time, up to 4 at once (more wait their turn). Don't use one for a lookup that a single grep, find or read answers; do that yourself.
- description: what the helper does, in three to six words; shown in the task's activity.
- prompt: the whole job: what to find or do, where to look, whether it may change files, and what its final message must contain (paths, line numbers, findings).
A helper stops after 100 model calls, and when you are interrupted or stopped.

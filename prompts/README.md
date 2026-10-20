# Prompt sources

The Markdown sources of every prompt the app sends, embedded into the binary at build time by
`crates/qm-prompts`. Each file is registered with one line in `crates/qm-prompts/src/index.rs`,
the prompts index.

| File | What |
|---|---|
| `quartermaster.md` | The Quartermaster's system prompt |
| `crewmate*.md` | The crewmate prompt, appended to Claude Code's preset or used whole on the provider engine |
| `secondmate*.md` | The secondmate prompt and charter |

Tool descriptions are not here: each tool's description lives next to its type, in
`crates/qm-tools/descriptions/<tool>.md`.

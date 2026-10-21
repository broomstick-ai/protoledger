# legacy-session-index-synthetic

The rollouts of `legacy-fork-inter-agent-synthetic` (a thread and its fork)
in a `CODEX_HOME` that also holds `session_index.jsonl`, the thread-name index
Codex appends to on every rename (`{id, thread_name, updated_at}`, newest wins).
The index lines are synthetic, written from openai/codex 0e67502 (2026-10-16):
`codex-rs/rollout/src/session_index.rs:20-50` (`SessionIndexEntry`,
`append_thread_name`, `serde_json::to_string` of the entry). The first thread
is named, its fork is named, then the first thread is renamed.

- **Origin:** synthetic (see above).
- **License:** MIT OR Apache-2.0 (this repository)
- **Sanitised:** Synthetic; neutral content.

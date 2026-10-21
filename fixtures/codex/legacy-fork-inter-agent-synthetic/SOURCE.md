# legacy-fork-inter-agent-synthetic

Synthetic, written from openai/codex 0e67502 (2026-10-16): `codex-rs/protocol/src/protocol.rs:3254-3330` (SessionMeta, `forked_from_id`), `:3426-3480` (TurnContextItem), `:913-929` (InterAgentCommunication), `:2391-2401` (TokenUsageRecord), `:3405` (WorldStateItem), `:2271-2324` (task_started/complete), `:4365-4398` (turn_aborted). The fork file starts with its own session_meta followed by a copy of the parent's records, as Codex writes a fork (`rollout/src/recorder.rs:1131-1135`). Loaded with Codex's own `load_rollout_items`: 0 parse errors.

- **Origin:** synthetic (see above).
- **License:** MIT OR Apache-2.0 (this repository)
- **Sanitised:** Synthetic; neutral content.

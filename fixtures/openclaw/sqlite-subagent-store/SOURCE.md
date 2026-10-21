# sqlite-subagent-store

- **Origin:** generated on 2026-10-16 by OpenClaw itself: the npm package
  `openclaw@2026.10.4` (383c124, MIT, OpenClaw Foundation) run on Node
  v24.22.0 as `openclaw doctor --session-sqlite import --non-interactive`,
  with `HOME` and `OPENCLAW_STATE_DIR` set to a throwaway directory holding
  this repository's `fixtures/openclaw/v4-subagent-store/native/agents/`
  (synthetic transcripts + sessions.json). The resulting
  `agents/main/agent/openclaw-agent.sqlite` (agent schema 24, app 2026.10.4 in
  `schema_meta`) was checkpointed, switched to `journal_mode=DELETE` and
  `VACUUM`ed so it is one self-contained file.
- **License:** store layout OpenClaw's (MIT); content synthetic (see
  `../v4-subagent-store/SOURCE.md`).
- **What it exercises:** the SQLite store OpenClaw reads since v2026.8.1:
  `transcript_events` rows, `session_nodes` / `session_windows` with
  `spawned_by` and a label, OpenClaw's derived tables (event identities,
  active-event projection, transcript FTS) and triggers.
- **Sanitisation:** none needed.

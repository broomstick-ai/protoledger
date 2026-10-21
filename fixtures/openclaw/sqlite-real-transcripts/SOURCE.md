# sqlite-real-transcripts

- **Origin:** generated on 2026-10-16 by OpenClaw itself (`openclaw@2026.10.4`
  on Node v24.22.0, `openclaw doctor --session-sqlite import
  --non-interactive`, throwaway `HOME` / `OPENCLAW_STATE_DIR`) from three
  real transcripts already in this repository: `tool-calls-thinking`,
  `delivery-mirror` and `corrupt-header` (see their SOURCE.md for origin,
  license and sanitisation), referenced by a minimal synthetic
  `sessions.json` (Telegram-style keys with placeholder ids). OpenClaw
  reported the corrupt-header transcript as `transcript_malformed` and kept
  only its readable prefix (no events), which this case records. The
  database was checkpointed, switched to `journal_mode=DELETE` and `VACUUM`ed.
- **License:** transcripts Apache-2.0 (kayba-ai/agentic-context-engine) and
  MIT (ollieb89/claw_imperium); store layout MIT (OpenClaw).
- **What it exercises:** real transcript lines as `transcript_events` rows,
  three session keys, OpenClaw's handling of a corrupt transcript.
- **Sanitisation:** as in the source cases.

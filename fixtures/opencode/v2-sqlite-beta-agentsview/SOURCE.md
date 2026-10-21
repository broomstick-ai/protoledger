# v2-sqlite-beta-agentsview

- **Source:** `internal/parser/testdata/opencode_v2/beta.sql` of
  [kenn-io/agentsview](https://github.com/kenn-io/agentsview) at commit
  `905a5fe2ee7c01fb55f620d9ce4e4728aed177ae`, loaded with
  `sqlite3 opencode.db < beta.sql`.
- **License:** MIT (agentsview, © 2026 Kenn Software LLC).
- **What it is:** a real `opencode.db` dump "captured from OpenCode
  0.0.0-beta-19381; scratch paths replaced" (the 2.0 beta line): tables
  `project`, `session_v2`, `session_message` with three sessions run against the
  free `opencode/big-pickle` model — a plain reply, a `read` tool round trip with
  a follow-up turn, and a `bash` tool with a non-zero exit. Message `seq` has gaps
  (the event log assigns it); the beta's tool state carries `structured`,
  `outputPaths` and `provider.executed`, which differ from 2.0.31.
- **Exercises:** v2-sqlite detection, row-identical re-export of a db whose
  schema predates 2.0.0 (no `event_sequence` table), turn inference without idle
  markers.
- **Sanitised:** by agentsview (paths are `/workspace/project-a`).

# v4-subagent-store

- **Origin:** SYNTHETIC, written for protoledger on 2026-10-16
- **Repository / commit:** built from openclaw/openclaw @ d1a7edc3198f329c440404cab33e9f28014a5379: src/config/sessions/transcript-header.ts (header), src/agents/sessions/session-manager-entries.ts (payload-first entry order, reset entries, compaction tokensAfter/__openclaw), session-manager-types.ts (leaf control rows), src/config/sessions/types.ts (sessions.json spawnedBy), docs/tools/subagents/tool-reference.md (sessions_spawn receipt)
- **License:** this repository's license
- **What it is:** Header v4 and OpenClaw's forked writer: UUID entry ids, payload before id/parentId/timestamp (custom entries put timestamp before id), a `sessions_spawn` subagent in its own transcript linked by `spawnedBy` in sessions.json, `reset`, `leaf`, a delivery-mirror row.
- **Sanitisation:** None (no real data). Not produced by a running OpenClaw.

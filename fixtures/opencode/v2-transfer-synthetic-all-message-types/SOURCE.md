# v2-transfer-synthetic-all-message-types

- **Synthetic**, written by `mkfix.py` from opencode 2.0.31's schema
  (anomalyco/opencode `180fd9d6`, MIT): `packages/schema/src/session-message.ts`
  (all eleven message types and their encode key order), `session.ts`,
  `session-transfer.ts`, `tool.ts`, `prompt.ts`, `packages/core/src/tool/plugin/subagent.ts`
  (subagent tool output and `metadata.sessionID`) and
  `packages/core/src/session/projector.ts` (`projectFork`: a fork copies settled
  history under ids `msg_<fork event>_<seq>`).
- **License:** MIT OR Apache-2.0 (this repository).
- **Content:** three `opencode session export` files — a parent session with
  model-switched, agent-switched, system, skill, user (file attachment, agent
  mention), synthetic, assistant (reasoning with provider state, a completed
  `subagent` tool, text, snapshot files, tokens and cost), shell (exit 7),
  assistant with an errored `patch` tool, an image tool result and an error,
  idle (failed), location-switched, a completed auto compaction, a failed manual
  compaction, idle (interrupted); the subagent's child session (`parentID`); and a
  fork of the parent through its first assistant step (`fork{sessionID, boundary}`).
- **Neutral prompts only; paths are `/home/user/project-a`.**

# v1-storage-json-synthetic-subagent-compaction

- **Synthetic**, written by `mkfix.py` (`fixtures/opencode/_tools/mkfix.py`)
  from opencode's own definitions at tag `v1.1.8` of anomalyco/opencode (MIT):
  `packages/opencode/src/storage/storage.ts` (layout, `JSON.stringify(x, null, 2)`,
  no trailing newline, the `migration` counter), `session/index.ts` (session
  info), `session/message-v2.ts` (message and part shapes), `tool/task.ts` (the
  task tool's `metadata.sessionId` and `<task_metadata>` output) and
  `project/project.ts` (project file). Ids follow `packages/schema/src/identifier.ts`
  (descending `ses_`, ascending `msg_`/`prt_`), with deterministic tails.
- **License:** MIT OR Apache-2.0 (this repository).
- **Content:** a parent session and the child session its `task` tool ran
  (`parentID`); a user prompt with a `data:` file attachment and a `@` mention
  source; reasoning with an Anthropic signature in `metadata`; step-start /
  step-finish parts with snapshots; a `retry` part (APIError 529); an `edit` tool
  and its `patch` part; a manual compaction (user message with a `compaction`
  part, `summary: true` assistant answer); a synthetic text part; an aborted
  assistant (`MessageAbortedError`) with an errored `glob` tool;
  `session_diff/<id>.json`; a project file.
- **Neutral prompts only; paths are `/home/user/project-a`.**

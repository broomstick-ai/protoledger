# subagent-task

- **Origin:** synthetic, written for protoledger on 2026-10-16.
- **Built from:** the verbatim droid subagent `session_start` line quoted in
  entireio/cli's tests (MIT; `callingSessionId`, `callingToolUseId`,
  `sessionTitle: "<subagentType>: <description>"`, Task-prompt `title`), and the
  message/tool shapes of the real captures in the other droid fixtures
  (superbasedapp/observer @ f80c1a36, Apache-2.0).
- **License:** same as this repository.
- **What it is:** a parent session whose `Task` tool_use spawns a worker
  subagent, written as droid does: a sibling `.jsonl` in the same cwd folder
  whose `session_start` names the calling session and tool_use id.
- **Unverified:** not produced by a running droid; no settings sidecars.
- **Sanitisation:** none needed (no real data).

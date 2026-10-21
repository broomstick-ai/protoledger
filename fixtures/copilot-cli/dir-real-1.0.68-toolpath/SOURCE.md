# dir-real-1.0.68-toolpath

**Origin:** `crates/toolpath-copilot/tests/fixtures/real-session.jsonl` in
https://github.com/empathic/toolpath at commit
`77dc16a598bfe8fa832495b0c583160f6f4400b3`, copied byte for byte as
`events.jsonl`.

A first-hand capture published by toolpath's authors: Copilot CLI 1.0.68
(`copilotVersion`) on a scratch directory (`/tmp/elicit-scratch`) running a
scripted tool-exercise prompt. Loads in Copilot CLI 1.0.98
(`_generators/verify-resume.sh`).

**License:** Apache-2.0.

**What it exercises:** `session.start` with context, `session.model_change`,
`system.message`, `user.message` with `transformedContent`, eleven
`assistant.turn_*` turns, `assistant.message` with `reasoningText` +
`reasoningOpaque`, `toolRequests`, `outputTokens`, tool execution events with
a failure (`error {message, code}`), a `task` subagent whose events carry the
envelope `agentId` (imported as a child ledger), and `session.shutdown`
model metrics (cumulative usage).

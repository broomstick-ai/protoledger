# legacy-synthetic-0.0.331

**Origin:** synthetic, written for this repository on 2026-10-16 from the
field names of Copilot CLI 0.0.331's session schema (the whole-file
`history-session-state/session_<id>_<ms>.json` that versions <= 0.0.341
wrote), formatted as the CLI did: `JSON.stringify(obj, null, 2)`, no trailing
newline. Not written by the CLI; no CLI code included.

**License:** MIT OR Apache-2.0 (this repository).

**What it exercises:** `chatMessages` (OpenAI format with `tool_calls` /
`tool` messages) and the parallel UI `timeline` (`user`, `copilot`,
`tool_call_requested`, `tool_call_completed`) whose timestamps the importer
borrows, and `selectedModel`.

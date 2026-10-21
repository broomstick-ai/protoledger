# GitHub Copilot CLI (`copilot-cli`)

GitHub's terminal agent, `copilot` (npm `@github/copilot`). Its package is
under a proprietary license, so nothing from it is copied here. The event
schema below comes from GitHub's MIT-licensed generated types in
[github/copilot-sdk] (`rust/src/generated/session_events.rs`, CLI 1.0.100-0
snapshot, and its git history), from real captures published under
permissive licenses, and from behaviour observed by running the real 1.0.98
binary offline (see Verified).

[github/copilot-sdk]: https://github.com/github/copilot-sdk

## Variants

| variant | where | producer versions | read by current Copilot? |
|---|---|---|---|
| `session-dir` | `$COPILOT_HOME/session-state/<session id>/` (default `~/.copilot`): `events.jsonl`, `workspace.yaml`, `plan.md`, `checkpoints/`, `files/` | >= 0.0.378 (2026-01) | yes: `copilot --resume <id>` |
| `flat-jsonl` | `session-state/<session id>.jsonl`, the same events | 0.0.342 - 0.0.377 (2025-10 - 2026-01) | no: refused since 1.0.44 |
| `legacy-json` | `history-session-state/session_<id>_<epoch ms>.json`, `JSON.stringify({sessionId, startTime, chatMessages, timeline, selectedModel?}, null, 2)` | <= 0.0.341 | no: refused since 1.0.44 |
| `interchange` | any `*.jsonl`: the "semantic JSONL interchange v1" (`session` header, `message`, `tool-call`, `tool-result`) | read by `copilot sessions import` >= 1.0.85 | yes, as a new session |

**Producer version.** Every `session.start` records `copilotVersion` (the VS
Code agent host writes the same format with its extension version and
`vscodeVersion`); the importer sets `agent.source.version` from the first one
and `native.version` on each event from the latest `session.start` /
`session.resume`, so a file resumed by several versions keeps them apart. The
event format's own `version` has been `1` throughout.

Shape history (event types are added, never changed): 0.0.342 event JSONL
(19 types); 0.0.350-0.0.376 truncation, handoff, reasoning, compaction;
0.0.378 session directories and `workspace.yaml`; 0.0.385 / 0.0.410
`subagent.*`, `session.shutdown`, `title_changed`, `skill.invoked`; 0.0.420
unknown event types are skipped on load; envelope `agentId` (early 2026);
1.0.44 legacy formats refused; 1.0.64 `session.binary_asset`; 1.0.81
`reasoningBlocks`; 1.0.85 `sessions import`; 1.0.92 `fileEdits`.

## Record → kind mapping (`session-dir`, `flat-jsonl`)

Each line is `{"type", "data", "id", "timestamp", "parentId"[, "ephemeral"][, "agentId"]}`;
event ids are the line `id` (`@<byte offset>` when absent).

| event | kind |
|---|---|
| `session.start`, `session.resume`, `session.context_changed` | `settings` (`selectedModel`, `context.cwd`, `context.branch` → `git_branch`, `reasoningEffort` → `effort`); `sessionId` → agent id, `startTime` → `created_at` |
| `session.model_change` | `settings` (`newModel`, `reasoningEffort`) |
| `session.mode_changed` / `subagent.selected` | `settings` (`permission_mode` / `agent`) |
| `session.info` | `text` (system), audience `record` |
| `session.warning`, `session.error` | `error` (`errorType` → `kind`, `statusCode` → `status`) |
| `session.truncation`, `session.compaction_start`, `session.context_cleared` | `compaction` boundary |
| `session.compaction_complete` with `summaryContent` | `compaction` summary |
| `session.title_changed`; `workspace.yaml` `name` | `title` |
| `session.shutdown` | one `usage` per model in `modelMetrics`, `cumulative: true`; `input` = `inputTokens` − `cacheReadTokens` − `cacheWriteTokens` (SPEC §3.1.1: Copilot's `inputTokens` counts both) |
| `user.message` | `text` (user) = `transformedContent` (what the model saw) or `content`; `blob` attachments with data → `media`; `file` / `directory` attachments → `media` with `path` |
| `system.message`, `system.notification` | `text` (system / developer) |
| `assistant.turn_start` / `turn_end` / `abort` | `turn` start / end (`aborted` with `reason`) |
| `assistant.message` | `reasoning` (`reasoningText`; `reasoningOpaque` → `sealed {anthropic, signature}` beside reasoning text, `sealed {anthropic, encrypted}` alone (SPEC §3.1.5), `encryptedContent` → `sealed {openai, encrypted}`), `text` (assistant), `tool_call` per `toolRequests` entry (`type`, `mcpServerName` → `tool_type`, `server`), `usage` (`outputTokens`); `group` = `messageId` |
| `assistant.reasoning` | `reasoning`, unless the turn's message already carried the same text |
| `tool.execution_start`, `tool.user_requested` | `tool_call`, only for a call no message requested |
| `tool.execution_complete` | `tool_result` (`result.content`, or `error.message` with `is_error`); `fileEdits` → `file_change` (with `detailedContent` as the diff when there is one edit) |
| `subagent.started` / `completed` / `failed` | `subagent` start / end in the parent |
| events with an envelope `agentId` | the subagent's own ledger, `<session id>.<agentId>`, with `parent = {agent: session, event: subagent.started}` |
| `hook.*`, `permission.*`, `skill.*`, `session.binary_asset`, usage records, unknown types, unparseable lines | `native`, kept verbatim |

Subagents are split into their own ledgers (the spec's one-file-per-agent
rule), so the parent's `agent.source.extra.order` records how their lines
were interleaved (`[[agentId or "", run length], ...]`) for an exact
re-export. Everything else in the session directory (`workspace.yaml`,
`checkpoints/`, `plan.md`, ...) is kept verbatim in `agent.source.extra.files`.

`legacy-json`: `chatMessages` (OpenAI format) → `text`, `media`
(`image_url`), `tool_call` (`tool_calls`, arguments parsed), `tool_result`
(`tool` messages; failure from the timeline's `result.type`), with timestamps
taken from the matching `timeline` entries; timeline `info` / `error` →
`text` (record) / `error`; other timeline entries are `native`;
`selectedModel` → `settings`.

`interchange`: header → agent id (`externalId`), `created_at`, `title`,
`settings` (`workspace.workingDirectory`, `branch`); `message` → `text` /
`media` (attachment `path`) and `settings` (`model`); `tool-call` →
`tool_call`; `tool-result` → `tool_result`.

## Export

Default variant: `session-dir` (the source's variant for a Copilot ledger;
`--target-version` < 0.0.378 / < 0.0.342 picks `flat-jsonl` / `legacy-json`;
`--variant interchange` for `copilot sessions import`).

A foreign ledger becomes `<session id>/events.jsonl` and `workspace.yaml`
(`id`, `cwd`, `branch`, `name` from the title, `summary_count`, `created_at`,
`updated_at`). The session id is the agent id if it is a UUID, else UUID v5 of
it; every line's `id` is UUID v5 of the event id it renders, `parentId` chains
the lines, timestamps end in `Z`. The log is what the loader requires and the
CLI itself writes: a full `session.start` (`version: 1`, `producer:
copilot-agent`, `copilotVersion` = `--target-version` or 1.0.98, `startTime`,
`context.cwd`), `user.message`, then per model message `assistant.turn_start`
→ `assistant.message` (`messageId`, `content`, `toolRequests`,
`reasoningText`, `model`, `outputTokens`) → `tool.execution_start` /
`tool.execution_complete` per call → `assistant.turn_end`;
`session.model_change` on model changes; `system.message` for system /
developer text; `session.info` for record-only notices; `session.error`.
A subagent ledger in the set is written inline after the parent's
`subagent.started`, its lines carrying `agentId`, inside the parent's open
turn, as the CLI does.

`interchange` export writes the header (`source.application: protoledger`),
`message` / `tool-call` / `tool-result` records with `turnId`s, and adds an
empty assistant message where a tool call has no assistant text before it
(the importer requires one). `copilot sessions import` re-stamps the times
and needs the working directory to exist (`--working-directory`).

### Loss codes

| code | variants | when |
|---|---|---|
| `reasoning-sealed-dropped` | session | opaque reasoning (session-bound; Copilot strips it on resume anyway) |
| `reasoning-dropped` | legacy | the legacy format has no reasoning |
| `media-dropped` | all | assistant media, media given only by path (session); any media (interchange: attachments must be files beside it) |
| `tool-result-media-dropped` | all | media parts in tool output |
| `tool-result-unpaired` | session | a result whose call is not in the ledger |
| `usage-cumulative-dropped`, `usage-dropped`, `usage-fields-dropped` | session | totals (only `session.shutdown` holds them, not synthesized); usage with no message; anything but output tokens |
| `settings-dropped` | session | settings other than model, cwd, branch |
| `error-fields-dropped` | session | `retryable` |
| `compaction-as-user-text`, `compaction-boundary-dropped` | session | a summary becomes a user message; boundaries have no event |
| `peer-message-fields-dropped` | session | peer messages are written as SPEC §3.1.6 wrapped text (user when incoming, assistant when outgoing), which the importer turns back into a `peer_message`; this code names the fields beyond direction, peer, network and text that the wrapper cannot hold (interchange and legacy drop peer messages as `peer-message-dropped`) |
| `system-text-dropped` | interchange | only user and assistant messages |
| `empty-assistant-message-added` | interchange | see above |
| `exact-text-fallback` | legacy | an unchanged document (every record its own) could not be rebuilt from its records' own texts (a piece has no text, or the rebuilt text does not parse to the document's value) and is written from the parsed value, so its layout, number spelling or escapes may differ from the source. No fixture reaches it; a unit test does (`an_unchanged_document_not_rebuilt_from_its_texts_names_the_fallback`) |
| `tool-input-text-wrapped`, `tool-input-wrapped` | all | a custom tool's raw `input_text` (SPEC §3.1.2) / a non-object input becomes `arguments: {"input": ...}` |
| `<kind>-dropped` | all | anything else (`file-change`, `app`, `native`, ...) |


## Branches (SPEC §2.3)

Copilot CLI's `parentId` chains each event to the one before it, and nothing shows Copilot resuming along it rather than in file order (the importer does not read `parentId` as a branch either); the legacy and interchange shapes are plain lists. So a branch is treated as not expressible. Decision: **not expressible** — the exporter renders the active
path, names `abandoned-branch-not-rendered`, and carries the abandoned events on
the next line. Done for `session-dir` and `flat-jsonl` (the `legacy-json` and `interchange` writers still write every event). The comparator now reads `after` (`<kind>.branch` costs what dropping the event costs, `<kind>.after` a wrong predecessor), so writing the abandoned branch as the conversation is no longer free: `protoledger/branch-rewind` via copilot-cli was 2 with the branch written inline (where the comparator could not see it) and is 15 with the active path rendered, its 4 abandoned events (13 facts) missing; written inline it would score 16 under the new comparator. With carry the trip still loses nothing (carried score 0).

## Carry

`session-dir` / `flat-jsonl`: Copilot accepts unknown envelope fields (tested
on 1.0.98), so each rendered line carries the events it shows under a
top-level `protoledger` key; lines the exporter adds on its own (turn bounds,
`tool.execution_start`) carry an empty list so a carry trip does not re-import
them; `session.start` (and a subagent's first line) also carries the agent
header. A carry trip comes back exact (unit test `carry_comes_back_exact`).
**`legacy-json` and `interchange` do not carry**: the legacy `chatMessages`
went to the model API, and the interchange importer validates strictly.

## Verified

- Return trips X → protoledger → here → protoledger → X with carry, for
  every fixture of the other two secondary formats (gemini-cli, cursor,
  copilot-cli), restore the source events exactly and write X's files back
  byte for byte (the carried agent header includes `source`, and events
  ride with the record at their own position). The only exception is a
  cursor `state.vscdb` source, whose self trip is row-exact only.

- Self trips are byte-exact on all ten fixtures, including the real 1.0.68
  CLI capture, two real VS Code agent-host captures, and a session directory
  written by Copilot CLI 1.0.98 itself.
- Loading by the real Copilot CLI 1.0.98 (darwin-arm64), offline, with
  `fixtures/copilot-cli/_generators/verify-resume.sh <copilot binary> <dir>`:
  Copilot validates `events.jsonl` before authenticating, so a loadable
  session stops at the auth error and a bad one at "Session file is
  corrupted". Exports of a foreign ledger (with and without carry) and
  conversions of gemini-cli and cursor fixtures (including a gemini subagent)
  all load; a known-bad session (missing `parentId`) is rejected.
- `interchange` exports are accepted by `copilot sessions import` 1.0.98
  (`--dry-run` and a real import).

## Unverified

- That the model continues correctly after resuming an exported session (no
  Copilot login was used); how the TUI renders it.
- `flat-jsonl` and `legacy-json` exports against the old CLIs that read them
  (current CLIs refuse both by design).
- Fields and event types the fixtures do not exercise (MCP details,
  compaction checkpoints on resume, `session.binary_asset` references).

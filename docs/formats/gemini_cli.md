# Gemini CLI (`gemini-cli`)

Google's open-source terminal agent, [google-gemini/gemini-cli]. The adapter
reads and writes the chat recordings Gemini CLI keeps for `/resume` and
`--resume`, and the checkpoints `/chat save` writes for `/chat resume`.
Everything here was read from the gemini-cli source, mainly
`packages/core/src/services/chatRecordingService.ts`,
`chatRecordingTypes.ts`, `packages/core/src/core/logger.ts` and
`packages/core/src/utils/sessionUtils.ts` at commit `ed96faedf`
(v0.49.0-preview.0-237, 2026-10-16), and from their git history.

[google-gemini/gemini-cli]: https://github.com/google-gemini/gemini-cli

## Where it lives

`~/.gemini/tmp/<project>/` (`<project>` is a hash or short id of the project
root, depending on version):

| file | what |
|---|---|
| `chats/session-<YYYY-MM-DDTHH-MM>-<first 8 of session id>.jsonl` | main session (>= 0.39.0) |
| `chats/session-<...>.json` | main session (0.4.0 - 0.38.x) |
| `chats/<parent session id>/<session id>.jsonl` (`.json`) | a subagent's session, `kind: "subagent"` |
| `checkpoint-<encodeURIComponent(tag)>.json` | `/chat save <tag>` |
| `logs.json` | the user-prompt log; not a transcript, ignored |
| `tool-outputs/`, `plans/` | large tool outputs and plans; not read |

The importer takes one file, or a directory laid out like `chats/` (sessions
and their subagent directories, or checkpoints). The exporter writes the same
layout, relative to that directory.

## Variants

Gemini CLI records no producer version in any of these files. The adapter
infers a version range from the shape and stores it as
`agent.source.extra.version_hint`.

| variant | producer versions | shape |
|---|---|---|
| `session-jsonl` | >= 0.39.0 (`f74491358`, #23749) | Append-only JSONL, compact `JSON.stringify` lines. Line 1 is metadata `{sessionId, projectHash, startTime, lastUpdated, kind?, directories?, summary?}`. Then message records (a message is **re-appended whole** every time it changes: tokens arrive, tool calls are added), `{"$set":{...}}` metadata updates (`lastUpdated` after every message, `summary`, `directories`, `memoryScratchpad` >= 0.41), `{"$rewindTo": "<message id>"}` (drop that message and everything after). 0.44.0 - 0.63.x also append the whole history as `{"$set":{"messages":[...]}}` on every sync; >= 0.64.0 (`d1cc08a8a`, #29568) instead append `{"$patch":{updates, removeIds, orderIds}}` deltas. Resuming a `.json` session migrates it to `.jsonl`. |
| `session-json` | 0.4.0 (`b5dd6f9ea`) - 0.38.x | The same `ConversationRecord` as one `JSON.stringify(record, null, 2)` document (no trailing newline), rewritten on every change. Fields gained over time: `info`/`error`/`warning` messages 0.18, `summary` 0.21, `displayContent` and `kind` 0.25, `directories` 0.28. |
| `checkpoint` | all | `JSON.stringify(x, null, 2)` of the Gemini API history. A bare `Content[]` before 0.16.0; `{history, authType}` from 0.16.0 (`48e3932f6`), which still reads the bare form. |

Message content also changed shape inside the JSONL variant: up to 0.43 a
gemini message's `content` is its text (thoughts in `thoughts`, calls in
`toolCalls` with inline `result`). From 0.44 (`055e0f645`) the recorder also
syncs the API history into the log, so a model message's `content` becomes
the full part list (thought parts with `thoughtSignature`, text,
`functionCall`), and tool responses are recorded as `user` messages made of
`functionResponse` parts. Both are read.

## Record → kind mapping

| native | kind | notes |
|---|---|---|
| metadata line / document head | header | `sessionId` → `agent.id` and `source.native_id`; `startTime` → `created_at`; `kind`, `projectHash`, file name → `source.extra`. The metadata line itself is kept in `source.extra.meta_record`. |
| `summary` (metadata, `$set`, document) | `title` | |
| `user` message, text part | `text` (user) | `turn` = this message's id |
| `user` message, `inlineData` / `fileData` part | `media` (user) | `data` / `url` |
| `user` message, `functionResponse` part | `tool_result` | the result appears again in the call's `toolCalls[].result`; one event is made |
| `gemini` `thoughts[]` | `reasoning` | `text` = `**subject** description`, as Gemini CLI shows a thought (SPEC §3.1.5: the subject heads the thought, it is not a summary of it); export splits a leading `**…** ` back off as the subject; only when `content` has no thought parts |
| `gemini` content thought part | `reasoning` | `sealed: {vendor: google, kind: signature}` from `thoughtSignature` |
| `gemini` content text | `text` (assistant) | `group` = message id |
| `gemini` `functionCall` part / `toolCalls[]` | `tool_call` | `call_id`, `name`, `input` = `args` |
| `toolCalls[].agentId` | `subagent` start | `agent` = the subagent's session id; the child ledger gets `parent` |
| `toolCalls[].result` + `status` | `tool_result` | `output` = `response.output`, or `response.error` with `is_error`; other responses as JSON text. `status: cancelled` → `is_error` + `status`. |
| `toolCalls[].resultDisplay` FileDiff | `file_change` | `path`, `op` (create/edit), `diff`, `content` = `newContent` |
| `toolCalls[].resultDisplay` SubagentProgress | `subagent` end | `outcome` from `state` (completed → ok, error → error, cancelled → aborted), `result` |
| `gemini` `tokens` | `usage` | `input` − `cached` → `input`, `output` + `thoughts` → `output` (SPEC §3.1.1: Gemini's `input` counts cached tokens and its `output` excludes thoughts), `cached` → `cache_read`, `thoughts` → `reasoning`; `model`. Export reverses both; `total` = input + output + thoughts |
| `gemini` `model` | `settings {model}` | when it changes |
| `info` | `text` (system), audience `record` | UI notices, never sent to the model on resume |
| `error`, `warning` | `error` | `kind` = `error`/`warning` |
| `$set`, `$rewindTo`, `$patch`, re-appended messages with nothing new | `native` | |
| checkpoint `Content` | per part, as above | ids `@<index>`; a call without an id is paired with its response by name |

Because a message is rewritten many times, the importer gives every neutral
fact a key (message id + text hash, call id, ...) and a record only produces
events for facts not seen before; a record with nothing new becomes one
`native` event. Event ids are the message id, `<message id>@<byte offset>` for
a later copy of the same message, and `@<byte offset>` for lines with no id.

## Export

A ledger imported from Gemini CLI is written back record for record (exact,
including the pretty-printed JSON and checkpoint files: the document minus
its message list is kept in `source.extra.document`, and the importer checks
that re-rendering reproduces the file, falling back to keeping the raw file).

A foreign ledger is rendered as `session-jsonl` by default (`--variant
session-json` or `--target-version <0.39` for the whole-file form; `--variant
checkpoint` for `/chat save`). The session uses the plain shape every reader
since 0.4 understands: user messages with text/`inlineData` parts, one
`gemini` message per model message (`group`) with `content` text,
`thoughts`, `toolCalls` (results inline, `status`), `tokens` and `model`.
Consecutive model messages are merged, and a new gemini message starts only
after tool calls (their responses come between), so the resumed history
alternates; consecutive user records are merged for the same reason. A tool
call with no result is closed the way Gemini CLI closes an interrupted call
(`{"error": "Response was lost or interrupted."}`, status `cancelled`), since
an unanswered `functionCall` would be rejected by the API. Ids: message ids
are UUID v5 of the first event id; the session id is the agent id if it is a
UUID, else UUID v5 of it; call ids are kept. `projectHash` is sha256 of the
ledger's `settings.cwd`, which is what Gemini CLI computes for that project,
so `gemini --resume` run in that directory finds the session. A subagent
ledger whose parent is in the set is written to
`<parent session id>/<session id>.jsonl` with `kind: subagent`, and the
parent's tool call gets its `agentId`.

### Loss codes

| code | when |
|---|---|
| `reasoning-sealed-dropped` | signatures / encrypted reasoning (sessions keep thought summaries; a checkpoint keeps a Google `thoughtSignature`) |
| `reasoning-redacted-dropped` | reasoning with no text |
| `media-dropped` | assistant media, or media given only by path |
| `tool-result-media-dropped` | media parts in a tool output |
| `tool-input-text-wrapped`, `tool-input-wrapped` | raw / non-object tool input becomes `args: {"input": ...}` |
| `tool-result-synthesized` | a call with no result gets Gemini CLI's interrupted-call response |
| `tool-result-unpaired` | a result whose call is not in the ledger becomes a `functionResponse` user part |
| `system-text-as-info` | system/developer text becomes an `info` record (not shown to the model on resume) |
| `system-text-dropped` | checkpoint: system/developer text |
| `compaction-as-user-text` | a compaction summary becomes a user message, as Gemini CLI's own `/compress` history does |
| `compaction-boundary-dropped` | compaction boundary |
| `peer-message-fields-dropped` | peer messages are written as SPEC §3.1.6 wrapped text (user when incoming, assistant when outgoing), which the importer turns back into a `peer_message`; this code names the fields beyond direction, peer, network and text that the wrapper cannot hold |
| `usage-cumulative-dropped`, `usage-unattached`, `usage-fields-dropped` | totals, usage with no message, `cache_write`/`cost_usd` |
| `settings-dropped` | settings other than `model` and `cwd` |
| `turn-dropped` | turn boundaries |
| `error-fields-dropped` | error `kind`/`retryable`/`status` |
| `file-change-dropped` | file changes (only a tool's `resultDisplay` holds them) |
| `subagent-event-dropped` | a subagent event with no matching tool call |
| `app-dropped`, `foreign-native-dropped` | `app` and other formats' `native` events |
| `agent-meta-dropped` | agent `meta` / `links` without carry |
| `exact-text-fallback` (session-json, checkpoint) | an unchanged document (every record its own) could not be rebuilt from its records' own texts (a piece has no text, or the rebuilt text does not parse to the document's value) and is written from the parsed value, so its layout, number spelling or escapes may differ from the source. No fixture reaches it; a unit test does (`an_unchanged_document_not_rebuilt_from_its_texts_names_the_fallback`) |
| checkpoint: `<kind>-dropped` | everything a Content history cannot hold (usage, settings, title, error, turn, ...) |
| import: `rewind-not-applied` | `$rewindTo` is kept as a native event; the rewound messages stay in the ledger |
| import: `part-kept-native` | a Content part with no neutral meaning (`executableCode`, ...) |
| import: `unparseable-line-kept` | a line that is not JSON (Gemini CLI skips these) |


## Branches (SPEC §2.3)

session-json and checkpoints cannot express a branch (one message list). session-jsonl has Gemini CLI's own `{"$rewindTo": id}`, which drops that message and everything after it on load; the importer keeps `$rewindTo` as a native event and does not apply it (`rewind-not-applied`), so writing one would not bring the branch back either. Decision: **not expressible** — the exporter renders the active
path, names `abandoned-branch-not-rendered`, and carries the abandoned events on
the next record. Done for `session-jsonl` and `session-json` (the `checkpoint` writer still writes every event). The comparator now reads `after` (`<kind>.branch` costs what dropping the event costs, `<kind>.after` a wrong predecessor), so writing the abandoned branch as the conversation is no longer free: `protoledger/branch-rewind` via gemini-cli was 0 with the branch written inline (where the comparator could not see it) and is 13 with the active path rendered, its 4 abandoned events (13 facts) missing; written inline it would score 14 under the new comparator. With carry the trip still loses nothing (carried score 0).

## Carry

Session records tolerate extra fields: the loader keeps the parsed objects
and `convertSessionToClientHistory` builds API `Content` from named fields
only, so a `protoledger` key on a message, `$set` or metadata record is never
sent to the model (and survives being re-appended). The exporter attaches the
events each record represents; events no record shows (turns, settings,
boundaries, ...) ride with a neighbouring record, titles with the `$set
summary` line, and the agent header (id, source, parent, links, meta) with the
metadata line (JSONL) or the document head (JSON). A carry trip through
`session-jsonl` or `session-json` restores the source ledger exactly (unit
test `carry_comes_back_exact`).

**Checkpoints do not carry**: their `Content` objects are sent to the Gemini
API as they are, and an unknown field would be rejected.

## Verified

- Return trips X → protoledger → here → protoledger → X with carry, for
  every fixture of the other two secondary formats (gemini-cli, cursor,
  copilot-cli), restore the source events exactly and write X's files back
  byte for byte (the carried agent header includes `source`, and events
  ride with the record at their own position). The only exception is a
  cursor `state.vscdb` source, whose self trip is row-exact only.

- Self trips are byte-exact on all fixtures: four generated by Gemini CLI's
  own `ChatRecordingService` / `Logger` from `@google/gemini-cli-core` 0.15.0,
  0.38.0, 0.50.0 and 0.64.0 (`fixtures/gemini-cli/_generators/`), a slice of a
  real anonymised session from the gemini-cli repository, and synthetic cases
  from trajectory and agentsview.
- Exports of a foreign ledger load with gemini-cli-core's own loaders, and
  the history Gemini CLI would resume with is well formed (alternating turns,
  every `functionCall` answered): `session-jsonl` with 0.50.0 and 0.64.0
  (`loadConversationRecord` → `convertSessionToClientHistory`, then
  `ChatRecordingService.initialize` in resume mode), `session-json` with
  0.38.0 and 0.64.0, `checkpoint` with `Logger.loadCheckpoint` in 0.64.0, and
  the bare-array form (`--target-version 0.15.0`) in 0.15.0. Run
  `node fixtures/gemini-cli/_generators/verify-load.mjs <core dir> <file> --resume`.

## Unverified

- Resuming an exported session in an interactive `gemini` with a live model
  (no API key was used). The model name recorded is the source's, which
  Gemini CLI does not use to pick a model on resume.
- `$patch` records with `orderIds` / `removeIds` and `$rewindTo` interplay
  beyond what the generated fixtures exercise.
- Producer versions before 0.4.0 (no chat recording) and the 0.1.x checkpoint
  files with call ids missing (paired by name, untested on real data).
- gemini-cli-core 0.64.0's own `convertSessionToClientHistory` duplicates the
  tool-response turn for sessions it recorded itself (fixed upstream after
  0.64); that is upstream behaviour and does not affect exports, which do not
  record tool responses as separate user messages.

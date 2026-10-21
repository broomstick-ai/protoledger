# Cursor (`cursor`)

Cursor is closed source, and keeps agent conversations in four places. All
four are supported, with what is known about each. The schemas below come
from Cursor's own documentation of the CLI's output format
(cursor.com/docs/cli/reference/output-format), from real captures published
under permissive licenses (see the fixtures), and from open-source readers
that reverse-engineered the stores: kenn-io/agentsview (MIT),
specstoryai/getspecstory (Apache-2.0), marcus/sidecar (MIT),
S2thend/cursor-history (MIT), SimHacker/moollm (MIT) and letta-ai/trajectory
(Apache-2.0). No Cursor code was read.

## Variants

| variant | where | producer versions | Cursor reads it back? |
|---|---|---|---|
| `stream-json` | stdout of `cursor-agent --print --output-format stream-json` (saved by whoever ran it) | cursor-agent 2025.08 - 2026.09 | no: output only |
| `agent-transcript` | `~/.cursor/projects/<path slug>/agent-transcripts/<id>.jsonl`, `<id>/<id>.jsonl`, `<id>/subagents/<child>.jsonl` | cursor-agent 2025 - 2026.09 | no: a projection (no tool results, no ids) |
| `agent-store` | `~/.cursor/chats/<md5(cwd)>/<agentId>/store.db` (+ `-wal`, `-shm`) and `meta.json` | cursor-agent chat store | **yes**: `cursor-agent --resume <agentId>` (run from that cwd) |
| `ide-state-vscdb` | `<userdata>/User/globalStorage/state.vscdb` (macOS `~/Library/Application Support/Cursor`, Linux `~/.config/Cursor`, Windows `%APPDATA%\Cursor`) | composer `_v` 1 - 17 | unverified: the IDE reads these rows, but nothing confirms it accepts rows written by another program |

None of the four records the producer version. `cursor-agent --version`
prints it (`2026.09.18-9a7762b`); captures say it in comments, if at all. The
composer's `_v` is the only version-like field (absent / < 3: inline
`conversation[]`; >= 3: separate `bubbleId:` rows; 16 and 17 seen in 2026).

### Shape changes

- `stream-json`, ~2026.08: `tool_call` objects gained siblings next to the
  `<name>ToolCall` key: `hookAdditionalContexts`, `toolCallId`, and
  `startedAtMs` / `completedAtMs` as decimal **strings**. Assistant lines in
  2026 captures carry `timestamp_ms` and `model_call_id` even without
  `--stream-partial-output`.
- `agent-store`, ~2026.09: the root blob gained field 8 (a protobuf turn
  index) and fields 9, 22, 26. When a root has no field-1 message list the
  importer reads field 8 with agentsview's provisional field numbers (text
  only) and says so (`turn-index-provisional`).
- IDE 3.12+ lists composers through `composerHeaders` and `glass.*` keys;
  the transcript itself is still `composerData` + `bubbleId` rows.

## Mapping

### `stream-json`

| line | kind |
|---|---|
| `system/init` | `settings` (`model`, `cwd`, `permission_mode`); `session_id` → agent id |
| `user` | `text` (user); starts a turn |
| run of `assistant` lines | one `text` (assistant): partial-output deltas are joined and the buffered / final flushes that repeat them are dropped; `group` = `model_call_id` |
| run of `thinking` lines (`delta`... `completed`) | one `reasoning` |
| `tool_call/started` | `tool_call`: `name` = the `<name>ToolCall` key without `ToolCall` (`read`, `shell`, `edit`, `grep`, `glob`, `task`, ...); `mcpToolCall` → `name` = `toolName`, `tool_type: mcp`, `server`; `function` → its `name`, `input` = parsed `arguments`. `taskToolCall` also → `subagent` start |
| `tool_call/completed` (`failed`) | `tool_result`: `success.content` / `stdout`+`stderr` (non-zero `exitCode` → `is_error`) / MCP `content[]` (text and image parts) / `markdown` / `message`; `error`, `failure`, `permissionDenied`, `rejected` → `is_error`. `editToolCall` with `diffString` and `writeToolCall` → `file_change`; `taskToolCall` → `subagent` end with the steps' text |
| `result` | `usage` (`inputTokens`, `outputTokens`, `cacheReadTokens`, `cacheWriteTokens`, `totalCost`/`total_cost_usd`), `error` if `is_error`, `turn` end (`duration_ms`) |
| `error` | `error` |
| `#` comments, other lines | `native`, kept verbatim |

A run of lines is one native record (`native.raw` holds the lines). Event ids
are `@<byte offset>` of the first line.

### `agent-transcript`

| line / block | kind |
|---|---|
| `{"role":"user","message":{"content":...}}` text | `text` (user); `<timestamp>Tuesday, Sep 29, 2026, 10:22 AM (UTC+9)</timestamp>` sets `ts` (minute precision) |
| `assistant` `text`, `thinking`, `tool_use` | `text`, `reasoning`, `tool_call` (an id is minted as `@<offset>#<n>` when the line has none, which real projections never do) |
| `tool_result` blocks (SWE-chat flavour) | `tool_result` |
| `message.model` | `settings` |
| `{"type":"turn_ended","status":..}` | `turn` end |
| `{"type":"error","error":..}` | `error` |
| `<id>/subagents/<child>.jsonl` | its own ledger with `parent` (no event: the projection does not say which call spawned it) |

### `agent-store`

`meta` row `"0"` (hex JSON) → agent id, `created_at`, `title` (`name`) and
`settings` (`lastUsedModel`, `meta.json` `cwd`). The root blob's field-1 refs
are the conversation, in order; each is an AI SDK message:

| message part | kind |
|---|---|
| `user` text (string or parts) | `text` (user), wrapped `<user_info>`/`<user_query>` kept as is (that is what the model saw) |
| `system` | `text` (system) |
| `assistant` `reasoning` (`signature`) / `redacted-reasoning` | `reasoning`, `sealed {vendor: cursor}` |
| `assistant` `text` | `text` (assistant), `group` = blob id |
| `assistant` `tool-call` | `tool_call` (`toolCallId`, `toolName`, `args`) |
| `tool` `tool-result` | `tool_result` (`result` string or text/image parts, `isError`) |
| `image` / `file` parts | `media` |

Ids are blob ids (sha256). Per-message timestamps are not stored.

### `ide-state-vscdb`

A `state.vscdb` is the whole IDE's state store, not a thread file. The
importer imports its threads (the `composerData` and `bubbleId` rows of
`cursorDiskKV`, and every other `cursorDiskKV` row whose key names a composer)
and nothing else. Everything else in the file is counted as skipped, by name,
and is neither carried nor written back:

- `cursorDiskKV` rows tied to no composer (an orphan bubble, `agentKv:` data,
  a row whose key is not UTF-8 text, such as a BLOB or NULL key, ...):
  `unlinked-row-skipped`, one per row;
- every row of every other table (`ItemTable`, the IDE's UI and settings
  state, and any table besides it): `ide-state-skipped`, one per row. SQLite's
  own `sqlite_*` tables are bookkeeping, not IDE state, and are not counted.

A row belongs to a composer when the second `:`-separated field of its key is
exactly the composer's id (`bubbleId:<composer>:<bubble>`,
`checkpointId:<composer>:...`); a composer whose id itself holds a `:` is
therefore given no rows (they are `unlinked-row-skipped`, and its listed
bubbles `missing-bubble`). Unit test
`ide_state_outside_the_threads_is_named_as_skipped`.

One ledger per `composerData:<id>` row: `createdAt` → `created_at`, `name` →
`title`, `modelConfig.modelName` / `workspaceIdentifier.uri.fsPath` →
`settings`. Bubbles come from `fullConversationHeadersOnly` →
`bubbleId:<composer>:<bubble>` rows, or from the inline `conversation[]`
(`_v` < 3):

| bubble | kind |
|---|---|
| `type` 1 `text` | `text` (user); starts a turn |
| `type` 2 `thinking.text` (`signature`) | `reasoning` |
| `type` 2 `text` | `text` (assistant), `group` = bubble id |
| `toolFormerData` | `tool_call` (`toolCallId`, `name`, `rawArgs` / `params` parsed) and `tool_result` (`result` string or JSON; `status` error / cancelled → `is_error`) |
| `tokenCount` (non-zero) | `usage` |
| `modelInfo.modelName` | `settings` when it changes |
| `errorDetails` | `error` |

A header's bubble is the row `bubbleId:<composer>:<bubbleId>`; when there is
none, the composer's row whose own `bubbleId` field is that id (cursor-history
reads it the same way; its v0.16 fixture lists a bubble under an id that is not
its key, and one under an empty id). Every other row whose key names the
composer (`<kind>:<composerId>:...`: a bubble no header lists, a NULL bubble,
`checkpointId:`, `messageRequestContext:`, ...) is kept in the composer's ledger
as a `native` event, after the conversation.

Each row is one native record, `{"key", "storage", value}`: `storage` is the
SQLite storage class (`blob` or `text`, both seen in fixtures, or `null`, `integer`,
`real`), and the value is `value` (a JSON object or array, embedded as its own
text, with `raw` beside it when a parsed value would re-spell it), `text`
(any other UTF-8 value) or `base64` (a value that is not UTF-8, BLOB or
TEXT alike: exported with its storage class and exactly these bytes). The composer
row's text is `source.extra.composer`, and its storage class
`source.extra.composer_storage` when it is not `text`. A row's event id is its
bubble id (for any other row, or an empty bubble id, its key). A NULL `composerData` value (Cursor
3.16 has been seen to null rows) is skipped and named.

## Export

| variant | foreign ledgers are written as |
|---|---|
| `stream-json` | an `init` line (from the first `settings`; a thread with no events is that line alone, so the file is still recognisably a session), `user` / `assistant` lines, `thinking` delta + completed per reasoning, `tool_call` started / completed pairs (`<name>ToolCall` when the name is one of Cursor's tool keys, `mcpToolCall` for `tool_type: mcp`, else the documented generic `function {name, arguments}`), and a `result` line per turn end (usage, duration) or error. File `<session id>.jsonl`. |
| `agent-transcript` | role/message lines with `text`, `thinking`, `tool_use` (with `id`) blocks, `tool_result` blocks as user lines, `turn_ended` and `error` lines. `<id>.jsonl`, or `<id>/<id>.jsonl` + `<id>/subagents/<child>.jsonl` when the set has subagents. |
| `agent-store` | `<agentId>/store.db` + `<agentId>/meta.json` in the layout specstory's writer uses and reports `cursor-agent --resume` loads: compact AI SDK message blobs (user; assistant `id: "1"` with reasoning, text, `tool-call` + `providerOptions.cursor.rawToolCallArgs`; one `role: tool` message per result), the title / empty / preview metadata subtree, a root of field-1 refs + field 2, and `meta` `"0"` = hex JSON (`agentId`, `latestRootBlobId`, `name`, `mode`, `createdAt`, `lastUsedModel`). Ids are sha256 of the data, so the export is deterministic. |
| `ide-state-vscdb` | a fresh `state.vscdb` (`ItemTable` and `cursorDiskKV`, as Cursor creates them) holding one `composerData` row per ledger (`_v: 3`, headers, name, times, model, workspace) and one `bubbleId` row per user message, assistant message, thinking block or tool call (with its result in `toolFormerData`), as TEXT. Events appended to a Cursor composer become bubbles listed after the composer's own headers. |

A ledger imported from Cursor is written back exactly. `stream-json` and
`agent-transcript` keep every line (`native.record`, or `native.raw` for a
run or an inexact line). `agent-store` keeps the chat directory's files
(base64 in `agent.source.extra.files`; a store holds one conversation) and
writes them back while nothing was appended; otherwise it rebuilds the store
from the message blobs (`store-rebuilt`). `ide-state-vscdb` writes back every row
of each composer (its key, storage class and bytes), but in a new database:
the rest of a user's `state.vscdb` (the IDE's own state; see Mapping,
`ide-state-vscdb`) was never copied into a ledger, and the import names it
(`unlinked-row-skipped`, `ide-state-skipped`). Its self trip is therefore row-identical, not
byte-identical per file (`expect.json` says so; unit test
`ide_fixtures_self_trip_row_identical`).

### Loss codes

| code | variants | when |
|---|---|---|
| `reasoning-sealed-dropped` | all | signatures / encrypted reasoning not from Cursor (stream-json and transcripts carry text only) |
| `system-text-dropped` | stream, transcript, store | no system message (a resumable store must not have one) |
| `media-dropped` | stream, transcript, store | images in messages (stream-json); media given only by path; assistant media (store) |
| `tool-result-media-dropped` | all | media parts in a tool output |
| `tool-input-text-wrapped`, `tool-input-wrapped` | all | a custom tool's raw `input_text` (SPEC §3.1.2) / a non-object input becomes `{"input": ...}` |
| `tool-result-unpaired` | stream, ide | a result whose call is not in the ledger |
| `compaction-as-user-text`, `compaction-boundary-dropped` | all | compaction summaries become user text; boundaries have no record |
| `peer-message-fields-dropped` | stream | peer messages are written as SPEC §3.1.6 wrapped text (user when incoming, assistant when outgoing), which the importer turns back into a `peer_message`; this code names the fields beyond direction, peer, network and text that the wrapper cannot hold |
| `settings-dropped` | stream, ide | settings other than model / cwd / permission mode |
| `usage-merged`, `usage-dropped` | stream, ide | a second usage in one turn; usage with no assistant bubble |
| `error-fields-dropped` | stream | error kind / retryable |
| `turn-dropped` | stream, transcript | turn starts (only turn ends have a line) |
| `<kind>-dropped` | all | anything else the variant has no record for (`file-change`, `subagent`, `app`, `native`, ...) |
| `store-rebuilt` | store | an imported store with appended events is rebuilt: orphan blobs, the old metadata subtree and newer root fields are not kept |
| `binary-message-dropped` | store | protobuf turn blobs (field-8 stores) are not rewritten |
| `inline-conversation-split` | ide | events appended to an inline (`_v` < 3) composer are stored as rows beside it |
| import: `turn-index-provisional`, `missing-blob` | store | field-8 store read with provisional field numbers; a referenced blob is missing |
| import: `null-value-skipped`, `unparseable-composer-skipped` | ide | a `composerData` row that is NULL / not JSON |
| import: `missing-bubble` | ide | a bubble the composer lists that has no row (by key or by `bubbleId`) |
| import: `unlinked-row-skipped` | ide | a `cursorDiskKV` row whose key names no imported composer (an orphan bubble, `agentKv:` data, ...) |
| import: `ide-state-skipped` | ide | a row of any table but `cursorDiskKV` (`ItemTable`, ...): the IDE's own state, not a thread |


## Branches (SPEC §2.3)

This adapter writes each of Cursor's four shapes as one sequence and reads it back in order (stream-json and transcripts by line, the agent store's blobs and the IDE's bubbles as written), so a branch is treated as not expressible. Decision: **not expressible** — the exporter renders the active
path, names `abandoned-branch-not-rendered`, and carries the abandoned events on
the next line. Done for `stream-json` (the `agent-transcript`, `agent-store` and `ide-state-vscdb` writers still write every event). The comparator now reads `after` (`<kind>.branch` costs what dropping the event costs, `<kind>.after` a wrong predecessor), so writing the abandoned branch as the conversation is no longer free: `protoledger/branch-rewind` via cursor was 5 with the branch written inline (where the comparator could not see it) and is 17 with the active path rendered, its 4 abandoned events (13 facts) missing; written inline it would score 19 under the new comparator. With carry the trip still loses nothing (carried score 0).

## Carry

- `stream-json` and `agent-transcript`: Cursor documents that stream-json
  consumers must ignore unknown fields, and the transcripts are read only by
  tools, so every rendered line carries the events it shows (`protoledger`);
  the first line also carries the agent header. Events with
  no line of their own ride with the next line. A carry trip comes back exact
  (unit test `carry_comes_back_exact`).
- `ide-state-vscdb`: bubbles carry their events and the composer row carries
  the title, settings and agent header. The IDE may rewrite a bubble and drop
  the field; the importer then reads it fresh.
- `agent-store`: **no carry.** Message blobs are what `cursor-agent` sends
  back to the model; an unknown field there is not known to be safe.

## Verified

- Return trips X → protoledger → here → protoledger → X with carry, for
  every fixture of the other two secondary formats (gemini-cli, cursor,
  copilot-cli), restore the source events exactly and write X's files back
  byte for byte (the carried agent header includes `source`, and events
  ride with the record at their own position). The only exception is a
  cursor `state.vscdb` source, whose self trip is row-exact only.

- Self trips: byte-exact for every `stream-json`, `agent-transcript` and
  `agent-store` fixture (real captures from cursor-agent 2026.03, 2026.09.09,
  2026.09.18 and 2026.09.26, plus synthetic cases); row-identical (key,
  storage class and bytes of every row) for the `ide-state-vscdb` fixtures (all
  synthetic): agentsview's rows are BLOBs, cursor-history's TEXT, one of them NULL.
- The `agent-store` export follows the layout specstory reports as loaded by
  `cursor-agent --resume`; it re-imports to the same events.

## Unverified

- That `cursor-agent --resume` loads an exported store: no Cursor account was
  used. In particular whether it accepts `tool-call` / `tool-result` / reasoning
  blobs (specstory only wrote text), and whether newer CLIs need field 8.
- That the IDE shows or continues an exported composer (it may also need
  `composerHeaders` / `glass.*` keys from 3.12 on).
- Field-8 turn index field numbers (one capture, via agentsview).
- `stream-json` `tool_call/failed` and `queued`, and `interaction_query`
  (only in hand-written fixtures; kept as native when unrecognised).
- The legacy `.txt` agent transcripts (`user:` / `assistant:` blocks) are not
  read.

## Prefix stability (SPEC §9.3)

`stream-json` is not prefix-stable. Cursor streams one item over several lines (its deltas, then `completed`), and the importer holds the item as one native record, the array of its lines, so that the self trip writes them back. The event appears at the item's first line, and its native record grows as later lines arrive, so a prefix ending inside an item differs from the whole import in that record. `tests/prefix.rs`: 264 of 430 (file, k) pairs differ, all in the four `stream-json` fixtures; the other 8 `.jsonl` fixture files are stable. The count is pinned there. The SPEC §4.1 way out (each later line a newer version of the record, carried by a later event) changes this adapter's native form and its exporter, and is not done here.

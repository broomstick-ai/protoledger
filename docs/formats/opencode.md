# opencode

[opencode](https://github.com/anomalyco/opencode) (formerly `sst/opencode`) is a
primary harness (weight 3). The adapter lives in `src/formats/opencode/`; it was
written against the opencode source at 2.0.31 (commit `180fd9d6`, 2026-10-16)
and the tags `v1.1.8`, `v1.2.0`, `v1.19.3` and `v2.0.0`.

One opencode **session** is one ledger. Its id is the session id (`ses_…`).

## Variants

opencode has stored sessions in five shapes. The importer detects them by
content; the exporter writes any of them (`--variant`).

| variant | producer versions | shape | detected by |
|---|---|---|---|
| `v1-storage-json` | ≤ 1.1.x (1.2+ reads it once, to migrate it into `opencode.db`) | `storage/session/<projectID>/<sessionID>.json`, `storage/message/<sessionID>/<messageID>.json`, `storage/part/<messageID>/<partID>.json`, plus `project/`, `session_diff/`, `todo/` and the `migration` counter. Files are `JSON.stringify(x, null, 2)` without a trailing newline. | a directory with `session/` and `message/` or `part/` (at its root, under `storage/` or `opencode/storage/`) |
| `v1-sqlite` | 1.2.0 (2026-02-14) – 1.x (1.19.3 is current) | `opencode.db`: tables `session` (columns), `message` and `part` (JSON in `data`, without the ids), `project`, `todo`; drizzle journal `__drizzle_migrations` | SQLite with `session`, `message`, `part` and no `session_v2` |
| `v1-export` | 1.x (`opencode export <id>`, read by `opencode import`) | one file `{info, messages:[{info, parts}]}`, `JSON.stringify(x, null, 2) + EOL` | JSON whose `messages[0]` has `info` and `parts` |
| `v2-sqlite` | ≥ 2.0.0 (2026-09-11) and the 2.0 betas | `opencode.db`: `session_v2` (columns) and `session_message` (`id, session_id, type, seq, time_created, time_updated, data`), `project`, `event_sequence`; journal table `migration` | SQLite with `session_v2` and `session_message` |
| `v2-transfer` | ≥ 2.0.0 (`opencode session export <id>`; `POST /api/experimental/session/import`, `opencode session import <file>`) | one file `SessionTransfer.Data {info, messages}` (`packages/schema/src/session-transfer.ts`), `JSON.stringify(x, null, 2) + EOL` | JSON whose `messages[0]` has `type` (or, empty, whose `info` has `location`/`tokens`) |

A directory of export (or transfer) files imports as one ledger set. The
producer version comes from the session's `version` (field or column).

**Default export.** A ledger set imported from opencode exports back to the
variant it came from. Anything else exports to **`v2-transfer`**, the shape the
supported import path takes: write the files, then
`opencode session import <file>` for each (parents before their children — the
endpoint refuses a `parentID` that does not exist yet; session ids are
descending, so a child's file name sorts *before* its parent's).

`v2-sqlite` and `v1-sqlite` exports are standalone databases (full schema,
journal rows marking every migration done), suitable as a fresh
`opencode.db` or for inspection. They are not merged into a live database:
opencode is not append-only (see below), so writing into its store behind its
back is not supported here.

### Storage locations

- Data directory: `$XDG_DATA_HOME/opencode` (default `~/.local/share/opencode`).
- 1.1.x: `<data>/storage/…` (the tree above).
- 1.2+: `<data>/opencode.db` (WAL mode; checkpoint or copy `-wal` too). Release
  channels other than latest/dev/beta/next/prod use `opencode-<channel>.db`;
  `OPENCODE_DB` overrides (`packages/cli/src/database-path.ts`).
- 2.0 upgrades a 1.x database in place: the 1.x tables stay, `session_v2` and
  `session_message` are added and filled by `packages/core/src/database/v1-migration.bun.ts`.
  Such a file imports as `v2-sqlite`; the 1.x rows are kept as native
  records (see Store rows below).

## Identifiers

`packages/schema/src/identifier.ts`: a prefix (`ses_`, `msg_`, `prt_`, `sh_`)
and 26 characters — 12 hex digits of the low 48 bits of
`timestamp_ms * 0x1000 + counter` (bit-negated for sessions, so newer sessions
sort first), then 14 characters of `[0-9A-Za-z]`. Exports mint ids with exactly
that shape; the time part comes from the event's timestamp (kept strictly
increasing, so 1.x's id order is the ledger's order) and the tail from a hash of
the ledger and event id, so re-exports mint the same ids (SPEC §6). Tool call
ids are kept as the source wrote them. A fork's copied messages have ids
`msg_<fork event>_<seq>` (`projector.ts`); the importer accepts any id.

Event ids: a message's first event takes the message id, a V1 part's first event
the part id, later events `<id>#<n>` (SPEC §2.2). The session's own events take
the session id. A record with no id (hand-made exports) gets a location anchor,
`<message id>/parts/<n>` or `@messages/<n>`. Records kept only for exactness
(`session_diff`, unknown storage files) are `native` events with anchor ids
`@<path>`; a database row outside the session model is one with
`@<table>/<primary key values joined by />` (see Store rows below).

## Mapping

### Session (both generations)

| opencode | event |
|---|---|
| session info / row | `settings` {`cwd` = directory, `agent`, `model`, `provider`, `effort` = model variant when not `default`}, carrying the session record; emitted even when it states nothing |
| `title` | `title` (opencode's placeholder `New session - <ISO time>` is not a title) |
| 2.x `cost`, `tokens` when they differ from the sum over messages | `usage` {`cumulative: true`}, placed after the messages |
| `parentID` | child ledger, `parent = {agent, event: the spawning tool call, relation: subagent}` |
| 2.x `fork {sessionID, boundary}` | link `forked-from` {agent, event = boundary message, `extra.boundary`} |

**2.x session settings are the current selection.** opencode rewrites a
session's `agent`, `model` and `location` on every switch, and each
`agent-switched` / `model-switched` / `location-switched` message records the
`previous` value (`packages/core/src/session/session.ts`). The opening
`settings` therefore takes each field from the first switch's `previous` when
the session switched (none when that switch has no `previous`: nothing was
selected before it), and from the session only when it never did. The
placeholders an export writes for what a ledger did not say — directory `/`,
provider `unknown` — read back as unknown.

### 2.x messages (`packages/schema/src/session-message.ts`)

| message type | events |
|---|---|
| `user` | `text` {role user} (and one per skill attachment with text), `media` {role user, `media_type`, `data`, `name`, `url` for a `uri` source} per file |
| `synthetic` | `text` {role user, `synthetic: true}`; a text that is exactly a SPEC §3.1.6 peer-message wrapper (also in a `user` message) reads back as `peer_message`, as does an earlier export's peer message (description `peer message (<dir>) <peer> [<network>]`) |
| `system` | `text` {role system} |
| `skill` | `text` {role user, `skill`} |
| `shell` | `tool_call` {name `shell`, `tool_type: shell`, `user: true`} + `tool_result` (`is_error` when exit ≠ 0) |
| `agent-switched` / `model-switched` / `location-switched` | `settings` {`agent`} / {`model`, `provider`, `effort`} / {`cwd`} |
| `assistant` | per content item: `text` {role assistant}, `reasoning`, `tool` → `tool_call` + `tool_result` once settled (`is_error` for `error`; a child session in `state.metadata.sessionID` adds a `subagent` start event, its type from `input.agent` or the 1.x `task` tool's `input.subagent_type`); then `file_change` per `snapshot.files`, `usage` (tokens, `cost_usd`, model), `error` — see the conventions below |
| `compaction` | completed: `compaction` {phase summary, trigger, summary}; running/failed: `compaction` {phase boundary} (+ `error`) |
| `idle` | `turn` {phase end, outcome ok / error / interrupted} |

Turns: a turn runs from the first message after an `idle` marker to the next
marker; in stores without markers (the betas), a user prompt after a finished
assistant step starts one. `group` is the message id for assistant steps and for
messages that make several events.

### Neutral conventions (SPEC §3.1)

| convention | import (2.x) | export (2.x) |
|---|---|---|
| usage counts | 2.x `tokens.output` is visible output only (`visibleOutputTokens`, `packages/core/src/session/usage.ts`), so neutral `output = output + reasoning`; `input` already excludes cache reads and writes | `tokens.output = output − reasoning`; session totals likewise |
| one usage per response | the step's `tokens`/`cost`, `model` = the step's model, dated and placed at `time.streamed` (when the provider response ended): after the response's own events, before the tool results that came back later (SPEC §3.1.1); at `time.completed` when the step has no `streamed` | a usage reports the step of its `group`, else the open step; a usage that comes before its response's events opens that response's step; a second identical usage for one step is written once (`usage-duplicate`), a different one is added in (`usage-merged`); the usage's model names the step's model and its time is the step's `time.streamed` |
| one step per response | each `assistant` message is one response | the events of one `group`, or ungrouped events until the step's usage, a tool result or an error arrives, make one step |
| settings on change | the opening snapshot (above), then one `settings` per switch message | changes after the conversation began are switch messages: `model-switched` with `previous` (an effort is the model's `variant`), `agent-switched`, `location-switched` (absolute paths, POSIX or Windows); a provider stated before any model stays in force for it; the session holds the current selection, and its model is the one the running totals were billed to |
| reasoning | `text`; an item with OpenAI state (`itemId` / `reasoningEncryptedContent`) is a summary part (`packages/ai/src/protocols/open-responses.ts` streams one item per summary part): `summary: [text]`, consecutive parts of one `itemId` merge. Seals (SPEC §3.1.5 names the vendor by the seal, not the relaying provider): `signature` → anthropic signature, `redactedData` → anthropic encrypted, `reasoningEncryptedContent` → openai encrypted, `thoughtSignature` → google signature | `text`; with no text, the summaries joined, with OpenAI state (`reasoningEncryptedContent`, `null` without a seal); Anthropic redacted thinking → `redactedData`; a Google signature → `thoughtSignature`; a seal's vendor names a step whose provider is unknown; a reasoning item ends (`time.completed`) where the text after it begins |
| text time | a text item has no time of its own: it is dated by the end of the reasoning before it, else by the step | — |
| tool input | a lone `input` string key is a free-text (`custom`) tool's `input_text` (the field OpenAI's `custom_tool_call` uses); a lone non-string `input` key is a non-object input; else `function` | `input_text` → `{"input": text}`; arguments recorded as JSON text are written parsed (`tool-input-parsed`); another string → `{"value": …}` (`tool-input-wrapped`) |
| tool results | after their call, moved past later parallel calls that started before they came back (`time.completed`); a `tool.execution` error whose message is the output is not repeated as an `error` object; the export's unsettled-call error (`No result was recorded for this tool call`) reads back as a call without a result | — |
| errors | the step's `error` | the open step, or the last message if it is a step; otherwise a step with no content and the error, which is what opencode records when a request fails (2.x; 1.19's import rewrites an assistant message with no parts, so 1.x keeps such an error in the carrier: `error-dropped`) |
| record-only text | — | not written in 2.x, where every message reaches the model (`record-text-dropped`); 1.x writes an `ignored` text part |
| peer messages | a text that is exactly the wrapper → `peer_message` (1.x and 2.x) | a `synthetic` message (1.x: a synthetic text part) whose text is `<peer-message direction="…" peer="…" network="…">\nTEXT\n</peer-message>` |

### 1.x messages and parts (`packages/schema/src/v1/session.ts`)

| record | events |
|---|---|
| user message info | `settings` {model, provider, agent, `system_prompt`, `tools`} when they change, else `native` |
| assistant message info | after its parts: `usage` (tokens, `cost_usd`, model), `error` (`MessageAbortedError`, `APIError`, …) |
| `text` part | `text` (`synthetic: true` kept; `ignored` → audience record); in a `summary: true` message → `compaction` {phase summary} |
| `reasoning` | `reasoning` (`metadata.<provider>.signature` / `.reasoningEncryptedContent` → `sealed`) |
| `file` | `media` (`data:` URLs → `data`, others → `url`/`path`) |
| `tool` | `tool_call` + `tool_result` (completed/error; attachments become media output parts); `metadata.sessionId` (the `task` tool) → `subagent` start |
| `compaction` | `compaction` {phase boundary, trigger auto/manual} |
| `retry` | `error` {kind, message, `retryable`, status} |
| `snapshot` / `patch` | `file_change` {op snapshot} / one `file_change` {op edit} per file |
| `step-start`, `step-finish`, `agent`, `subtask`, unknown | `native` |

### Export of foreign ledgers

Events that did not come from opencode (or came from the other generation) are
rendered from their neutral fields:

- 2.x: user text → `user` (consecutive user text of one group merges; media
  with base64 data → `files`), system/developer text → `system`, synthetic text →
  `synthetic`, record-only text → nothing (`record-text-dropped`), assistant
  text/reasoning/tool calls of one response → one `assistant` step (`finish`
  `tool-calls` or `stop`; `time.streamed` = its usage, `time.completed` = its
  last event), tool results settle the matching tool anywhere earlier
  (`completed` with a non-empty `content`, or `error` with `{type:
  tool.execution}`), usage → step `tokens`/`cost` (a cumulative usage → session
  totals), settings changes after the first message → `model-switched` /
  `agent-switched` / `location-switched`, compaction summary → completed
  `compaction`, turn end → `idle`, peer message → `synthetic` with a peer
  description, error → the step's `error` (or a step of its own). A tool call
  with no result is written as an interrupted error, so every message is
  settled. The session info gets `projectID: global`, the current agent and
  model, `location.directory` from the last absolute `cwd` (or `/`), cost and
  token totals, the title, `parentID` for a subagent whose parent is in the
  set, and `fork` for a `forked-from` link. The conventions table above says
  how each neutral field is spelled.
- 1.x: the same, as user messages with parts and assistant messages with
  `parentID` (the last user message), `path`, `mode`/`agent`, `tokens`, `cost`;
  compaction → a user message with a `compaction` part answered by a
  `summary: true` message; system text → a synthetic text part; a subagent's tool
  gets `metadata.sessionId`.
- A 1.x record exported to 2.x (or the reverse) is rendered this way too.
  Moving records between containers of one generation keeps them (converting
  rows to files and back); a 2.x record leaving a database for a transfer file is
  brought into the import endpoint's shape (unsettled messages dropped,
  running tools → interrupted errors, beta tools with empty `content` get their
  `structured.content`).

## Carry

- **2.x:** each rendered message carries the events it represents in
  `metadata.protoledger` (`{v, h, events}`; `metadata` is
  `Record<string, unknown>` in every message type). The import endpoint and
  the projector store `metadata` as given, and `to-llm-message.ts` does not send
  it to the model. The hash covers the message without its carrier, with
  numbers in `JSON.stringify` form, so opencode's decode/encode on import does
  not break it. The agent header rides on the session: `info.metadata.protoledger
  = {v, session, agent, title, events?}`; it is honoured only when `session`
  equals the session id, because opencode copies session metadata into child and
  forked sessions. A title edited in opencode after the export is imported as a
  new `title` event.
- **1.x:** the carrier sits in `metadata.protoledger` of the message's first
  `text`, `reasoning` or `tool` part (a field opencode declares, so 1.19's schema
  decode on `opencode import` keeps it), or, for a message with no such part, as
  a top-level `protoledger` field on the message info (1.1 and 1.2 store records
  as given; 1.19 strips it). It is hashed over the info and all its parts. The
  header is on the session info (`metadata.protoledger`) and on the first
  rendered message's carrier (`header`), because a 1.2 `session` row has no
  column for it.
- opencode's own 1.x → 2.x database migration (`v1-migration.bun.ts`) moves a
  part's `metadata` into the 2.x content `state`, so the carrier's bytes survive
  it, but its hash was taken over the 1.x shape: messages migrated by opencode
  are imported fresh (neutral mapping), not restored.
- Events with no message of their own (a lone `file_change`, `app`, …) ride on
  the next rendered message, or the last one, or the session carrier. A native
  record that must carry such events carries its own events too and is written
  from its logical value.
- On import with carry restore, a message whose carrier still matches is
  replaced by its carried events; an edited message is imported fresh.

## Store rows (everything else in `opencode.db`)

An `opencode.db` holds more than sessions: an event log (`event`), project
directories and worktrees, instruction blobs and states, todos, accounts, kv,
permissions, workspaces, inbox, and in a database 2.0 upgraded in place the
leftover 1.x tables. Every row of every table is a native record (SPEC §4),
so a store re-exports row-identical, with the file's header values
(`PRAGMA application_id`, `user_version`, and `encoding` when not UTF-8)
kept beside the schema in `source.extra.sqlite_header` (a ledger imported
before this release carries none and exports with SQLite's defaults):

- The session model's rows are what they always were: the session row, its
  messages and parts (events), its project row and its `event_sequence`
  watermark (bookkeeping on the header, `source.extra.rows`), and the
  migration journals (`source.extra.bookkeeping`).
- Any other row is one `native` event with id `@<table>/<primary key>`, the
  row as `record` and `file` `opencode.db:<table>`. A row that names a session
  (a `session_id` column, or an event log's `aggregate_id`) goes in that
  session's ledger, after the session's own events and before its messages.
  Every other row goes in the store ledger.
- **The store ledger** holds an `opencode.db`'s rows that belong to no
  session, and only those, as native records: every project row (a project
  is shared by its sessions, and opencode writes it before the first one; a
  session's header keeps a copy of its own, so a single session still exports
  with it), project directories, worktrees, instruction blobs, a watermark
  whose aggregate is not a session, rows naming a session the database no
  longer holds, and every row of a database that holds no session at all.
  Its agent id is `opencode:store:<project id>:<time_created>` of the project
  row opencode wrote first (the smallest `time_created`, then the smallest
  id), or `opencode:store` for a database with no project row. opencode
  writes a project row the first time it opens a directory, before any
  session, and neither deletes it nor rewrites its `time_created`, so the id
  stays the same when sessions are added or removed, when the rows are
  written in another order, and when the file is vacuumed; two databases
  whose first projects differ get two store ledgers.
- The store ledger has no parent; every session ledger of the database has a
  `member-of` link to it (as a Slack channel has to `slack:workspace`).
  Attaching the database binds one of its sessions, not the store ledger,
  unless the database holds no session.
- It never moves. Adding or removing a session changes which session ledgers
  exist, never which ledger holds a row that names no session, so a sync of
  an attached database after a new session is an append, not a divergence. A
  re-import or a sync appends only the rows (by row id) the ledger does not
  hold yet. Export writes every row once: a row the store holds twice (a
  project row on a session's header and in the store ledger) is written once,
  by primary key. A row that still names a session opencode has deleted
  moves to the store ledger on the next re-import, so the store then holds
  it in both the session's ledger and the store ledger; export writes it
  once, by primary key, so nothing fails and nothing is lost. The store
  ledger is the only place protoledger keeps these rows: garbage-collecting
  it is the only way they leave the store.
- Store rows are kept for exactness only: they go back to an `opencode.db` of
  the container they came from, and are `native-dropped` anywhere else (with
  or without carry), where the store ledger has nothing to write. The store
  ledger is left out of an export only when all four hold: it holds only
  native or app records; another ledger names it `member-of` (every session
  ledger does, so a database with no sessions does not qualify); the export
  is without carry; and the target is neither `opencode` nor `protoledger`.
  It is then counted as `container-not-rendered` (SPEC §2.3) instead of being
  written as an empty session. With carry, or exported to `opencode` or
  `protoledger`, it is written as before. A session
  rendered from a ledger does not regenerate them. A database written from
  rendered sessions holds the project rows the writer made for them, so
  importing it back gives a store ledger with those rows.
- Sync appends a new row (an event-log entry) as a new fact. Which records
  version when opencode changes them (SPEC §4.1): every row that is not a
  session, message or part (a store row, a todo row) versions when it is
  rewritten or deleted, as the next two bullets say; a message or part
  versions only as Sync below says, and one gone from a session, or
  rewritten so that events read earlier are gone, is divergence (§9.3); a
  session row versions only on a new session fact.
- Rewriting a store row (a kv, permission, project or worktree row) or a todo
  row in place in an attached `opencode.db` (a todo going `pending` →
  `completed`, which opencode does by writing the session's list again) is a
  newer version of that row: the next sync compares the row with the newest
  version the ledger holds and, when it differs, appends one `native` event
  carrying the row as it now stands, to the ledger that holds it, and keeps
  following the binding. A row written again unchanged appends nothing.
  Exports use the newest version, so they write the row as it now stands.
  The comparison is with the ledger, not the binding's cursor, so a row
  rewritten while the cursor was lost is still appended.
- Deleting a store row (a worktree, a project directory, a kv, permission or
  account row) or a todo row in an attached `opencode.db` is a newer version
  of that row (SPEC §4.1): the next sync appends one tombstone for it (a
  `native` event naming the row, body `{"deleted": true}`) to the ledger
  that holds it, and keeps following the binding. Exports use the newest
  version of each row, so they leave a deleted row out; a row opencode writes
  again under the same key comes back as one more version, with its new
  value. This holds for every row that is not a session, message or part; a
  message or part gone from a session is still divergence (§9.3).
  A row deleted while the binding cannot see it is not tombstoned: one
  deleted between importing the database and attaching it, or after the
  binding's cursor is lost (the index rebuilt). A later export still writes
  it; re-import the database to drop it.

## Mutation caveat (opencode is not append-only)

opencode updates rows in place (a running assistant message is rewritten as it
streams, titles change, `time_updated` moves), `revert` deletes messages after
a point, compaction rewrites history the model sees, and a fork copies messages
under new ids. For re-import identity this means:

- A record's **id** is stable, so re-importing a session after opencode changed
  it yields the same event ids for unchanged records and for changed ones. A
  record that changed (more parts, a settled tool, an edited title) yields events
  with the same ids but new content; a ledger is append-only, so a consumer that
  already holds those ids must import into a new ledger (or diff) rather than
  append — which is what sync does for a bound location (see Sync below). Use `restore_carried` + carrier hashes to tell edited from unedited
  records: an edited record's carrier no longer matches and it is imported fresh.
- A **reverted** (deleted) message simply is not there on re-import: the ledger
  imported earlier keeps it. opencode's `session.revert` field records where a
  revert stands; it is kept in the session record. A **staged** revert (the
  messages still present) is imported as an `after` branch (SPEC §2.3): one
  more part of the session record, after the messages, continuing from the
  event before the reverted message (1.x `partID`: before that part), so the
  reverted messages stay in the ledger off the active path.
- **Forks** copy the parent's settled history; the copies are separate events in
  the fork's ledger (new ids), and the fork links back with `forked-from`. The
  2.0.31 import endpoint does not import `fork` (nor `location`, which comes from
  the importing directory); a session export of an imported fork has no fork.
- A **live** database is read as it is at that moment: running messages are
  imported as they stand (unsettled tools give a `tool_call` with no result).
  Sync (below) does not take a live store's transient states.

## Sync (following a live store)

A bound opencode location (SPEC §9.2: a `v1-storage-json` tree, a `v1-sqlite`
or a `v2-sqlite` `opencode.db`) is followed by re-importing it whole on every
change and handing the result to sync's opencode hook (`src/sync/inplace.rs`,
with the opencode half in `src/formats/opencode/live.rs`) instead of the
generic engine, which compares event ids only. It follows SPEC §4.1:

- **Each fact the moment it is settled, never a transient state.** A tool's
  `tool_call` (and its `subagent` start) is appended when the tool reaches
  `running` (its input is final); before that (1.x `pending`, 2.x `streaming`,
  the betas' `pending`) nothing is. Its `tool_result` is appended at
  `completed` or `error`. A 1.x text or reasoning part is appended when its
  `time.end` is set (a part with no `time`, such as a user prompt's, is settled
  as written); a 2.x text item, which has no time of its own, when something
  follows it in the step or the step's `time.streamed`/`time.completed` is set;
  2.x reasoning when the step streamed or the item completed and something
  follows it. A step's usage is appended when the step completed (1.x
  `time.completed`) or the response ended (2.x `time.streamed`); its error,
  file changes and an empty step's placeholder when it completed (1.x
  assistant info, 2.x `time.completed`). A running 2.x compaction is
  appended when it completed or failed.
- **Facts, not ids.** A 2.x message's events are numbered by position
  (`<message id>#<n>`), and positions move while the step grows (a result is
  placed after its call, past later parallel calls). So the hook identifies a
  fact by its record and what it states — a tool call or result by its call id,
  a title or settings by their values, anything else by its kind and ordinal
  within its record — and appends each settled fact the ledger does not hold.
  An appended event whose id the ledger already holds for another fact gets
  the next free `<record id>#<n>`. Each fact is appended once.
- **Newer native record versions.** An appended event whose record (a part file
  or row, a message row) differs from the newest version the ledger carries
  carries the new version: the same `native.id`, `part: 0`, and the record. A
  settled message or part rewritten in place with no new fact (a completed
  tool's `metadata` updated afterwards) gets one `native` event carrying its
  newer version. An unchanged record yields nothing. The session record and the
  bookkeeping on a session's header (its copy of the project row, its
  `event_sequence` watermark) are never versioned on their own: opencode
  rewrites the session on every message (`time.updated`), and its newer
  versions ride only on a new session fact (a title, a changed setting).
  Todo rows and store rows (project rows, the other store rows) version
  whenever opencode rewrites or deletes them (Store rows above).
- **Export uses the newest version** of each native record: a later version of
  a message or part replaces the earlier one where it stands, so a ledger
  synced through a tool's life exports the tool once, completed.

`Format::follow` (read only what was appended since the last sync) is not
implemented for opencode: none of its stores grows by appending. A database
rewrites rows in place (and in WAL mode keeps recent writes in
`opencode.db-wal`), and a storage tree rewrites part and message files; there is
no tail to read on from, so every sync re-imports the location whole.

Known limits: a binding's location is the `opencode.db` file, and
sync's stamp, its file list and the watcher's match also cover the
`opencode.db-wal` beside it (`src/sync/mod.rs` `wal`; `tests/opencode_sync.rs`
`wal_uncheckpointed_write_synced_within_one_200ms_poll`), so a write still
sitting in the WAL (opencode runs SQLite in WAL mode) reaches a follower
without waiting for a checkpoint; a title that changes back to an earlier value is the same
fact and is not appended again; consecutive 2.x OpenAI reasoning summary
parts of one item merge into one event, which is appended once (later parts
of it reach the ledger only in the record's newer version).

## Loss codes

Import:

| code | meaning |
|---|---|
| `orphan-message`, `orphan-part` | storage files for a session/message that does not exist (database rows are kept, see Store rows) |
| `file-unparsed`, `file-unrecognised`, `file-unreadable`, `file-unowned` | files in an input directory that are not opencode records |
| `export-file-reformatted` | an export file not laid out as `JSON.stringify(x, null, 2)` (re-emitted in that layout) |
| `revert-not-located` | a `session.revert` naming a message (or 1.x part) the session does not hold: no branch is made; the revert stays in the session record |

Export:

| code | meaning |
|---|---|
| `media-dropped` | assistant media; 2.x attachments without base64 data |
| `reasoning-sealed-dropped`, `reasoning-redacted-dropped` | a seal opencode has no field for; redacted reasoning without text |
| `tool-call-unsettled` | a tool call without a result, written as an interrupted error |
| `abandoned-branch-not-rendered` | events on an abandoned branch (SPEC §2.3) other than messages a staged `session.revert` of the same generation abandons (those are written back with it); only the active path is rendered, and with carry they ride on the next message |
| `tool-result-orphan` | a result whose call is not in the ledger |
| `tool-input-wrapped` | a non-object tool input, written as `{"input": …}` (a string that is not free text as `{"value": …}`) |
| `tool-input-parsed` | tool arguments recorded as JSON text, written parsed |
| `usage-duplicate`, `usage-merged` | a second usage for one response: identical (written once) or different (added in) |
| `reasoning-summaries-joined`, `reasoning-summary-dropped` | several summaries written as one item; a summary beside reasoning text |
| `record-text-dropped` | 2.x: text the model never saw (opencode shows every message to the model) |
| `settings-field-dropped` | permission mode, sandbox, approval, git branch; 2.x system prompt and tool list; an effort with no model; a relative working directory (2.x) |
| `usage-dropped`, `usage-cumulative-dropped` | usage with no assistant step; running totals in 1.x |
| `compaction-boundary-dropped` | a boundary with no summary |
| `turn-dropped` | 1.x has no turn marker |
| `file-change-dropped` | file changes (no snapshot store to point at) |
| `error-dropped` | an error with no assistant step to hold it |
| `time-unrepresentable` | export: a time no opencode id can encode (`ms × 0x1000` past a `u64`, a damaged source); the id minted then follows the one before it, so later ids keep their order |
| `native-dropped` | `native`/`app` events, and opencode records that only live in another container (todo rows, session diffs) |
| `role-system-as-synthetic`, `role-developer-as-system` | role changes |
| `model-unknown` | no model was recorded (`unknown/unknown` is written) |
| `link-dropped`, `parent-outside-set` | links opencode has no field for; a subagent exported without its parent |
| `unsettled-message-dropped` | a running message left out of a session export |
| `column-dropped` | a field the target database's schema has no column for (a 1.19 session's agent, model and totals written into the 1.2.0 schema) |
| `sqlite-header-conflict` | ledgers from databases with different header values (`PRAGMA application_id`, `user_version`, `encoding`) written to one `opencode.db`, which keeps the values of the ledger whose id sorts first |

With carry, everything above comes back on re-import, except a store
ledger's rows (`native-dropped`), which only an `opencode.db` holds.

## Neutral loss through opencode

Measured by the conformance suite: for every non-opencode fixture, X →
protoledger → opencode (`v2-transfer`) → protoledger, without carry, compared
with the first import (`tests/reports/*/*.json`, `no_carry_there.opencode`),
summed over all fixtures. The comparator changed twice while this pass ran, so
each step is given on the scale it was measured on.

| comparator | before | after |
|---|---|---|
| 9d89529 (first scale) | 3567 | 2445 |
| 1c16276 (SPEC defaults, gains not scored, usage paired by group) | 3018 | 2214 |
| ddc8bda (a dropped event weighs 1 + its facts; natives out of alignment) | 5902 | 3223 |

By category on the current scale (occurrences, before → after): missing usage
252 → 50 and extra usage 171 → 21 (output/reasoning convention, one step per
response, usage at `time.streamed`); missing/extra tool calls 118/118 → 0/0 and
extra tool results 95 → 7 (step segmentation, parallel results, unsettled
calls); reasoning seals 86 → 64 (Anthropic, OpenAI and Google provider state;
what remains is sources naming vendors other than SPEC §3.1.5's) and summaries
49 → 23; text times 111 → 19; tool input text 70 → 0 (free-text tools); usage
model 56 → 0, input 49 → 0, output 50 → 10, time 65 → 6; tool type 100 → 74.
Usage `context_window` (17 → 63) and `provider` (38 → 71) grow because usages
that used to go unmatched now pair up and show the fields opencode has no place
for.

What remains is what opencode does not record:

| loss | why |
|---|---|
| file changes (92) | 2.x keeps a step's changed files only as paths in `snapshot.files`, next to a snapshot store this export does not have; listing them was measured and costs more than it keeps when a source records the change before the tool result |
| turn starts (90), turn `duration_ms`/`reason` | 2.x records only the end of a turn (`idle`, with an error for a failed one) |
| titles (74) | one title per session |
| developer messages (part of missing/extra text) | 2.x has `system` messages; a `developer` role reads back as `system` |
| record-only text | not shown to the model, so not written in 2.x (see below) |
| tool types (74) | opencode stores no tool kind: shell, MCP (and its `server`), web search read back as `function` |
| subagent events (56) | end markers and spawns that are not tool calls have no 2.x record; child ids are reminted as `ses_…` |
| settings: git branch, permission mode, approval, sandbox, system prompt, tool list | no 2.x session fields |
| `context_window`, `phase`, `namespace`, peer seals, harness `meta` | no opencode field |
| usage `provider` (71), user-message `group` (51) | facts the importer could state for opencode's own sessions, but every adapter that cannot carry them would then lose them on opencode fixtures (the ratchet); left out |
| a second usage per response (Codex reports two) | one usage per step |

**Reports that grow** (the ratchet; both deliberate):

- opencode 2.x fixtures via `atif` (+256 over 5 reports): a 2.x step's usage now
  sits at `time.streamed`, before tool results that came back later, as SPEC
  §3.1.1 places it; ATIF's exporter writes it after the results.
- record-only text (+22 over 6 reports: `droid/hooks-torn-line`,
  `droid/compaction-turns`, `claude-code/v1.0-summary-lines`, three `gemini-cli`
  cases, the last two partly from Gemini's usage-first order): text the model
  never saw (a billing notice,
  a rewound "Never mind, delete a.txt") is no longer written as a message the model
  would see in a continued session. The comparator then also mis-pairs the user
  texts after it (it aligns text by role only).

## Self trips

JSON variants are byte-exact: every record is kept in `native.record` and the
file layout (pretty, pretty + EOL, compact, or raw text for anything else) is
remembered per file. SQLite variants are not byte-exact — the fixtures say so
in `expect.json` — but re-export recreates the stored schema statements, the
journal rows and every row of every table identically
(`formats::opencode::tests::sqlite_fixtures_rows_identical` compares every
table of every database fixture and requires an import without losses;
`v1_sqlite_1_19_3_store_self_trips_row_identical` and
`v2_sqlite_2_0_31_store_self_trips_row_identical` name the two stores whose
event log, project directory, instruction and worktree rows used to be
dropped). The `data` column's exact text is kept in `native.raw` when JSON would
not reproduce it.

## Verified

Against real opencode binaries (npm `opencode-ai@1.1.8`, `@1.2.21`, `@1.19.3`,
`@opencode/cli@2.0.31`, darwin-arm64, each in an isolated `HOME`/`XDG_*`), on
2026-10-17:

- **Fixtures.** Nine fixture cases were produced by those four versions (storage
  tree, `opencode.db`, `opencode export`, `opencode session export`, including
  `--sanitize`), covering a plain reply, read/glob tools, a failing shell
  command, a subagent with its child session, a continued session, a file
  edit and a manual compaction. Each self-trips (JSON byte-exact, databases
  row-identical).
- **Exports accepted by opencode.** For each case below, the ledger was exported
  by this adapter, loaded into opencode, read back out with opencode's own
  export, and imported again with carry restore; the result was compared with the
  original ledger:
  - `v2-transfer` → `opencode session import` (2.0.31), parents first →
    `opencode session export`: clean for Claude Code fixtures (plain, tool calls,
    image results, compaction, sync and async subagents, a fork, queued peer
    messages, snapshots), the protoledger fixtures (minimal, subagent +
    compaction, orchestrated peers with links and meta) and opencode 1.x
    fixtures rendered for 2.x.
  - `v1-export` → `opencode import` (1.1.8, 1.2.21, 1.19.3) → `opencode export`: clean.
  - `v1-storage-json` placed as `<data>/storage` → 1.1.8 `opencode export`, and
    1.2.21 (which migrates it into `opencode.db` on start): clean.
  - `v1-sqlite` placed as `<data>/opencode.db` → 1.2.21 and 1.19.3 (which migrate
    its drizzle journal forward) `opencode export`: clean. 2.0.31 also opens it
    and runs its 1.x → 2.x migration; the migrated messages are imported fresh
    (see Carry), with no error.
  - `v2-sqlite` placed as `<data>/opencode.db` → 2.0.31 `opencode session export`: clean.
  - opencode's own sessions (generated, captured beta and 1.18 rows) exported as
    `v2-transfer` and imported by 2.0.31: accepted; the only differences are the
    ones opencode's import makes by design — `location` comes from the importing
    directory, and `fork` is not imported.
- **After the SPEC §3.1 pass** (2026-10-16, this adapter's final build): every
  non-opencode fixture (173) exported as `v2-transfer` without carry and with
  carry was accepted by `opencode session import` (2.0.31) and read back by
  `opencode session export`. Without carry the re-import equals our own reading
  of the export except `cwd`, which opencode takes from the importing directory;
  with carry every trip is clean. A sixth of them (29) exported as `v1-export`
  went through `opencode import` / `opencode export` of 1.19.3, 1.2.21 and 1.1.8,
  with and without carry: all accepted; with carry clean except
  `omp/tool-calls`, whose two empty fork branches 1.19.3 does not export and
  1.x reads back as subagents.
- Things this does **not** verify: the opencode UI's rendering of imported
  sessions, continuing an imported session with a real model, and 1.x versions
  other than the four above.

## Unverified / open

- The 1.x carrier and the 1.x header carry are extra fields opencode does not
  declare; 1.x keeps them on disk until it rewrites the record.
- `v1-sqlite` foreign exports use the 1.2.0 schema; later 1.x versions migrate
  it forward through their drizzle journal.
- Message types newer than 2.0.31 import as `native` events (kept exactly).
- 1.x `tokens.output`: 1.1.8, 1.2.0 and 1.2.21 store it with reasoning, 1.19.3
  without; versions in between are read as stored (with reasoning).
- Whether every 2.x provider sets `time.streamed` (2.0.31's do); a step without
  it has its usage at `time.completed`, after its tool results.

## Prefix stability (SPEC §9.3)

Not tested for prefix stability by `tests/prefix.rs`: opencode stores are SQLite databases, one JSON file per record (`v1-storage-json`), or one JSON document (`v1-export`, `v2-transfer`), not lines.

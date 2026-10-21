# OpenAI Codex CLI

`codex` reads and writes the rollout files Codex keeps for every thread, in
all four shapes Codex has used since April 2025, plain or zstd-compressed. It
is a primary format, with weight 3 in the cross-format matrix.

- **Self trip:** all 19 fixtures (Codex 0.1.2504 TS through 0.153.4, real
  sessions and third-party fixtures, a `CODEX_HOME` with `session_index.jsonl`)
  come back byte for byte, and re-import clean.
- **Neutral loss through Codex** (every other format's fixtures, X → Codex →
  X without carry): 5308 → 3222. See [Neutral loss](#neutral-loss).
- **Foreign export:** loaded by Codex's own `codex-rollout` crate built from
  openai/codex `0e67502` (2026-10-16): 0 parse errors, the thread is listed by
  the resume picker, the transcript builds. See [Verified](#verified).
- **Carry:** supported. The carrier is the top-level key `protoledger` of a
  rollout line, which Codex's decoder ignores.

## Where Codex stores data

```
$CODEX_HOME/sessions/YYYY/MM/DD/rollout-YYYY-MM-DDThh-mm-ss-<thread uuid>.jsonl   (CODEX_HOME defaults to ~/.codex)
$CODEX_HOME/sessions/YYYY/MM/DD/rollout-...-<thread>_<rollout>.jsonl             a reverted thread's new rollout
$CODEX_HOME/sessions/.../rollout-....jsonl.zst                                   compressed after 7 days (zstd level 3)
$CODEX_HOME/archived_sessions/...                                                archived threads, same shape
$CODEX_HOME/session_index.jsonl                                                  thread names, one {id, thread_name, updated_at} line per rename
~/.codex/sessions/rollout-YYYY-MM-DD-<uuid>.json                                 TypeScript CLI (April-May 2025)
```

The file-name timestamp is local time, and Codex parses it back as UTC
(`rollout/src/recorder.rs:1739-1760`, `rollout_file_name.rs:44-86`); the
exporter uses the ledger's UTC start time. Every agent is its own file: a
`thread_spawn` subagent, a guardian review and a fork each have their own
rollout, which names its parent in `session_meta`.

The importer accepts one rollout file (any name), a day directory, a
`sessions/` tree or a whole `CODEX_HOME`: it reads every `rollout-*.jsonl`,
`*.jsonl.zst` and `*.json` below it (any `*.jsonl` when none is named
`rollout-*`). The exporter writes paths relative to `CODEX_HOME`
(`sessions/YYYY/MM/DD/...`), so `protoledger export codex <ledgers> -o ~/.codex`
puts them where `codex resume` looks.

## Variants

The variant is detected per file from its first record. The producer version
is `session_meta.cli_version` and is recorded on every event's
`native.version`.

| variant | versions | shape | import | export |
|---|---|---|---|---|
| `ts-json` | TypeScript CLI, April-May 2025 (`codex-cli/src/utils/storage/save-rollout.ts`) | one pretty-printed JSON document `{session: {timestamp, id, instructions}, items: [...]}`, rewritten whole on every turn | ✓ | ✓ (`--variant ts-json`) |
| `rollout-v0` | Rust rust-v0.0.2504291921 – 0.32.x (tags: 0.32.0 is the last without an envelope) | JSONL; line 1 bare `{id, timestamp, instructions[, git]}`, then bare Responses API items, and `{"record_type":"state"}` lines from 0.10 | ✓ | ✓ (`--variant v0`; current Codex cannot load it) |
| `rollout-legacy` | ≥ 0.33.0 (envelope introduced between 0.32.0 and 0.33.0, Sept 2025), still the default | `{timestamp, type, payload}`; `session_meta` first; UI transcript in `event_msg` lines that copy the `response_item`s | ✓ | ✓ (default) |
| `rollout-paginated` | ≥ 0.143.0 (0.142.0 has no `Paginated`), opt-in or via `codex migrate-rollouts` | the same envelope plus a per-line `ordinal` from 0, `session_meta.history_mode: "paginated"`, UI records as `event_msg/item_completed` (`rollout/src/policy.rs:117-140`) | ✓ | ✓ (`--variant paginated`) |

Any variant may be stored as `.jsonl.zst`. The importer decompresses it and
records `source.extra.compression = "zstd"`; the exporter compresses with
Codex's own parameters (level 3, content size in the frame, no checksum:
`rollout/src/compression.rs:1011-1018`). When the stored frame is not what
those parameters produce, the original frame is kept in
`source.extra.zstd_frame` and written back while the content is unchanged.

A ledger imported from Codex is always exported in its own variant; the
requested variant applies to foreign ledgers.

## Ledgers

| native | ledger |
|---|---|
| a rollout file | One ledger. `agent.id` = `source.native_id` = `session_meta.payload.id` (the thread id); `<thread>_<rollout>` for a reverted thread's extra rollout, the file stem when there is no id. `agent.created_at` = `session_meta.payload.timestamp`. `source.extra.file` = the path relative to the input; `final_newline: false` when the file did not end in `\n`. |
| `source.subagent.thread_spawn.parent_thread_id`, or `parent_thread_id` with a subagent source / non-user `thread_source` (guardian review, review, compact) | `parent = {agent: parent thread, event, relation: subagent}`. `event` is the parent's `subagent` event for this child (`collab_agent_spawn_end.new_thread_id`, `sub_agent_activity`, or a paginated `SubAgentActivity` item), when the parent is in the set. A parent outside the set is still named. |
| `forked_from_id` | `forked-from` link. Forks are new threads, not subagents. A `thread_spawn` child that was forked from its parent's context (multi-agent v2) gets both. |
| records a paginated subagent inherited (`ordinal < subagent_history_start_ordinal`) | kept, with `meta.codex.inherited = true`. |

## Events

Each line is one native record; its event id is `@<byte offset>` of the line
in the (decompressed) file. Codex lines have no ids of their own (item ids
inside payloads repeat across forks and compactions), and Codex only ever
appends, so offsets are stable. A line that makes several events gives them
`@<offset>#<n>`. `ts-json` uses `@session` and `@items/<n>`. The first event
of a line carries the whole line in `native.record`, and `native.raw` too when
the line does not re-serialise to the same bytes (spaced JSON, CRLF). Blank,
torn and malformed lines become `native` events holding only `raw`, the way
Codex skips them (`recorder.rs:1119-1125`).

`turn` is the current `turn_id` (from `task_started`, `turn_context`,
`item_completed`, `token_usage_record`); it ends after `task_complete` /
`turn_aborted`.

| line | kind | body |
|---|---|---|
| `session_meta` (the first) | `settings` | `cwd`, `provider` (`model_provider`), `model`, `system_prompt` (`base_instructions.text`, or v0 `instructions`), `git_branch`, `agent` `{role, nickname, path}`. The placeholders the exporter writes for what a thread does not state (`cwd: "/"`, the epoch as `timestamp`, here and on every line) read back as unknown. |
| later `session_meta` | `native` | copies Codex puts in fork and subagent files |
| `turn_context` | `settings` only when it differs from the previous one, else `native` | `model`, `cwd`, `effort`, `approval` (`approval_policy`), `sandbox` (`sandbox_policy`), `reasoning_summary`, `collaboration_mode` |
| `response_item/message` | one `text` per text part, `media` per `input_image` / `input_audio` | `role` as written (`user`, `assistant`, `developer`, `system`), `text`, `phase`; parts of one message share a `group` (the item id or the line anchor); `refusal: true` for refusal parts; user messages Codex injects itself (`<environment_context>`, `# AGENTS.md instructions`, `<turn_aborted>` …) carry `meta.codex.injected = true`. A user message that is exactly the SPEC §3.1.6 wrapper `<peer-message direction="in" …>` is a `peer_message` (`direction: in`, `peer`, `network`, `text`). See [Groups](#groups) for `group`. |
| `response_item/reasoning` | `reasoning` | `summary` (the `summary_text`s), `text` (`content` reasoning text, when Codex kept raw reasoning), `sealed` `{vendor: "openai", kind: "encrypted", data: encrypted_content}` |
| `function_call` | `tool_call` | `call_id`, `name`, `input` (the parsed `arguments` when they re-serialise exactly, else `input_text`), `tool_type: function`, `namespace`. In an `mcp__<server>__` namespace (or, before namespaces, named `mcp__<server>__<tool>`): `tool_type: mcp`, `server`, no `namespace` |
| `custom_tool_call` | `tool_call` | `input_text` (e.g. an `apply_patch` body), `tool_type: custom` |
| `local_shell_call` / `web_search_call` / `tool_search_call` | `tool_call` | `tool_type` `shell` / `web_search` / `tool_search`, `input` = `action` / `arguments`, `status` |
| `function_call_output` / `custom_tool_call_output` | `tool_result` | `output`: the string, or parts `{type: text}` / `{type: media}`; the 2025 `{content, success}` object gives `output` + `is_error` |
| `tool_search_output` | `tool_result` | `output` = the tools as JSON text |
| `image_generation_call` | `media` | `role: assistant`, `image/png` data, `revised_prompt` |
| `compaction` / `context_compaction` (remote, encrypted) | `compaction` | `phase: summary`, `sealed` |
| `response_item/agent_message`, `inter_agent_communication` | `peer_message` | `direction` (out when `author` is this thread's agent path), `peer` (the other agent path), `text`, `sealed`, `trigger_turn` |
| `additional_tools` | `settings` | `tools` |
| `compacted` | `compaction` | `phase: summary`, `summary` (`message`), `preserved` (ids of earlier response-item events repeated verbatim in `replacement_history`), `replacement_items` (its length), `tokens_before`, `group` = `compaction_response_id` |
| `event_msg/context_compacted`, `item_completed/ContextCompaction` | `compaction` | `phase: boundary` |
| `task_started` | `turn` | `phase: start` |
| `task_complete` / `turn_aborted` | `turn` | `phase: end`, `outcome` (`ok`, `error`, `interrupted`, `aborted`), `reason`, `duration_ms` |
| `token_usage_record` (0.153+) | `usage` | one response's `usage` (SPEC §3.1.1, below), `cumulative: false`, `group` = the response's (see [Groups](#groups); `response_id` when it had no output item), `model` (from the last `turn_context`) |
| `token_count`, the copy of the `token_usage_record` just before it | `native` | |
| `token_count` with new totals, no record | `usage` | the response's `last_token_usage`, `cumulative: false`, `model`, `context_window` |
| `token_count` whose last-response counts are all zero | `usage` | the running total `total_token_usage`, `cumulative: true` |
| `token_count` repeating the previous totals (rate-limit refreshes, context re-estimates), or holding only an estimate (an importer's seed) | `native` | |
| `session_index.jsonl` line (at the root of a `CODEX_HOME` input) | `title` | `text` (`thread_name`), `ts` (`updated_at`), in the thread `id` names, after the first run of events stamped with that time (else before the first later one). Lines for threads not in the input: `title-thread-missing`. |
| `collab_agent_spawn_end`, `sub_agent_activity`, `item_completed/SubAgentActivity` | `subagent` | `phase`, `agent` (the child thread), `call_id`, `agent_type`, `description` |
| `patch_apply_end`, `item_completed/FileChange` | one `file_change` per path | `path`, `op` (`create`/`edit`/`delete`), `diff`, `content`, `move_path` |
| `error`, `stream_error` | `error` | `message`, `retryable` |
| `world_state`, `retained_context`, `security_risk_score`, `realtime_item`, `inter_agent_communication_metadata`, `thread_settings_applied`, `ghost_snapshot`, `exec_command_end`, `mcp_tool_call_end`, `web_search_end`, unknown types | `native` | |
| v0 `{"record_type":"state"}` | `native` | |

### Usage

SPEC §3.1.1: neutral `input` excludes cache reads and writes. Codex's
`input_tokens` is the Responses API total, of which `cached_input_tokens` and
`cache_write_input_tokens` are details (`codex-api/src/sse/responses.rs:128-141`),
so `input = input_tokens - cached - cache_write`; `output` is `output_tokens`
(reasoning included), `reasoning` is `reasoning_output_tokens`. Codex writes
every count, so a zero cannot say "not reported" (OpenAI reports no cache
writes at all): a zero cache write or reasoning count is read as not stated; a
zero cache read is kept. Codex 0.153+ writes both a `token_usage_record` and a
`token_count` per response; the record is the usage event and the count is
its copy, so real 0.153 rollouts no longer count every response twice.

### Groups

The codex import is prefix-stable (SPEC §9.3): a group is decided as each item is read, never re-labelled by a later record.

SPEC §3.1.4. Codex gives every item its own id (`msg_…`, `rs_…`, `fc_…`), and
names the response only in its `token_usage_record` (`response_id`), which it
writes after the response's items. The importer therefore groups:

- in a rollout that writes `token_usage_record`s (`session_meta` says
  `cli_version` 0.153 or later), the model output items (assistant messages,
  reasoning, calls) of one response that have no group of their own, under
  the response's **first** such item: its `id` (`rs_…`), or its event id when
  it has none. The response starts at its first output item and ends at a
  user or developer message, a turn start, a compaction or its record; a
  tool's output ends its items, and the record after it still reports on
  them. The record's `usage` takes that same group, or the `response_id`
  when the response had no such item; the `response_id` itself stays in the
  usage's native record. A later record never re-groups an item already read;
- an output item whose `id` is not shaped like a Responses API id (no
  `<prefix>_<suffix>`, `protocol/src/response_item_id.rs:36-39`; Codex strips
  such ids from requests, `core/src/client.rs:1098-1103`) under that id;
- the parts of one multi-part message under its item id (or line anchor), as
  before.

API-shaped ids on single items are not read as groups: they are Codex's item
handles, and reading them would change every Codex rollout's neutral form.

### UI copies

In legacy rollouts the transcript a person sees is a second copy of the
conversation: `event_msg/user_message`, `agent_message`, `agent_reasoning`
and `agent_reasoning_raw_content` repeat what the `response_item`s already
hold. Paginated rollouts do the same with `item_completed` `UserMessage`,
`AgentMessage` and `Reasoning` items. Counting both would double every
message.

A UI record and its item are paired as they are read (SPEC §9.3: nothing an
import emits depends on a later record), by class and by text (equal, or
contained: a user message's content can hold more than the typed text). Every
UI record becomes a `native` event that still carries its line for the self
trip:

- **Its item was read before it** (Codex records a prompt, then shows it):
  it copies the nearest unclaimed item, and `meta.codex.ui_copy_of` names that
  item's event.
- **Otherwise it waits**, with `ui_copy_of: null`. Codex shows an answer or a
  reasoning summary, then records it (in 0.47, every UI record of a response,
  then its `token_count`, then its items), so the item read next normally
  claims it and holds the content. If a record after which its item can no
  longer come arrives first, the content is emitted there, as an event of its
  own (`<UI event id>#1`, part 1 of the UI line's native record): a file that
  kept only the UI side, reasoning Codex showed but did not persist, or a
  message the model never saw. Such a record is a user or developer message,
  a tool's output, the `token_usage_record`, a turn's start or end, or a
  compaction for every UI record; an assistant message for reasoning; a call
  for any; and any record but a UI record for a prompt's copy.
- A UI record still waiting when the file ends (`ui-record-unsettled`) has its
  content only in its native record until the next line settles it: a live
  session writes the item, or the turn's end, next.

## Export

Events that came from Codex are written back as their native lines,
untouched. Every other event is rendered from its neutral fields, following
the records Codex itself writes, its decoder, and its own external-session
importer (`external-agent-migration/src/sessions/export.rs:80-162`):

- **File and `session_meta`.** `sessions/YYYY/MM/DD/rollout-<UTC start>-<thread>.jsonl`.
  The thread id is the Codex thread id for a Codex ledger, the agent id when
  it is a UUID, else `uuid_for(agent id)`. `session_meta` comes first:
  `session_id` (the root thread), `id`, `forked_from_id` (from a
  `forked-from` link), `parent_thread_id`, `timestamp`, `cwd` (from settings,
  inherited from the parent for a subagent, else `/`), `originator:
  codex_cli_rs`, `cli_version` (`--target-version`, default `0.163.0`),
  `source: "cli"` (one of the sources the resume picker lists,
  `rollout/src/lib.rs:89-96`) or `{subagent: {thread_spawn: {parent_thread_id,
  depth, agent_path, agent_nickname, agent_role}}}` for a structural child,
  `thread_source`, `model_provider` (`"openai"` when the ledger says OpenAI,
  else `null`, which the resume picker matches with the default provider,
  `rollout/src/list.rs:958-963`; naming another provider would hide the
  thread, and naming OpenAI would be false), `base_instructions` (the system
  prompt or null), `history_mode`, and `git.branch`. `cwd` is `/` and
  `timestamp` the epoch when the ledger does not state them; both read back
  as unknown.
- **Turns.** Neutral `turn` events become `task_started` / `task_complete` /
  `turn_aborted`. A ledger without turns gets one per user prompt, as Codex's
  importer does; turn ids are the event's `turn` or `uuid_for(<agent>/turn/<n>)`.
- **No `turn_context`.** A `turn_context` tells Codex the initial model context
  (environment, permissions, AGENTS.md) was already sent, and on resume it
  would then only send differences against it. Without one, Codex injects
  the full context on the next turn (`core/src/session/rollout_reconstruction.rs:30-52`,
  test `record_context_updates_and_set_reference_context_item_injects_full_context_when_baseline_missing`).
  Codex's importer writes none either. Every record that names a model
  (`turn_context`, `thread_settings_applied`) also requires an approval
  policy and a sandbox / permission profile, which Codex applies on resume
  (`app-server/src/request_processors/persisted_resume_settings.rs`); a
  foreign thread does not state them, and inventing them would change how the
  resumed thread runs. Settings are therefore not rendered
  (`settings-not-rendered`) beyond what `session_meta` holds, and usage loses
  its `model` (`usage-model-dropped`).
- **Messages.** `response_item/message` with `input_text` / `output_text` /
  `input_image` (data URL or URL) parts; text and media of one `group` form
  one message. The legacy UI copy (`user_message` after a prompt, as a live
  session records it; `agent_message` after an answer) or the paginated
  `item_completed` is written alongside, except for injected context. An
  assistant message, reasoning or call whose `group` is that one item and is
  not shaped like an API id gets the group as its item `id`; API-shaped ids
  are not written, since Codex would send them to OpenAI with the wrong
  prefix. A message whose audience is `record` (the model never saw it) is
  never a model-visible item: a user or assistant one is only its transcript
  event, anything else is `record-message-dropped`.
- **Reasoning.** `response_item/reasoning` with the summary as `summary_text`,
  raw text as `reasoning_text` content, and `encrypted_content` only when the
  sealed data is OpenAI's own ciphertext, otherwise `null`; it is never
  fabricated. Another vendor's signature is dropped. Legacy UI copies:
  `agent_reasoning` per summary, `agent_reasoning_raw_content` for the text.
- **Tools.** `function_call` with `arguments` = the input as compact JSON (an
  `mcp` call in its server's `mcp__<server>__` namespace,
  `codex-mcp/src/mcp/mod.rs:80-86`), `custom_tool_call` for raw-text input,
  then `function_call_output` / `custom_tool_call_output` with the output
  string or content items. A function call with no result is left so: Codex
  adds its `aborted` output itself when it builds the next prompt
  (`core/src/context_manager/normalize.rs:21-110`, logged at info). A custom
  call with none gets that output written (`tool-output-synthesized`), since
  Codex treats a missing one as an invariant violation (`error_or_panic`). A
  result with no call is dropped, as Codex drops it (`normalize.rs:171`).
- **Usage.** A response's `usage` with a `group` becomes the
  `token_usage_record` Codex writes per response (`response_id` = group,
  `protocol.rs:2389-2400`; only for a target of 0.153 or later) followed by its
  `token_count`; without a group, only the `token_count` (running
  `total_token_usage`, the event as `last_token_usage`). Counts follow SPEC
  §3.1.1 (`input_tokens = input + cache_read + cache_write`);
  `cache_write_input_tokens` is written only when stated. A `cumulative`
  usage is a `token_count` whose totals are the running total and whose
  last-response counts are all zero, then a refresh line restating the totals
  with the last response's usage, as Codex refreshes them, so the latest token
  info a resumed thread reads still knows how full its context is. `cost_usd`
  has no place (`usage-cost-dropped`). A ledger with no usage gets one
  `token_count` at the end of its last turn seeded with `ceil(text bytes / 4)`
  tokens, exactly as Codex's importer seeds an imported thread
  (`export.rs:131-150, 195-207`).
- **Titles.** Each title is a `session_index.jsonl` line `{id, thread_name,
  updated_at}` at the root of the output (`rollout/src/session_index.rs:20-50`).
  With carry, the title rides the rollout's carrier and the index line has an
  empty one. **Exporting into a live `CODEX_HOME`:** the output's
  `session_index.jsonl` replaces the existing one when written with `--force`
  (protoledger writes whole files); append its lines to the existing index
  instead.
- **File changes.** A change Codex's `FileChange` can state whole (a creation
  or deletion with its content, an edit with its diff, no group) is the
  `patch_apply_end` event Codex records when a patch is applied (legacy,
  `protocol.rs:3859-3877`) or a `FileChange` `item_completed` (paginated);
  anything else is `file-change-dropped`.
- **Compaction.** A summary becomes `compacted {message}` (Codex rebuilds the
  replacement history from it on resume), its group as
  `compaction_response_id`; a boundary becomes
  `event_msg/context_compacted`; OpenAI-sealed remote compaction becomes
  `response_item/compaction`.
- **Peers.** A `peer_message` between agents of one thread tree (a peer that
  is an agent path, or a ledger of the set under the same root), with no
  network, becomes `inter_agent_communication` with valid agent paths: `/root`
  for a root thread, `/root/<name>` for a structural child (names lowercased
  to `[a-z0-9_]`). Any other peer (an orchestrated peer, one on a network)
  has no Codex channel: an incoming message is the SPEC §3.1.6 wrapped user
  text the model was shown, an outgoing one is
  `peer-message-out-not-rendered`.
- **Subagents.** A structural child is its own rollout with a `thread_spawn`
  source; the parent's `subagent` event has no line of its own.

The UI copies, seeded token counts and synthesized outputs/turns are written
with empty carriers, so a carried round trip restores exactly the original
events.

### Carry

Codex decodes a line by removing `timestamp` and `ordinal` and deserialising
the rest as the internally tagged `RolloutItemWire` (`rollout/src/lib.rs:52-75`,
`history/src/rollout_payload.rs:33-72`), and no rollout payload type uses
`deny_unknown_fields` (the only ones in `protocol/` are permission and MCP
policy types). An extra top-level key is ignored on load, and dropped
whenever Codex itself re-serialises history (compaction, forks). The carrier
therefore sits at the top level of each rendered line (of the bare item in
`rollout-v0`, of the item object in `ts-json`, and on the `session` object for
anything that has no item of its own there).

It is `carry::attach_with`'s carrier: `{v, h, events}` plus the agent header
(`agent`) on the first rendered line of each foreign ledger. A rendered line
that only accompanies another (a UI copy, a seeded `token_count`, a
synthesized output or turn marker) carries an empty `events` list, so on
re-import it is absorbed instead of becoming a new event. A line edited in
Codex no longer matches its hash and is imported fresh; lines Codex appends
after a resume have no carrier and are imported normally. Codex never edits a
line in place, so in practice the carried part of a file always restores.

## Loss codes

| code | when |
|---|---|
| `line-malformed` | import: a line that is not a JSON object (kept as raw native text). |
| `file-not-rollout` | import: a file in a directory that is not a rollout (skipped). |
| `abandoned-branch-not-rendered` | export: events on an abandoned branch (SPEC §2.3) other than turns a native `thread_rolled_back` dropped (those are written back with it); only the active path is rendered, and with carry they ride on the next line. |
| `revert-not-located` | never raised by this format (an opencode import code). |
| `ts-json-raw-kept` | import: a `ts-json` document that does not re-serialise exactly (its text is kept in `source.extra.raw_text`). |
| `settings-not-rendered` | export: a settings event; see "No `turn_context`" above. |
| `provider-not-rendered` | export: a provider Codex has no id for is left `null`. |
| `usage-model-dropped` | export: usage names a model (Codex names it only in `turn_context`). |
| `usage-cost-dropped` | export: usage states a cost (Codex token counts have none). |
| `reasoning-sealed-dropped` | export: another vendor's sealed reasoning (signature, redacted block). |
| `media-dropped` | export: media Codex has no input for (assistant media, a path, non-image data). |
| `tool-output-synthesized` | export: a custom tool call with no result got Codex's `aborted` output. |
| `tool-result-orphan-dropped` | export: a result with no call in the thread. |
| `tool-error-flag-dropped` | export: `is_error: true` (Codex outputs carry no error flag on disk). |
| `compaction-preserved-dropped` | export: `preserved` ids (Codex rebuilds the kept prefix itself). |
| `compaction-dropped` | export: a compaction in `rollout-v0` / `ts-json`, which have no record for it. |
| `peer-message-out-not-rendered` | export: a message sent to a peer outside the thread's agent tree. |
| `peer-message-fields-dropped` | export: fields of a wrapped peer message other than peer, network and text. |
| `record-message-dropped` | export: a message the model did not see, of a role with no transcript-only event. |
| `title-dropped` | export: a title in `rollout-v0` / `ts-json`, which predate `session_index.jsonl`. |
| `title-thread-missing` | import: a `session_index.jsonl` line naming a thread not in the input. |
| `ui-record-unsettled` | import: the file ends with a UI record whose item may still be written (see "UI copies"); its content is in its native record until a later line settles it. |
| `subagent-event-dropped` | export: a `subagent` event (the child is its own rollout). |
| `file-change-dropped` | export: a snapshot, or a change Codex's `FileChange` cannot state whole. |
| `error-dropped` | export: an `error` event (Codex does not persist errors). |
| `app-dropped`, `native-dropped` | export: application events and other formats' native records. |
| `agent-meta-dropped` | export without carry: header links (other than `forked-from`) and metadata. |
| `event-not-carried` | export with carry: events after the last rendered line of a Codex ledger. |

## Verified

- **Self trip.** Every fixture is reproduced byte for byte and re-imports with
  no difference (`FIXTURE=codex cargo test --test fixtures`). The fixtures
  cover a TS-era JSON document (2025-04), v0 files (2025-07 shapes and a rust-v0.20.0 synthetic), legacy 0.47 / 0.112 /
  0.130 / 0.142 / 0.150, paginated 0.147 (spaced JSON), 0.149.1 and 0.153.4 with
  `thread_spawn` subagents, a guardian review, a fork with
  `inter_agent_communication` and `token_usage_record`,
  malformed lines, a torn last line, and `.jsonl.zst`.
- **Codex's own loader, after the loss pass.** The same throwaway binary
  (below), extended to read each thread's name with Codex's
  `find_thread_name_by_id` and to list threads with the resume picker's
  provider filter (`["openai"]`, default provider `openai`), ran on the unit
  tests' exports and on the legacy and paginated, carried and plain exports of
  22 fixtures from 18 other formats (claude-code, opencode, gemini-cli, atif,
  droid, cursor, openclaw, protoledger, copilot-cli, deepagents, hermes,
  slack, pi, omp, openhands, letta-code): 0 rejected lines and 0 parse errors
  in every file, `get_rollout_history` and the file name parse for every file,
  `token_usage_record`, `patch_apply_end` and `FileChange` lines load, every
  `session_index.jsonl` name is found by thread id, and the provider filter
  lists exactly the threads the unfiltered listing does (a `null`
  `model_provider` matches the default). The only threads not listable are
  those with no user prompt at all (a subagent, a branch, an empty thread),
  which Codex lists for no thread.
- **Codex's own loader.** A throwaway binary built against `codex-rollout`,
  `codex-history`, `codex-protocol` and `codex-app-server-protocol` from
  openai/codex `0e67502` (2026-10-16, built with its pinned Rust 1.95.0) ran
  on the foreign exports of `src/formats/codex/tests.rs` (a Claude-shaped
  parent with a subagent, reasoning, three tool kinds, usage, compaction, a
  peer message, media; legacy and paginated, with and without carriers). For
  every file: `parse_rollout_line` accepts every line (carriers included),
  `RolloutRecorder::load_rollout_items` reports 0 parse errors and the right
  thread id, `get_rollout_history` returns `Resumed`, the file name parses
  (`rollout_id_from_path`), `read_thread_item_from_rollout` yields a thread
  with the first prompt as preview, cwd and source (the child's
  `SubAgent(ThreadSpawn { parent_thread_id, depth: 1, agent_path: /root/... })`),
  `get_threads` with the interactive sources lists the root thread (and,
  correctly, not the subagent), and `build_turns_from_rollout_items` builds
  the legacy transcript (user message, reasoning, agent messages, context
  compaction per turn). A real 0.142.2 rollout continued with a foreign turn
  (`continued_thread_appends_after_native_lines`) loads the same way, the new
  turn showing as a third transcript turn. The same binary loaded the synthetic fixtures with 0
  parse errors and Codex's `.jsonl.zst` reader read the compressed fixture.
  The binary lives outside this repository; to repeat the check, write the
  exports with `PROTOLEDGER_CODEX_EXPORT_DIR=<dir> cargo test --lib
  codex::tests -- --test-threads=1` and point a `main.rs` calling those
  functions at `<dir>/<variant>-<carry|plain>` (a `CODEX_HOME`). The
  `rollout-v0` and `ts-json` exports are, as expected, rejected line by line
  by the current loader.
- **Loader rules as a test.** `foreign_export_follows_codex_loader_rules`
  checks the same rules in-tree, each with its source line: file-name
  pattern, `session_meta` first with a UUID id and the required fields, a
  known `history_mode`, consecutive ordinals in paginated files, only known
  `event_msg` types (`EventMsg` has no catch-all variant), no legacy UI events
  in paginated files, `encrypted_content: null` on rendered reasoning, valid
  agent paths, one output per custom call and none twice, a `token_usage_record` with every field, `model_provider` `openai` or null, item ids Codex would send only with the right prefix, and well-formed `session_index.jsonl` lines.
- **Old formats.** The variant boundaries come from openai/codex release tags:
  rust-v0.32.0 has no `RolloutLine`, 0.33.0 does; 0.142.0 has no
  `ThreadHistoryMode::Paginated`, 0.143.0 does.

## Neutral loss

The sum of `no_carry_there["codex"]` over every other format's fixtures
(`tests/reports/*/*.json`), before (main `1c16276`, with the comparator of
that commit) and after this adapter's loss pass:

| category | before | after | |
|---|---:|---:|---|
| **total** | **5308** | **3222** | |
| `text.group` | 609 | 191 | a response's items carry its `token_usage_record` id; a one-item group is the item id |
| `tool_call.group` | 582 | 169 | as above |
| `usage.group` | 424 | 2 | `token_usage_record.response_id` |
| `reasoning.group` | 246 | 83 | as above |
| `extra usage` / `usage.body.cumulative` | 149 / 62 | 0 / 0 | seeds and record copies are not usage; running totals are cumulative |
| `usage.body.input` / `output` / `cache_read` | 35 / 38 / 27 | 1 / 1 / 0 | SPEC §3.1.1 input convention |
| `missing title` | 152 | 21 | `session_index.jsonl` |
| `extra tool_result` | 84 | 12 | no fabricated `aborted` outputs for function calls |
| `missing text` / `extra text` | 51 / 51 | 17 / 10 | the prompt's transcript copy follows it |
| `missing file_change` | 92 | 72 | whole changes as patch records |
| `usage.body.model` | 527 | 566 | Codex names models only in `turn_context` (not written); more usage events now match, each without its model |
| `usage.body.cost_usd` | 347 | 360 | Codex has no cost |
| `usage.body.cache_write` / `reasoning` | 272 / 174 | 265 / 156 | a zero Codex writes cannot say "not reported" (see [Usage](#usage)) |
| `missing settings` / `extra settings` / `settings.body.*` | 173 / 58 / 207 | 218 / 10 / 164 | no `turn_context` (see Export) |
| `reasoning.body.sealed` | 139 | 139 | another vendor's signature cannot be replayed to OpenAI |
| `tool_result.group` | 91 | 91 | a tool output is input to the next request, not part of a response |
| `tool_result.body.is_error` | 52 | 52 | `FunctionCallOutputPayload.success` is never serialised |
| `missing subagent` | 49 | 49 | the child is its own rollout; the parent has no line for it |

Making import prefix-stable ( a response's items are grouped as they are
read, so a response with no usage, which no record names, keeps its group
too) moved the same sum from 4010 (main `fa28460`) to 3664, with no via
rising; every codex fixture's own total is unchanged (30278).

Then the comparator began to charge a change in how events are
grouped into responses, whatever the group ids are called: `<kind>.group-split`
for each extra piece a source response comes back in, `<kind>.group-merged` for
each extra source response a target response holds. Through Codex that charges
68 splits (`tool_call` 51, `text` 13, `reasoning` 2, `compaction` 2) and 4
merges, so the same sum goes from 3664 to 3736. The merges are accepted
costs, charged rather than fixed: a rollout has no record that closes a
response other than its `token_usage_record` or a tool output, so two
responses written back to back with neither between them are one response
when read back, and a usage record a source writes before its own response's
tool call lands in the response before it. By name:

- `claude-code/transession-2.1.71`: `msg_01KXUr…` (reasoning, text) and
  `msg_01Jy6N…` (tool call) come back as one response (`tool_call.group-merged`);
- `cursor/transcript-real-webfetch-plan-2026.09.26`: `@558` and `@938` come back
  as one (`tool_call.group-merged`);
- `gemini-cli/jsonl-v0.50-history-sets`, `gemini-cli/jsonl-v0.64-patch-subagent`:
  a usage written before its response's tool call joins the previous response
  (`usage.group-merged`).

What remains is what Codex cannot say: the model and settings of a turn
(only `turn_context`, which this exporter does not write), cost, a stated zero
in a count Codex always writes, another vendor's sealed reasoning, error flags
on tool outputs, groups of several items in a rollout older than 0.153 (no
record names them), tool outputs' groups, structural subagent events, file snapshots, peer
messages sent outside the agent tree, and error / application events.

## Unverified

- No live `codex resume` was run (Codex is not installed and there is no
  OpenAI account here), so the model-side replay of a foreign history is
  untested: in particular whether the Responses API accepts a replayed
  `reasoning` item that has a summary but no `encrypted_content`.
- In paginated mode, `build_turns_from_rollout_items` projects only tool-like
  items (Codex's own paginated files show the same); the paginated thread
  view in the app server (`thread-store` materialisation) was not exercised.
- Codex 0.33 – 0.142 loaders were not built; the exporter writes the current
  shape, which older readers accept for the fields they know.
- The TypeScript CLI's exact release range is taken from the repository
  history (its saver at rust-v0.0.2505191518), not from npm releases.
- `turn_context` and settings round trips through other formats rely on
  carry; without it Codex keeps only `session_meta`'s settings.
- Item ids written from groups are never API-shaped, so Codex strips them
  from requests; this follows `client.rs`, but no request was sent.
- `token_usage_record` is written only for a target of 0.153 or later, the
  first release seen writing it; the exact release that introduced it was not
  established (the source checkout is shallow).

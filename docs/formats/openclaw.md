# openclaw (OpenClaw)

Adapter: `src/formats/openclaw/` (`mod.rs` the pi-session mapping and JSONL
store, `sqlite.rs` the SQLite store, `jsonl.rs` line and carrier plumbing
shared with droid). Format id: `openclaw`. The pi-session parsing here is
independent of the `pi` adapter.

## Storage

| OpenClaw | store |
|---|---|
| ≤ v2026.8.0 (and clawdbot) | `~/.openclaw/agents/<agentId>/sessions/<sessionId>.jsonl` (`-topic-<id>` for Telegram topics) plus `sessions.json`, a `{sessionKey: SessionEntry}` index (`~/.clawdbot` before the rename; `$OPENCLAW_STATE_DIR` overrides) |
| ≥ v2026.8.1 | `~/.openclaw/agents/<agentId>/agent/openclaw-agent.sqlite`; JSONL is read only by `openclaw doctor --session-sqlite import`, which moves it into SQLite |

Import takes a `.jsonl`, an `openclaw-agent.sqlite`, or a directory (every
`.jsonl`, `sessions.json` and `.sqlite` below it). Archived copies
(`.jsonl.reset.<ts>`, `.deleted.<ts>`) are not read.

## Variants and versions

The transcript header's `version` is the pi / OpenClaw session format.

| variant | producers | shape |
|---|---|---|
| `session-v1` (import) | pi ≤ 0.30, clawdbot | no `version`, no entry ids (anchored `@<byte offset>`), `firstKeptEntryIndex` compactions, `branchedFrom` |
| `session-v2` (import) | pi 0.31–0.34 | `id` / `parentId` tree, `hookMessage` role |
| `session-v3` (import) | pi ≥ 0.35; OpenClaw ≤ 2026.9.6 | `custom` role. pi's writer (8-hex ids, base fields first) until OpenClaw 2026.5.27, then OpenClaw's fork (UUID ids, payload first) |
| `session-v4` (import, export) | OpenClaw ≥ 2026.9.7 | same entries; v4 only changes how user text is escaped into the prompt |
| `sqlite` (import, export) | OpenClaw ≥ 2026.8.1 | `transcript_events(session_id, seq, event_json ‖ event_zstd, …)`, one row per transcript line; `session_nodes` / `session_windows` |

Detection: a first JSON line `type:"session"` with an `id` (confidence 90
when the path or content shows OpenClaw markers such as `.openclaw`,
`agents/…/sessions/`, `delivery-mirror`, `"provider":"openclaw"`, otherwise
55 so the `pi` adapter can win on plain pi files); a SQLite file with
`transcript_events` and `session_windows`. The producer version is recorded
only in the SQLite store (`schema_meta.app_version`, e.g. `2026.10.4 (agent
schema 24)`); for JSONL `agent.source.version` is `session-v<N>`.

## Mapping

| pi / OpenClaw entry | protoledger |
|---|---|
| `session` header | `settings{cwd}`; `parentSession` (a file path) → `forked-from` link (id from the file name) and `agent.parent{relation: fork}` when that session is in the set |
| `message` user / `custom` / `hookMessage` | `text{role:user}`, `image` → `media{media_type, data}` |
| `message` system (pi 0.86) | `text{role:system}` |
| `message` assistant | `reasoning{text, redacted, sealed{…, data: thinkingSignature}}` (SPEC §3.1.5, by `api`: `openai-completions` → no seal, the signature only names the field the reasoning came in; `openai-*` → `{openai, encrypted}`; `anthropic-*` → `{anthropic, signature}`; `google-*` → `{google, signature}`; otherwise `{provider, signature}`), `text`, `tool_call{call_id, name, input: arguments, tool_type: function}`, `usage{model, input, output, cache_read, cache_write, reasoning, cost_usd: cost.total}` (all-zero usage is skipped), `error{kind: provider, message}` for `errorMessage`, `turn{phase:end, outcome:aborted}` for `stopReason:"aborted"`; `group` = entry id |
| `message` toolResult | `tool_result{call_id, output (string, or text/media parts), is_error}` |
| `message` bashExecution (`!cmd`) | `tool_call{tool_type: shell, name: bash}` + `tool_result`; `audience: record` with `excludeFromContext` |
| `model_change` / `thinking_level_change` | `settings{provider, model}` / `settings{effort}` |
| `usage` entry | `usage` |
| `compaction` | `compaction{phase: summary, trigger, summary, tokens_before}` |
| `branch_summary` | `app{type: pi/branch_summary, data}` (audience model) |
| `custom_message` | `text{role:user}` |
| `reset` (OpenClaw) | `compaction{phase: boundary, trigger: manual (new/reset) ‖ auto, reason}` |
| `session_info` | `title` |
| `session_nodes.label` / `display_name` (SQLite) | `title` |
| `custom`, `label`, `context_edit`, `leaf` (OpenClaw), unknown types | `native` |
| a line that is not JSON | `native` with `native.raw` |
| `sessions.json` `spawnedBy` / SQLite `spawned_by` | `agent.parent{relation: subagent, event: the sessions_spawn call whose result names the child's key}` |
| SQLite `previous_session_id` | `continued-from` / `continued-in` links |

`after` is set when an entry's `parentId` is not the previous entry. Event
ids are entry ids (`#n` parts), or `@<byte offset>` for id-less lines.

## Self trip

- JSONL: exact, including corrupt lines, a UTF-8 BOM
  (`agent.source.extra.bom`), a missing final newline, and `sessions.json`
  (kept whole on the first ledger).
- SQLite: **not byte-exact** (`self_exact:false`). The whole store is
  captured (schema objects in order, every ordinary table's rows, FTS
  content) and rebuilt tables-first, with indexes, triggers and views
  created last so OpenClaw's triggers do not fire on restored rows.
  Verified with `sqlite3`: identical schema and rows in every table; FTS5
  content identical, but FTS5's internal segment blocks (`*_fts_data`,
  `*_fts_idx`) can be laid out differently. A ledger with events added after
  import is written as JSONL instead.

## Export of foreign ledgers

Writes v4 JSONL in OpenClaw's own writer shape (payload first, then
`id, parentId, timestamp`; UUID v5 entry ids; header `type, version: 4, id,
timestamp, cwd, parentSession?`) and always a `sessions.json` index
(`agent:main:protoledger:<sid>` / `agent:main:subagent:<sid>` keys,
`spawnedBy` for subagents) — OpenClaw imports only transcripts its index
references. Assistant parts sharing a `group` become one message with
zeroed usage unless a usage event follows. A `peer_message` is written as
SPEC §3.1.6 wrapped text (a user message when incoming, assistant text when
outgoing), and the importer turns user or assistant text holding that wrapper
back into a `peer_message`.

**Branches (SPEC §2.3): expressed.** An event whose `after` names an earlier
event than the previous one (a rewind) starts an entry whose `parentId` is the
entry that event went into; the abandoned entries stay in the file as a branch
of the tree. OpenClaw (pi's session tree) resumes from the last entry, so it
follows the active path, and the importer's `after` brings the branch back.
Nothing is dropped, so there is no `abandoned-branch-not-rendered` loss.
Pinned by `openclaw::tests::a_rewound_ledger_is_written_as_a_branch_of_the_entry_tree`.

**Verified with OpenClaw:** exports of the droid `subagent-task`, hermes
`state-db-v31-lineage` and deepagents `tool-calls-files` fixtures were
imported by `openclaw@2026.10.4`'s own `openclaw doctor --session-sqlite
import` with no issues; the carriers survived into `event_json`, and
importing the resulting SQLite store gave back every original event (the
droid files were reproduced byte for byte).

| loss code | when |
|---|---|
| `system-text-as-custom-message` | system/developer text becomes a `custom_message` (pi keeps no system messages) |
| `media-dropped` | non-image media, or media in assistant messages |
| `reasoning-sealed-dropped` | encrypted reasoning (only signatures have a slot) |
| `tool-input-text-wrapped` | raw or non-object tool input becomes `{"input": …}` |
| `usage-unattached` | usage with no assistant message to hold it |
| `usage-cumulative-dropped` | running totals (`cumulative: true`, SPEC §3.1.1): messages hold per-response usage only |
| `settings-dropped` | settings other than model / effort / cwd |
| `compaction-boundary-dropped` | a compaction boundary without a summary |
| `error-dropped` | an error with no assistant message to hold it |
| `subagent-flattened` | a subagent whose parent is not in the set |
| `subagent-marker-dropped`, `file-change-dropped`, `app-dropped`, `foreign-native-dropped` | no pi entry exists |

Text with `audience: record` is written as a `custom` entry
(`customType: protoledger.record-text`), which pi keeps out of the model's
context, and read back as such.

## Carry

Yes: every rendered entry gets a top-level `protoledger` carrier; the
synthesised header carries the agent header; events with no rendering ride
on the next entry, or on a `custom` entry (`customType: protoledger`) when
nothing else was rendered. OpenClaw keeps unknown top-level keys through its
JSONL → SQLite import (verified).

## Unverified

- That OpenClaw JSONL written between 2026-05-27 and the SQLite switch used
  the forked writer (the code shows it today).
- Writing a SQLite store for foreign ledgers (deliberately not done: the
  store carries OpenClaw's own projections; use the JSONL + doctor path).
- `.jsonl.reset.<ts>` archives and `session-sqlite-import-archive/`.

## Prefix stability (SPEC §9.3)

JSONL sessions are prefix-stable (`tests/prefix.rs`, 160 (file, k) pairs, 0 unstable). A session file with nothing in it yet is not imported: it would name an agent after the file, not the session.

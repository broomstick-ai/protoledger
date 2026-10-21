# hermes (Nous Research Hermes Agent)

Adapter: `src/formats/hermes/` (`mod.rs` mapping and legacy files, `db.rs`
state.db, `pyjson.rs` Python `json.dumps` formatting, `schema_v31.sql` the
embedded v31 schema, MIT (c) 2025 Nous Research). Format id: `hermes`.

## Storage

| form | path | status |
|---|---|---|
| `state.db` (SQLite) | `$HERMES_HOME/state.db`, default `~/.hermes/state.db` | the live store; the only one `--resume` reads |
| CLI session log | `~/.hermes/sessions/session_<id>.json` | written up to v2026.5.16, writer removed 2026-05-20 |
| gateway transcript | `~/.hermes/sessions/<id>.jsonl` | written up to v2026.5.16, removed 2026-05-20 |

Import takes a `state.db`, a legacy file, or a directory (a hermes home: every
`state.db`, `session_*.json` and `*.jsonl` below it). A session present in
both state.db and a legacy file becomes two ledgers (`<id>~<file>`).

Not handled: `trajectory_samples.jsonl` (ShareGPT training output hermes
never reads back) and `hermes sessions export` output.

## Variants and versions

| variant | producers | notes |
|---|---|---|
| `state-db` (import, export) | v2026.3.12 (schema 4) … v0.22.x (schema 31) | version from `schema_version` → `agent.source.version = schema-v<N>`. Columns were added over time (reasoning columns at 3.28, `cwd` at 6.5, `system_prompts` dedup at 8.3, `message_uid` at v0.21.6); the importer reads whatever columns exist |
| `session-json` (import) | ≤ v2026.5.16 | pretty JSON (`indent=2, ensure_ascii=False`), whole file kept in `native.raw` |
| `gateway-jsonl` (import) | ≤ v2026.5.16 | one message per line, Python separators, first line `session_meta` |

Detection: SQLite magic plus `sessions` and `messages` tables; a `.json` with
`session_id` and a `messages` array; a `.jsonl` whose first record has `role`
and no `type`.

## Mapping

| hermes | protoledger |
|---|---|
| `sessions` row / legacy header | `settings{model, provider: billing_provider, cwd, git_branch, effort: model_config.reasoning_effort, system_prompt (column, or system_prompts by hash), tools}` + `title` + `usage{cumulative:true, input, output, cache_read, cache_write, reasoning, cost_usd: actual ‖ estimated}` (the usage part sits after the messages) |
| `parent_session_id` with `_delegate_from` / `source` subagent or tool | `agent.parent{relation: subagent}` |
| `parent_session_id` with `_branched_from` | `agent.parent{relation: fork}` |
| `parent_session_id` otherwise (compression, reset) | `continued-from` link; the parent gets `continued-in` |
| user / system row | `text` (string content) or `text` + `media` parts (`\x00json:` content: `text`, `image_url` data URLs → `media{media_type, data}`, other URLs → `media{url}`) |
| user row with `_compressed_summary = 1` | `compaction{phase: summary, trigger: auto, summary}` |
| assistant row | `reasoning{text: reasoning ‖ reasoning_content, summary, sealed}` (`reasoning_details` `reasoning.text` signature → `sealed{kind: signature}`, `reasoning.encrypted` / `codex_reasoning_items` → `sealed{kind: encrypted}`, the vendor from the item's `format` (`anthropic-claude-v1` → `anthropic`, `openai-*` → `openai`, `google-*` → `google`, SPEC §3.1.5), `reasoning.summary` items → `summary`; export writes them back the same way, summaries as `reasoning.summary` items, folded into `reasoning` only in a schema without `reasoning_details`), `text`, one `tool_call` per `tool_calls` entry (`arguments` parsed to `input`, or `input_text` when not a JSON object), `usage{output: token_count}`; `group` = row id |
| tool row | `tool_result{call_id: tool_call_id, output}` |
| `session_meta` (gateway) | `settings{model, tools}` |
| row with `active = 0` (rewound / compacted away) | same kinds, `audience: record` |

Event ids: the session id for the session row (`<id>#n` parts); the integer
row id for a message row; `<session>/<index>` for legacy JSON messages;
`@<byte offset>` for gateway lines.

## Self trip

- Legacy files: exact (whole-file `native.raw` / per-line raw).
- An **edited** `session-json` ledger (a record that no longer says what the
  file's `raw` says) is not written back as its session file: it goes to
  `state.db` like a foreign ledger, its edit included. Deliberate: hermes'
  session-file writer was removed in v2026.5.20 and `--resume` reads only
  `state.db`, so a session someone changed is written where hermes can resume
  it; the unchanged file is still written back verbatim (the self trip).
  `state.db` rows are rendered from the events' neutral fields, so an edit
  made to a legacy record alone (its neutral `body` left as it was) reaches
  `state.db` only as far as the neutral fields say it. A `gateway-jsonl`
  ledger keeps its file either way: each line is written from its record's
  own text, edited or not (no whole-file text exists to tell an edit by).
  Pinned by `hermes::tests::an_edited_session_file_is_exported_to_state_db_with_its_edit`.
- `state.db`: **not byte-exact** (`expect.json` `self_exact:false`). Export
  rebuilds a database from the captured schema text (`sqlite_master` in
  order, minus what SQLite makes itself), every row of every other table and
  `sqlite_sequence` (all kept on the first session's ledger in
  `agent.source.extra.store`), and the `sessions` / `messages` rows from
  `native.record` (column order kept, REAL values keep their decimal point,
  BLOBs as `{"$blob": base64}`). Verified with `sqlite3` on both fixtures:
  identical schema text, identical rows in every table, identical FTS5 index
  contents (rebuilt by hermes' own triggers). Session rows can come back in a
  different rowid order.

## Export of foreign ledgers

Always to `state.db` (the legacy files are no longer read by hermes): the
embedded v31 schema plus `schema_version = 31`, one `sessions` row per
ledger with an id in hermes' own pattern `YYYYMMDD_HHMMSS_<hex6>`
(`is_known_session_id` accepts it), `source = cli` (or `subagent` with
`model_config._delegate_from`), and `messages` rows in hermes' shapes:
assistant parts sharing a `group` become one row, `tool_calls` and JSON
columns are written with Python's `json.dumps` defaults, multimodal user
content with the `\x00json:` prefix, a leading system text becomes the
session's system prompt (deduplicated into `system_prompts` by SHA-256),
`message_uid` is minted deterministically. The first settings' `model`,
`provider` (→ `billing_provider`, SPEC §3.1.3), `effort`, `cwd`, `git_branch`
and system prompt fill the session row. Foreign keys are off while writing.

`sessions.title` is UNIQUE (`idx_sessions_title_unique`). A title already
held by a session written earlier in the same export (omp's subagent repeats
its parent's title, `omp/subagent-branches`) is renamed the way hermes renames
a taken title, `hermes_state_titles.next_title_in_lineage` (its auto-titler
and `hermes sessions retitle` fall back to it on the collision): strip a
` #N` suffix, then `<base> #<highest + 1>`, the unnumbered original counting
as #1, so the second holder becomes `<title> #2`, the third `<title> #3`,
and a taken `<title> #2` becomes `<title> #<highest + 1>`. The number is an
integer of any size, as in hermes (`#009` counts as 9, and a number past
any fixed width is still numbered one higher). Sessions are
written in ledger-id order, so the holder with the smallest ledger id keeps
the bare title and the rest are numbered in id order: the choice is
deterministic (the same set always exports the same titles), and it is not
by time, so omp's parent is not guaranteed to be the one that keeps it. Each
renamed session is one `title-renamed`. A carried trip restores the original
title on both paths: a rendered session carries its title event with its
lead, and a hermes session row read from a store, when renamed, carries its
own session events in `model_config` (a row that is not renamed is written
as read, with no carrier). Without carry the number stays. Pinned by
`hermes::tests::taken_titles_are_numbered_from_the_highest_holder`,
`hermes::tests::a_title_numbered_past_any_fixed_width_is_numbered_one_higher`,
`hermes::tests::omp_subagent_branches_exports_with_duplicate_titles_numbered`,
`hermes::tests::a_renamed_hermes_session_is_restored_by_a_carried_trip`
and, for a renamed row that cannot carry (`carry-unavailable`),
`hermes::tests::a_renamed_hermes_row_that_cannot_carry_reports_carry_unavailable`.

**Verified by running hermes:** exports of the droid `subagent-task` and
openclaw `tool-calls-thinking` fixtures were opened with hermes-agent
774a49c5's own `SessionDB`; `resolve_resume_session_id`,
`get_resume_conversations` and `export_session` returned the expected role
sequences, titles and the subagent's parent, and the subagent stayed out of
the root listing as hermes' delegates do.

| loss code | when |
|---|---|
| `assistant-text-joined` | several assistant text parts become one `content` string |
| `system-text-dropped` | system/developer text after the first (hermes keeps one prompt per session) |
| `media-dropped` | assistant media; media without data or URL; images in tool results |
| `reasoning-sealed-dropped` | a schema without `reasoning_details` |
| `compaction-boundary-dropped` | a compaction boundary without a summary |
| `peer-message-fields-dropped` | peer messages are written as SPEC §3.1.6 wrapped text (user when incoming, assistant when outgoing), which the importer turns back into a `peer_message`; this code names the fields beyond direction, peer, network and text that the wrapper cannot hold (user rows when incoming, assistant rows when outgoing) |
| `subagent-marker-dropped`, `error-dropped`, `file-change-dropped`, `app-dropped`, `foreign-native-dropped` | no hermes row exists |
| `title-renamed` | a title another exported session already holds, numbered `<title> #N` |
| `carry-unavailable` | carry requested into a schema without `display_metadata` (v4-era stores); a renamed hermes session row whose schema has no `model_config` or whose `model_config` is not a JSON object |


## Branches (SPEC §2.3)

hermes keeps a rewound row with `active = 0`, which the importer reads as `audience: record` (no longer in context); it has no way to say where the conversation continued from, so the branch itself cannot be expressed. Decision: **not expressible** — the exporter renders the active
path, names `abandoned-branch-not-rendered`, and carries the abandoned events on
the next row. Done for every foreign ledger (always written to `state-db`). The comparator now reads `after` (`<kind>.branch` costs what dropping the event costs, `<kind>.after` a wrong predecessor), so writing the abandoned branch as the conversation is no longer free: `protoledger/branch-rewind` via hermes was 9 with the branch written inline (where the comparator could not see it) and is 18 with the active path rendered, its 4 abandoned events (13 facts) missing; written inline it would score 23 under the new comparator. With carry the trip still loses nothing (carried score 0).

## Carry

Yes, in two JSON columns hermes reads by key (`json_extract` of named keys
only), under a `protoledger` key:

- `messages.display_metadata` holds the events a rendered row stands for; the
  hash covers the row's content columns (`role, content, tool_call_id,
  tool_calls, tool_name, timestamp, token_count, finish_reason, reasoning*,
  active, _compressed_summary`), so a rewind (`active = 0`) or an edit
  invalidates it.
- `sessions.model_config` holds the agent header and the events before the
  first row.

Unverified: whether a hermes rewrite path (compression, `/branch` copies)
copies `display_metadata` along; a copy would be imported fresh, since the
copied row lives in another session.

## Unverified

- Writing `state.db` while hermes runs (it uses WAL and leases): don't.
- Whether every v4–v30 build tolerates rows written with the v31 column set
  (hermes upgrades older stores on open by adding columns).
- `session_<id>.json` messages with per-message timestamps (agentsview's
  fixtures have them; the writer dumped whatever the in-memory dicts held).

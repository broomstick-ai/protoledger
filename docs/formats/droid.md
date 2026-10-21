# droid (Factory Droid)

Adapter: `src/formats/droid.rs` (JSONL plumbing shared with openclaw in
`src/formats/openclaw/jsonl.rs`). Format id: `droid`.

## Storage

```
~/.factory/sessions/<cwd-slug>/<session uuid>.jsonl
~/.factory/sessions/<cwd-slug>/<session uuid>.settings.json       pretty JSON, 2-space indent
~/.factory/sessions/<cwd-slug>/<session uuid>.settings.json.bak   droid's copy of the previous settings
```

The slug is the cwd with `/` and `\` turned into `-` and a Windows drive colon
dropped (`/home/user/p` → `-home-user-p`, `C:\Users\u\p` → `-C-Users-u-p`). It
is lossy; the real cwd is always read from `session_start.cwd`. Import accepts
a single `.jsonl` (no sidecars) or a directory (every `.jsonl` below it, with
its sidecars).

## Variants and versions

The file records only a format version (`session_start.version`, `2` in
every file seen from January to October 2026). The droid CLI version is not
recorded anywhere, so `agent.source.version` is `session-v2`.

| variant | producers | shape |
|---|---|---|
| `session-v2-linear` (import) | droid ≤ ~0.13x, Jan–May 2026 | messages have no `parentId`; `sessionTitle` on `session_start`; `tool_result` may lack `is_error` |
| `session-v2` (import, export) | droid ~0.14x–0.24x, Factory Desktop 0.168 | `parentId` tree, `agent_turn_outcome`, `compaction_state` + `compactionSummaryId`, hook audit rows, forks (`parent`, `forkedAtMessageId`), subagents (`callingSessionId`, `callingToolUseId`), OpenAI / chat-completion reasoning fields |

Detection: a first JSON line with `type:"session_start"` and an `id`. The
variant is chosen by whether any message carries `parentId`. Field order is
not stable across droid builds (`parentId` before or after `message`), which
is why every record is kept verbatim.

## Mapping

| droid record | protoledger |
|---|---|
| `session_start` | `settings{cwd}` + `title{text: sessionTitle ‖ title}`; `callingSessionId` → `agent.parent{relation: subagent, event: the parent's Task tool_call}`; `parent` → `forked-from` link, and `agent.parent{relation: fork}` when the parent session is in the set |
| `message` user `text` / `image` | `text{role:user}` / `media{role:user, media_type, data}` |
| `message` with `visibility:"user_only"` | same kinds, `audience: record` |
| `message` with `visibility:"llm_only"` (injected `<system-reminder>`s) | `text{role:user}` (the model saw it) |
| hook audit rows (`content: []` + `hook*`) | `native` |
| assistant `text` / `thinking` / `tool_use` | `text{role:assistant}` / `reasoning{text, sealed}` (SPEC §3.1.5: `signatureProvider: generic-chat-completion-api` → no seal, its `signature` only names the field the reasoning came in; `openai` → the signature is the Responses API reasoning item as JSON, `sealed{vendor: openai, kind: encrypted, data: encrypted_content}`; otherwise `sealed{vendor: signatureProvider, kind: signature}`) / `tool_call{call_id, name, input, tool_type: function ‖ mcp + server for mcp__srv__tool}`; `group` = message id |
| assistant `modelId` / `reasoningEffort` | `settings{model, effort}` emitted when they change (part 0 of that message) |
| user `tool_result` | `tool_result{call_id, output (string, or text/media parts), is_error}` |
| `agent_turn_outcome` | `turn{phase:end, outcome: ok ‖ interrupted ‖ error, reason}`, `turn` = `turnId`; no id, so anchored `@<byte offset>` |
| `compaction_state` | `compaction{phase:summary, trigger:auto, summary: summaryText, tokens_after: summaryTokens}` |
| `todo_state`, unknown types | `native` |
| `<id>.settings.json` | `settings{model, effort, permission_mode: autonomyMode, provider: providerLock}` + `usage{…, cumulative:true}` from `tokenUsage`, placed after `session_start` |
| `<id>.settings.json.bak` | `native` (verbatim only) |
| a line that is not JSON (torn tail) | `native` with `native.raw` |

Event ids are droid's record ids (`<id>#n` for parts); records without an
id use `@<byte offset>`. droid can write two different records under one id
(seen in real data); the second gets a `~2` suffix, and the exporter counts
parts per occurrence, not per id.

## Self trip

Exact for every fixture. Each line is kept in `native.record`, plus
`native.raw` when compact `serde_json` would not reproduce it (re-serialised
files, torn lines); sidecars are kept as `native.raw`. A missing final
newline is recorded in `agent.source.extra.final_newline`.

## Export of foreign ledgers

Writes `<slug>/<sid>.jsonl` (`<sid>.jsonl` when no cwd is known) in the
`session-v2` shape: `session_start` (`owner: "protoledger"`, `version: 2`),
then `message` records with UUID v5 ids and a `parentId` chain, and a
`.settings.json` when model, provider (→ `providerLock`, SPEC §3.1.3) or usage facts exist. Assistant parts that share
a `group` become one message; consecutive tool results share one user
message, as droid writes them.

**Branches (SPEC §2.3): expressed.** The `parentId` chain is linear except
where an event's `after` names an earlier event than the last message (a
rewind): that message's `parentId` is the message the named event went into,
and the messages between stay in the file as a branch of the tree, which the
importer reads back as `after`. Nothing is dropped, so there is no
`abandoned-branch-not-rendered` loss. Unverified: that droid itself resumes
along `parentId` from the last message rather than reading every message in
file order (no droid rewind has been seen in a real session). Pinned by
`droid::tests::a_rewound_ledger_is_written_as_a_branch_of_the_parent_tree`.

| loss code | when |
|---|---|
| `system-text-as-context` | system/developer text becomes an `llm_only` user message |
| `media-dropped` | media that is not an inline image in a user message |
| `reasoning-sealed-dropped` | encrypted reasoning other than OpenAI's (signatures go in `signature`; OpenAI encrypted content is written as a `{"type":"reasoning","content":[],"encrypted_content":…}` item with `signatureProvider: openai`, without the item's `id`, which the ledger does not keep) |
| `tool-input-text-wrapped` | raw or non-object tool input becomes `{"input": …}` |
| `usage-aggregated` | per-message usage is summed into `settings.json` `tokenUsage` |
| `compaction-boundary-dropped` | compaction boundary without a summary |
| `peer-message-fields-dropped` | peer messages are written as SPEC §3.1.6 wrapped text (user when incoming, assistant when outgoing), which the importer turns back into a `peer_message`; this code names the fields beyond direction, peer, network and text that the wrapper cannot hold |
| `subagent-marker-dropped`, `error-dropped`, `file-change-dropped`, `app-dropped`, `foreign-native-dropped` | no droid record exists |

Validity: verified only structurally (the records follow the shapes of the
real captures in `fixtures/droid/`); no droid binary was run on an export.
droid rejects a file without `session_start` as its first line, which the
exporter always writes.

## Carry

Yes. Every rendered record gets a top-level `protoledger` carrier holding the
events it stands for (plus events with no droid rendering, which ride on the
next record); the synthesised `session_start` carries the agent header. A
foreign `settings.json` gets an empty carrier, so a restoring import adds
nothing for it. Unverified: whether droid preserves unknown top-level keys
when it rewrites a session (it appends, so existing lines are kept).

## Unverified

- The Anthropic-route `thinking` signature format (no real example seen).
- `todo_state.todos` as an array (only anonymised fixtures show it).
- Whether a compaction always starts a new session file.
- Slug handling of dots, spaces and case.

## Prefix stability (SPEC §9.3)

Prefix-stable (`tests/prefix.rs`, 170 (file, k) pairs, 0 unstable). Two fixes were needed. The variant (`session-v2` or `session-v2-linear`) is decided by whether any message carries a `parentId`, so it can depend on a later line: it is kept on the agent header only, and no native record carries it. A file with nothing in it yet is not imported (it would name an agent after the file, not the session).

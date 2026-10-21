# Claude Code

`claude-code` reads and writes the transcripts Claude Code keeps for every
session, including subagents, persisted tool outputs and the older layouts.
It is a primary format, with weight 3 in the cross-format matrix.

- **Self trip:** every fixture comes back byte for byte. The importer takes
  a session file, a project directory or the whole `projects/` root, in
  every layout under Variants, and the exporter writes each line it imported
  back unchanged.
- **Foreign export:** loaded by a real `claude --resume` (2.1.280), carrier
  included. See [Verified](#verified).
- **Carry:** supported. The carrier sits under the top-level key `protoledger`
  on any line.

## Where Claude Code stores data

```
~/.claude/projects/<slug>/                 slug = cwd with every non-alphanumeric char → "-"
  <session>.jsonl                          one JSON object per line, appended by Claude Code
  <session>/subagents/agent-<id>.jsonl     subagent transcripts (2.1.3+)
  <session>/subagents/agent-<id>.meta.json {agentType, description, toolUseId, spawnDepth, parentAgentId?, ...} (≈2.1.30+)
  <session>/tool-results/<name>            large tool outputs (2.1.x; named by tool_use id in 2.1.11)
  agent-<id>.jsonl                         subagent / "Warmup" transcripts, flat beside the sessions (2.0.30 – 2.1.2)
```

The importer accepts a session file (its `<session>/` sidecar directory comes
with it), a project directory, or the whole `projects/` root. The exporter
writes paths relative to the project directory. Place them in
`~/.claude/projects/<slug_for(cwd)>/`. `claude_code::slug_for` computes the
slug. On macOS, use the real path, for example `/private/tmp/...`.

## Variants

The variant is detected from the layout and the line content, not from the
file name. When a file shows no layout evidence of its own, the producer
`version` decides. Each event's `native.version` holds the version of its own
line, because a resumed session can mix versions.

| variant | versions | shape | import | export |
|---|---|---|---|---|
| `jsonl-sidecar` | ≥ 2.1.3 (fixtures 2.1.11 – 2.1.280) | subagents in `<session>/subagents/`, meta files, `tool-results/` | ✓ | ✓ (foreign ledgers are always written in this shape) |
| `jsonl-flat-agents` | 2.0.30 – 2.1.2 (fixtures 2.0.70, 2.0.76, 2.1.1) | `agent-<id>.jsonl` beside the sessions, lines carry `agentId`; `summary` title lines | ✓ | ✓ (native records go back to their own files) |
| `jsonl-inline-sidechain` | < 2.0.30 (fixtures 1.0.69 – 1.0.83, 2.0.22) | sidechain lines inline in the session file: `isSidechain: true`, no `agentId` | ✓ | ✓ (sidechain ledgers are re-interleaved at their original line positions) |

The cut-over versions are approximate. They are bracketed by the fixtures
(2.0.22 is inline, 2.0.70 is flat, 2.1.1 is flat, 2.1.3 is sidecar), not
taken from release notes.

## Ledgers

| native | ledger |
|---|---|
| `<session>.jsonl` | Root ledger. `agent.id` and `source.native_id` are the session id. `source.extra.file` holds the relative path. `no_final_newline: true` is set when the file did not end in `\n`. |
| `subagents/agent-<id>.jsonl` or flat `agent-<id>.jsonl` | Ledger `agent-<id>`. `parent = {agent, event, relation: subagent}`, where `event` is the spawning `Task`/`Agent` `tool_call` event, found by the meta file's `toolUseId` or the parent's `toolUseResult.agentId`. "Warmup" agents have no spawning call, so their `event` is absent. `source.extra.meta` holds the meta file (plus `meta_raw` if it does not re-serialise). `source.extra.session` holds the session id. |
| inline sidechain | Ledger `sidechain-<root uuid>` with `source.extra.inline = {file, lines: [line indexes]}`. Its lines are removed from the root ledger and put back at those indexes on export. |
| `--fork-session` copy | A second root whose first conversation line also opens an older root gets a `forked-from` link (`event` = the last shared line). Claude writes no explicit marker: the copy repeats the uuids. |
| `continued-in` line | `continued-in` link to `continuedInSessionId`. |

## Events

Each line is one native record. Its id is the line's `uuid`, or `@<byte
offset>` for side-state lines that have none. Part *n* of a line is
`<uuid>#<n>`. Part 0 carries the whole line in `native.record`, and also
`native.raw` when `serde_json` would not reproduce the bytes. A line that is
not JSON is a `native` event with only `raw`, and a non-UTF-8 line is kept as
`{"base64": …}`. Repeated uuids get `~2` suffixes. `ts` is the line's
`timestamp`. `group` is `message.id`. A `timestamp` of exactly `1970-01-01T00:00:00.000Z`
is what the exporter writes when the time is unknown, and reads as unknown.

`turn` is the `promptId` of the latest line that has one. Claude Code stamps
it on the prompt and on everything that follows in that turn. `after` marks a
rewind (SPEC §2.3) and nothing else: it is set on a line whose `parentUuid`
goes back past conversation lines that are not part of its own conversation,
and names the last event of the line it continues from. A line's conversation
is its `parentUuid` chain plus the sibling blocks of each response on it and
their tool results, as Claude Code loads it, so parallel tool results (each
pointing at its own `tool_use` line), progress lines, and a parent reached past
bookkeeping lines only (an `attachment`) are no branch. A
`parentUuid` naming a line outside the ledger (a resumed or continued session)
is kept as `after` verbatim and starts no branch.

| line | events (in order) |
|---|---|
| any uuid line whose `cwd` / `gitBranch` / `permissionMode` / assistant `message.model` changed | `settings` (`cwd`, `git_branch`, `permission_mode`, `model`), first, and only on change. An empty `gitBranch` (no repository) is no fact until it replaces a known branch. |
| `user`, string content `<peer-message direction="in" …>\n…\n</peer-message>` (exactly SPEC §3.1.6) | `peer_message` (`in`, `peer`, `network`, `text`) |
| `user`, string content `[tool result <id>]\n<output>` | `tool_result` (`call_id` unless empty, `output`): how the exporter writes a result whose call is not in the history |
| `user`, string content | `text` role `user`; role `system` when `isMeta` (harness-injected, but the model sees it); audience `record` when `isVisibleInTranscriptOnly` |
| `user` blocks | `text` per text block; `media` per `image` / `document` (base64, url or text source); `tool_result` (`call_id`, `output` as a string or `{type:text}` / `{type:media}` parts, `is_error`); `native` for unknown blocks |
| `user` with `isCompactSummary` | `compaction` phase `summary` (audience model, because it *is* sent; in 2.1.x it also carries `isVisibleInTranscriptOnly`, which governs the UI, not the model) |
| `user` `tool_result` of a subagent call | then `subagent` phase `end` (`outcome` = `toolUseResult.status`, `result` = its text), unless `status` is `async_launched` |
| `user` `<task-notification>` | then `subagent` phase `end` for the async agent named by `<task-id>` |
| `user` whose tool output was persisted | then one `file_change` (op `create`, `path` as written, `content` = the file's full text) with its own native record: `native.id` and `native.file` = `<session>/tool-results/<name>`, `raw` = the file. The `tool_result` keeps what the model saw (the `<persisted-output>` preview). The file is found by `toolUseResult.persistedOutputPath` or by the "Full output saved to:" line (2.1.11). |
| `assistant` blocks | `text` role `assistant`; `reasoning` from `thinking` (`text`, `sealed {vendor: anthropic, kind: signature}`) or `redacted_thinking` (`redacted`, `sealed … kind: encrypted`); `tool_call` from `tool_use` (`tool_type` `function`, or `mcp` + `server` for `mcp__<server>__…`, or `custom` with `input_text` when the input is exactly `{"input_text": <string>}`), `server_tool_use` (`web_search` / `server`), `mcp_tool_use`; `tool_result` from `*_tool_result` blocks; `native` for anything else (e.g. `fallback`) |
| `assistant` spawning a subagent | after its `tool_call`: `subagent` phase `start` (`agent`, `call_id`, `agent_type`, `description`) when a meta file names the call; in layouts without meta files the spawning `tool_result` (`toolUseResult.agentId`) names the agent, and the `start` follows that result instead (SPEC §9.3: no line is read with what a later line says) |
| `assistant`, the line that settles its `message.id` | then `usage` (`model`, `input`, `output`, `cache_read`, `cache_write`, `reasoning` = thinking tokens), id `<message.id>#usage`, once per message. Claude writes one line per content block, each repeating the message's usage as it then stood, so early lines can hold partial streaming counts. The import is prefix-stable (SPEC §9.3), so a line cannot wait for the last one: a line holding only reasoning whose `stop_reason` is unset, `end_turn` or `tool_use` (what Claude Code writes on a thinking line that a reply follows: measured over every fixture, it writes the message's final stop reason on every line from 2.1.2xx) or a line with `stop_reason: tool_use` and no tool call is known not to be the last, and the first other line of the message that carries usage settles it. A line holding only reasoning with any other stop reason (`max_tokens`: the message ended on its reasoning) settles it too. That is the line with the final counts whenever Claude Code wrote them on a line it knew ended the message (2.1.2xx: the line with `stop_reason`); in older versions a text block followed by a tool call keeps the text line's counts. All-zero usage is skipped (the next line may settle). |
| `assistant` `isApiErrorMessage` | `error` (`message`, `kind` = `error`, `status` = `apiErrorStatus`) |
| `assistant` model `<synthetic>` | its text, audience `record` (filler Claude Code writes itself and never sends) |
| `system/compact_boundary` | `compaction` phase `boundary` (`trigger`, `tokens_before` = `preTokens`, `tokens_after` = `postTokens`, `preserved` = `preservedMessages.uuids`) |
| `system/turn_duration` | `turn` phase `end`, `duration_ms` |
| `system/api_error`; `system/informational` with level `error` | `error` |
| `attachment` with `rendered` content | `text` role `system`, audience model: the system-reminder text the model was shown |
| `attachment` `queued_command` | `text` (role `user`, or `system` if `isMeta`); `peer_message` (`in`, `peer`) when `origin.kind` is `peer` |
| `attachment` `prompt_snapshot` | `settings` `system_prompt` (+ `tools` names), only when it changed |
| `summary` (1.x – 2.1.x), `ai-title`, `custom-title` | `title` |
| `permission-mode` | `settings` `permission_mode` when it changed |
| `file-history-snapshot` / `file-history-delta` | `file_change` op `snapshot` (`snapshot`) / op `edit` (`path` = `trackingPath`) |
| `cost-state` | `usage` with `cumulative: true` (`cost_usd`, token sums over `modelUsage`, `reasoning` = `thinkingTokens`; a count no model states is left out) |
| `queue-operation`, `last-prompt`, `mode`, `atis-latch`, `agent-name`, `bridge-session`, `fork-context-ref`, `progress`, other `system` subtypes, other attachments, unknown types | `native` |

## Export

A record whose part 0 is present is written back unchanged, whether as raw
text, the record, or a sidecar file. Its remaining parts are covered by it.
Every other event is rendered from its neutral fields into the 2.1.280 file
dialect, with keys in the order Claude Code writes them:

- **Branches (SPEC §2.3).** A rewind Claude Code itself wrote (a native line
  whose `parentUuid` goes back) is written back whole: its abandoned lines are
  part of Claude Code's own `parentUuid` tree, off the active chain, and the
  session comes back byte for byte. A foreign branched ledger renders only its
  active path; its abandoned events are not rendered
  (`abandoned-branch-not-rendered`) and, with carry, ride on the next line.
- **Chain.** Each conversation line's `parentUuid` is the previous
  conversation line, native lines included, so events appended after a native
  thread continue it. A compaction boundary has `parentUuid: null` and
  `logicalParentUuid` set to the old leaf.
- **Identity.** `sessionId` is the ledger id if it is a UUID, otherwise
  `uuid_for(id)`. A line's uuid is `uuid_for("<session>[/<agentId>]/<event id>")`.
  `promptId` is `uuid_for("prompt:…/<turn>")`. `message.id` is
  `msg_<short_id>` and `requestId` is `req_<short_id>`. All of these are
  seeded by the session id, not the ledger id, so P → C → P → C is stable
  even though the ledger id becomes the session id.
- **Environment.** `cwd`, `gitBranch`, `permissionMode` and `message.model`
  come from `settings` events (the model falls back to the first `usage`
  model). Their initial values come only from the events before the first
  native record. Nothing is invented: an unknown `cwd` or model is left out
  of the line, `gitBranch` is `""`, and an unknown time is the epoch
  (Claude Code will not load a line without a `timestamp`). `version` is
  `--target-version`, else the last native line's, else `2.1.280`.
  `entrypoint` is `cli` and `userType` is `external`.
- **Messages.** Consecutive assistant-side events (text, reasoning,
  tool_call) of one `group`, or with no group, share a `message.id`, one line
  per block, as Claude writes them. A later event with the same `group`
  rejoins its message. A `usage` event sets `message.usage` on the
  message's last line before it that is not reasoning alone, the line the
  importer settles it on; the other lines keep zero usage. A message of
  reasoning alone has only reasoning lines, so its usage goes on the last
  one, which is written with `stop_reason: max_tokens` so that the importer
  settles the usage there without reading ahead (SPEC §9.3). The
  ledger holds no stop reason: `max_tokens` says only that the message ended
  on its reasoning, and the importer reads nothing else from it. The export
  names it as the loss `stop-reason-synthesised`.
- **Tools.** Ids that do not match `^[a-zA-Z0-9_-]{1,64}$` become
  `toolu_<short_id>`, and the result uses the same mapping. Names are
  sanitised the same way; an MCP call (`tool_type: mcp` + `server`) takes
  Claude Code's own `mcp__<server>__<name>`. The Messages API only takes
  object inputs, so a free-text (`custom`) call's `input_text` is written as
  `{"input_text": …}` and read back as a custom call; arguments given as a
  JSON string become the object they encode; any other non-object input
  becomes `{"input": …}` (and stays a JSON tool, as droid's `{"input": …}`
  ApplyPatch does). A `tool_result` carries `sourceToolAssistantUUID`. A
  result whose call is not in the ledger becomes the user text
  `[tool result <id>]\n<output>`, because the API refuses orphan results, and
  is read back as that result.
- **Reasoning.** Anthropic-signed reasoning is a `thinking` block with its
  signature; an Anthropic seal over readable text is a signature too (redacted
  thinking has no text). Any other reasoning with text is a `thinking` block
  with an empty `signature`. `claude --resume` (2.1.280) accepts that, also in
  the middle of a tool loop, and the model does not see it, exactly as it
  does not see its own earlier thinking. Reasoning with no text (only a
  foreign seal or a summary) has no block.
- **Peer messages.** An incoming `peer_message` is the SPEC §3.1.6 user text
  `<peer-message direction="in" peer="…" network="…">\n…\n</peer-message>`,
  which the model sees as it would through a peer channel. Claude Code's own
  peer channel (`queued_command` with `origin.kind: peer`) carries a socket
  address the ledger does not have.
- **Roles.** `system` and `developer` text become `isMeta` user lines, which
  the model sees. They re-import as role `system` (see Remaining loss).
- **Kinds with their own lines.** Compaction becomes `system/compact_boundary`,
  and the summary becomes a user line with `isCompactSummary` and
  `isVisibleInTranscriptOnly`. A turn end with `duration_ms` becomes
  `system/turn_duration`. `title` becomes an `ai-title` line. `error` becomes
  `system/informational` with level `error`.
- **Subagents.** A child ledger (`parent.relation: subagent`) becomes
  `<session>/subagents/agent-a<16 hex>.jsonl`, with `isSidechain: true` and
  `agentId` on every line and the session's `sessionId`. Its
  `agent-….meta.json` has `agentType` / `description` from the parent's
  `subagent` start event, `toolUseId` = the spawning call's tool id, and
  `spawnDepth`. A `fork` child, or a ledger with only a link, becomes a
  session of its own.

### Carry

With carry on, every rendered line holds its events under `protoledger`
(`carry::attach`, no nesting). Events with no line of their own (settings,
usage, turn starts, subagent markers, reasoning that cannot be sent, other
harnesses' natives) ride on the next rendered line.

If the next line is a native one, they stay on the foreign line just before
it, which is where they were when that line was last. If there is no line
after them, they ride on the last line. If there is no line at all, a
`{"type":"protoledger-carry"}` line is written. On import, a line whose hash
still matches gets its carried events back verbatim, in place, and the
importer's settings, chain and tool-call state are updated from it. A line
edited in Claude Code is imported fresh as a new Claude Code event.

## Loss codes

On import, everything is kept natively. These codes flag things that have no
neutral form:

| code | meaning |
|---|---|
| `unparsable-line` | A line that is not JSON (a torn write, junk). It is kept byte for byte in `native.raw`. |
| `block-native-only` | A content block type with no neutral meaning (for example `fallback`). It becomes a `native` part. |
| `sidecar-file-native-only` | A file in the session directory that no line can refer to. It is kept with `native.file`, in a `claude-code-files` ledger of its own (at the end of a transcript's ledger, later lines would go before it). |
| `tool-result-file-unreferenced` | A persisted tool output (`<session>/tool-results/…`) that no line refers to yet. Claude Code writes the file before the line that refers to it, and that line takes it in; it is not read before then. |

On export of foreign events, these are lost without carry. With carry, they
are carried, and come back on re-import:

| code | meaning |
|---|---|
| `reasoning-sealed-dropped` | The seal of reasoning sealed by another vendor (OpenAI encrypted content). Its text, if any, is kept as unsigned thinking. |
| `reasoning-summary-dropped` | Reasoning summaries (Codex). A thinking block has no place for them. |
| `reasoning-empty-dropped` | Unsealed reasoning with no text. |
| `reasoning-seal-kind-changed` | An Anthropic seal of kind `encrypted` over readable text is written as a signature. |
| `developer-as-meta-user` | Developer text becomes an `isMeta` user line, and re-imports as role `system`. |
| `abandoned-branch-not-rendered` | Events on an abandoned branch (SPEC §2.3) of a branch Claude Code did not write itself. Only the active path is rendered. |
| `revert-not-located` | Never raised by this format (an opencode import code). |
| `record-text-not-rendered` | Text with audience `record`. Any line would show it to the model. |
| `empty-text-not-rendered` | Empty text. The API rejects empty text blocks. |
| `media-not-rendered` | Assistant media, media given by `path`, or an unsupported media type. |
| `tool-name-sanitised` / `tool-id-sanitised` / `tool-name-mcp-prefixed` | See Tools above. A sanitised id comes back as a different call id. |
| `tool-input-wrapped` / `tool-input-parsed` / `tool-input-text-as-custom` / `tool-input-dropped` | A non-object input becomes `{"input": …}`; a JSON-string input becomes its object; `input_text` on a call not typed `custom` comes back as `custom`; a call with both `input` and `input_text` keeps the text. |
| `orphan-tool-result-as-text` | A tool result with no tool call in the ledger. |
| `compaction-preserved-dropped` | The `preserved` id list is not rendered. |
| `turn-not-rendered` / `turn-outcome-dropped` | Turn starts, and turn ends without `duration_ms`. A `turn_duration` line has no outcome. |
| `usage-not-rendered` / `usage-cumulative-not-rendered` / `usage-cost-dropped` | Usage with no assistant message before it (or a second usage for one message), running totals, and per-message cost. |
| `stop-reason-synthesised` | A message of reasoning alone ends on a line written with `stop_reason: max_tokens` (see Messages above), a value the ledger did not hold. Raised with carry too, once per such line, and only where the exporter writes the value: a Claude Code line that states its own stop reason is written back as it was and raises nothing. Nothing new is lost (the ledger holds no stop reason); the code names the invented value. Its kind is `usage`, not `reasoning`: the value is written so that the importer settles the message's usage on that line. |
| `settings-not-rendered` | Any settings field except `cwd`, `git_branch`, `permission_mode` and `model`. |
| `peer-message-fields-dropped` / `peer-message-out-not-rendered` | Fields of an incoming peer message beyond `peer`, `network` and `text` (a seal, `trigger_turn`). Outgoing peer messages have no line. |
| `file-change-not-rendered` | Claude's file history needs its own backup files. |
| `app-not-rendered` / `foreign-native-not-rendered` | Application records and other harnesses' native-only events. |

Subagent events are not losses: they are structural, and the child is its own
file.

## Remaining loss through Claude Code

The neutral loss of X → protoledger → claude-code → protoledger without
carry, summed over every fixture of every other format (`no_carry_there
["claude-code"]` in `tests/reports/`), went from **4065** to **3430** on
branch `lossopt-cc`. The trips of Claude Code fixtures through the other
formats went down by 35 as well.

| category | before | after | why what is left is left |
|---|---:|---:|---|
| missing `reasoning` | 273 | 100 | No text: only an OpenAI seal or a Codex summary, which a thinking block cannot hold. |
| `reasoning.body.sealed` / `.summary` | 0 | 79 / 23 | The text now crosses as unsigned thinking; the foreign seal and summaries do not. |
| `usage.ts` | 267 | 269 | Usage is read off the line that settles the message, so it takes that line's time. |
| `usage.body.cost_usd` | 234 | 235 | Per-message usage has no cost field. |
| `usage.body.context_window` | 61 | 61 | No field. |
| `usage.body.model` | 81 | 82 | The source's usage had no model; the Claude message names it. A gained fact, scored as a change. |
| missing `usage` | 121 | 120 | Running totals (see below) and a second usage for one message (Codex writes two). |
| extra / missing `settings`; `settings.body.model`, `.cwd`, `.ts` | 229 / 123; 106, 83, 53 | 150 / 155; 74, 48, 54 | Settings have no line: they come back where a line states them, cwd on the first line, the model on the first assistant line. `provider` (72), `agent` (53), `effort` (21) have no field. |
| `settings.body.git_branch` | 133 | 0 | `""` is no branch. |
| missing / extra `tool_result` | 194 / 171 | 185 / 184 | Mostly order (≈150): Claude Code states a response's usage before its tool results, opencode and Codex after them. Also results with no or a sanitised call id. |
| `tool_call.body.tool_type` | 165 | 119 | 94 are the importer writing the default `function` where the source omitted it (see below); `shell` and `tool_search` have no Claude Code form. |
| `tool_call.body.input` / `.input_text` | 97 / 81 | 16 / 0 | Free-text calls now round trip; left are non-object inputs. |
| missing `turn`, `turn.body.outcome` | 157, 28 | 157, 28 | Claude Code marks turns only by `promptId` and `turn_duration`, which has no outcome. |
| `header.created_at`, `title.ts` | 126, 57 | 126, 57 | A session's time is its first line's; `ai-title` lines have no timestamp. |
| missing / extra `text` | 97 / 139 | 98 / 86 | `developer` text comes back as `system` (76): both are `isMeta` lines. Also agents paired wrongly by the comparator. |
| `text.group`, `text.body.phase`, `text.meta.*` | 65, 44, 80 | same | User lines have no message id; Codex phases and metadata have no field. |
| missing `peer_message` | 34 | 2 | Now the SPEC wrapper; outgoing messages have no line. |
| `subagent.*`, missing `subagent` | 45 + 24 + 20 + 18 | same | Subagent events are structural: they come back only for a child spawned by a tool call the meta file names, under Claude Code's `agent-a…` id. |
| missing `tool_call` / extra | 35 / 35 | same | Ids the API refuses (`""`, ids with `.`/`/`/newlines) are re-minted. |
| missing `file_change` | 31 | 31 | Claude Code's file history needs its own backup files. |

Tried and left out, because they moved positions that other adapters'
conventions depend on, so trips of Claude Code fixtures through them got
worse: importing `isMeta` as `developer`; omitting the default
`tool_type: function`; putting a response's usage after its tool results;
naming the session model on the first settings event; and writing running
totals as a `cost-state` line (Claude Code accepts one without its duration
fields, but the line shifts the comparator's alignment).

## Verified

- **Self trip.** Every fixture in `fixtures/claude-code/` (27 cases) round
  trips byte-exact, and its re-import is clean (`FIXTURE=claude-code cargo test
  --test fixtures`). What the adapter supports is a session file, a project
  directory or the whole `projects/` root, in any layout under Variants; the
  fixtures are what measures it.
- **Resume of a foreign export, carry on.** Run with Claude Code 2.1.280 on
  2026-10-16, haiku.
  1. A hand-built Codex-shaped ledger was exported and written to
     `~/.claude/projects/-private-tmp-pl-cc-resume/`. It held a developer
     message, the user message "My favourite animal is the axolotl. Please
     list the files here.", OpenAI-sealed reasoning, a `shell` tool call and
     its result, an assistant answer, usage, a turn end and a title.
     `claude -p --resume <id>` was then run from `/private/tmp/pl-cc-resume`.
     Asked what the earlier message said, it answered "Your favorite animal is
     the axolotl, … listed notes.txt and README.md". It named the tool "Bash",
     not "shell"; whether Claude Code renames unknown tools or the model
     guessed is unverified.
  2. A second ledger set added a subagent child ledger (exported as
     `subagents/agent-….jsonl` + `.meta.json`) and a compaction (boundary plus
     a summary that alone mentions the colour teal), followed by a further
     turn. Resumed, Claude answered "Your favourite colour is teal, your
     favourite animal is the axolotl, a helper agent counted 3 files, and the
     last thing you said before this message was "Thanks!"". So it loaded
     from the compact boundary on, and it accepted the carrier key on every
     line.
  3. **Take-back.** The session file Claude appended to was imported again.
     The Codex events came back exactly, with their Codex natives, followed by
     Claude's new lines. Re-exporting the set reproduced Claude's files byte
     for byte (3/3).

- **Resume of what the no-carry export now writes.** Claude Code 2.1.280,
  2026-10-16, haiku and sonnet, hand-written sessions in a scratch project:
  an assistant `thinking` block with `"signature": ""` (after a completed
  turn, and between a `tool_use` and its pending result) resumes, and the
  model does not see its text; lines with no `cwd`, an assistant message
  with no `model`, a `compact_boundary` with no `preTokens`, a
  `<peer-message>` user line and a bare `cost-state` line all resume, and
  the model read the peer message. A session whose lines have no
  `timestamp` is not found by `--resume`.

## Unverified

- The exact versions where the layout changed (see Variants).
- Whether `claude --resume` loads an exported transcript that contains:
  - `redacted_thinking`, or signed thinking blocks carried over from another
    Claude session;
  - image or document blocks;
  - a tool_use with no tool_result;
  - a `protoledger-carry` line (written only for a ledger with no renderable
    event).

  How Claude Code treats a line type it does not know was not tested.
- Nested foreign subagents (`parentAgentId`). They are written, but not
  resumed.
- 1.x sessions with real inline Task sidechains. The 1.x fixture holds only
  summary, synthetic and meta lines; the inline sidechain fixture is a
  2.0.22 Warmup agent.

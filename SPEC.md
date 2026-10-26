# protoledger 0.1: specification

protoledger is a harness-neutral record of what happened in a coding-agent
thread. It is designed for one job: holding a thread so that it can be
**projected into any agent harness at any time** (Claude Code, Codex, opencode,
and others), used there, and **taken back** without duplicates and with as
little loss as the target allows.

The key words MUST, SHOULD and MAY are used in their RFC 2119 sense.

## 1. Principles

1. **One file per agent.** Every agent (a top-level thread, a harness-native
   subagent, a forked branch) is its own file (a compressed ledger in a store:
   its own pair of files, §10). Files reference each other; nothing nests.
   Deleting a thread is deleting its files.
2. **Append-only.** An event, once written, MUST NOT be changed or removed while
   its ledger exists. What is immutable is the ledger's **decompressed byte
   stream** (§10.1): every byte of every line, and so every `seq` and every
   offset. The bytes on disk are an encoding of that stream. A compressed
   ledger's encoding MAY be rewritten (§10), but only after the new encoding's
   decompressed stream has been checked byte-identical to the one it replaces,
   and only so that a crash at any point leaves the whole stream readable
   (§10.6). New facts are new events. A ledger MAY be
   deleted whole. Redaction (§11.1) is a new fact too: a redacted event stays
   in the ledger, and only what is projected from the ledger leaves it out.
3. **Neutral first, native always.** Every event carries a harness-neutral
   `kind` and `body` that any exporter can render. When the event was imported
   from a harness, it ALSO carries the harness's original record in `native`,
   so exporting back to that same harness is exact.
4. **Loss is named.** An importer or exporter that cannot represent something
   says so in a loss report. Silent loss is a bug.
5. **Identity is stable.** Importing the same native data twice yields the same
   event ids. Exporting and re-importing yields the same event ids. This is what
   makes a round trip duplicate-free.

## 2. Files

A **ledger** is one UTF-8 JSONL byte stream, stored as one file,
conventionally `<agent_id>.jsonl`, or compressed in a store (§10); the stream
is the same either way. A
**ledger set** is a directory of ledgers that reference each other (one thread
and its subagents, or a whole store). Line 1 is the header; every later line is
one event. Lines end in `\n`. A reader MUST ignore a final line that has no
trailing newline (a torn append) and MAY report it.

Object keys in protoledger's own bodies, meta and headers are written in
sorted order; that is the canonical form. Numbers in protoledger's own bodies,
meta and headers are written in serde_json's shortest round-trip spelling;
native records keep their exact text (§4).

### 2.1 Header (line 1)

```json
{"protoledger":"0.1","agent":{
  "id":"<agent id>",
  "created_at":"<RFC 3339>",
  "source":{"format":"claude-code","variant":"jsonl","version":"2.1.234","native_id":"<session uuid>"},
  "parent":{"agent":"<agent id>","event":"<event id>","relation":"subagent"},
  "links":[{"rel":"spawned-by","agent":"<agent id>","event":"<event id>"}],
  "meta":{"<namespace>":{}}
}}
```

| Field | Rule |
|---|---|
| `protoledger` | The spec version the file was written to. |
| `agent.id` | Unique within a ledger set. Importers SHOULD use the harness's own id. |
| `agent.created_at` | When the agent began, if known. |
| `agent.source` | Where the ledger was imported from. Absent for a ledger authored natively. |
| `agent.parent` | **Structural** parent: present only when this agent is a subagent the *harness itself* ran (Claude Code's Task tool, Codex `thread_spawn`, opencode's `subagent` tool). An exporter renders it into that harness's own subagent structure. `relation` is `subagent` or `fork`. The parent file MUST exist before the child names it, so edges point backwards and cycles cannot form. |
| `agent.links` | **Non-structural** references: `spawned-by` (an orchestrator outside the harness started this agent as a peer), `forked-from`, `continued-from`, `continued-in`, `member-of` (a messaging network). Exporters never turn a link into a native subagent. |
| `agent.meta` | Namespaced metadata (§5). |

**Native subagents versus orchestrated agents.** A daemon that runs its own
agents (for example one that disables the harness's subagent tool and spawns
peers through its own tool) records them as **root** ledgers with a
`spawned-by` link and its own metadata. They are not `parent` children,
because the harness did not run them, and rendering them as native subagents
would misrepresent what happened.

### 2.2 Event (every later line)

```json
{"id":"<event id>","seq":7,"ts":"<RFC 3339>","kind":"tool_call",
 "turn":"<turn id>","group":"<message id>","after":"<event id>",
 "audience":"model",
 "body":{},
 "meta":{"<namespace>":{}},
 "native":{"format":"claude-code","version":"2.1.234","id":"<native id>","part":0,"record":{}}}
```

| Field | Rule |
|---|---|
| `id` | Unique within the ledger, stable (§1.5). Importers derive it from the native id: the native id itself when one native record makes one event, `<native id>#<n>` for part *n* of a record that makes several, or a location anchor (`@<byte offset>`) when the source has no ids. |
| `seq` | 1, 2, 3… in file order. A position, not an identity. |
| `ts` | When it happened, if known. |
| `kind` | One of §3. Unknown kinds MUST be preserved by readers and treated as `native`. |
| `turn` | The turn this event belongs to, if the source has turns. |
| `group` | Events that came from one model message share a group (Claude `message.id`, an opencode message id, a Responses API response). Exporters use it to re-assemble multi-part messages. |
| `after` | Causal predecessor, only when it is not simply the previous event (tree-shaped sources such as Claude Code's `parentUuid`). |
| `audience` | `model` if the model saw this content; `record` if it is attached for people and programs only. Defaults are per kind (§3). |
| `body` | Kind-specific neutral fields. Unknown fields MUST be preserved. |
| `meta` | Namespaced metadata (§5). |
| `native` | The source record (§4). |

### 2.3 Branches

A ledger is append-only, but a conversation can go back: Claude Code's
`/rewind`, Codex's rollback, opencode's revert. Going back is recorded by
appending, never by removing:

- An event whose `after` names an earlier event continues the conversation from
  that event. Everything between that event and the new one is an **abandoned
  branch**: it stays in the file, and it is no longer part of the conversation.
- The **active path** is found by starting at the last event and following
  `after` (or, where `after` is absent, the previous event) back to the start.
  Readers that only care about the current conversation use the active path;
  readers that care about history read the file.
- Exporters render the active path. An exporter whose target can express
  branches (Claude Code's `parentUuid` tree) MAY render abandoned branches too.
- What an exporter renders is the **projected view** (§11.1): the active path
  (and any abandoned branch it renders) minus the redacted events, honouring
  unredaction, the latest fact winning, and minus the §11 facts.
- A **store container** holds no conversation: a ledger with at least one
  event, every one a `native` or `app` record, that another ledger of the set
  names with a `member-of` link (a store's session-less rows, a workspace's
  directory files). An export without carry (§6) to a format other than the
  container's own and `protoledger` leaves it out, naming the loss
  `container-not-rendered`, rather than rendering an empty conversation. A
  ledger with one event of any other kind is a conversation and is rendered.

## 3. Kinds

Content kinds default to `audience: model`; the rest default to `record`.

| kind | body | default audience |
|---|---|---|
| `text` | `role` (`user`\|`assistant`\|`system`\|`developer`), `text` | model |
| `media` | `role`, `media_type`, and one of `data` (base64), `url`, `path`; optional `name` | model |
| `reasoning` | `text`, `summary` (array of strings), `redacted` (bool), `sealed` `{vendor, kind: signature\|encrypted, data}` | model |
| `tool_call` | `call_id`, `name`, `input` (JSON), `input_text` (when the tool takes raw text, e.g. a patch), `tool_type` (`function`\|`custom`\|`shell`\|`web_search`\|`mcp`\|…), `server` | model |
| `tool_result` | `call_id`, `output` (string, or array of `{type:"text",text}` / `{type:"media",…}` parts), `is_error`, `status` | model |
| `compaction` | `phase` (`boundary`\|`summary`), `trigger` (`auto`\|`manual`), `summary`, `tokens_before`, `tokens_after`, `preserved` (event ids kept verbatim after it) | boundary: record; summary: model |
| `turn` | `phase` (`start`\|`end`), `outcome` (`ok`\|`error`\|`interrupted`\|`aborted`), `reason`, `duration_ms` | record |
| `usage` | `model`, `input`, `output`, `cache_read`, `cache_write`, `reasoning`, `cost_usd`, `cumulative` (bool) | record |
| `settings` | A snapshot of what governs the model from here on: `model`, `provider`, `cwd`, `git_branch`, `effort`, `permission_mode`, `approval`, `sandbox`, `system_prompt`, `tools`, `agent`. `complete: true` means absent fields are unset; otherwise absent fields are unchanged. | record |
| `title` | `text` | record |
| `subagent` | `phase` (`start`\|`end`), `agent` (child agent id), `call_id`, `agent_type`, `description`, `outcome`, `result` | record |
| `peer_message` | `direction` (`in`\|`out`), `peer` (agent id), `network`, `text` | model |
| `file_change` | `path`, `op` (`create`\|`edit`\|`delete`\|`snapshot`), `diff`, `content`, `snapshot` | record |
| `error` | `kind`, `message`, `retryable`, `status` | record |
| `app` | `type` (`<namespace>/<name>`), `data` | record |
| `native` | `{}`, or `{"deleted": true}` for a tombstone (§4.1). A source record with no neutral meaning; it exists only in `native`. | record |

### 3.1 Neutral conventions

Every adapter imports to, and exports from, these conventions, so that the same
fact has one neutral spelling whichever harness it came from.

1. **Usage.** One `usage` event per model response, immediately after that
   response's own events (before any tool result answering it), sharing its `group`. `input` counts input tokens **excluding** cache
   reads and writes, which are `cache_read` and `cache_write`; `output` counts
   all output tokens **including** reasoning, of which `reasoning` is the
   subset. (Anthropic reports input this way; an OpenAI importer subtracts
   `cached_input_tokens` from `input_tokens`.) Running totals a source reports
   (a Claude Code `result` line, Codex `token_count` totals) are imported as
   `usage` with `cumulative: true`; exporters never render a cumulative event
   as a per-response one. `cost_usd` appears only when the source states it.
2. **Tool types.** `function` (the default, and may be omitted), `shell` (runs a
   command line), `custom` (takes free text, kept in `input_text`), `mcp` (with
   `server`), `web_search`, `tool_search`. Tool names are kept verbatim.
3. **Settings.** A `settings` event appears when something changes, carrying the
   new values of the fields that changed; `complete: true` only when the source
   states the whole configuration. Exporters render the fields the target can
   express and name the rest as a loss.
4. **Groups.** `group` is the provider's response or message id when known.
5. **Reasoning.** `text` is the full reasoning when the source has it;
   `summary` holds summary texts (Codex). `sealed.vendor` is `anthropic` for a
   signature, `openai` for encrypted content.
6. **Peer messages in a harness without a peer channel.** An exporter renders an
   incoming `peer_message` as user text wrapped exactly as
   `<peer-message direction="in" peer="ID" network="NET">\nTEXT\n</peer-message>`
   (attributes omitted when absent), and an importer that finds that wrapper
   restores the `peer_message`. The model sees the same message either way.
7. **Times** are RFC 3339 in UTC.

A **harness-specific record becomes neutral** when two or more harnesses have
an equivalent, or a consumer needs it. Otherwise it stays `native`, and only
an exporter for its own format can render it.

## 4. Native records

```json
"native":{"format":"codex","variant":"rollout-legacy","version":"0.147.0",
          "id":"<native id>","part":0,"record":{},"raw":"…","file":"<relative path>",
          "altered":true}
```

- One native record can produce several events (a Claude Code line with two
  content blocks, an opencode message with three parts). The **first** of them
  carries `record`; the rest carry the same `id` and their `part` index only.
- `record` is the record as JSON. An importer SHOULD write it exactly as the
  source spelled it (key order, number spelling, escapes), and a reader MUST
  keep it byte for byte rather than re-serialise it. `raw` is the exact
  source text, and is used only when the format's own serialiser does not
  round-trip through JSON (§7.1). `raw` is emitted only while it matches
  `record`; an editor need not remove it.
- `altered: true` marks a `record` that is no longer the source's: it was
  replaced by a scrubbed copy (for example to redact a secret), and `raw`, if
  any, was dropped with it. Exporters and comparators MUST NOT treat an altered
  record as byte-exact. Absent means `false`.
- `file` names the source file within the source set, when a format spreads one
  agent over several files (opencode 1.x, Claude Code `tool-results/`).
- An exporter for the same `format` that finds every part of a native record
  intact (none of them redacted, §11.1) MUST emit `record` (or `raw`)
  unchanged. That is what makes X → P → X exact.

### 4.1 Versions of one native record

Some harnesses rewrite a record in place as it progresses (an opencode tool part
goes `running` → `completed`). A ledger takes in each **fact** the moment it is
settled and never waits for, or records, the transient states in between:

- A `tool_call` is appended when its input is final; its `tool_result` when the
  call has finished. Text and reasoning are appended when the part has ended.
  Partial input, progress metadata and growing text are transient: they may be
  streamed to live subscribers (§9.4), and are never written to a ledger.
- When a later event comes from a newer version of a native record that an
  earlier event already carries, the later event carries the newer `record` with
  the same `native.id` and `part: 0`. **Exporters use the newest version of each
  native record.** The older version stays where it was; nothing is rewritten.
- A harness deleting a native record it owns (a row it removes from its store)
  is a newer version of that record too: a `native` event whose body is
  `{"deleted": true}` and whose `native` names the record (`format`,
  `variant`, `id`, `file`) and carries no `record`, a **tombstone**. Readers
  and exporters take the newest version, so they leave out a record whose
  newest version is a tombstone, and a later version brings it back.

## 5. Metadata namespaces

`meta` on a header or an event holds data that is **attached**, not shown to
the model: an application's bookkeeping, a daemon's routing, the raw text of a
message before the daemon wrapped it. Keys are namespaces, conventionally a
reverse-DNS or product name (`example.orchestrator`, `example.app`). Values are opaque to
protoledger. Readers MUST preserve them.

What the model was actually shown belongs in `body`. When an orchestrator
wraps a message (adds a timestamp line, headers, an injected preamble), the
`text` event's `body.text` is the wrapped text, and the unwrapped original MAY
be kept in `meta`.

## 6. Identity across round trips

- **Import** derives event ids from native ids (§2.2), so re-importing the same
  file yields the same ids.
- **Export** to a format that needs ids mints them deterministically from the
  event id (UUID v5 under the protoledger namespace
  `6f1f0d3c-7b0e-5a3e-9a43-70726f746f6c`), so re-exporting yields the same
  native ids.
- **Carry.** Where a target format tolerates an extra field on its records, the
  exporter MAY write a `protoledger` carrier field holding the source events of
  that record. On re-import, if the record's neutral content still matches the
  carried events, the importer restores the carried events exactly, including
  their original `native`. If the record was edited in the harness, it is
  imported fresh as a new event. Carry is what makes X → P → Y → P → X exact for
  the parts Y cannot express.
- **Header carry.** When the target cannot express an agent's header (its
  original id, links, metadata), the exporter adds `"agent": <header>` to the
  carrier on that agent's first record, and the importer restores the header
  from it.

## 7. Conformance

### 7.1 Round trips

For every supported format X and fixture F:

1. **Self round trip.** `export_X(import_X(F))` MUST equal F byte for byte, or
   the difference MUST be listed as a known loss for that format variant.
2. **Cross round trip.** For every target Y: `import_Y(export_Y(import_X(F)))`
   is compared with `import_X(F)`. With carry enabled the difference SHOULD be
   empty. With carry disabled, the difference is that pair's **neutral loss**,
   which the test suite records and does not allow to grow.
3. **Return trip.** `export_X(import_Y(export_Y(import_X(F))))` SHOULD equal F.

### 7.2 Loss reports

Every import and export returns a list of losses:
`{code, kind?, count, detail}`. `code` is a stable kebab-case string such as
`reasoning-sealed-dropped` or `subagent-flattened`.

## 8. Versioning

The header's `protoledger` field is the spec version. A minor version adds
optional fields or kinds; a reader for 0.1 MUST accept a 0.2 file and preserve
what it does not understand. Each format adapter names the native **variants**
and **producer versions** it supports (see `docs/formats/`).

## 9. Stores, bindings and sync

A **store** is a directory holding many ledgers and an index over them:

```
<store>/ledgers/<agent>.jsonl       the truth: one ledger per agent (§2), plain
<store>/ledgers/<agent>.jsonl.zst   or compressed: its sealed blocks (§10.2)
<store>/ledgers/<agent>.jsonl.tail    and its hot tail (§10.2)
<store>/dicts/<id>.zdict            compression dictionaries (§10.7)
<store>/store.json                  store configuration (§10.10)
<store>/index.db                    derived: deletable and rebuildable at any time
```

### 9.1 The index

`index.db` (SQLite) holds, per agent: path, size, modified time (of each of
its files, for a compressed ledger), last seq and byte length (of its
decompressed stream), header fields (created_at, source, parent, links, title); per event:
`(agent, seq) → (byte offset, length, id, kind, ts, group, turn)`, the offset
being in the ledger's decompressed stream (§10.5); per sealed block and per
dictionary, the rows of §10.8; and every
binding (§9.2) with its sync cursor. Everything in it is either read from the
ledgers (and the store's dictionaries) or is a cursor whose loss costs only
a rescan. **Rebuilding the index
from the ledgers MUST reproduce every answer the index gives**, except cursor
positions.

### 9.2 Bindings

A **binding** ties a ledger set to a native location in some harness's store,
and is followed by sync:

- a **projection**: the ledger was exported there (for a harness to resume it),
  and what the harness appends there comes back;
- an **attachment**: the native session came first; it was imported, and what
  is appended there keeps coming in.

A binding is recorded **in the ledger itself**, as `app` events of type
`protoledger/bound` (`{binding, format, variant, location, native_id, mode}`)
and `protoledger/unbound` (`{binding}`), so the index can rebuild the list of
bindings from the ledgers. Bindings are only ever created explicitly; nothing
scans a machine for sessions.

### 9.3 Sync

Sync reads what a bound location gained and appends to the ledger the events
whose ids the ledger does not yet hold. Because importers derive ids from native
ids (§2.2) and restore carried events (§6), reading a location again never
duplicates anything, and protoledger's own exported events come back as
themselves. Sync follows §2.3 for rewinds and §4.1 for records a harness rewrites
in place. A location whose earlier content changed in a way that is neither an
append, a rewind nor a newer version of a settled record is **diverged**: sync
appends an `app` event `protoledger/diverged` describing it, stops following that
binding, and changes nothing else. A location protoledger re-projected
(§11.2) is measured against the binding's `protoledger/projected` facts as
§11.2 says: the bytes protoledger wrote itself are never divergence.

**An importer is prefix-stable.** Importing any prefix of a native source that
ends on a record boundary yields exactly the first events of importing the
whole source (same ids, same bodies, same native records): nothing an importer
emits depends on records not yet written. A fact that only a later record
completes is emitted where it is settled, under an id that does not depend on
which record was read last. This is what lets sync read a location while the
harness is still writing it.

### 9.4 Events out

Every append to a store's ledger, by a program or by sync, is published to
subscribers as `{agent, seq, offset, event}`, `offset` being in the ledger's
decompressed stream (§10.5). A subscriber resumes from
`(agent, seq)`: ledgers are append-only, so that is the whole cursor. Transient
states (§4.1) may be published as `{agent, transient: {...}}` messages that are
never written and cannot be resumed.

### 9.5 Garbage collection

An event is never changed or removed while its ledger exists (§1.2), but a whole
ledger may be deleted. Garbage collection deletes ledgers (all of a ledger's
files, §10.11) chosen by an
explicit policy (age, size, archived), removes their index rows and bindings, and
removes their projected native files only when asked to. It never deletes a
dictionary (§10.7). A compressed ledger's *size* is the length of its
decompressed stream, so a policy selects the same ledgers whether or not they
are compressed. A ledger's *age* is
measured from the `ts` of its newest event, which travels with the ledger and
survives a copy of the store; a ledger with no timestamped event is aged by its
header's `created_at`, then by its file's modification time (the newest of
its files', for a compressed ledger), and a dry run
names, per ledger, the clock it used. A ledger is
*archived* when its newest `app` event of type `protoledger/archived` or
`protoledger/unarchived` is `protoledger/archived`. Removing a ledger also
removes its structural children (agents whose `parent` names it: the harness's
own subagents and forks, §2.1), never the reverse, and never agents merely
linked to it: a `spawned-by` peer, or any other `links` entry, is a separate
thread and stays.

## 10. Compression

A store MAY keep its ledgers compressed. Compression changes how a ledger is
stored, never what it is: §1–§9 hold for every ledger, compressed or not,
measured on its decompressed stream. Compression is off by default, and a
compressed ledger uses no dictionary unless one is configured or named (§10.7).

### 10.1 The decompressed stream

A ledger's **decompressed stream** is the byte sequence a plain ledger file
holds (§2): the header line and every event line, each ending in `\n`, and no
torn final line. It is the immutable record (§1.2). Every offset in the index
(§9.1), in a published event (§9.4) and in a read (§10.5) is an offset in this
stream, so an event's offset is the same number whether its ledger is plain,
compressed, or recompressed.

`protoledger cat <store> <agent>` MUST write an agent's decompressed stream to
standard output exactly, in either form.

### 10.2 Files

A compressed ledger is two files in `<store>/ledgers/`:

```
<agent>.jsonl.zst    the block file: an append-only sequence of sealed blocks
<agent>.jsonl.tail   the hot tail: plain JSONL, the lines not yet sealed
```

The stream is the concatenation of the blocks' decompressed bytes, in file
order, followed by the hot tail. Either file MAY be absent, meaning empty: a new
ledger has only a hot tail, holding its header; a ledger just migrated has no
hot tail until a writer opens it. A ledger's form is told by which files exist,
never by configuration: a store MAY hold plain and compressed ledgers side by
side. A **temporary file** is the unfinished output of a rewrite (§10.9,
§10.10), and is named exactly `<agent>.jsonl.zst.tmp-<pid>` or
`<agent>.jsonl.tmp-<pid>`, where `<pid>` is one or more decimal digits that
end the name. A name ending `.jsonl`, `.jsonl.zst` or `.jsonl.tail` is never a
temporary file. Readers and the index MUST ignore temporary files.

### 10.3 Blocks

A block is one **meta frame** followed immediately by one **zstd frame**. The
block file holds blocks and nothing else.

The meta frame is a zstd skippable frame (RFC 8878 §3.1.2), which every zstd
decoder skips: the 4-byte magic number `0x184D2A5C` (little-endian, bytes
`5C 2A 4D 18`), a 4-byte little-endian payload length, then the payload, one
JSON object in the canonical form of §2 (sorted keys, no whitespace) with
exactly these fields:

| Field | Value |
|---|---|
| `block` | `1`, the meta frame's format version. A reader MUST refuse a block whose `block` it does not know, and MUST ignore fields it does not know. |
| `dict` | The id (§10.7) of the dictionary the zstd frame was compressed with, or `null` for none. |
| `first_seq` | The seq of the block's first line; `0` stands for the header line. |
| `last_seq` | The seq of the block's last line. |
| `generation` | `0` when the block file is first written; every block of a block file written by recompression carries the replaced file's highest `generation` plus one, and a block sealed onto a file carries that file's `generation`. |
| `len` | The block's length in the decompressed stream, in bytes. |
| `offset` | The decompressed offset at which the block starts. |
| `zlen` | The length in bytes of the zstd frame that follows. |

The zstd frame MUST be a single frame whose `Frame_Content_Size` is present and
equals `len`, with `Content_Checksum_flag` set. A reader selects the dictionary
by `dict`, never by the frame's own `Dictionary_ID` field.

A block is **whole** when: both frames lie within the file; the zstd frame
decodes, with the dictionary `dict` names, to exactly `len` bytes and its
checksum matches; those bytes are whole lines, the first of which has seq
`first_seq` (the header, when `0`) and the last `last_seq`; its `offset` is the
previous block's `offset + len` (`0` for the first block); and its `first_seq`
is the previous block's `last_seq + 1`. A block holds at least one line, and a
line is never split between blocks or between a block and the hot tail.

### 10.4 Appending and sealing

A ledger has one writer at a time, as a plain ledger does; for a compressed
ledger its exclusive lock is held on the hot tail, which no rewrite replaces.
A writer that has taken the lock MUST check that the ledger's form and files
are still the ones it opened, and re-open if not.

An append is one line written to the hot tail and fsynced, exactly as for a
plain ledger. **Sealing** moves the whole hot tail into a block:

1. Compress every line of the hot tail into one block, with the store's default
   dictionary (§10.10), and append the block to the block file in one write;
   fsync the block file (and, if it was created, its directory).
2. Read the block back from the file and check that it is whole and that its
   decompressed bytes equal the hot tail byte for byte. If not, truncate the
   block file back to its previous length, and keep the hot tail.
3. Truncate the hot tail to zero and fsync it.

A seal does not update the index's block rows; the index learns of a new
block when it next catches up (§9.1).

A writer MUST seal when, after an append, the hot tail holds at least
`seal_bytes` (§10.10), and MAY seal earlier (when idle, or on close). Nothing is
appended between steps 1 and 3. Sealing SHOULD use zstd level 3, which keeps the
append path fast; recompression (§10.9) SHOULD use level 19. The level is not
part of the format.

### 10.5 Reading: offsets, seek and tail

Reading at decompressed offset `o` finds the block with `offset ≤ o < offset +
len` (from the index, §10.8, or by walking the meta frames, which needs no
decompression), decodes it and slices the line; an `o` at or past the block
file's end `L` (the last block's `offset + len`, or `0`) is read from the hot
tail at physical offset `o − L`. A reader MUST take `L` from the block file
itself, opened by path, and MUST NOT take it from the index, which a seal does
not update (§10.4). Tailing from offset `o` yields every line from `o` on
across the boundary, in order: the rest of the blocks, then the hot tail.

A reader takes no lock, and MUST still get a consistent stream while a writer
seals and while a ledger is recompressed or migrated:

- A block file is never written in place: it is appended to (§10.4), its torn
  final block is cut (§10.6), or it is replaced whole by rename (§10.9). A
  reader holding it open keeps reading the inode it opened, which stays
  complete until it closes.
- A line read from the hot tail at `o − L` MUST be checked: it starts at a
  line boundary of the tail (physical offset `0`, or just after a `\n`); it is
  the header when `o = 0`, and otherwise its seq is greater than the last
  block's `last_seq` and, when the reader knows which seq it wants (from the
  index, or as the next line of a sequential read), equal to it; and, re-opened
  by path after the read, the block file still has the length and identity it
  had when `L` was read. The reader holds the descriptor it took `L` from open
  across the tail read (or also compares the last meta frame's `generation`),
  so the identity it compares cannot be a reused one. On any failure, or when
  the tail is shorter than `o − L` plus one line, it re-opens the block file by
  path. If the block file changed (it was sealed onto or replaced under the
  reader), the reader re-reads `L` and retries; the decompressed offset it
  wants never moves (§1.2). If the block file is unchanged, the reader does
  not retry: a tail shorter than `o − L` plus one line means the line is not
  written yet (the end of the stream), a leftover tail (§10.6) is skipped and
  the tail then read as empty, and a line that fails the check is an error
  naming the ledger.
- A reader that goes on from the block file into the hot tail MUST check that
  the tail continues the last line it read. A tail line it has already read (an
  unfinished seal or recompression, §10.6) is skipped. If the tail starts past
  the next line, **or is empty or ends before the reader expected**, the reader
  re-opens the block file by path; if it has grown or been replaced, the reader
  resumes from it at the same decompressed offset. The stream ends only where
  the hot tail ends and the block file, re-opened by path, is unchanged, so a
  one-shot read such as `protoledger cat` never ends short of what was durable
  when it began.
- A reader that follows a ledger as it grows (a tailer) MUST re-open by path
  when a file it holds is unlinked or the ledger's form changes (a migration,
  §10.10), and resume at the same decompressed offset in the new form.
- A reader that resolved a block's physical position from the index MUST check
  the meta frame it finds there (magic, `offset`, `generation`) and, on any
  mismatch, re-resolve from the file's meta frames. When the index has no row
  for the block holding `o < L` (a block sealed since the index last caught
  up), the reader walks the meta frames past the last block the index knows.
  Any whole block naming `offset` holds the same bytes of the stream,
  whichever file it is in.

### 10.6 Torn tails, interrupted writes and damage

A crash can leave a torn final line in the hot tail, a torn final block, or the
leftovers of an interrupted seal, recompression or migration. Each is detected
and repaired mechanically, and none loses a line, because the hot tail is
truncated only after the block holding its lines is durable and checked:

- **Torn final line** of the hot tail: as for a plain ledger (§2), a reader
  ignores it and the writer, on open, cuts the tail back to its last `\n`.
- **Torn final block**: a final block that is not whole (§10.3) while the hot
  tail still holds its lines (the tail is non-empty and its first line
  continues the last whole block, or is the header when there is none). A
  reader ignores it; the writer, on open, truncates the block file to the end of
  the last whole block.
- **Leftover tail**: a hot tail whose first line is already in the block file
  (it is the header and the block file is non-empty, or its seq is at most the
  last block's `last_seq`). A seal and a recompression both take the whole tail
  and append nothing before truncating it, so its whole content MUST equal the
  last bytes of the block file's stream; a reader skips it, and the writer, on
  open, checks that and truncates it to zero.
- **Both forms present** (an interrupted migration, §10.10): both MUST hold the
  same decompressed stream, since the migration checked that before the second
  form appeared. A reader reads the plain file; the writer, on open and before
  any append, checks that, keeps the form `store.json` names, and deletes the
  other.
- **Temporary files** (§10.2): the lock holder, on open, deletes its own
  ledger's temporary files, the two names of §10.2 for its `<agent>`, and no
  other file.

Anything else (a block that is not whole and is not the final one, a final
block that is not whole while the tail does not hold its lines, a leftover tail
that does not match, two forms that differ, a hot tail that does not continue
the block file, a dictionary that is missing or whose bytes do not match its
id) is **damage**. That includes a whole final block that a leftover tail
contradicts (a seal whose read-back check failed, then a crash before step 2
cut the block back): two whole copies of the same lines disagree and nothing on
disk records which one is true, so recovery is manual; `protoledger cat` fails
naming the ledger, and nothing is deleted. Damage is never repaired
automatically: a writer MUST refuse to open a damaged ledger for appending, and
a read that needs the damaged part MUST fail with an error naming the ledger and the reason, never skip it.

A block whose dictionary is missing or does not match its id is damage
wherever it is, the final block included: it is never a torn final block,
never cut, and never decoded without that dictionary. Whether a block is whole
(§10.3) is judged only with its dictionary, and since a dictionary is durable
before any block naming it (§10.7), no crash leaves one missing. Adding the
dictionary back (`store dict add`) repairs the ledger.

### 10.7 Dictionaries

A ledger is compressed with **no dictionary** unless the store is configured
with one or a recompression names one. Dictionaries are an option for users who
want a better ratio, at the cost of retraining (§10.9).

- A dictionary's **id** is the lowercase hex SHA-256 of its bytes. It is stored
  once per store, at `<store>/dicts/<id>.zdict`, written atomically. A reader
  that loads a dictionary MUST check that its bytes hash to its id.
- `store dict add <file>` adds a dictionary the user passes: any file zstd
  accepts as a dictionary (a trained one, or raw content). It MUST refuse an
  empty file or one zstd rejects. Adding an id already present changes nothing.
- `store dict train --size <bytes>` trains one from the store's own ledgers,
  sampling lines of their decompressed streams, newest first, and adds it as
  `add` does. `--size` defaults to 112640 bytes (110 KiB). It MUST fail, adding
  nothing, when the store holds too little to train on.
- Adding or training a dictionary changes nothing else: it becomes the default
  only through `store config --dictionary <id>`, and encodes existing ledgers
  only through `store recompress`.
- `store dict ls` lists every dictionary with its size, whether it is the
  default, and the ledgers and blocks that use it (§10.8).
- `store dict rm <id>` deletes a dictionary. A dictionary MUST NOT be deleted
  while any block in the store references it or while it is the default, and
  `store dict rm` (or any operation that would delete one) MUST refuse then.
  References are counted by walking the meta frames of every block file in the
  store, never from the index, which can lag behind a seal or a rename
  (§10.4, §10.9). A recompression or migration writes its blocks under a
  temporary name that this walk ignores, so a store has one lock,
  `<store>/dicts/.lock` (an advisory file lock): every seal, recompression
  and migration that writes blocks with a dictionary MUST hold it shared from
  just before it makes them part of the ledger (the write of §10.4 step 1, or
  the rename of §10.9 step 3 or §10.10 step 3) until that is durable, and
  first, holding it, check that `dicts/<id>.zdict` exists and matches its id
  and, if not, write it back durably from the bytes it compressed with.
  `store dict rm` holds the lock exclusively while it counts references and
  deletes, and refuses while it cannot take it. So a dictionary is durable
  before any block naming it is, and no crash leaves a block naming a deleted
  dictionary. Garbage collection never deletes a dictionary (§9.5).

A ledger whose blocks use a dictionary is not self-contained: copying it out of
the store needs that dictionary too, or `protoledger cat` (§10.1).

### 10.8 The index

The index (§9.1) holds a row per sealed block:
`(agent, offset, len, physical_offset, physical_len, dict, first_seq,
last_seq, generation)`, where `physical_offset` is the position of the block's
meta frame in the block file and `physical_len` the length of the two frames;
and a row per dictionary in `<store>/dicts/` with its size. Every column of a
block row is a meta-frame field or a position in the file, so the rows are
rebuilt from the meta frames alone. From them the index answers which
dictionary encoded each sealed block, and which dictionaries (with how many
blocks each) each ledger uses. `store reindex` MUST reproduce these rows, as it
reproduces every other answer (§9.1).

### 10.9 Recompression

`store recompress [--dictionary <id>] [agents…]` rewrites the named compressed
ledgers (every compressed ledger, if none is named), with the named dictionary
or, without `--dictionary`, the store's default. `--dictionary none` uses no
dictionary. A ledger another writer holds, and a plain ledger, are skipped and
reported. For each ledger, under its writer lock:

1. Write `<agent>.jsonl.zst.tmp-<pid>`: the whole decompressed stream, the hot
   tail's lines included, as blocks of about `recompress_bytes` (§10.10), with
   `generation` one more than the current file's; fsync it.
2. **Check**: read the new file back from disk, check that every block is whole
   and that their decompressed bytes equal the ledger's decompressed stream byte
   for byte. On any difference, delete the new file, leave the ledger as it
   was, and report the failure.
3. Rename the new file over the block file and fsync the directory.
4. Truncate the hot tail to zero and fsync it.
5. Update the ledger's block rows in the index.

A crash leaves the old block file or the new one, each whole, and at worst a
leftover tail (§10.6). Readers see a consistent stream throughout (§10.5).
Recompressing with a new dictionary is how a store **retrains** its ledgers.

### 10.10 Store configuration and migration

A store's configuration is `<store>/store.json`, written atomically, in the
canonical form of §2. It is not in `index.db`, which is derived. A missing file
or field means its default:

```json
{"compression":{"dictionary":null,"enabled":false,"recompress_bytes":1048576,"seal_bytes":262144}}
```

| Field | Meaning |
|---|---|
| `enabled` | New ledgers are created compressed; set by `store config --compression on\|off`. |
| `dictionary` | The default dictionary id for new blocks (§10.4, §10.9), or `null`; set by `store config --dictionary <id>` (`none` clears it). It MUST name a dictionary in the store. Changing it does not touch existing blocks. |
| `seal_bytes` | The hot tail's size at which a writer seals (§10.4); set by `store config --seal-bytes <n>`. |
| `recompress_bytes` | The target block size for recompression and migration. |

Readers never consult the configuration. A compressed ledger is sealed by these
rules whatever `enabled` says.

**Migration.** `store config --compression on` records the setting, then
migrates every plain ledger of the store to the compressed form; `off` migrates
every compressed ledger to the plain form. Each ledger migrates on its own,
under its writer lock:

1. Write the target form under its temporary name (§10.2:
   `<agent>.jsonl.zst.tmp-<pid>` for `on`, `<agent>.jsonl.tmp-<pid>` for
   `off`): for `on`, the plain file's decompressed stream as blocks, as
   recompression writes them (`generation` `0`); for `off`, the decompressed
   stream as one plain file. fsync it.
2. **Check**: read it back from disk and compare its decompressed stream with
   the source's, byte for byte. On any difference, delete it and leave the
   ledger as it was.
3. Rename it into place (`<agent>.jsonl.zst`, or `<agent>.jsonl`) and fsync
   the directory.
4. Delete the source form's files and fsync the directory.

A ledger another writer holds is skipped and reported; running the same command
again migrates what was skipped. A crash between steps 3 and 4 leaves both
forms, resolved as §10.6 says. Two migrations of one store at once are not
supported.

### 10.11 Sync, serve and garbage collection

- **Sync** (§9.3) appends through the store's writer, so a compressed ledger's
  new events go to its hot tail; what sync reads of a ledger it reads through
  the store (§10.5), never by path.
- **Events out and serve** (§9.4): new events are published from the append
  path. A subscriber's backlog, from an old `(agent, seq)`, is read through the
  store from the blocks and the tail.
- **Garbage collection** (§9.5) removes a compressed ledger by deleting its
  block file, its hot tail and its temporary files (§10.2), under its writer
  lock; its block rows go with its other index rows. Dictionaries stay (§10.7).
- **Redaction, re-projection and delivery** (§11) only append, through the
  store's writer; re-projection reads the ledgers it renders through the store.

## 11. Redaction, re-projection and message delivery

This section adds seven `app` event types (§3), the **§11 facts**:

| `type` | `data` |
|---|---|
| `protoledger/redacted` | `{events, reason}` |
| `protoledger/unredacted` | `{events}` |
| `protoledger/projected` | `{binding, bytes, sha256, through}` |
| `protoledger/message` | `{message_id, text, meta}` |
| `protoledger/delivered` | `{message_ids, turn?, event?}` |
| `protoledger/cancelled` | `{message_ids, reason}` |
| `protoledger/withdrawn` | `{message_ids, reason}` |

Every one of them is appended; none edits or removes anything (§1.2). Like
§1–§9, this section holds for a compressed ledger exactly as for a plain one
(§10). Every id a §11 fact names (in `events`, `message_ids`, `through`,
`event`) is an event id or message id **of the ledger the fact is in**.

**Writers and refusal.** A §11 fact is appended through the ledger's one
writer (§10.4, `Appender`), which checks it against the ledger as that writer
holds it, under the writer's lock, before appending it; so a check and its
append are one step, and two programs cannot interleave between them (a second
writer is refused the lock, as for any append). An append this section calls
**refused** appends nothing, not even the part of the fact that would have
passed, and fails with an error naming the id and the reason. "Latest" means
highest `seq`. A §11 fact takes effect wherever it stands in the file, on the
active path or on an abandoned branch (§2.3): the facts are read from the whole
ledger, as bindings are (§9.2). A reader that finds a §11 fact the writer
would have refused at its position (a ledger written by another program, or
a `protoledger` export that left out a redacted event the fact names, §11.1)
MUST ignore it when it derives redaction (§11.1) or delivery state (§11.3),
and MAY report it; the ledger stays readable.

**The §11 facts are protoledger metadata, never conversation.** No projected
view (§11.1) holds them, so no exporter renders or carries (§6) them, except
the `protoledger` format's own, which keeps them (§11.1). What the
model was shown of a delivered message is the `text` event that carried it
(§5), not its `message` event.

Retries, parks, budgets and account switches are an application's own
operational facts and have no §11 type; they belong in that application's
database, not in a ledger.

### 11.1 Redaction

`protoledger/redacted {events, reason}` marks the events whose ids `events`
lists as **redacted**; `reason` is a string saying why. It is how an event is
taken out of every projection of a ledger without editing the ledger. (This is
not the scrubbed native record of §4, `altered: true`, which replaces a record's
text before it is written, nor the `redacted` flag of a `reasoning` body, §3,
which says the harness withheld the text.)

- `events` MUST be a non-empty list. A redaction is refused when any id it
  lists is not an event the ledger holds, or names a `protoledger/redacted` or
  `protoledger/unredacted` event: a redaction is withdrawn by
  `protoledger/unredacted`, never by redacting it. Any other event MAY be
  redacted, including a model-visible one in the middle of a conversation, an
  event already redacted (which changes nothing), and a §11 fact (which also
  changes no projection, since none holds it).
- `protoledger/unredacted {events}` withdraws redactions, by appending. It is
  refused when `events` is empty or any id it lists is not an event the ledger
  holds; naming an event that is not redacted changes nothing.
- An event is **redacted** when the latest `protoledger/redacted` or
  `protoledger/unredacted` fact that lists it is a `protoledger/redacted`.
  Redact, unredact, redact leaves it redacted.
- A redacted event stays in the file and in every query (§9.1, `Store::events`,
  `protoledger cat`); its `seq`, offset and native record are unchanged.

The **projected view** of a ledger is its active path (§2.3) in file order,
minus every redacted event and minus every §11 fact. Every exporter MUST
render the projected view, for every format, `protoledger` included: a redacted
event, and its native record, appears in no projection and in no export,
rendered or carried. The `protoledger` format's exporter also writes every §11
fact, where it stands and whether redacted or not, since they are its own
records: it leaves out only the redacted events that are not §11 facts, so a
ledger with none exports as itself (§7.1). A ledger's
record itself is what its files and `protoledger cat` hold, and copying the
record is copying those, not exporting. Leaving out a redacted event, or a §11
fact from any other format, is not a loss (§7.2). An
exporter that may render abandoned branches (§2.3) leaves the redacted events
out of those as well.

The rest of the view stays a valid conversation:

- An event of the view whose `after` (or, where `after` is absent, whose
  previous event) is a redacted event continues, in the view, from the nearest
  earlier event of the view on its path. An exporter whose target records that
  reference (Claude Code's `parentUuid`) MUST write it re-pointed there; that is
  the one change it makes to a record whose parts are otherwise intact (§4).
- A native record any of whose parts is redacted is not intact (§4): the
  exporter MUST NOT emit that `record` or `raw`, which would show the redacted
  part, and renders the parts that remain from their neutral bodies.
- Of the versions of one native record (§4.1), an exporter uses the newest one
  carried by an event of the view; a version carried only by a redacted event
  is never used, and when every version is, the record is rendered from the
  remaining events' neutral bodies.
- A redaction lists exactly the events it removes; nothing is redacted with
  them implicitly. A caller that redacts one half of a pair (a `tool_call`
  without its `tool_result`) SHOULD redact both; an exporter whose target
  cannot hold the half left behind MAY leave it out too, naming that as a loss.

A redaction reaches a bound harness location only when that binding is next
re-projected (§11.2).

### 11.2 Re-projection

A projection binding (§9.2, `mode: projection`) MAY be re-projected at any
time, for example before every turn a harness takes. Re-projecting writes the
binding's location again from the projected view of the bound agent and of
the structural descendants its projection holds, and appends to the bound
agent's ledger `protoledger/projected {binding, bytes, sha256, through}`:

| Field | Value |
|---|---|
| `binding` | The binding id (§9.2). |
| `bytes` | The length in bytes of the render of the binding's location file, which is that file's whole content once the re-projection's rename is done (or at once, when the file already equalled it, step 3). |
| `sha256` | The lowercase hex SHA-256 of those bytes. |
| `through` | The id of the last event of the bound agent's projected view (§11.1) in file order: on a branched ledger, the last event of its active path that is neither redacted nor a §11 fact. `null` when the view holds no event. |

`protoledger reproject <binding>` does this. Re-projection is defined only
**between harness runs**: an application MUST NOT re-project a location while
a harness process that may write it is running, whether or not that process is
writing at the moment, because a harness that holds the file open (Codex holds
its rollout file) goes on appending to the file the rename replaced, and those
lines would reach neither the new file nor the ledger. protoledger cannot see
whether a process holds a file, so this binds the application; an orchestrator
re-projects before it starts each turn's harness process, after the previous
one has exited. Re-projection:

1. MUST refuse a binding that is not active (unbound, or stopped by
   divergence), a binding whose `mode` is `attachment` (protoledger never writes
   a location the harness owns), and a binding whose location is not a single
   file the harness only appends to (a SQLite store, a directory, a format
   whose harness rewrites records in place, §4.1). Claude Code's and Codex's
   transcript files qualify.
2. First syncs the binding (§9.3), so that nothing the harness appended is
   overwritten before the ledger holds it, and keeps the length and SHA-256 of
   the location file as that sync read it. If that sync finds the location
   diverged, re-projection MUST refuse and leave the location as it is.
3. Renders the projected view as the binding's format, and MUST check, before
   anything is written, that the render imports back as itself: only agents and
   event ids the store already holds (as projecting does, §9.2). If not, it
   refuses, writing nothing and appending nothing. When the render's bytes
   equal the location file's, it goes to step 6.
4. Writes the complete render to a temporary file beside the location file
   and fsyncs it. Then, immediately before step 5, it MUST check that the
   location file still has the length and SHA-256 kept at step 2; if it does
   not, the harness wrote to it since that sync, and re-projection deletes the
   temporary file and goes back to step 2 (or refuses, appending nothing and
   leaving the location as it is).
5. Appends the `protoledger/projected` fact step 6 owes, if any, **before** it
   renames the temporary file over the location file and fsyncs the
   directory, so that a reader sees the old file or the new one, never part
   of either, and the fact that names the new bytes is durable before they
   are in place. Files of the
   location other than its location file (a sidecar directory) are rewritten
   the same way.
6. MUST append a `protoledger/projected` fact when, and only when, its `bytes`,
   `sha256` or `through` differ from the binding's last one (or the binding has
   none): re-projecting a ledger whose projected view has not changed appends
   nothing, so there is one `protoledger/projected` fact per change. When the
   render equals the location file (step 3), nothing is written; a fact, if
   owed, is still appended.
7. Re-baselines the binding's sync cursor (§9.1) on what it wrote.

A temporary file a crash leaves behind is deleted by the next re-projection of
that binding; readers and sync ignore it.

**Sync after re-projection** (§9.3). Once a binding has a
`protoledger/projected` fact, sync measures the location file against the
latest of the binding's `protoledger/projected` facts that matches it:

- A fact **matches** when the file is at least `bytes` long and its first
  `bytes` bytes have SHA-256 `sha256`. Those bytes are protoledger's own: they
  are never divergence, whatever they hold or leave out compared with what the
  location held before. What follows them is read as an append, and its new
  events are ingested as §9.3 says.
- A match on a fact older than the latest one is how a crash is survived: a
  re-projection that appended its fact and stopped before its rename (step 5)
  left the old file in place, every byte of which is protoledger's or was
  ingested at step 2. The next re-projection writes the file the latest fact
  names (step 3 finds it differs from the file; no new fact is owed).
- Beyond the matching fact's `bytes`, §9.3 applies unchanged: an event that
  a sync matching this same fact read past its `bytes`, and that is now gone
  or no longer in the same order, makes the location diverged. (So a file put
  back to an older projection after a turn that a sync matching that
  projection read is diverged. An exact restore with no turn read after it is
  not diverged and loses nothing, since every byte is protoledger's render of
  the ledger; the next re-projection rewrites it.)
- If no fact matches (the file is shorter than each `bytes`, or its prefixes
  hash otherwise), something other than protoledger and the harness's appends
  changed it: the location is **diverged** (§9.3).

So that every projection binding has a fact to be measured against before its
first re-projection, projecting (§9.2) MUST append, with its
`protoledger/bound` fact, a `protoledger/projected` fact for the location file
it wrote. A binding made before this section has no such fact: until a sync has
matched one of its `protoledger/projected` facts, a file none of them matches
is measured as §9.3 says without this section, which accepts the file
`project` wrote left in place by a crash in its first re-projection.

Because the facts are in the ledger, this holds when the sync cursor is lost
(§9.1): the binding's `protoledger/projected` facts are how sync recognises
the bytes protoledger wrote. A diverged projection binding stays stopped:
re-projection refuses it (step 1), and the way out is `unbind` and a new
projection to a new location (§9.2); protoledger never overwrites a diverged
location, whose foreign content the ledger does not hold.

The harness's next events continue from `through`. Under §2.3 the events
between `through` and them (redacted events, §11 facts) then lie on an
abandoned branch; that changes nothing, because §11 facts take effect wherever
they stand.

### 11.3 Message delivery

A message an application queues for an agent, and what became of it, is
recorded in that agent's ledger by four facts:

- `protoledger/message {message_id, text, meta}`: one per message, appended
  when the message is queued and before it is delivered. `message_id` is a
  string the application chooses; `text` is the message as queued (what the
  model was shown is the `text` event that carried it, §5); `meta` is an object
  of namespaced metadata (§5), `{}` when there is none. A `protoledger/message`
  whose `message_id` the ledger already holds is refused.
- `protoledger/delivered {message_ids, turn?, event?}`: the listed messages
  were delivered to the model. `turn` is the id of the turn (§2.2) that carried
  them; `event` the id of the prompt event that carried them. `event` is not
  checked against the ledger, since the prompt may arrive later, by sync.
- `protoledger/cancelled {message_ids, reason}`: one attempt to deliver exactly
  the listed messages did not send them (an interrupt, a failed turn); `reason`
  is a string saying why. A cancelled message is **still owed**.
- `protoledger/withdrawn {message_ids, reason}`: the sender retracts the listed
  messages; `reason` is a string saying why. This is the only fact that makes a
  message no longer owed without delivering it.

A `protoledger/delivered`, `protoledger/cancelled` or `protoledger/withdrawn`
fact is refused when its `message_ids` is empty or lists an id twice, or when
any message it lists is not in a `protoledger/message` fact of the ledger, or
is in a state that fact may not leave (below).

Each message has a **delivery state**, derived from the facts that name it in
`seq` order and from nothing else:

| State | Means | owed | `delivered` | `cancelled` | `withdrawn` |
|---|---|---|---|---|---|
| `pending` | queued; no fact names it yet | yes | → `delivered` | → `cancelled` | → `withdrawn` |
| `cancelled` | its latest fact is a `protoledger/cancelled` | yes | → `delivered` | → `cancelled` (a retry can fail again) | → `withdrawn` |
| `delivered` | a `protoledger/delivered` lists it | no | refused | refused | refused |
| `withdrawn` | a `protoledger/withdrawn` lists it | no | refused | refused | refused |

A `protoledger/message` fact starts a message in `pending`. **Should send** is
every message queued and neither delivered nor withdrawn: the `pending` and the
`cancelled` ones. **Sent** is the `delivered` ones. protoledger never resends
anything: the state is derived, and the application acts on it, delivering an
owed message by a new `protoledger/delivered` fact.

A message's **history** is every `protoledger/delivered`,
`protoledger/cancelled` and `protoledger/withdrawn` fact that lists it, in
`seq` order; the state is a function of it. Nothing is rewritten and no
message is lost: a message is in exactly one state from the moment its
`protoledger/message` fact is appended, for as long as its ledger exists.

Redaction (§11.1) does not change a delivery state or a history: redacting a
`protoledger/message` fact, a `protoledger/delivered`, `protoledger/cancelled`
or `protoledger/withdrawn` fact, or the `text` event that carried a message,
keeps it out of projections (where §11 facts never are anyway), and the state
is still derived from every fact in the ledger.

# protoledger

**One record for every coding-agent thread, in any harness.**

Claude Code, Codex, opencode, Gemini CLI, Cursor and a dozen other agent
harnesses each store a conversation in their own private shape. Move a thread
between them and you lose things: the reasoning, the subagents, the tool
results, the compaction history, the ids that let you come back.

protoledger is a single, harness-neutral, append-only ledger for agent threads,
and the importers and exporters that move a thread **into and out of** every
one of those shapes, so a thread can be resumed in any harness and brought home
again without duplicates and with as little loss as the target allows.

```sh
# A Claude Code session, resumed in Codex, then brought back
protoledger convert claude-code codex ~/.claude/projects/-home-me-app/<session>.jsonl -o out/

# What would that trip lose? Measured, field by field
protoledger roundtrip claude-code <session>.jsonl --via codex

# A store: every thread as a file, plus an index you can delete and rebuild
protoledger store import ./store auto ./rollout-2026-10-16T10-00-00-<id>.jsonl
protoledger project ./store <agent> claude-code ~/.claude/projects/-home-me-app/
protoledger watch ./store            # what the harness appends lands in the ledger, once
```

A Rust crate and a CLI. No service required.

## What it does

- **17 formats, import and export, with version-specific variants.** Claude
  Code, Codex, opencode, Gemini CLI, Cursor, Copilot CLI, Droid, Hermes, Deep
  Agents, OpenClaw, OpenHands, pi, oh-my-pi, Letta Code, Harbor ATIF (v1.0–v1.8),
  Slack, and protoledger itself. Where a harness changed its storage between
  releases (opencode's JSON tree → SQLite → v2, Codex's rollout generations,
  Claude Code's subagent layouts), each shape is its own detected variant.
- **Byte-exact round trips.** Importing a native session and exporting it back
  reproduces it byte for byte. Every event keeps the harness's original record as
  its exact bytes, beside a neutral form any other harness can render.
- **Carry.** When a harness tolerates an extra field, the exporter stows the
  original events in it, so Claude Code → Codex → Claude Code comes back exact,
  even though Codex cannot express everything Claude Code wrote.
- **Loss is named and measured.** Every import and export reports what it could
  not represent, by a stable code. The test suite measures every trip between
  formats and refuses any change that makes one lossier.
- **One file per agent.** A thread is `<agent>.jsonl`; its subagents are their
  own files that point back at their parent. Deleting a thread is deleting a
  file.
- **Append-only, with branches.** Nothing is ever edited. A rewind in the harness
  is recorded as a branch, and the abandoned path stays in the file.
- **A store with a rebuildable index.** Ledgers on disk are the truth; a SQLite
  index answers queries and can be deleted and rebuilt at any time. Garbage
  collection deletes whole threads by an explicit policy, never edits one.
- **Live sync.** Project a thread into a harness, let it run, and what the
  harness appends comes back into the ledger, once each, safe to read mid-write.
  Attach an existing session the same way. Nothing is ever scanned: every
  session is bound explicitly.
- **Redaction and re-projection.** Hide an event from what a harness sees by
  appending a redaction, not by editing history; protoledger re-renders the file
  the harness resumes from, and the ledger keeps everything.
- **Message delivery as events.** Messages owed to an agent, cancelled
  attempts, deliveries and withdrawals are appended facts, so "what was sent"
  and "what is still owed" are derived from history and no message is lost.
- **Compression, optional.** Ledgers can be stored as compressed blocks behind a
  plain recent tail, with or without a trained zstd dictionary, and recompressed
  later with a better one. The decompressed bytes never change. On this repository's fixture corpus, compression without a dictionary stores ledgers about 11× smaller.
- **A server, optional.** `protoledger serve` streams every append over a
  WebSocket (resume from any point, no gaps, no duplicates) and answers queries
  over HTTP. It is a thin layer over the library; embed the library directly if
  you prefer.

## Lossless into protoledger, from every format

Converting **into** protoledger loses nothing, from every one of the 16 harness
formats: for every fixture, exporting the ledger back to its own format
reproduces the native session byte for byte (SQLite stores: row for row), so the
ledger holds everything the harness wrote. The chart shows the rest: for every
pair of formats, the share of facts that survive a trip from one to the other
**without** carry, because some harnesses cannot express what others record.
With carry, every trip through Claude Code, Codex and opencode comes back exact.
[`tests/reports/MATRIX.md`](tests/reports/MATRIX.md) has the numbers per
fixture, regenerated by the test suite.

## How well does it convert?

[`tests/reports/MATRIX.md`](tests/reports/MATRIX.md) is regenerated by the test
suite. For every fixture it shows whether X → protoledger → X is byte-exact, and
how many facts a trip through each other harness loses without carry. With
carry, every trip through Claude Code, Codex and opencode comes back exact.

## The format

Each ledger is UTF-8 JSONL: a header line naming the agent (and its parent, or
the orchestrator that spawned it), then one event per line:

```json
{"id":"…","seq":7,"ts":"…","kind":"tool_call","group":"msg_…",
 "body":{"call_id":"toolu_…","name":"Bash","input":{"command":"cargo test"}},
 "native":{"format":"claude-code","version":"2.1.280","id":"…","record":{…}}}
```

Sixteen kinds cover what harnesses have in common: text, media, reasoning,
tool calls and results, compaction, turns, usage, settings, titles, subagents,
peer messages, file changes, errors, application events, and harness-only
records. Applications attach their own namespaced metadata to any event.
[SPEC.md](SPEC.md) is the full specification.

## Library

```rust
use protoledger::formats::{self, ImportOptions, ExportOptions};

let claude = formats::by_name("claude-code").unwrap();
let codex = formats::by_name("codex").unwrap();
let imported = claude.import(path, &ImportOptions::standard())?;
let exported = codex.export(&imported.set, &ExportOptions::standard())?;
for loss in &exported.losses.0 {
    eprintln!("{} ×{}", loss.code, loss.count);
}
exported.output.write_to(out_dir, false)?;
```

Features: `store` (store and index), `sync` (bindings and live sync), `serve`
(the server; the only feature that brings in tokio), `sqlite` and `zstd`
(formats stored that way), `cli` (the binary). A library dependent turns
default features off and picks what it needs.

## Fixtures and the conformance suite

`fixtures/<format>/<case>/` holds about 190 real threads in each harness's own
layout, from permissively licensed projects (credited in each `SOURCE.md` and in
[NOTICE](NOTICE)), generated by running the harness on neutral prompts, or
synthetic from the harness's own schema code (and labelled so). The suite runs
every fixture through a self trip and through every other format, with and
without carry, plus a mutation harness that feeds every importer corrupted
input. It needs no network and no credentials.

The fixtures live in this repository, not in the published crate (they are about 11 MB), so `cargo add protoledger` stays small.

**A harness shipped a new version? Send a fixture.** [CONTRIBUTING.md](CONTRIBUTING.md)
says how; the suite shows what your fixture changed.

## License

MIT OR Apache-2.0. Fixtures keep their own licenses, recorded in each
`SOURCE.md`.

Built by [Sam Haaf](https://github.com/samhaaf) at Broomstick.

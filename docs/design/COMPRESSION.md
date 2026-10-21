# Corpus compression: design

Status: normative text is SPEC §10, and the store implements it without
dictionaries (dictionaries and recompression come later, on the same format).
This doc is the design rationale: the options weighed, and why the layout of
SPEC §10 was chosen. Where it describes an earlier sketch that SPEC §10 then
changed (names, the lock, the read-back check), SPEC §10 is what holds.

Every figure below was measured on the repository's own fixtures (§1). The
headline figures (§2.1) come from the crate's own compressor, through the
store, by `fixture_corpus_ratio_by_block_size` in `tests/compress.rs`, which
prints them; anyone can re-run it. A few figures in §2.2 come from an earlier
prototype measurement on the same fixtures, and are labelled so.

**The short answer.** 20× is not real for protoledger ledgers under any scheme
that keeps one file per agent. What the store gets on the fixtures, counting
every physical byte, no dictionary:

| What you get | Ratio on the fixtures |
|---|---|
| Blocks a writer seals as it appends, L3, sealed at 256 KiB (the sealed part) | **7.86×** |
| Blocks a migration writes, L19, 1 MiB | **7.79×** |
| Each ledger compressed whole, L19 (no random access, no append) | 7.95× |
| The whole corpus as one stream, L19 (no per-ledger deletion either) | 11.16× |

The layout chosen (§3.2, "hot tail, cold blocks"; SPEC §10) keeps every
guarantee on the *logical* byte stream and needs one SPEC change: an event's
**bytes in the decompressed stream** are what may never change, not the bytes
on disk.

## 1. Corpora

| Corpus | How it was made | Ledgers | Events | Bytes |
|---|---|---|---|---|
| **fixtures** | every `fixtures/**/native` through `Store::import`, one store, cases whose agent ids collide with an earlier case's left out | 236 | 5,099 | 10,258,871 |

The fixtures are small (most ledgers are a few KiB), so at every block size
most ledgers are one block plus, for a sealing writer, a hot tail: the ratios
below are those of small ledgers, and a store of long sessions compresses
better per block than this. They are what the repository can show.

Method (§2.1): the plain store's ledgers are copied into a store whose
`store.json` sets `recompress_bytes`, then `store config --compression on`
migrates them (zstd level 19); separately, the same ledgers are imported into
a store with compression on and `seal_bytes` set, so a writer seals them as it
appends (zstd level 3). Ratios are plain bytes over physical bytes: every
block file and hot tail, meta frames, zstd frame headers and content checksums
included. Every compressed ledger is read back and compared byte for byte.

## 2. Measurements

### 2.1 Ratio by block size, no dictionary (this crate)

| Block size | Migrated, L19 (blocks) | Sealed while appending, L3: the sealed part (blocks) | Sealed while appending, with the hot tails |
|---|---|---|---|
| 16 KiB | 5.83 (547) | 5.67 (314) | 3.33 |
| 64 KiB | 7.04 (303) | 7.15 (67) | 2.14 |
| 256 KiB | 7.65 (246) | 7.86 (10) | 1.39 |
| 1 MiB | 7.79 (236) | – (0) | 1.00 |
| each ledger as one frame, L19 | 7.95 | | |
| the corpus as one frame, L19 | 11.16 | | |

"The sealed part" is the stream bytes now in blocks over the block files'
bytes; "with the hot tails" counts every ledger's plain hot tail too. Most
fixtures are smaller than a seal, so at 256 KiB only 10 blocks were sealed
(at 1 MiB, none) and nearly the whole corpus stayed in hot tails, which is
what the last column shows: a live store of small ledgers is mostly hot
tail until it is migrated or recompressed. A migration writes blocks of
whole ledgers regardless, which is why its column barely moves past 256 KiB.

The gap between the per-ledger figures and the one-stream figure is
redundancy **between** ledgers: subagents repeating their parent's prompts,
sessions repeating the same system prompt and tool schemas. A per-file scheme
can recover part of it only through a shared dictionary.

### 2.2 Per-row frames and dictionaries (earlier prototype, fixtures)

Before SPEC §10 was written, a prototype (libzstd 1.5.7 through
python-zstandard) compared schemes on the same fixtures, with the files split
at random into a training set for dictionaries (2 MB) and a measured set
(8 MB). It counted the same physical bytes, plus a 4-byte length per row frame
or a 12-byte seek-table entry per block, and the dictionary itself:

| Scheme | Ratio | Random read |
|---|---|---|
| whole file, L3 / L19 | 6.60 / 7.37 | 50 µs |
| per row, no dict, L3 / L19 | 2.75 / 2.86 | 3 µs |
| per row, dict 112 KiB, L3 / L19 | 3.88 / 4.46 | 2 µs |
| block 256 rows, L19, no dict / dict | 7.35 / 7.76 | 57 µs |
| block 64 KiB, L19, no dict / dict | 6.74 / 7.34 | 25 µs |

"Random read" is the mean time to produce one event, chosen uniformly over
rows, from bytes already in the page cache (decompress the frame that holds
it, then slice).

### 2.3 Dictionaries

What the prototype rows say, and what the design takes from them:

- **A dictionary matters a lot for small frames and little for large ones**:
  on the fixtures, 112 KiB lifts per-row frames from 2.86× to 4.46× (+56%)
  but 256-row blocks only from 7.35× to 7.76× (+6%).
- **A dictionary is a bet on the corpus looking like what it was trained
  on.** The text sessions repeat (system prompts, tool schemas, project
  instructions) changes over weeks, so a dictionary goes stale and has to be
  retrained, and every ledger it encoded has to be recompressed to benefit.
  The fixtures are too small and too static to measure staleness; it is
  stated here as a risk, not a figure.
- A trained dictionary is the only way a per-file scheme can reach the
  redundancy between ledgers (§2.1's gap).

### 2.4 Is 20× real?

No. On the fixtures, the best per-ledger figure is 7.95× (each ledger as one
frame, with no random access and no append), and the best figure at all is
11.16×, which needs the whole corpus as one stream: no per-ledger deletion, no
random access, no append. The layout chosen gives 7.79× once migrated, and
7.86× on what a writer has sealed at the default 256 KiB (the hot tails stay
plain until they are sealed), with random access and append, and without a
dictionary.

## 3. Layout options

Every option keeps the **logical ledger**: the byte stream a reader gets after
decompression is exactly today's `<agent>.jsonl`, header line and all, so every
native record is byte-exact by construction. What differs is the physical file.

### 3.1 Per-row frames with a shared dictionary (the idea as stated)

Each appended row is written as one zstd frame (dictionary id in the frame
header, content checksum on) behind a 4-byte length. Appending stays one
write + fsync per event.

- Ratio on the fixtures 3.88× (L3) – 4.46× (L19) with a 112 KiB dictionary
  (§2.2); random read in microseconds; compressing one row at L3 is fast
  enough that the write path is unaffected.
- Keeping the file "JSONL" by base64-ing the frame into a JSON line costs a
  third of the frame's bytes again. Not worth it: the file is binary either
  way.
- Torn tail: a frame whose length prefix or body is short, or whose checksum
  fails, is the torn append; cut the file back to the end of the last good
  frame, exactly as today's newline rule does.
- Every frame depends on a dictionary, so the ratio is hostage to dictionary
  freshness (§2.3), and the per-row ceiling is far below a block's.

Good as a **hot-tail encoding**; not good as the whole answer.

### 3.2 Hot tail, cold blocks (recommended)

```
<store>/ledgers/<agent>.jsonl.zst   cold: append-only sequence of sealed blocks
<store>/ledgers/<agent>.jsonl.tail  hot: plain JSONL rows not yet sealed
<store>/dicts/<sha256>.zdict        dictionaries, content-addressed (if used)
<store>/ledgers/<agent>.jsonl       a store or agent with compression off: unchanged
```

- **Appends** go to the tail exactly as today: one line, one write, one fsync.
  The live part of every ledger stays greppable and `tail -F`-able.
- **Sealing.** When the tail reaches ~256 KiB, or the agent has been idle for
  a set time, or the writer closes, the writer compresses the tail's complete
  rows into one block and appends it to the cold file:
  1. one write of `[skippable frame: meta][zstd frame]`, then fsync;
  2. truncate the tail to zero, fsync.
  The meta frame (a zstd *skippable* frame, ignored by every zstd decoder)
  holds `{logical_offset, logical_len, first_seq, last_seq, dict sha256,
  generation}`. Sealing inline at L3 is cheap; L19 is left to the repack
  (§4), which is how "recompress every so often" earns its place.
- **Crash between 1 and 2**: rows are in both files. On open, tail rows whose
  seq is ≤ the cold file's last `last_seq` are dropped (the tail is cut). The
  rule is mechanical, so recovery is deterministic.
- **Torn compressed frame**: a crash during step 1 leaves a short skippable or
  zstd frame at the end of the cold file. It is detected by any of: frame
  shorter than its header says, checksum mismatch, decompressed length ≠
  `logical_len`, last row's seq ≠ `last_seq`. Repair = truncate the cold file
  to the end of the last good frame. **Nothing is lost**, because the tail is
  only cut after the block is durable.
- **One writer**: the lock is taken on the cold file, and an opener verifies
  after locking that the path still names the inode it locked (re-open and
  retry if not), because a repack (§4) replaces that inode by rename.
- Ratio on the fixtures, no dictionary (§2.1): freshly sealed 256 KiB L3
  blocks 7.86×; 1 MiB L19 blocks, as a migration (or a repack) writes them,
  7.79×. A random read decodes one block. The hot tail is at most one
  seal's worth (≤ 256 KiB) per live agent, so in a store of long sessions it
  is noise in the total; in the fixtures, where most ledgers are smaller than
  one seal, it is most of the corpus (§2.1's last column).

### 3.3 Options rejected

| Option | Why not |
|---|---|
| Whole-file compression (`.jsonl.zst`, one frame) | Not appendable, random read is O(file). |
| One file per agent with plain tail and blocks interleaved | Sealing would have to remove plain rows from the middle of the file: a rewrite at every seal. |
| Blocks across agents / a store-wide stream (11.16× on the fixtures) | Breaks "deleting a thread is deleting its files", random access and append. |
| Raw-prefix chaining (each row compressed against the previous rows of the same ledger) | Near whole-file ratio, but reading one row means decoding every row before it. |

### 3.4 The index

The index's offsets are defined over the **logical** stream, so `(agent, seq) →
(offset, len)` keeps its meaning and its values: an offset is the same number
whether the ledger is plain, compressed, or repacked. One table is added:

```
frames(agent, logical_offset, logical_len, physical_offset, physical_len,
       dict, first_seq, last_seq, generation)
```

`event_at` finds the frame containing the logical offset (or the tail, whose
physical offset is `logical − cold logical length`), decompresses it, slices
the row. A small per-reader cache of the last decompressed block makes
sequential reads cost one decompression per block. `frames` is rebuilt from the
skippable meta frames alone, without decompressing.

### 3.5 Dictionaries

- Stored once per store as `<store>/dicts/<sha256>.zdict`. The zstd frame's
  32-bit dictionary id is set to the first 4 bytes of that hash; the meta
  frame carries the full hash, which is the identity. Two dictionaries whose
  ids collide are refused at training time.
- **Never deleted while a frame references one.** References are derivable from
  the `frames` table; in practice a dictionary is 112 KiB, so "keep every
  version forever" is the simple rule and costs nothing.
- Consequence: a `.jsonl.zst` that uses a dictionary is **not self-contained**.
  Copying one agent's file out of the store needs the dictionary too (or
  `protoledger cat`, §5). Embedding the dictionary in each file is not an
  option: a 112 KiB dictionary is larger than most compressed ledgers.

## 4. Recompression and SPEC §1.2

The tension: SPEC §1.2 says an event, once written, MUST NOT be changed while
its file exists, and the design reads immutable as no edits. Repacking changes bytes
on disk.

**Resolution: the logical ledger is the immutable thing; its physical encoding
may be rewritten, atomically, only into an encoding whose decompressed stream
is byte-identical.** Nothing a reader can observe through the library changes:
no event's bytes, no seq, no logical offset.

Repack procedure (run by the lock holder, or by `protoledger store repack`,
which takes the writer lock and skips agents that are live):

1. Read the cold file; optionally seal the tail first.
2. Write `<agent>.jsonl.zst.repack-<pid>` with new blocks: target size (merging
   the small blocks idle-sealing produced), level 19, the current dictionary,
   `generation + 1` in every meta frame.
3. **Verify**: decompress the new file and compare its SHA-256 with the old
   file's logical stream, and every block's seq range. Any difference aborts
   and deletes the temp file.
4. fsync, rename over the cold file, fsync the directory, update `frames`.

Crash at any step leaves either the old file or the new one, both valid. An
index whose `frames.generation` disagrees with the file's meta frames rebuilds
that agent's `frames` rows (cheap: meta frames only).

Readers during a rewrite:
- A reader holding an open descriptor keeps reading the old inode, which stays
  complete until it closes.
- A reader that resolved a physical offset from the index and then opens the
  file checks the generation in the meta frame at that offset; on mismatch it
  re-resolves. Because logical offsets do not change, nothing it holds is
  invalidated.
- Subscribers resume by `(agent, seq)` and see nothing at all.

When to repack and retrain:
- **Retrain** the store dictionary on the most recent N MB of rows when the
  ratio of newly sealed blocks drops a set fraction below the ratio at the
  last training, or monthly.
  Each training adds a dictionary version; old versions stay.
- **Repack** an agent when it has been idle for a set period and (a) its cold
  file has many small blocks, or (b) its blocks use a dictionary at least one
  version behind *and* a trial repack of one block gains more than a set
  fraction. On large blocks a dictionary's whole gain is small (§2.3), so
  the incremental-version idea pays, but modestly.

**SPEC sentences that change:**

| Where | Today | Becomes |
|---|---|---|
| §1.1 | "Deleting a thread is deleting its file." | "…deleting its files." |
| §1.2 | "An event, once written, MUST NOT be changed or removed while its file exists." | "An event, once written, MUST NOT be changed or removed while its ledger exists: its bytes in the ledger's logical stream (§2.4) are fixed. A compressed ledger's physical encoding MAY be rewritten, atomically, into another whose logical stream is byte-identical." |
| §2 | "A ledger is one UTF-8 JSONL file…" | "A ledger is one UTF-8 JSONL byte stream, its **logical stream**, stored as one plain file or compressed (§2.4)." |
| §2 | torn-append rule | unchanged for the plain form; §2.4 adds the torn-frame rule. |
| new §2.4 | — | the cold/tail files, the meta frame, sealing, the crash and torn-frame rules, dictionaries. |
| §9 | the `ledgers/<agent>.jsonl` layout | adds `.jsonl.zst`, `.jsonl.tail`, `dicts/`. |
| §9.1 | "(agent, seq) → (byte offset, …)" | "…(byte offset **in the logical stream**, …)", plus the `frames` table. |
| §9.4 | `offset` | "logical offset". |
| §9.5 | GC deletes ledger files | deletes an agent's files; a dictionary is deleted only when no frame references it. |
| README | "Append-only. Events never change once written." / "no event is edited" | add "measured on the logical stream". |

## 5. Interaction with the rest

| Area | Effect |
|---|---|
| **Projection / sync / export** | Export reads the whole ledger: decompression is fast next to JSON parsing. Sync appends through the store, so it writes the tail like any program. |
| **Index rebuild** | Today it parses every row. Compressed, it decompresses every block first, a small cost next to parsing. `frames` alone rebuilds from meta frames. `catch_up`'s size/mtime check covers both files. |
| **`Appender::open`** | Reads the whole ledger to recover its id set; now it decompresses too. Same order of cost. (Recovering ids from the index would remove both costs, separately.) |
| **`serve` / subscribe** | New events are published from the append path, from plain rows: unchanged. Catch-up from an old seq reads blocks: one decompression per 256 KiB. |
| **Library consumers** | Reading through the library (`Store::events`, `event_at`, `subscribe`): unchanged. But `protoledger::read_at(path, offset)` and `tail(path, offset)` are public, **path-based**: they must become ledger-handle based (open by store and agent, or by the cold+tail pair), with the path forms still working for plain files. This is the one API break. |
| **External tools** | The tail is plain JSONL. The cold file is a valid zstd stream (skippable frames are ignored): `zstd -dc` reads it when no dictionary is used, `zstd -dc -D <dict>` when one dictionary is (true after every repack, since a repack uses one). A mixed-dictionary file needs **`protoledger cat <store> <agent>`**, which should exist anyway: it prints the logical ledger, for `grep`, `jq` and `diff`. |
| **Configuration** | Per store, in a small file at `<store>/store.json` (`{"compression": {"seal_bytes": 262144, "idle_seal_s": 600, "dictionary": true}}`), not in `index.db`, which is derived. Readers never consult it: a ledger's form is self-describing from which files exist, so a store can hold plain and compressed ledgers side by side. |
| **Migrating on** | `protoledger store compress <store>`: for each agent not live, write the cold file from the plain ledger, verify as in §4, rename, then delete the plain file. Per agent, atomic, resumable. |
| **Migrating off** | `protoledger store decompress <store>`: write `<agent>.jsonl` from the logical stream, verify, then delete the cold and tail files. |
| **GC** | Deletes the cold and tail files (or the plain file) and the index rows. Dictionaries outlive them. |

## 6. Costs to be clear about

- Two files per agent where there was one; "a ledger is a file" becomes "a
  ledger is a logical stream".
- The cold file is binary. Debugging a ledger needs `protoledger cat`.
- A store with dictionaries owns state (the dictionaries) that a single file
  copied out of it does not carry.
- Gain is 7.79× on disk on the fixtures once migrated (§2.1), against
  which: a new on-disk form, a repack job, the read/tail API change, and the
  SPEC edits above.

## 7. Recommended scheme

Hot plain tail + cold sealed blocks (§3.2), logical-stream offsets (§3.4),
sealing at L3 inline at 256 KiB or on idle, repack to L19 1 MiB blocks when an
agent goes idle. **Start without a dictionary**: at 1 MiB L19 that is
7.79× on the fixtures, self-contained files, `zstd -dc` works, no
retraining. Add the store dictionary later as an option on the same format
(the meta frame already names it): it helps most on small idle-sealed blocks
and small subagent ledgers (§2.3).

## 8. Open decisions

**1. Do you accept that "immutable" means the decompressed bytes, not the bytes on disk?**
Any compression that improves after the first write, including
"recompress old ledgers with a newer dictionary", rewrites files, which SPEC
§1.2 forbids as written. Options: (a) redefine immutability on the
logical stream, with rewrite only by atomic rename after a byte-identity check
(§4); (b) keep on-disk immutability, which allows only per-row frames written
once and never improved (the per-row ratios of §2.2, dictionary frozen at
write time).
*Pick: (a). Decided (a): SPEC §1.2 and §10.1.*

**2. Should the first version use a trained dictionary?**
A dictionary is a sample-derived table (~112 KiB) shared by every frame in a
store. It is worth much more for per-row frames than for sealed blocks
(§2.3), goes stale as the corpus changes, and makes a ledger file depend on a
store file.
Options: (a) no dictionary first, blocks only (self-contained);
(b) a dictionary from the start (plus retraining, repack by dictionary
version, and dictionary GC);
(c) per-row frames with a dictionary and no blocks (simplest write path).
*Pick: (a), with the format leaving room for (b). Decided (a), with a
dictionary as an option a user passes or trains (SPEC §10.7).*

**3. Is the gain worth a second on-disk form at all, today?**
The store holds the neutral form beside every native record, so it is larger
than the native transcripts it was built from; compression makes it smaller
than them. Options: (a) build it now behind per-store config, off by
default; (b) defer until a store's size is an actual problem, and only add
`protoledger cat` and handle-based `read_at`/`tail` now so the later change is
not an API break.
*Pick: (b): the API change is cheap now and expensive after dependents adopt the path-based calls.
Decided (a): built now, off by default (SPEC §10).*

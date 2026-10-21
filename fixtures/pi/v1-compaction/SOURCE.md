# pi / v1-compaction

- **Origin:** `packages/coding-agent/test/fixtures/before-compaction.jsonl` in
  https://github.com/badlogic/pi-mono at commit `8efd314fa3d3232ce197d8d7a6a5660530b9d83e` (a real v1 session, Dec 2025).
- **License:** MIT (pi-mono).
- **Excerpt:** line 1 (header) plus lines 619-648 of 1003. The compaction's
  `firstKeptEntryIndex` (551) refers to the original file, so in this excerpt it
  points past the end and `preserved` is not derived.
- **Sanitised:** `/Users/badlogic` → `/home/user` and the session-dir slug
  `--Users-badlogic-` → `--home-user-` (in the header's `branchedFrom`).
- **Exercises:** v1 `compaction` with `firstKeptEntryIndex`, `bashExecution`
  user command, a `branchedFrom` header (forked-from link), thinking blocks with
  signatures.

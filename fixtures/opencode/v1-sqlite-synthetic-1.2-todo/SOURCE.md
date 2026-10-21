# v1-sqlite-synthetic-1.2-todo

- **Synthetic.** The sessions of `v1-storage-json-synthetic-subagent-compaction`
  written into an `opencode.db` the way opencode 1.2.0 does: schema from the three
  drizzle migrations under `packages/opencode/migration/` at tag `v1.2.0`
  (`src/formats/opencode/schema_v1.sql`), the `__drizzle_migrations` journal
  (sha256 of each `migration.sql`, folder timestamp as `created_at`), and rows as
  `packages/opencode/src/storage/json-migration.ts` and `session/index.ts`
  (`updateMessage`, `updatePart`) at v1.2.0 write them: message `data` without
  `id`/`sessionID`, part `data` without `id`/`messageID`/`sessionID`. A three-item
  `todo` list (`TodoTable`) was added to the parent session.
- **License:** MIT OR Apache-2.0 (this repository). Built by `mkfix.py` (anomalyco/opencode MIT sources).
- **Exercises:** v1-sqlite detection, row-identical re-export, todo rows kept as
  native records, subagent link through the task tool.

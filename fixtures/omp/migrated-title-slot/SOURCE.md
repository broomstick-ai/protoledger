# omp / migrated-title-slot

- **Origin:** `packages/coding-agent/test/fixtures/large-session.jsonl` in
  https://github.com/can1357/oh-my-pi at commit `d060e142cc5c938b2c66c1881c550be0c660928c`. It is pi's v1
  `large-session.jsonl` after omp loaded and rewrote it: omp prepended its
  fixed-width 256-byte `title` slot, set `version: 3` and appended `id` /
  `parentId` to every entry (the v1 → v2 migration).
- **License:** MIT (oh-my-pi).
- **Excerpt:** the first 40 of 1021 lines.
- **Sanitised:** `/Users/badlogic` → `/home/user`.
- **Exercises:** the title slot, a migrated tree whose ids sit at the end of
  each record, v1-era header fields on a v3 header.

# pi / v1-linear

- **Origin:** `packages/coding-agent/test/fixtures/large-session.jsonl` in
  https://github.com/badlogic/pi-mono at commit `8efd314fa3d3232ce197d8d7a6a5660530b9d83e`
  (a real session the pi author recorded with pi before session v2, ~Nov 2025).
- **License:** MIT (pi-mono).
- **Excerpt:** the first 40 of 1019 lines (header + 39 entries). A prefix of a v1
  session is itself a valid v1 session.
- **Sanitised:** `/Users/badlogic` rewritten to `/home/user`. Prose and tool output
  are pi's own repository source, already public in the same repo.
- **Exercises:** v1 header with provider/modelId/thinkingLevel, entries without
  `id`/`parentId` (ids become `@<byte offset>`), `model_change`,
  `thinking_level_change`, openai-responses and anthropic assistants, edit/write
  tool results with `details.diff` (file_change), an empty assistant message.

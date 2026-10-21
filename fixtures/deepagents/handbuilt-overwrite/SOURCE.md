# handbuilt-overwrite

- **Origin:** https://github.com/letta-ai/trajectory/blob/b8c508fd1e05d1eeb1e2c7e13f41b8c2f3f09773/fixtures/deepagents/checkpoint.db
  (built by `fixtures/deepagents/generate_fixture.py` in that repository with
  `SqliteSaver.put` / `put_writes`, not by a graph run).
- **License:** Apache-2.0 (letta-ai/trajectory).
- **What it exercises:** three threads written by hand: version-2 checkpoints
  (`empty_checkpoint()`, `pending_sends`), messages inline in `channel_values`,
  a ToolMessage write, an AIMessage with reasoning/text blocks and tool calls,
  an `Overwrite` (msgpack ext 2) write, and a `writes` table without `task_path`
  (an older checkpoint-sqlite).
- **Sanitisation:** none needed (synthetic).

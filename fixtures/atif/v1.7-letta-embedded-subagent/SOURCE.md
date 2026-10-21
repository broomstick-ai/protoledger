# ATIF-v1.7 with an embedded subagent and malformed tool arguments (letta-ai/trajectory)

- **Origin:** `fixtures/atif/cleanup/input.json` in [letta-ai/trajectory](https://github.com/letta-ai/trajectory) at commit `b8c508fd1e05d1eeb1e2c7e13f41b8c2f3f09773`. Prettier-formatted (inline small objects), so it exercises the
  raw-text path. Its tool call has `arguments: ["fast"]` (a list, which Harbor's model rejects) and an observation result
  with no `source_call_id`; its embedded subagent is never referenced by a ref.
- **License:** Apache-2.0 (the repository's `LICENSE`).
- **Sanitisation:** none.

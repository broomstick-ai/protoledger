# bare-items-malformed

Robustness: a non-JSON line (Codex skips it, `rollout/src/recorder.rs:1119-1125`; kept as raw native text) and enveloped items with no session_meta.

- **Origin:** https://github.com/letta-ai/trajectory/blob/b8c508fd1e05d1eeb1e2c7e13f41b8c2f3f09773/fixtures/codex/cleanup/input.jsonl
- **License:** Apache-2.0
- **Sanitised:** Unchanged.

# v1-export-trajectory-cleanup

- **Source:** `fixtures/opencode/cleanup/input.json` of
  [letta-ai/trajectory](https://github.com/letta-ai/trajectory) at commit
  `b8c508fd1e05d1eeb1e2c7e13f41b8c2f3f09773`, renamed to `session.json`.
- **License:** Apache-2.0 (trajectory).
- **What it is:** a hand-written `opencode export` (1.x) document with parts that
  have **no `id`**, a `step-start` part, an unknown part type
  (`future-semantic-part`) and a tool in `error` state. The adapter gives id-less
  parts location anchors (`<message id>/parts/<n>`) and keeps unknown parts as
  `native` events.
- **Sanitised:** nothing (synthetic paths).

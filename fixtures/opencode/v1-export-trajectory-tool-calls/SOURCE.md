# v1-export-trajectory-tool-calls

- **Source:** `fixtures/opencode/tool-calls/input.json` of
  [letta-ai/trajectory](https://github.com/letta-ai/trajectory) at commit
  `b8c508fd1e05d1eeb1e2c7e13f41b8c2f3f09773`, renamed to `session.json`.
- **License:** Apache-2.0 (trajectory).
- **What it is:** an `opencode export` (1.x) document, `{info, messages:[{info, parts}]}`,
  that the trajectory authors wrote by hand in the shape of an export from
  opencode 1.2.21 (`info.version`). It is minimal: ids are not opencode-minted
  (`oc-session-1`, …) and messages lack fields opencode always writes (`agent`,
  `model`, `tokens`, `parentID`). It exercises a tolerant importer.
- **Exercises:** user text, reasoning, a completed `glob` tool, assistant text.
- **Sanitised:** nothing (already synthetic paths, `/workspace/opencode-demo`).

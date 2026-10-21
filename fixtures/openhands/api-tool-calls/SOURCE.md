# openhands / api-tool-calls

- **Origin:** `fixtures/openhands/tool-calls/input.json` in
  https://github.com/letta-ai/trajectory at `b8c508fd1e05d1eeb1e2c7e13f41b8c2f3f09773`. Synthetic and not valid for
  the SDK (it mixes V1 `kind` tags with simplified fields), but it is the shape
  trajectory's OpenHands decoder accepts: a bare JSON array of V1-style events.
- **License:** Apache-2.0 (letta-ai/trajectory).
- **Exercises:** the `v1-api` file variant, an ActionEvent with an `action`
  but no `tool_call`, `UserRejectObservation`; the file's own formatting is
  kept verbatim.

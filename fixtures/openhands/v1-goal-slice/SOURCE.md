# openhands / v1-goal-slice

- **Origin:** `tests/sdk/conversation/goal/fixtures/events.jsonl` in
  https://github.com/OpenHands/software-agent-sdk at `bf9b6057b00e1b2b532ecff508c0d92d818adced` (added in
  `cf563075`): per its test, a trimmed slice of the persisted events of a real
  PR-review conversation. Each line was written verbatim as its own
  `events/event-<idx>-<id>.json` file in line order (directory name invented,
  no `base_state.json`); SDK 1.55.1's `EventLog` loads all ten.
- **License:** MIT (software-agent-sdk).
- **Sanitised:** none; event text copied verbatim.
- **Exercises:** events serialised with nulls (API/websocket dump shape),
  `ConversationStateUpdateEvent` (stats → cumulative usage), events without
  `parent_id`, a conversation directory without `base_state.json`.

# openhands / v0-events-dir

- **Origin:** derived from `tests/runtime/trajs/basic_gui_mode.json` in
  https://github.com/OpenHands/OpenHands at tag `1.0.0` (`0cb27a4aa8c2656c37c00a8d52ad574c1c21649b`), a real,
  contiguous V0 GUI session (ids 0-17). Each event was written to
  `sessions/<sid>/events/<id>.json` with Python `json.dumps(event)`, exactly
  as `EventStream.add_event` writes it (`openhands/events/stream.py:188`).
  The session id is invented; `metadata.json` is **synthesised** with the
  field set and order of `ConversationMetadata` at 1.0.0 (illustrative values).
- **License:** MIT (OpenHands).
- **Sanitised:** none needed.
- **Exercises:** the native V0 directory layout, `agent_state_changed`
  (turns), `change_agent_state` and `null` events, `edit` observations with
  diffs (file_change), `metadata.json` (title, model, cumulative usage).

# letta-ai/trajectory Slack fixture `cleanup` (api-replies wrapper)

- **Origin:** `fixtures/slack/cleanup/input.json` in [letta-ai/trajectory](https://github.com/letta-ai/trajectory) at commit `b8c508fd1e05d1eeb1e2c7e13f41b8c2f3f09773`: duplicate copies of one message, a `thread_broadcast` with a nested root copy, a `channel_join`. The file is the
  `{channel, channel_name?, users?, messages}` object that library's Slack normaliser takes, which protoledger reads as an
  `api-replies`-shaped wrapper (the extra keys stay in the wrapper record).
- **License:** Apache-2.0 (the repository's `LICENSE`).
- **Sanitisation:** none; the upstream fixture is already synthetic (example ids and names). Renamed to `thread.json`.

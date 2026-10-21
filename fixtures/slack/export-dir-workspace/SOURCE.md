# Synthetic Slack workspace export directory (two channels)

- **Origin:** synthetic, written for protoledger from the documented shapes of Slack's `conversations.replies` response and
  workspace export (message objects with `ts`, `thread_ts`, `subtype`, `bot_id`, `files`, `reactions`, `edited`, `blocks`).
  The byte layout (escaped slashes, 4-space indented ASCII-escaped export files) is modelled on what Slack emits; it is not
  a captured Slack response.
- **License:** MIT OR Apache-2.0 (protoledger's own).
- **Sanitisation:** none needed: made-up ids (`T0EXAMPLE`, `U0ALICE`, ...), neutral text, example.com URLs.
- **Exercises:** `users.json`, `channels.json`, `integration_logs.json`, one directory per channel with one file per
  day, a thread spanning two day files, join/topic subtypes, a bot reply, a file share, Slack's export layout.

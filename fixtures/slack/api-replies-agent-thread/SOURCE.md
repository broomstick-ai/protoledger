# Synthetic `conversations.replies` response: a person asks a bot agent to fix a build

- **Origin:** synthetic, written for protoledger from the documented shapes of Slack's `conversations.replies` response and
  workspace export (message objects with `ts`, `thread_ts`, `subtype`, `bot_id`, `files`, `reactions`, `edited`, `blocks`).
  The byte layout (escaped slashes, 4-space indented ASCII-escaped export files) is modelled on what Slack emits; it is not
  a captured Slack response.
- **License:** MIT OR Apache-2.0 (protoledger's own).
- **Sanitisation:** none needed: made-up ids (`T0EXAMPLE`, `U0ALICE`, ...), neutral text, example.com URLs.
- **Exercises:** API wrapper, bot messages (`bot_message`, `bot_profile`), file shares with URLs (text and image),
  reactions, `edited`, rich-text `blocks`, a `thread_broadcast` with a nested `root`, non-ASCII text.

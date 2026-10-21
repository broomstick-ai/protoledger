# Synthetic single export day file (bare message array)

- **Origin:** synthetic, written for protoledger from the documented shapes of Slack's `conversations.replies` response and
  workspace export (message objects with `ts`, `thread_ts`, `subtype`, `bot_id`, `files`, `reactions`, `edited`, `blocks`).
  The byte layout (escaped slashes, 4-space indented ASCII-escaped export files) is modelled on what Slack emits; it is not
  a captured Slack response.
- **License:** MIT OR Apache-2.0 (protoledger's own).
- **Sanitisation:** none needed: made-up ids (`T0EXAMPLE`, `U0ALICE`, ...), neutral text, example.com URLs.
- **Exercises:** the `messages-array` variant, a bot message with legacy `attachments`, a `me_message`; 2-space indent.

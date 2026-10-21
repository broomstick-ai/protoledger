# store-index

- **Origin:** two real transcripts from ollieb89/claw_imperium agents/kiloclaw/sessions/ (5bea5721-…-topic-1.jsonl, 7390e73a-….jsonl); `sessions.json` is SYNTHETIC
- **Repository / commit:** ollieb89/claw_imperium @ 3299ee7d970ee7cf33f02fd7ff9221f6a6e7a7e2
- **License:** MIT (transcripts); sessions.json written for protoledger from `src/config/sessions/types.ts` at openclaw/openclaw @ d1a7edc3
- **What it is:** A store directory laid out as OpenClaw keeps it (`agents/<agentId>/sessions/`), with a Telegram topic transcript (`-topic-1` file name) and a `sessions.json` index mapping session keys to session ids.
- **Sanitisation:** Telegram user/group ids and Tailscale IPs replaced by placeholders. The real sessions.json in that repo holds personal data, so a minimal one was written instead.

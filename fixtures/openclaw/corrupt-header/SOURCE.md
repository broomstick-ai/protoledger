# corrupt-header

- **Origin:** same directory, session b3db607f-…
- **Repository / commit:** kayba-ai/agentic-context-engine @ 3a319831785ede6ba4a3a4002756233095c59e17
- **License:** Apache-2.0
- **What it is:** Real transcript whose first line has 6 stray bytes (`/speci`) before the header, so the header is not JSON. Exercises a corrupt line kept verbatim and a session id taken from the file name.
- **Sanitisation:** A Telegram user id replaced by `1000000001`; a partially masked API key in tool output replaced by `sk-REDACTED`.

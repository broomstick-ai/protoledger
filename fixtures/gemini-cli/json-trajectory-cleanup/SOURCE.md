# json-trajectory-cleanup

**Origin:** `fixtures/gemini-cli/cleanup/input.json` in
https://github.com/letta-ai/trajectory at commit
`b8c508fd1e05d1eeb1e2c7e13f41b8c2f3f09773`, copied byte for byte and renamed
to Gemini CLI's file naming (`session-<YYYY-MM-DDTHH-MM>-<id8>.json`).

**License:** Apache-2.0.

**Sanitisation:** none; the case is synthetic (written by trajectory's authors
from the whole-session JSON shape). It is pretty-printed with a trailing
newline, which Gemini CLI itself does not write, so it also exercises the
exporter's formatting memory.

**What it exercises:** an unknown message `type` (`telemetry`, kept as a
native event), an empty-content gemini message, and a failed tool call whose
response is `{"error": ...}`.

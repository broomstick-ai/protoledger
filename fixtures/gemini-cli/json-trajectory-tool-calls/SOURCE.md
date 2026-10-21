# json-trajectory-tool-calls

**Origin:** `fixtures/gemini-cli/tool-calls/input.json` in
https://github.com/letta-ai/trajectory at commit
`b8c508fd1e05d1eeb1e2c7e13f41b8c2f3f09773`, copied byte for byte and renamed
to Gemini CLI's file naming (`session-<YYYY-MM-DDTHH-MM>-<id8>.json`).

**License:** Apache-2.0.

**Sanitisation:** none; the case is synthetic (written by trajectory's authors
from the whole-session JSON shape). It is pretty-printed with a trailing
newline, which Gemini CLI itself does not write, so it also exercises the
exporter's formatting memory.

**What it exercises:** an `info` notice, user content as text parts, a gemini
message with a thought, text and one successful `read_file` call with an
inline `functionResponse`.

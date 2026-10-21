# json-agentsview-system-messages

**Origin:** `internal/parser/testdata/gemini/system_messages.json` in
https://github.com/kenn-io/agentsview at commit
`905a5fe2ee7c01fb55f620d9ce4e4728aed177ae`, copied byte for byte.

**License:** MIT.

**Sanitisation:** none; synthetic test data written by agentsview's authors
(keys are sorted alphabetically, as Go's encoder writes them, and timestamps
have second precision, so it also checks that key order and number/timestamp
text survive a round trip). It is not a file name Gemini CLI would write.

**What it exercises:** a session made only of `info`, `error` and `warning` notices.

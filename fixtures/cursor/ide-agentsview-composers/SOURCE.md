# ide-agentsview-composers

**Origin:** derived from `TestCursorIDEProviderDiscoverAndParse` in
`internal/parser/cursor_ide_test.go` and `internal/parser/testdata/cursor-ide-object-tool-result.json`
of https://github.com/kenn-io/agentsview at `905a5fe2ee7c01fb55f620d9ce4e4728aed177ae`
(MIT): the test's `cursorDiskKV` rows written to a `state.vscdb`.

**License:** MIT.

**Sanitisation:** none needed (synthetic test data; `/Users/alice/...` is the
test's own placeholder).

**What it exercises:** three composers: one with user/assistant bubbles and a
`glob_file_search` toolFormerData bubble, an empty draft, and a `_v: 16`
composer whose `todo_write` tool result is an object rather than a string.

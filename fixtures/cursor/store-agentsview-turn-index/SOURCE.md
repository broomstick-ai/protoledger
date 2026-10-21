# store-agentsview-turn-index

**Origin:** derived from `setupCursorStoreFixture` in `internal/parser/cursor_store_test.go`
of https://github.com/kenn-io/agentsview at `905a5fe2ee7c01fb55f620d9ce4e4728aed177ae`
(MIT): the same rows written to a `store.db`. agentsview modelled it on one
capture from a newer (~2026.09) cursor-agent.

**License:** MIT.

**Sanitisation:** none needed (test data). Note its blob ids are sha256 of
labels, not of the data as Cursor writes them.

**What it exercises:** a root with no field-1 message list but a field-8 turn
index of protobuf turns (user, reasoning, assistant blobs with millisecond
stamps), read by the importer with agentsview's provisional field numbers.

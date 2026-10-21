# store-specstory-layout

**Origin:** synthetic, written for this repository on 2026-10-16 following the
layout of specstoryai/getspecstory's `specstory-cli/pkg/providers/cursorcli/reconstruct.go`
(https://github.com/specstoryai/getspecstory at `f5e1c728c1acb9ecc24553a61cf75fc2a46c3b97`,
Apache-2.0), whose authors report that `cursor-agent --resume` loads stores
built this way. `meta.json` follows the shape in S2thend/cursor-history's test
fixtures (MIT, `c7f88062`). No Cursor code or user data.

**License:** MIT OR Apache-2.0 (this repository).

**What it exercises:** a `~/.cursor/chats/<md5(cwd)>/<agentId>/` chat directory:
`store.db` (`meta` row `"0"` = hex JSON, `blobs` with sha256 ids, a root whose
field 1 lists the user/assistant message blobs and field 2 the title/empty/
preview subtree) and `meta.json`.

# Source

Synthetic, written for protoledger by hand (2026-10-16). It models native-subagent-compaction.
No real transcript content; no license restrictions (MIT OR Apache-2.0, like the crate).

What it exercises: a harness-native subagent (child ledger with `parent.relation = subagent`,
`subagent` start/end events); a sealed (signed) reasoning block; an image; a `file_change`;
compaction boundary + summary with a preserved event; an erroring tool result; an API error.

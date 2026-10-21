# Source

Synthetic, written for protoledger by hand (2026-10-18). It models a rewind
(SPEC §2.3): after turn `t2` (a tool call and its answer) the conversation goes
back to the end of turn `t1`, and event `rewind-1:u:3` continues from
`rewind-1:a:1` with `after`. The four events of `t2` stay in the file as an
abandoned branch; the active path is `u:1, a:1, u:3, a:3`.
No real transcript content; no license restrictions (MIT OR Apache-2.0, like the crate).

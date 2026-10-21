# Source

Synthetic, written for protoledger by hand (2026-10-16). It models orchestrated-peers.
No real transcript content; no license restrictions (MIT OR Apache-2.0, like the crate).

What it exercises: an orchestrator that disables harness subagents and runs **peer agents** instead
(roots with `spawned-by` and `member-of` links, never `parent`); network messages (`peer_message`);
a daemon-wrapped user prompt whose unwrapped text, headers and queue id live in `meta`;
an injected system context line; full `settings` snapshots with a system prompt; app events;
a title; usage; turn outcomes including an interrupted turn after a rate-limit `error`.

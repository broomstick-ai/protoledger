//! Live sync against a simulated harness (docs/LIVE-TESTS.md): a session
//! projected (or attached) into a temporary directory, a simulator writing to
//! it what the real harness writes, at the pace the real harness writes it,
//! and the real sync engine following it with a [`Follower`] on its default
//! options (watcher on, poll 1 s, debounce 150 ms). The live-sync property is asserted
//! on what the ledger holds: every event the turn made is there exactly once,
//! with its exact native record, the last of them within 2 s of the turn
//! ending, and no sync made while a message (or a line) was half written led
//! to a divergence or a whole re-import, up to one poll period (1 s) past the
//! last event.
//!
//! Nothing here runs a harness, logs in, or reads anything under the real
//! home directory: every path the tests resolve or write is under their own
//! temporary directory, and each test asserts so ([`common::Paths`]). The
//! shapes the simulator writes come from committed fixtures, named in each
//! module.
//!
//! `PL_SIM_FORCE` forces a guard red (never set in CI):
//!
//! | value | what it does | what must fail |
//! |---|---|---|
//! | `no-follower` | nothing follows the binding | "once each" (0 times) |
//! | `late-follower` | the follower stops for 2.5 s when the turn ends | the 2 s latency bound |
//! | `old-usage` | the Claude Code simulator writes partial counts on the early lines of a message (usage on a line that does not settle it) | the usage the ledger books |
//! | `home-path` | one path under the real `$HOME` is handed to the population check (computed only; nothing is written there) | the isolation check |
//! | `late-divergence` | once every event is in, the location is changed so the next sync finds it diverged (Claude Code: its last line dropped; opencode: the prompt row deleted) | "no divergence", which holds only because the follower steps a poll period past the last event |
//! | `short-hold` | the Claude Code simulator completes the split line at once | "syncs while a line was half written" (0 of them) |
//!
//! [`Follower`]: protoledger::sync::Follower
#![cfg(all(feature = "sync", feature = "sqlite"))]

// The store modes (plain, and compressed three ways): every simulated
// turn runs in each (`<test>::no_dictionary`, `::passed_dictionary`,
// `::retrained`).
#[macro_use]
#[path = "modes/mod.rs"]
mod modes;

#[path = "sim_harness/claude.rs"]
mod claude;
#[path = "sim_harness/common.rs"]
mod common;
#[path = "sim_harness/opencode.rs"]
mod opencode;
#[path = "sim_harness/redact.rs"]
mod redact;
#[path = "sim_harness/reproject.rs"]
mod reproject;
#[path = "sim_harness/rewind.rs"]
mod rewind;

//! protoledger: a harness-neutral, append-only ledger for coding-agent threads.
//!
//! See `SPEC.md` for the format. This crate provides the data model
//! ([`model`]), ledger files and sets ([`ledger`]), format adapters
//! ([`formats`]), carriers for exact round trips ([`carry`]), message
//! delivery state ([`delivery`], SPEC §11.3), and the loss
//! comparison the test suite is built on ([`loss`], [`roundtrip`]), and with
//! the `store` feature a directory of ledgers with a rebuildable index (`store`).

pub mod carry;
pub mod delivery;
pub mod formats;
pub mod ledger;
pub mod loss;
pub mod model;
pub mod roundtrip;
#[cfg(feature = "serve")]
pub mod serve;
#[cfg(feature = "store")]
pub mod store;
#[cfg(feature = "sync")]
pub mod sync;
pub mod util;

pub use delivery::{Delivery, DeliveryError};
pub use ledger::{Appended, Appender, Ledger, LedgerSet, read_at, tail};
pub use model::{Agent, Audience, Event, Header, Kind, Link, Native, Parent, Relation, Source};
#[cfg(feature = "store")]
pub use store::Store;

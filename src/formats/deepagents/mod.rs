//! LangChain Deep Agents threads: the LangGraph SQLite checkpointer store
//! that `deepagents-code` (`dcode`, formerly `deepagents-cli`) keeps at
//! `~/.deepagents/.state/sessions.db` (`~/.deepagents/sessions.db` before
//! cli 0.0.49).
//!
//! A thread is not a transcript but a chain of graph checkpoints. The
//! conversation is rebuilt the way LangGraph does it: along the latest
//! checkpoint's parent chain, seeded by any `messages` value held in a
//! checkpoint (full lists before deepagents 0.6, delta snapshots after) and
//! replayed from the `writes` rows of the `messages` and `files` channels.
//! Each `(thread_id, checkpoint_ns)` is one ledger; `tools:<task>`
//! namespaces are subagents. See `docs/formats/deepagents.md`.

#[cfg(feature = "sqlite")]
pub(crate) mod msgpack;

use crate::formats::{Detection, ExportOptions, Exported, Format, ImportOptions, Imported, Variant};
use crate::ledger::LedgerSet;
use anyhow::Result;
#[cfg(not(feature = "sqlite"))]
use anyhow::bail;
use std::path::Path;

pub(crate) const FORMAT: &str = "deepagents";

pub struct DeepAgents;

static VARIANTS: &[Variant] = &[
    Variant {
        name: "checkpoint-full",
        versions: "deepagents-cli 0.0.13 - deepagents 0.5.x (langgraph < 1.2)",
        import: true,
        export: false,
        notes: "messages/files/todos stored whole in every checkpoint",
    },
    Variant {
        name: "checkpoint-delta",
        versions: "deepagents >= 0.6.0 / deepagents-code (langgraph >= 1.2, checkpoint-sqlite >= 3.1)",
        import: true,
        export: true,
        notes: "messages/files as DeltaChannel writes, snapshots every 50 updates; writes.task_path from checkpoint-sqlite 3.1.2",
    },
];

impl Format for DeepAgents {
    fn name(&self) -> &'static str {
        FORMAT
    }
    fn describe(&self) -> &'static str {
        "LangChain Deep Agents threads (LangGraph SQLite checkpoints, ~/.deepagents/.state/sessions.db)"
    }
    fn variants(&self) -> &'static [Variant] {
        VARIANTS
    }
    fn detect(&self, input: &Path) -> Option<Detection> {
        #[cfg(feature = "sqlite")]
        {
            store::detect(input)
        }
        #[cfg(not(feature = "sqlite"))]
        {
            let _ = input;
            None
        }
    }
    fn import(&self, input: &Path, opts: &ImportOptions) -> Result<Imported> {
        // SPEC §3.1.6: user or assistant text holding the peer-message wrapper is that peer message.
        let mut im = import_records(input, opts)?;
        crate::formats::pi_session::unwrap_peers(&mut im.set, self.name());
        Ok(im)
    }
    fn export(&self, set: &LedgerSet, opts: &ExportOptions) -> Result<Exported> {
        #[cfg(feature = "sqlite")]
        {
            store::export(set, opts)
        }
        #[cfg(not(feature = "sqlite"))]
        {
            let _ = (set, opts);
            bail!("deepagents: needs the `sqlite` feature")
        }
    }
}

/// What [`Format::import`] reads before peer messages are unwrapped.
fn import_records(input: &Path, opts: &ImportOptions) -> Result<Imported> {
    #[cfg(feature = "sqlite")]
    {
        store::import(input, opts)
    }
    #[cfg(not(feature = "sqlite"))]
    {
        let _ = (input, opts);
        bail!("deepagents: needs the `sqlite` feature")
    }
}

#[cfg(feature = "sqlite")]
mod store;

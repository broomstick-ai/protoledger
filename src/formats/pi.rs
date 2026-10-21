//! pi coding agent sessions (`badlogic/pi-mono`, now `earendil-works/pi`):
//! `~/.pi/agent/sessions/--<cwd>--/<timestamp>_<session id>.jsonl`.
//!
//! The format is shared with omp; see `formats::pi_session` (private) and
//! `docs/formats/pi.md`.

use super::pi_session::{self, Dialect};
use crate::formats::{Detection, ExportOptions, Exported, Format, ImportOptions, Imported, Variant};
use crate::ledger::LedgerSet;
use anyhow::Result;
use std::path::Path;

pub struct Pi;

const VARIANTS: &[Variant] = &[
    Variant {
        name: "v1",
        versions: "pi-coding-agent < 0.31.0",
        import: true,
        export: false,
        notes: "linear: entries have no id/parentId; compaction keeps firstKeptEntryIndex; model settings on the header",
    },
    Variant {
        name: "v2",
        versions: ">= 0.31.0, < 0.35.0",
        import: true,
        export: false,
        notes: "tree: id/parentId on every entry; extension messages use role hookMessage",
    },
    Variant {
        name: "v3",
        versions: ">= 0.35.0 (current, 1.x)",
        import: true,
        export: true,
        notes: "hookMessage renamed custom; later additive entries: session_info (0.44), usage and system messages (0.86), context_edit (0.87)",
    },
];

impl Format for Pi {
    fn name(&self) -> &'static str {
        "pi"
    }
    fn describe(&self) -> &'static str {
        "pi coding agent session trees (~/.pi/agent/sessions/**/*.jsonl)"
    }
    fn variants(&self) -> &'static [Variant] {
        VARIANTS
    }
    fn detect(&self, input: &Path) -> Option<Detection> {
        pi_session::detect(Dialect::Pi, input)
    }
    fn import(&self, input: &Path, opts: &ImportOptions) -> Result<Imported> {
        pi_session::import(Dialect::Pi, input, opts)
    }
    fn export(&self, set: &LedgerSet, opts: &ExportOptions) -> Result<Exported> {
        pi_session::export(Dialect::Pi, set, opts)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn detects_pi_over_omp_for_a_plain_session() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("2026-01-01T00-00-00-000Z_s.jsonl");
        std::fs::write(
            &p,
            concat!(
                r#"{"type":"session","version":3,"id":"s","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/x"}"#,
                "\n",
                r#"{"type":"message","id":"a","parentId":null,"timestamp":"2026-01-01T00:00:01.000Z","message":{"role":"user","content":"hi","timestamp":1}}"#,
                "\n",
            ),
        )
        .unwrap();
        let (f, d) = crate::formats::detect(&p).unwrap();
        assert_eq!((f.name(), d.variant), ("pi", "v3"));
    }
}

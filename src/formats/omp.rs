//! oh-my-pi (`can1357/oh-my-pi`) sessions:
//! `~/.omp/agent/sessions/<cwd bucket>/<timestamp>_<session id>.jsonl`, with
//! subagent transcripts at `<timestamp>_<session id>/<AgentId>.jsonl`.
//!
//! omp is a pi fork and shares its session tree; see `formats::pi_session` (private) and
//! `docs/formats/omp.md`.

use super::pi_session::{self, Dialect};
use crate::formats::{Detection, ExportOptions, Exported, Format, ImportOptions, Imported, Variant};
use crate::ledger::LedgerSet;
use anyhow::Result;
use std::path::Path;

pub struct Omp;

const VARIANTS: &[Variant] = &[
    Variant { name: "header-first", versions: "omp < 16.2.3", import: true, export: true, notes: "pi-shaped session v3 file starting with the session header" },
    Variant {
        name: "title-slot",
        versions: ">= 16.2.3 (current, 18.x)",
        import: true,
        export: true,
        notes: "a fixed-width 256-byte `title` record precedes the header; entries carry runtime.version from 18.11.0",
    },
];

impl Format for Omp {
    fn name(&self) -> &'static str {
        "omp"
    }
    fn describe(&self) -> &'static str {
        "oh-my-pi session trees with subagent transcripts (~/.omp/agent/sessions/**/*.jsonl)"
    }
    fn variants(&self) -> &'static [Variant] {
        VARIANTS
    }
    fn detect(&self, input: &Path) -> Option<Detection> {
        pi_session::detect(Dialect::Omp, input)
    }
    fn import(&self, input: &Path, opts: &ImportOptions) -> Result<Imported> {
        pi_session::import(Dialect::Omp, input, opts)
    }
    fn export(&self, set: &LedgerSet, opts: &ExportOptions) -> Result<Exported> {
        pi_session::export(Dialect::Omp, set, opts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_omp_by_its_title_slot() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.jsonl");
        let slot = pi_session::title_slot_line("", "2026-01-01T00:00:00.000Z");
        std::fs::write(&p, format!("{slot}{}\n", r#"{"type":"session","version":3,"id":"s","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/x"}"#)).unwrap();
        let (f, d) = crate::formats::detect(&p).unwrap();
        assert_eq!((f.name(), d.variant), ("omp", "title-slot"));
    }
}

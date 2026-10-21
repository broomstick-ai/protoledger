//! protoledger itself, as a format: a ledger file or a directory of ledgers.
//! Import and export are the identity, so `convert` can start or end here and
//! natively authored ledgers can be pushed through every other format.

use super::{Detection, ExportOptions, Exported, Format, ImportOptions, Imported, Losses, Output, Variant};
use crate::ledger::{LedgerSet, file_safe};
use anyhow::Result;
use std::io::{BufRead, BufReader};
use std::path::Path;

pub struct Protoledger;

const VARIANTS: &[Variant] = &[Variant { name: "ledger-set", versions: "0.1", import: true, export: true, notes: "one <agent>.jsonl per agent" }];

fn is_ledger_file(p: &Path) -> bool {
    if p.extension().is_none_or(|e| e != "jsonl") {
        return false;
    }
    let Ok(f) = std::fs::File::open(p) else { return false };
    let mut line = String::new();
    if BufReader::new(f).read_line(&mut line).is_err() {
        return false;
    }
    serde_json::from_str::<serde_json::Value>(&line).ok().is_some_and(|v| v.get("protoledger").is_some_and(|x| x.is_string()) && v.get("agent").is_some())
}

impl Format for Protoledger {
    fn name(&self) -> &'static str {
        "protoledger"
    }
    fn describe(&self) -> &'static str {
        "protoledger ledger files (this crate's own format)"
    }
    fn variants(&self) -> &'static [Variant] {
        VARIANTS
    }
    fn detect(&self, input: &Path) -> Option<Detection> {
        let hit =
            if input.is_file() { is_ledger_file(input) } else { std::fs::read_dir(input).ok()?.filter_map(|e| e.ok()).any(|e| is_ledger_file(&e.path())) };
        hit.then_some(Detection { variant: "ledger-set", confidence: 100, version: Some(crate::model::SPEC_VERSION.into()) })
    }
    fn import(&self, input: &Path, _opts: &ImportOptions) -> Result<Imported> {
        Ok(Imported { set: LedgerSet::read_path(input)?, losses: Losses::default(), variant: "ledger-set".into() })
    }
    fn export(&self, set: &LedgerSet, _opts: &ExportOptions) -> Result<Exported> {
        let mut output = Output::default();
        for l in set.iter() {
            output.put(format!("{}.jsonl", file_safe(l.id())), l.to_jsonl());
        }
        Ok(Exported { output, losses: Losses::default(), variant: "ledger-set".into() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::Ledger;
    use crate::model::{Agent, Event, Kind, Native, Obj};

    /// A native record is kept as its exact text through the protoledger
    /// format itself: import holds it byte for byte and export writes the
    /// ledger line unchanged (SPEC §4).
    #[test]
    fn native_records_pass_through_as_spelled() {
        let rec = "{\"z\": \"caf\\u00e9\",\"n\":1e3,\"x\":0.000005,\"a\":1.50}";
        let mut l = Ledger::new(Agent { id: "a1".into(), created_at: None, source: None, parent: None, links: vec![], meta: Obj::new() });
        let mut e = Event::new("e1", Kind::Native);
        e.native = Some(Native::new("codex", Some("r1".into())).record_raw(rec).unwrap());
        l.append(e).unwrap();
        let text = l.to_jsonl();
        assert!(text.contains(rec), "{text}");
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("a1.jsonl");
        std::fs::write(&p, &text).unwrap();
        let set = Protoledger.import(&p, &ImportOptions::standard()).unwrap().set;
        let held = set.iter().next().unwrap().events[0].native.as_ref().unwrap().record_text().map(str::to_string);
        assert_eq!(held.as_deref(), Some(rec));
        let out = Protoledger.export(&set, &ExportOptions::standard()).unwrap();
        assert_eq!(std::str::from_utf8(&out.output.files["a1.jsonl"]).unwrap(), text);
    }
}

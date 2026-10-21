//! Round-trip drivers (SPEC §7.1): self trips, cross trips and return trips,
//! each reported as byte-level file differences plus a neutral `Diff`.

use crate::formats::{ExportOptions, Format, ImportOptions, Losses, Output};
use crate::ledger::LedgerSet;
use crate::loss::{CompareOptions, Diff, compare};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::Path;
use walkdir::WalkDir;

/// Every file under `root` (or `root` itself), keyed relative to the input
/// root the way exporters key their output.
pub fn read_tree(root: &Path) -> Result<Output> {
    let mut out = Output::default();
    if root.is_file() {
        let name = root.file_name().unwrap().to_string_lossy().to_string();
        out.put(name, std::fs::read(root)?);
        return Ok(out);
    }
    for e in WalkDir::new(root).sort_by_file_name() {
        let e = e?;
        if !e.file_type().is_file() {
            continue;
        }
        let rel = e.path().strip_prefix(root)?.to_string_lossy().replace('\\', "/");
        // Fixture bookkeeping is not part of the native data.
        if rel == "SOURCE.md" || rel == "expect.json" || rel.starts_with('.') {
            continue;
        }
        out.put(rel, std::fs::read(e.path())?);
    }
    Ok(out)
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FileDiff {
    pub exact: Vec<String>,
    pub differ: Vec<String>,
    pub missing: Vec<String>,
    pub extra: Vec<String>,
    /// Files holding a native record marked `altered` (SPEC §4): whatever their
    /// bytes, they are not the source's, so neither exact nor a difference.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub altered: Vec<String>,
}

/// Where the `altered` native records of `set` live, among `original`'s files.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AlteredFiles {
    /// Files an altered record names (its `native.file`, or the only file).
    pub files: BTreeSet<String>,
    /// An altered record whose file cannot be named: then no file is exact.
    pub unplaced: bool,
}

impl AlteredFiles {
    /// The files of `original` (a `format` source set) that hold an altered
    /// `format` record of `set`.
    pub fn of(set: &LedgerSet, format: &str, original: &Output) -> AlteredFiles {
        let mut a = AlteredFiles::default();
        let only = (original.files.len() == 1).then(|| original.files.keys().next().unwrap().clone());
        for n in set.iter().flat_map(|l| l.events.iter()).filter_map(|e| e.native.as_ref()).filter(|n| n.altered && n.format == format) {
            match n.file.clone().or_else(|| only.clone()) {
                Some(f) => {
                    a.files.insert(f);
                }
                None => a.unplaced = true,
            }
        }
        a
    }
    pub fn union(mut self, o: AlteredFiles) -> AlteredFiles {
        self.files.extend(o.files);
        self.unplaced |= o.unplaced;
        self
    }
}

impl FileDiff {
    /// Byte comparison of every file; nothing is known to be altered.
    pub fn of(original: &Output, produced: &Output) -> FileDiff {
        FileDiff::of_altered(original, produced, &AlteredFiles::default())
    }

    /// Byte comparison of every file. A file holding an altered record is
    /// `altered` whether or not its bytes match; with an altered record no file
    /// can be named for, no file is `exact` (one that matches is `altered`).
    pub fn of_altered(original: &Output, produced: &Output, altered: &AlteredFiles) -> FileDiff {
        let mut d = FileDiff::default();
        for (k, v) in &original.files {
            match produced.files.get(k) {
                Some(_) if altered.files.contains(k) => d.altered.push(k.clone()),
                Some(p) if p == v && altered.unplaced => d.altered.push(k.clone()),
                Some(p) if p == v => d.exact.push(k.clone()),
                Some(_) => d.differ.push(k.clone()),
                None => d.missing.push(k.clone()),
            }
        }
        for k in produced.files.keys() {
            if !original.files.contains_key(k) {
                d.extra.push(k.clone());
            }
        }
        d
    }
    /// Every file came back byte for byte, and none holds an altered record.
    pub fn is_exact(&self) -> bool {
        self.differ.is_empty() && self.missing.is_empty() && self.extra.is_empty() && self.altered.is_empty()
    }
}

/// Materialise an export somewhere an importer can read it. A single-file
/// export is returned as that file's path, a multi-file one as the directory.
pub fn materialise(out: &Output, dir: &Path) -> Result<std::path::PathBuf> {
    out.write_scratch(dir)?;
    if out.files.len() == 1 {
        let only = out.files.keys().next().unwrap();
        if !only.contains('/') {
            return Ok(dir.join(only));
        }
    }
    Ok(dir.to_path_buf())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelfTrip {
    pub variant: String,
    pub files: FileDiff,
    /// Neutral + native comparison of import(export(import(F))) with import(F).
    pub reimport: Diff,
    pub import_losses: Losses,
    pub export_losses: Losses,
}

/// X → P → X.
pub fn self_trip(f: &dyn Format, input: &Path) -> Result<SelfTrip> {
    let original = read_tree(input)?;
    let p0 = f.import(input, &ImportOptions::standard()).with_context(|| format!("{} import", f.name()))?;
    let out = f.export(&p0.set, &ExportOptions::standard()).with_context(|| format!("{} export", f.name()))?;
    let files = FileDiff::of_altered(&original, &out.output, &AlteredFiles::of(&p0.set, f.name(), &original));
    let tmp = tempfile::tempdir()?;
    let path = materialise(&out.output, tmp.path())?;
    let p1 = f.import(&path, &ImportOptions::standard()).with_context(|| format!("{} re-import", f.name()))?;
    Ok(SelfTrip {
        variant: p0.variant,
        files,
        reimport: compare(&p0.set, &p1.set, CompareOptions::default()),
        import_losses: p0.losses.sorted(),
        export_losses: out.losses.sorted(),
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrossTrip {
    pub from: String,
    pub via: String,
    pub carry: bool,
    /// import_Y(export_Y(P0)) against P0, natives ignored.
    pub there: Diff,
    /// export_X(that) against the original files.
    pub back_files: FileDiff,
    /// import_X(export_X(import_Y(export_Y(P0)))) against P0, natives compared.
    pub back: Diff,
    pub export_losses: Losses,
}

/// X → P → Y → P → X.
pub fn cross_trip(x: &dyn Format, input: &Path, y: &dyn Format, carry: bool) -> Result<CrossTrip> {
    let original = read_tree(input)?;
    let p0 = x.import(input, &ImportOptions::standard())?.set;
    let eopts = if carry { ExportOptions::standard() } else { ExportOptions::no_carry() };
    let iopts = ImportOptions { variant: None, restore_carried: carry };
    let ey = y.export(&p0, &eopts).with_context(|| format!("{} export", y.name()))?;
    let t1 = tempfile::tempdir()?;
    let py = materialise(&ey.output, t1.path())?;
    let p1 = y.import(&py, &iopts).with_context(|| format!("{} import of its own export", y.name()))?.set;
    let there = compare(&p0, &p1, CompareOptions { ignore_native: true, ..Default::default() });
    let ex = x.export(&p1, &eopts).with_context(|| format!("{} export after {}", x.name(), y.name()))?;
    let altered = AlteredFiles::of(&p0, x.name(), &original).union(AlteredFiles::of(&p1, x.name(), &original));
    let back_files = FileDiff::of_altered(&original, &ex.output, &altered);
    let t2 = tempfile::tempdir()?;
    let px = materialise(&ex.output, t2.path())?;
    let p2 = x.import(&px, &ImportOptions::standard())?.set;
    let back = compare(&p0, &p2, CompareOptions::default());
    let mut export_losses = ey.losses;
    export_losses.extend(ex.losses);
    Ok(CrossTrip { from: x.name().into(), via: y.name().into(), carry, there, back_files, back, export_losses: export_losses.sorted() })
}

/// Convenience: import with detection.
pub fn import_auto(input: &Path) -> Result<(&'static dyn Format, LedgerSet)> {
    let (f, _) = crate::formats::detect(input).ok_or_else(|| anyhow::anyhow!("{}: no format recognised it", input.display()))?;
    Ok((f, f.import(input, &ImportOptions::standard())?.set))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::Ledger;
    use crate::model::{Agent, Event, Kind, Native, Obj};

    fn out(files: &[(&str, &str)]) -> Output {
        let mut o = Output::default();
        for (k, v) in files {
            o.put(*k, v.as_bytes().to_vec());
        }
        o
    }

    fn set_with(natives: Vec<Native>) -> LedgerSet {
        let mut l = Ledger::new(Agent { id: "a".into(), created_at: None, source: None, parent: None, links: vec![], meta: Obj::new() });
        for (i, n) in natives.into_iter().enumerate() {
            l.append(Event::new(format!("e{i}"), Kind::Native).native(n)).unwrap();
        }
        LedgerSet::single(l)
    }

    fn native(format: &str, file: Option<&str>, altered: bool) -> Native {
        let mut n = Native::new(format, Some("1".into())).record_raw(r#"{"k":"[redacted]"}"#).unwrap().file(file);
        n.altered = altered;
        n
    }

    #[test]
    fn file_holding_an_altered_record_is_altered_not_exact() {
        let original = out(&[("s.jsonl", "{\"k\":\"[redacted]\"}\n")]);
        let set = set_with(vec![native("fmt", None, true)]);
        let altered = AlteredFiles::of(&set, "fmt", &original);
        // The same bytes came back: still not exact, because they are not the source's.
        let d = FileDiff::of_altered(&original, &original.clone(), &altered);
        assert_eq!((d.exact.clone(), d.altered.clone()), (vec![], vec!["s.jsonl".to_string()]), "{d:?}");
        assert!(!d.is_exact());
        // Different bytes: altered, not a difference (a loss).
        let d = FileDiff::of_altered(&original, &out(&[("s.jsonl", "{\"k\":\"s3cret\"}\n")]), &altered);
        assert_eq!((d.differ.clone(), d.altered.clone()), (vec![], vec!["s.jsonl".to_string()]), "{d:?}");
        assert!(!d.is_exact());
    }

    #[test]
    fn respelled_file_without_an_altered_record_is_not_exact() {
        // Nothing altered: the comparison is bytes, so a file that differs only in
        // spelling (same JSON: key order, whitespace) differs, and only identical bytes are exact.
        let original = out(&[("s.jsonl", "{\"b\":1,\"a\":2}\n"), ("t.jsonl", "{\"x\":1}\n")]);
        let set = set_with(vec![native("fmt", None, false)]);
        let altered = AlteredFiles::of(&set, "fmt", &original);
        assert_eq!(altered, AlteredFiles::default());
        let d = FileDiff::of_altered(&original, &out(&[("s.jsonl", "{\"a\":2, \"b\":1}\n"), ("t.jsonl", "{\"x\":1}\n")]), &altered);
        assert_eq!((d.exact.clone(), d.differ.clone()), (vec!["t.jsonl".to_string()], vec!["s.jsonl".to_string()]), "{d:?}");
        assert!(d.altered.is_empty() && !d.is_exact());
    }

    #[test]
    fn altered_records_are_placed_by_file_and_format() {
        let original = out(&[("a.json", "1"), ("b.json", "2")]);
        // Named file: only that one is altered; the other is still exact.
        let named = AlteredFiles::of(&set_with(vec![native("fmt", Some("b.json"), true)]), "fmt", &original);
        let d = FileDiff::of_altered(&original, &original.clone(), &named);
        assert_eq!((d.exact.clone(), d.altered.clone()), (vec!["a.json".to_string()], vec!["b.json".to_string()]));
        // No file named among several: no file may be exact; a differing one stays a difference.
        let unplaced = AlteredFiles::of(&set_with(vec![native("fmt", None, true)]), "fmt", &original);
        assert!(unplaced.unplaced && unplaced.files.is_empty());
        let d = FileDiff::of_altered(&original, &out(&[("a.json", "1"), ("b.json", "3")]), &unplaced);
        assert_eq!((d.exact.clone(), d.altered.clone(), d.differ.clone()), (vec![], vec!["a.json".to_string()], vec!["b.json".to_string()]));
        // Another format's altered record says nothing about these files.
        assert_eq!(AlteredFiles::of(&set_with(vec![native("other", None, true)]), "fmt", &original), AlteredFiles::default());
    }
}

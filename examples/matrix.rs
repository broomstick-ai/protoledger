//! The transfer matrix: for every fixture of every implemented format X and
//! every target Y among all formats (protoledger included), how much of X's
//! thread survives X → protoledger → Y → protoledger, and whether protoledger
//! itself holds everything X had.
//!
//! ```sh
//! cargo run --release --example matrix -- --out docs/
//! ```
//!
//! Writes `<out>/MATRIX.md` and `<out>/matrix.svg`. Read-only over `fixtures/`.
//! Re-run it whenever fixtures or adapters change; nothing in it is pinned to
//! today's results. Dev aids: `MATRIX_ONLY=<format>[/<case>]` runs some
//! fixtures (to a scratch `--out`), `MATRIX_DEBUG=1` prints SQLite row
//! mismatches of the self trip, `MATRIX_EXPLAIN=<target>` prints each
//! no-carry comparison through that target.
//!
//! What it measures, per fixture and pair:
//!
//! - **facts** in the source: every non-`native` event of `import_X(F)` counts
//!   1 plus its non-empty body fields (the weighting `Diff::missing_facts`
//!   uses; an empty `settings` event counts 0, as it does there).
//! - **lost** without carry, from the `cross_trip` "there" comparison
//!   (`import_Y(export_Y(P0))` against `P0`, natives ignored): only what went
//!   missing or changed, `missing_facts` plus changed body fields and kinds.
//!   What the target ADDED (`Diff::extra`, extra agents) is never a loss; it
//!   is reported on its own as **added**. Every other change (ts, group, turn,
//!   audience, meta, ordering, `Diff::header`, links included) is reported as
//!   **metadata changed**. For Y = X and Y = protoledger the self trip's
//!   re-import comparison is used.
//! - **carry exact**: whether X → P → Y → P → X with carry reproduced the native
//!   files (bytes; an SQLite file by schema and rows, since its page bytes
//!   cannot be reproduced; for a store in [`ROW_SCOPE`] by rows alone).
//!
//! The library has no row-level SQLite comparison and no public fact count, so
//! both are computed here (`sqlite_rows`, `facts`). The cross trip is re-done
//! here (same steps as `roundtrip::cross_trip`) because that function does not
//! hand back the exported files the row comparison needs.

use protoledger::formats::{self, ExportOptions, Format, ImportOptions, Output};
use protoledger::ledger::LedgerSet;
use protoledger::loss::{CompareOptions, Diff, compare};
use protoledger::model::Kind;
use protoledger::roundtrip::{AlteredFiles, FileDiff, materialise, read_tree};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

const P: &str = "protoledger";
/// Harnesses whose exporters carry protoledger events (the footnote's claim).
const CARRIERS: [&str; 3] = ["claude-code", "codex", "opencode"];
/// SQLite files, by format and file name, that are a larger application's
/// state store, of which the importer's scope is the thread rows (each row tied to a thread, with its
/// key, value bytes and storage class), not the store's table declarations.
/// Their self trip is exact when every row comes back identical with no extra
/// row in any table (one only the export declares included); how the export declares its tables is reported beside the verdict.
const ROW_SCOPE: [(&str, &str); 1] = [("cursor", "state.vscdb")];
/// Is `file` (a path in the format's output) one of [`ROW_SCOPE`]'s stores?
fn row_scoped(format: &str, file: &str) -> bool {
    let name = Path::new(file).file_name().and_then(|n| n.to_str());
    ROW_SCOPE.iter().any(|(f, n)| *f == format && name == Some(*n))
}

fn implemented(f: &dyn Format) -> bool {
    f.variants().iter().any(|v| v.import && v.export)
}

fn is_empty_value(v: &Value) -> bool {
    match v {
        Value::Null | Value::Bool(false) => true,
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
        _ => false,
    }
}

/// Facts in a set, weighted as `Diff::missing_facts` weights a missing event.
fn facts(set: &LedgerSet) -> u64 {
    let mut n = 0;
    for e in set.iter().flat_map(|l| l.events.iter()) {
        if e.kind == Kind::Native {
            continue;
        }
        let f = e.body.values().filter(|v| !is_empty_value(v)).count() as u64;
        if e.kind == Kind::Settings && f == 0 {
            continue;
        }
        n += 1 + f;
    }
    n
}

fn case_input(case: &Path) -> PathBuf {
    let native = case.join("native");
    let entries: Vec<_> = std::fs::read_dir(&native)
        .map(|r| r.filter_map(|e| e.ok()).filter(|e| !e.file_name().to_string_lossy().starts_with('.')).collect())
        .unwrap_or_default();
    if entries.len() == 1 && entries[0].path().is_file() { entries[0].path() } else { native }
}

fn is_sqlite(b: &[u8]) -> bool {
    b.len() >= 16 && &b[..16] == b"SQLite format 3\0"
}

/// Row counts of one SQLite comparison: original rows found identically in
/// the produced file (as a multiset, per table), and whether the schema
/// objects (type, name, sql) match.
#[derive(Default, Clone)]
struct Rows {
    same: u64,
    total: u64,
    extra: u64,
    schema_same: bool,
    /// Schema objects only in the source, then only in the export.
    schema_diff: Vec<String>,
}

#[cfg(feature = "sqlite")]
fn sqlite_rows(a: &[u8], b: &[u8]) -> anyhow::Result<Rows> {
    use rusqlite::{Connection, OpenFlags, types::ValueRef};
    let dir = tempfile::tempdir()?;
    let (pa, pb) = (dir.path().join("a.db"), dir.path().join("b.db"));
    std::fs::write(&pa, a)?;
    std::fs::write(&pb, b)?;
    let ca = Connection::open_with_flags(&pa, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let cb = Connection::open_with_flags(&pb, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    // Every object `sqlite_master` lists is compared: its declaration, and for
    // a table its rows, SQLite's own tables (`sqlite_sequence`, `sqlite_stat*`)
    // and automatic indexes included. No name is excluded. So are the header
    // values an application reads back: its id, its schema version and the
    // text encoding.
    let schema = |c: &Connection| -> anyhow::Result<Vec<(String, String, Option<String>)>> {
        let mut st = c.prepare("SELECT type, name, sql FROM sqlite_master ORDER BY type, name")?;
        let mut v = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<Vec<_>, _>>()?;
        for p in ["application_id", "user_version", "encoding"] {
            let val: rusqlite::types::Value = c.query_row(&format!("PRAGMA {p}"), [], |r| r.get(0))?;
            v.push(("pragma".into(), p.into(), Some(format!("{val:?}"))));
        }
        Ok(v)
    };
    // The tables a file holds rows in, less the shadow tables of its virtual
    // tables, as SQLite itself classifies them (`pragma_table_list` type
    // `shadow`): a shadow table holds the virtual table's index, laid out like
    // pages, and the virtual table's own rows (its content) are compared instead.
    // A virtual table whose module this SQLite lacks fails to read, an error.
    let tables = |c: &Connection| -> anyhow::Result<Vec<String>> {
        let mut st = c.prepare(
            "SELECT name FROM sqlite_master WHERE type='table' AND name NOT IN (SELECT name FROM pragma_table_list WHERE schema='main' AND type='shadow') ORDER BY name",
        )?;
        let v = st.query_map([], |r| r.get(0))?.collect::<Result<Vec<String>, _>>()?;
        Ok(v)
    };
    // Every row of a table, each value tagged with its storage class and
    // spelled exactly (TEXT and BLOB as their raw bytes, in hex). A table that
    // cannot be read is an error, never an empty table.
    let dump = |c: &Connection, t: &str| -> anyhow::Result<Vec<String>> {
        let mut st = c.prepare(&format!("SELECT * FROM \"{}\"", t.replace('"', "\"\"")))?;
        let n = st.column_count();
        let rows = st.query_map([], |r| {
            let mut s = String::new();
            for i in 0..n {
                match r.get_ref(i)? {
                    ValueRef::Text(t) => s.push_str(&format!("T:{}|", hex::encode(t))),
                    ValueRef::Blob(b) => s.push_str(&format!("B:{}|", hex::encode(b))),
                    other => s.push_str(&format!("{other:?}|")),
                }
            }
            Ok(s)
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    };
    let (sa, sb) = (schema(&ca)?, schema(&cb)?);
    let mut r = Rows { schema_same: sa == sb, ..Default::default() };
    let decl = |(ty, name, sql): &(String, String, Option<String>)| match sql {
        Some(q) => format!("{ty} `{name}` `{q}`"),
        None => format!("{ty} `{name}` (created by SQLite, no SQL)"),
    };
    r.schema_diff.extend(sa.iter().filter(|x| !sb.contains(x)).map(|x| format!("source declares {}", decl(x))));
    r.schema_diff.extend(sb.iter().filter(|x| !sa.contains(x)).map(|x| format!("export declares {}", decl(x))));
    let tb = tables(&cb)?;
    let debug = std::env::var_os("MATRIX_DEBUG").is_some();
    let ta = tables(&ca)?;
    for t in &ta {
        let ra = dump(&ca, t)?;
        let mut rb: BTreeMap<String, i64> = BTreeMap::new();
        if tb.contains(t) {
            for row in dump(&cb, t)? {
                *rb.entry(row).or_default() += 1;
            }
        }
        let (before_same, total_t) = (r.same, ra.len() as u64);
        r.total += total_t;
        for row in ra {
            if let Some(c) = rb.get_mut(&row).filter(|c| **c > 0) {
                *c -= 1;
                r.same += 1;
            }
        }
        let same_t = r.same - before_same;
        let extra = rb.values().filter(|c| **c > 0).map(|c| *c as u64).sum::<u64>();
        if debug && (extra > 0 || same_t != total_t) {
            eprintln!(
                "rows: table {t}: {same_t} of {total_t} identical, {extra} extra in export; first extra: {:?}",
                rb.iter().find(|(_, c)| **c > 0).map(|(k, _)| k.chars().take(300).collect::<String>())
            );
        }
        r.extra += extra;
    }
    // A table only the export declares: every row in it is extra (an empty
    // one is a declaration, reported in `schema_diff`).
    for t in tb.iter().filter(|t| !ta.contains(t)) {
        let extra = dump(&cb, t)?.len() as u64;
        if debug && extra > 0 {
            eprintln!("rows: table {t} (only in export): {extra} extra in export");
        }
        r.extra += extra;
    }
    if debug && sa != sb {
        let only_a: Vec<_> = sa.iter().filter(|x| !sb.contains(x)).map(|x| &x.1).collect();
        let only_b: Vec<_> = sb.iter().filter(|x| !sa.contains(x)).collect();
        eprintln!("schema: only in source {only_a:?}; only in export {only_b:?}");
    }
    Ok(r)
}

#[cfg(not(feature = "sqlite"))]
fn sqlite_rows(_: &[u8], _: &[u8]) -> anyhow::Result<Rows> {
    anyhow::bail!("built without the sqlite feature")
}

/// How the native data came back: byte-exact, or (SQLite) row-identical.
#[derive(Default, Clone)]
struct Native {
    bytes_exact: bool,
    /// Every differing file is SQLite in both and row-identical (schema and rows).
    rows_exact: bool,
    /// Row-identical with the table declarations differing, under [`ROW_SCOPE`].
    scoped: bool,
    rows: Option<Rows>,
    /// A file whose rows could not be compared, and why.
    error: Option<String>,
}
impl Native {
    fn exact(&self) -> bool {
        self.bytes_exact || self.rows_exact
    }
}

fn native_compare(format: &str, original: &Output, produced: &Output, altered: &AlteredFiles) -> Native {
    let d = FileDiff::of_altered(original, produced, altered);
    if d.is_exact() {
        return Native { bytes_exact: true, ..Default::default() };
    }
    let mut ok = d.missing.is_empty() && d.extra.is_empty() && d.altered.is_empty() && !d.differ.is_empty();
    let mut rows: Option<Rows> = None;
    let mut error: Option<String> = None;
    for k in &d.differ {
        let (a, b) = (&original.files[k], &produced.files[k]);
        if !(is_sqlite(a) && is_sqlite(b)) {
            ok = false;
            continue;
        }
        match sqlite_rows(a, b) {
            Ok(r) => {
                ok &= (r.schema_same || row_scoped(format, k)) && r.same == r.total && r.extra == 0;
                let acc = rows.get_or_insert_with(|| Rows { schema_same: true, ..Default::default() });
                acc.same += r.same;
                acc.total += r.total;
                acc.extra += r.extra;
                acc.schema_same &= r.schema_same;
                acc.schema_diff.extend(r.schema_diff);
            }
            Err(e) => {
                ok = false;
                error.get_or_insert(format!("`{k}` could not be compared: {e:#}"));
            }
        }
    }
    let scoped = ok && rows.as_ref().is_some_and(|r| !r.schema_same);
    Native { bytes_exact: false, rows_exact: ok, scoped, rows, error }
}

/// One comparison split three ways, so the % counts the same things in its
/// numerator and denominator: facts **lost** (`missing_facts`, plus changed
/// body fields `<kind>.body.<field>` and kind changes `<kind>.kind`), records
/// **added** (`Diff::extra`, extra agents), and **metadata changed** (every
/// other `Diff::changed` entry: ts, group, turn, audience, meta, ordering,
/// branch; plus `Diff::header`). Only `lost` enters the %.
#[derive(Default, Clone, Copy)]
struct Split {
    lost: u64,
    added: u64,
    meta: u64,
}
fn split(d: &Diff) -> Split {
    let is_fact = |k: &str| k.contains(".body.") || k.ends_with(".kind");
    let changed_facts: u64 = d.changed.iter().filter(|(k, _)| is_fact(k)).map(|(_, v)| v).sum();
    let changed_meta: u64 = d.changed.iter().filter(|(k, _)| !is_fact(k)).map(|(_, v)| v).sum();
    Split {
        lost: d.missing_facts + changed_facts,
        added: d.extra.values().sum::<u64>() + d.agents_extra.len() as u64,
        meta: changed_meta + d.header.values().sum::<u64>(),
    }
}

#[derive(Default, Clone)]
struct SelfResult {
    native: Native,
    lost: u64,
    added: u64,
    meta: u64,
    error: Option<String>,
}

#[derive(Default, Clone)]
struct Cell {
    lost: Option<u64>,
    added: u64,
    meta: u64,
    carry_exact: Option<bool>,
    error: Option<String>,
}

struct CaseResult {
    format: String,
    case: String,
    variant: String,
    facts: u64,
    /// What the importer itself reported it could not carry (`code ×count`).
    import_losses: Vec<String>,
    selft: SelfResult,
    cells: BTreeMap<String, Cell>,
}

fn self_trip(x: &dyn Format, input: &Path, original: &Output, p0: &LedgerSet) -> SelfResult {
    let run = || -> anyhow::Result<SelfResult> {
        let out = x.export(p0, &ExportOptions::standard())?;
        let native = native_compare(x.name(), original, &out.output, &AlteredFiles::of(p0, x.name(), original));
        let t = tempfile::tempdir()?;
        let p = materialise(&out.output, t.path())?;
        let p1 = x.import(&p, &ImportOptions::standard())?.set;
        let sp = split(&compare(p0, &p1, CompareOptions::default()));
        Ok(SelfResult { native, lost: sp.lost, added: sp.added, meta: sp.meta, error: None })
    };
    let _ = input;
    run().unwrap_or_else(|e| SelfResult { error: Some(format!("{e:#}")), ..Default::default() })
}

/// `roundtrip::cross_trip`, step for step. With carry off returns the
/// "there" (lost, added); with carry on, also whether the return trip's files are exact.
fn cross(x: &dyn Format, y: &dyn Format, original: &Output, p0: &LedgerSet, carry: bool) -> anyhow::Result<(Split, bool)> {
    let eopts = if carry { ExportOptions::standard() } else { ExportOptions::no_carry() };
    let iopts = ImportOptions { variant: None, restore_carried: carry };
    let ey = y.export(p0, &eopts).map_err(|e| e.context(format!("{} export", y.name())))?;
    let t1 = tempfile::tempdir()?;
    let py = materialise(&ey.output, t1.path())?;
    let p1 = y.import(&py, &iopts).map_err(|e| e.context(format!("{} import of its own export", y.name())))?.set;
    let diff = compare(p0, &p1, CompareOptions { ignore_native: true, ..Default::default() });
    if !carry && std::env::var("MATRIX_EXPLAIN").is_ok_and(|v| v == y.name()) {
        eprintln!("explain {} → {}: facts {}; {}", x.name(), y.name(), facts(p0), diff.summary());
    }
    let there = split(&diff);
    if !carry {
        return Ok((there, false));
    }
    let ex = x.export(&p1, &eopts).map_err(|e| e.context(format!("{} export after {}", x.name(), y.name())))?;
    let altered = AlteredFiles::of(p0, x.name(), original).union(AlteredFiles::of(&p1, x.name(), original));
    Ok((there, native_compare(x.name(), original, &ex.output, &altered).exact()))
}

/// An error message with scratch paths replaced, so the report is the same on every run.
fn clean_err(e: &anyhow::Error) -> String {
    format!("{e:#}")
        .split(' ')
        .map(|w| match w.find("/.tmp").or_else(|| w.find("/tmp")) {
            Some(_) if w.starts_with('/') => format!("<scratch>/{}", w.rsplit('/').next().unwrap_or("")),
            _ => w.to_string(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn catch<T>(f: impl FnOnce() -> anyhow::Result<T>) -> anyhow::Result<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(p) => {
            let msg = p.downcast_ref::<String>().cloned().or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default();
            anyhow::bail!("panicked: {msg}")
        }
    }
}

fn run_case(x: &'static dyn Format, case: &Path, targets: &[&'static dyn Format]) -> CaseResult {
    let input = case_input(case);
    let name = case.file_name().unwrap().to_string_lossy().to_string();
    let mut r = CaseResult {
        format: x.name().into(),
        case: name,
        variant: String::new(),
        facts: 0,
        import_losses: vec![],
        selft: SelfResult::default(),
        cells: BTreeMap::new(),
    };
    let original = match read_tree(&input) {
        Ok(o) => o,
        Err(e) => {
            r.selft.error = Some(format!("read: {e:#}"));
            return r;
        }
    };
    let p0 = match catch(|| x.import(&input, &ImportOptions::standard())) {
        Ok(i) => {
            r.variant = i.variant;
            r.import_losses = i.losses.sorted().0.iter().map(|l| format!("{} ×{}", l.code, l.count)).collect();
            i.set
        }
        Err(e) => {
            r.selft.error = Some(format!("import: {e:#}"));
            return r;
        }
    };
    r.facts = facts(&p0);
    r.selft = catch(|| Ok(self_trip(x, &input, &original, &p0))).unwrap_or_else(|e| SelfResult { error: Some(format!("{e:#}")), ..Default::default() });
    for y in targets {
        if std::env::var_os("MATRIX_DEBUG").is_some() {
            break;
        }
        if y.name() == x.name() || y.name() == P {
            continue;
        }
        let mut c = Cell::default();
        match catch(|| cross(x, *y, &original, &p0, false)) {
            Ok((sp, _)) => {
                c.lost = Some(sp.lost);
                c.added = sp.added;
                c.meta = sp.meta;
            }
            Err(e) => c.error = Some(clean_err(&e)),
        }
        match catch(|| cross(x, *y, &original, &p0, true)) {
            Ok((_, exact)) => c.carry_exact = Some(exact),
            Err(e) => {
                if c.error.is_none() {
                    c.error = Some(format!("carry: {}", clean_err(&e)));
                }
            }
        }
        r.cells.insert(y.name().into(), c);
    }
    r
}

#[derive(Default, Clone)]
struct Agg {
    fixtures: u64,
    facts: u64,
    lost: u64,
    added: u64,
    meta: u64,
    errors: u64,
    first_error: Option<String>,
    carry_exact: u64,
    carry_runs: u64,
    over: u64,
}
impl Agg {
    fn pct(&self) -> Option<f64> {
        (self.facts > 0).then(|| (1.0 - self.lost as f64 / self.facts as f64).max(0.0) * 100.0)
    }
}

fn main() -> anyhow::Result<()> {
    let start = Instant::now();
    let args: Vec<String> = std::env::args().collect();
    let out = args.iter().position(|a| a == "--out").and_then(|i| args.get(i + 1)).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("docs"));
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let all = formats::all();
    // Columns: protoledger first, then the rest alphabetically.
    let mut cols: Vec<&'static dyn Format> = all.clone();
    cols.sort_by_key(|f| (f.name() != P, f.name()));
    let targets: Vec<&'static dyn Format> = cols.iter().copied().filter(|f| implemented(*f)).collect();
    let mut jobs: Vec<(&'static dyn Format, PathBuf)> = Vec::new();
    for x in &cols {
        if !implemented(*x) {
            continue;
        }
        let dir = root.join("fixtures").join(x.name());
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        let mut cases: Vec<PathBuf> = rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.join("native").is_dir())
            // The conformance suite's rule: `.` and `_` cases are not fixtures.
            .filter(|p| !p.file_name().unwrap().to_string_lossy().starts_with(['.', '_']))
            .collect();
        cases.sort();
        let only = std::env::var("MATRIX_ONLY").ok();
        for c in cases {
            let key = format!("{}/{}", x.name(), c.file_name().unwrap().to_string_lossy());
            if only.as_ref().is_some_and(|o| !key.starts_with(o.as_str())) {
                continue;
            }
            jobs.push((*x, c));
        }
    }
    eprintln!("matrix: {} fixtures × {} targets", jobs.len(), targets.len());
    let queue = Mutex::new(jobs.into_iter().enumerate().collect::<Vec<_>>());
    let results: Mutex<Vec<(usize, CaseResult)>> = Mutex::new(Vec::new());
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                loop {
                    let Some((i, (x, case))) = queue.lock().unwrap().pop() else { break };
                    let r = run_case(x, &case, &targets);
                    results.lock().unwrap().push((i, r));
                }
            });
        }
    });
    let mut results = results.into_inner().unwrap();
    results.sort_by_key(|(i, _)| *i);
    let results: Vec<CaseResult> = results.into_iter().map(|(_, r)| r).collect();

    // Aggregate.
    let names: Vec<&str> = cols.iter().map(|f| f.name()).collect();
    let mut agg: BTreeMap<(String, String), Agg> = BTreeMap::new();
    for r in &results {
        for y in &names {
            let a = agg.entry((r.format.clone(), y.to_string())).or_default();
            a.fixtures += 1;
            let (lost, added, meta, carry, err) = if *y == r.format || *y == P {
                (
                    r.selft.error.is_none().then_some(r.selft.lost),
                    r.selft.added,
                    r.selft.meta,
                    r.selft.error.is_none().then(|| r.selft.native.exact()),
                    r.selft.error.clone(),
                )
            } else if let Some(c) = r.cells.get(*y) {
                (c.lost, c.added, c.meta, c.carry_exact, c.error.clone())
            } else {
                (None, 0, 0, None, Some("target not implemented".into()))
            };
            if let Some(l) = lost {
                a.facts += r.facts;
                a.lost += l;
                a.added += added;
                a.meta += meta;
                a.over += (l > r.facts) as u64;
            }
            if let Some(e) = err {
                a.errors += 1;
                a.first_error.get_or_insert(format!("{}: {}", r.case, e.lines().next().unwrap_or("")));
            }
            if let Some(c) = carry {
                a.carry_runs += 1;
                a.carry_exact += c as u64;
            }
        }
    }
    let elapsed = start.elapsed();
    std::fs::create_dir_all(&out)?;
    let rows: Vec<&str> = names.iter().copied().filter(|x| results.iter().any(|r| r.format == *x)).collect();
    std::fs::write(out.join("matrix.svg"), svg(&rows, &names, &agg, &results))?;
    std::fs::write(out.join("MATRIX.md"), markdown(&rows, &names, &agg, &results))?;
    print_summary(&rows, &names, &agg, &results);
    eprintln!("matrix: wrote {} and {} in {:.1}s", out.join("MATRIX.md").display(), out.join("matrix.svg").display(), elapsed.as_secs_f64());
    Ok(())
}

/// Per source format: (fixtures, byte-exact, row-exact, failed) self trips.
fn self_counts(rows: &[&str], results: &[CaseResult]) -> BTreeMap<String, (u64, u64, u64, Vec<String>)> {
    let mut m: BTreeMap<String, (u64, u64, u64, Vec<String>)> = BTreeMap::new();
    for x in rows {
        let e = m.entry(x.to_string()).or_default();
        for r in results.iter().filter(|r| r.format == *x) {
            e.0 += 1;
            if r.selft.lost == 0 && r.selft.native.bytes_exact {
                e.1 += 1;
            } else if r.selft.lost == 0 && r.selft.native.rows_exact {
                e.2 += 1;
            } else {
                let why = match (&r.selft.error, r.selft.native.rows.as_ref()) {
                    (Some(err), _) => format!("{}: error {}", r.case, err.lines().next().unwrap_or("")),
                    (None, _) if r.selft.native.error.is_some() => format!("{}: {}", r.case, r.selft.native.error.as_deref().unwrap_or("")),
                    (None, Some(rw)) => {
                        format!(
                            "{}: {}/{} rows identical, {} extra, schema {}",
                            r.case,
                            rw.same,
                            rw.total,
                            rw.extra,
                            if rw.schema_same { "same" } else { "differs" }
                        ) + &rw.schema_diff.iter().map(|x| format!("; {x}")).collect::<String>()
                    }
                    (None, None) if r.selft.native.exact() => format!("{}: files exact", r.case),
                    (None, None) => format!("{}: files differ", r.case),
                };
                let why = if r.selft.lost > 0 { format!("{why}; {} facts lost on the re-import", r.selft.lost) } else { why };
                let why = if r.import_losses.is_empty() {
                    format!("{why}; importer reported no loss")
                } else {
                    format!("{why}; importer reported: {}", r.import_losses.join(", "))
                };
                e.3.push(why);
            }
        }
    }
    m
}

fn print_summary(rows: &[&str], names: &[&str], agg: &BTreeMap<(String, String), Agg>, results: &[CaseResult]) {
    let mut pairs: Vec<(f64, String, String)> = Vec::new();
    for x in rows {
        for y in names {
            if *y == *x || *y == P {
                continue;
            }
            if let Some(p) = agg[&(x.to_string(), y.to_string())].pct() {
                pairs.push((p, x.to_string(), y.to_string()));
            }
        }
    }
    pairs.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
    println!("best cross pairs:");
    for (p, x, y) in pairs.iter().take(3) {
        println!("  {x} → {y}: {p:.1}%");
    }
    println!("worst cross pairs:");
    for (p, x, y) in pairs.iter().rev().take(3) {
        println!("  {x} → {y}: {p:.1}%");
    }
    println!("errored pairs:");
    for ((x, y), a) in agg {
        if a.errors > 0 {
            println!("  {x} → {y}: {} errors; {}", a.errors, a.first_error.as_deref().unwrap_or(""));
        }
    }
    println!("self trips:");
    for (x, (n, b, rw, fail)) in self_counts(rows, results) {
        println!("  {x}: {b}/{n} byte-exact, {rw} row-identical, {} not", fail.len());
        for f in fail {
            println!("    {f}");
        }
    }
}

fn fmt_pct(p: f64) -> String {
    if (99.95..100.0).contains(&p) { "99.9".into() } else { format!("{p:.1}") }
}

fn markdown(rows: &[&str], names: &[&str], agg: &BTreeMap<(String, String), Agg>, results: &[CaseResult]) -> String {
    let mut s = String::new();
    let selfc = self_counts(rows, results);
    let total: u64 = selfc.values().map(|v| v.0).sum();
    let exact: u64 = selfc.values().map(|v| v.1 + v.2).sum();
    let _ = writeln!(s, "# Transfer matrix\n");
    let _ = writeln!(
        s,
        "Generated by `cargo run --release --example matrix -- --out docs/` (from `examples/matrix.rs`) over the {} fixtures in `fixtures/`. Do not edit by hand.\n",
        results.len()
    );
    let _ = writeln!(s, "![Transfer matrix: % of facts preserved from each source format to each target format](matrix.svg)\n");
    let _ = writeln!(s, "## How to read it\n");
    let _ = writeln!(
        s,
        "Each row is a source format X, each column a target format Y. A cell is the share of X's facts that survive X → protoledger → Y → protoledger **without carry**, pooled over every fixture of X: 1 − Σ lost / Σ facts. A fact is an event (1) plus each of its non-empty body fields, the weighting `Diff::missing_facts` uses; harness-only `native` records are not counted. *Lost* counts the same things the fact total does: the facts of missing events (`Diff::missing_facts`), each changed body field of a matched event (`Diff::changed` `<kind>.body.<field>`) and each event whose kind changed (`<kind>.kind`), so it never exceeds the facts. Two things are reported beside the %, never in it: what the target **added** (`Diff::extra`: events, or agents, the source did not have), and **metadata changed** (event-level `ts`, `group`, `turn`, `audience`, `meta`, ordering and branch changes, plus agent header fields from `Diff::header`, links included), each in its own table below. A store container ledger, the one an opencode database keeps its session-less rows in or a Slack export's workspace ledger, holds no conversation: a target that cannot hold links loses the `member-of` links that tie the sessions or channels to it, and that is counted under metadata changed, not in the %. The **protoledger column** is the import itself: a cell there reads *lossless* when every fixture of X came home from X → protoledger → X with its native data unchanged, so protoledger held everything X had. The **diagonal** is that same self trip. Without carry a trip through Y keeps only what Y can express; **with carry**, a Claude Code, Codex or opencode export stows the protoledger events in a field the harness tolerates, and the last table shows how often X → P → Y → P → X then returned the source exactly.\n"
    );
    let _ = writeln!(s, "## Lossless into protoledger\n");
    let _ = writeln!(
        s,
        "**{exact} of {total} fixtures, across all {} formats, return unchanged from X → protoledger → X.** The evidence for each format is that self trip: `export_X(import_X(F))` compared with the fixture's own files. A file that is text or JSON must match **byte for byte**. An SQLite store cannot be reproduced byte for byte (page layout, freelist, FTS blocks), so it is compared by **schema and rows**: every object in `sqlite_master` the same (SQLite's own tables and automatic indexes included), the header values an application reads back (`application_id`, `user_version`, the text encoding) the same, and every row of every table present in the export with identical values (text and blobs compared as bytes), with no extra row in any table; the shadow tables SQLite keeps for a virtual table's index are not compared, the virtual table's own rows are; a Cursor `state.vscdb` is compared by its rows alone (see *Cursor's scope* below). Since protoledger is the only thing between the import and the export, whatever came back was in protoledger.\n",
        rows.len()
    );
    let _ = writeln!(s, "| source format | fixtures | byte-exact | row-identical (SQLite) | not exact | lossless into protoledger |");
    let _ = writeln!(s, "|---|---:|---:|---:|---:|---|");
    for x in rows {
        let (n, b, rw, fail) = &selfc[*x];
        let verdict = if fail.is_empty() { format!("yes ({}/{n})", b + rw) } else { format!("**no** ({}/{n})", b + rw) };
        let _ = writeln!(s, "| `{x}` | {n} | {b} | {rw} | {} | {verdict} |", fail.len());
    }
    if rows.iter().any(|x| ROW_SCOPE.iter().any(|(f, _)| f == x)) {
        let _ = writeln!(
            s,
            "\n**Cursor's scope.** A Cursor IDE `state.vscdb` is the whole IDE's state store, not a thread file. The cursor importer's scope is the threads in it: each composer, its bubbles and every other `cursorDiskKV` row whose key names the composer, each with its key, its value bytes and its storage class. A `cursorDiskKV` row tied to no thread is counted as `unlinked-row-skipped`, and every row of any other table (`ItemTable` and the rest of the IDE's own state) as `ide-state-skipped`; neither is carried or written back (`docs/formats/cursor.md`). So for cursor, *unchanged* means: every row of the fixture comes back with the same key, the same value bytes and the same storage class, and no extra row appears in any table, a table only the export declares included. A fixture holding rows outside the threads would not come back that way and would be listed as not exact. The export writes a new `state.vscdb` that declares its tables as Cursor does, so the table declarations are not part of the comparison; where they differ from the fixture's, they are listed here:\n"
        );
        let scoped: Vec<&CaseResult> = results.iter().filter(|r| r.selft.native.scoped).collect();
        if scoped.is_empty() {
            let _ = writeln!(s, "- none: every cursor fixture's table declarations came back as they were.");
        }
        for r in scoped {
            let rw = r.selft.native.rows.as_ref();
            let _ = writeln!(
                s,
                "- `{}/{}`: {}/{} rows identical, {} extra; {}",
                r.format,
                r.case,
                rw.map_or(0, |x| x.same),
                rw.map_or(0, |x| x.total),
                rw.map_or(0, |x| x.extra),
                rw.map(|x| x.schema_diff.join("; ")).unwrap_or_default()
            );
        }
    }
    let failing: Vec<&CaseResult> = results.iter().filter(|r| !r.selft.native.exact() || r.selft.lost > 0).collect();
    if !failing.is_empty() {
        let _ = writeln!(s, "\nFixtures whose self trip did not come back unchanged, with what the comparison found and what the importer itself reported:\n");
        for (x, v) in &selfc {
            for f in &v.3 {
                let (case, why) = f.split_once(": ").unwrap_or((f.as_str(), ""));
                let _ = writeln!(s, "- `{x}/{case}`: {why}");
            }
        }
        let thread_clean = failing.iter().all(|r| r.selft.error.is_none() && r.selft.lost == 0);
        let _ = writeln!(
            s,
            "\n{}",
            if thread_clean {
                format!(
                    "For every one of these {} the neutral comparison of the re-import with the import loses nothing (the 100% in the protoledger column): everything the importer took into protoledger came back out. What does not come back is what the importer did not take in, or how the store spelled it. So, precisely: **{exact} of {total} fixtures come back unchanged through protoledger; the other {} come back with every fact of the thread, and differ only in store content the list above names.**",
                    failing.len(),
                    failing.len()
                )
            } else {
                format!("At least one of these {} also loses thread facts on the self trip (see the protoledger column).", failing.len())
            }
        );
    }
    let _ = writeln!(s, "\n## % of facts preserved without carry\n");
    let _ = writeln!(
        s,
        "Rows: source. Columns: target. *self* marks the diagonal. A cell with errors is computed over the fixtures that ran and says how many did not.\n"
    );
    let mut head = String::from("| source ↓ · target → | fixtures |");
    let mut rule = String::from("|---|---:|");
    for y in names {
        head.push_str(&format!(" {y} |"));
        rule.push_str("---:|");
    }
    let _ = writeln!(s, "{head}\n{rule}");
    for x in rows {
        let n = agg[&(x.to_string(), P.to_string())].fixtures;
        let mut line = format!("| **{x}** | {n} |");
        for y in names {
            let a = &agg[&(x.to_string(), y.to_string())];
            let mut c = a.pct().map(fmt_pct).unwrap_or_else(|| "—".into());
            if *y == *x {
                c = format!("{c} *self*");
            }
            if a.errors > 0 {
                c = format!("{c} ({} err)", a.errors);
            }
            line.push_str(&format!(" {c} |"));
        }
        let _ = writeln!(s, "{line}");
    }
    let _ = writeln!(s, "\n## Facts and losses\n");
    let _ = writeln!(
        s,
        "Per source: facts in its fixtures, and the facts lost on the way through each target (Σ over fixtures of missing facts + changed body fields + kind changes).\n"
    );
    let mut head = String::from("| source | facts |");
    let mut rule = String::from("|---|---:|");
    for y in names {
        head.push_str(&format!(" {y} |"));
        rule.push_str("---:|");
    }
    let _ = writeln!(s, "{head}\n{rule}");
    for x in rows {
        let f: u64 = results.iter().filter(|r| r.format == *x).map(|r| r.facts).sum();
        let mut line = format!("| **{x}** | {f} |");
        for y in names {
            let a = &agg[&(x.to_string(), y.to_string())];
            line.push_str(&format!(" {} |", a.lost));
        }
        let _ = writeln!(s, "{line}");
    }
    let _ = writeln!(s, "\n## Added (not counted as loss)\n");
    let _ = writeln!(
        s,
        "Per source and target: events (and agents) present after the trip that the source did not have (`Diff::extra`, Σ over fixtures): turn markers a target inserts, a message split in two, a synthetic session record. Reported, never subtracted from the % preserved.\n"
    );
    let mut head = String::from("| source |");
    let mut rule = String::from("|---|");
    for y in names {
        head.push_str(&format!(" {y} |"));
        rule.push_str("---:|");
    }
    let _ = writeln!(s, "{head}\n{rule}");
    for x in rows {
        let mut line = format!("| **{x}** |");
        for y in names {
            line.push_str(&format!(" {} |", agg[&(x.to_string(), y.to_string())].added));
        }
        let _ = writeln!(s, "{line}");
    }
    let _ = writeln!(s, "\n## Metadata changed (not counted as loss)\n");
    let _ = writeln!(
        s,
        "Per source and target: event-level fields that changed on matched events (`ts`, `group`, `turn`, `audience`, `meta.*`, ordering, branch: every `Diff::changed` entry that is not a body field or kind) plus changed agent header fields (`Diff::header`), Σ over fixtures. A target that re-times, re-groups or re-numbers messages without dropping one shows here, not in the %.\n"
    );
    let mut head = String::from("| source |");
    let mut rule = String::from("|---|");
    for y in names {
        head.push_str(&format!(" {y} |"));
        rule.push_str("---:|");
    }
    let _ = writeln!(s, "{head}\n{rule}");
    for x in rows {
        let mut line = format!("| **{x}** |");
        for y in names {
            line.push_str(&format!(" {} |", agg[&(x.to_string(), y.to_string())].meta));
        }
        let _ = writeln!(s, "{line}");
    }
    let _ = writeln!(s, "\n## With carry: return trip exact\n");
    let _ = writeln!(
        s,
        "X → P → Y → P → X with carry on, compared with the source as in the self trip (bytes; SQLite by schema and rows, a Cursor `state.vscdb` by its rows). Cell: exact / fixtures that ran.\n"
    );
    let mut head = String::from("| source ↓ · via → |");
    let mut rule = String::from("|---|");
    for y in names {
        head.push_str(&format!(" {y} |"));
        rule.push_str("---:|");
    }
    let _ = writeln!(s, "{head}\n{rule}");
    for x in rows {
        let mut line = format!("| **{x}** |");
        for y in names {
            let a = &agg[&(x.to_string(), y.to_string())];
            line.push_str(&format!(" {}/{} |", a.carry_exact, a.carry_runs));
        }
        let _ = writeln!(s, "{line}");
    }
    let errs: Vec<_> = agg.iter().filter(|(_, a)| a.errors > 0).collect();
    let _ = writeln!(s, "\n## Errors\n");
    if errs.is_empty() {
        let _ = writeln!(
            s,
            "No trip errored: every target accepted every source (a target that cannot hold something, such as subagents, still converts and the loss is counted)."
        );
    } else {
        for ((x, y), a) in errs {
            let _ =
                writeln!(s, "- `{x}` → `{y}`: {} of {} fixtures; first: {}", a.errors, a.fixtures, a.first_error.as_deref().unwrap_or("").replace('|', "\\|"));
        }
    }
    let _ = writeln!(s, "\n## Regenerate\n\n```sh\ncargo run --release --example matrix -- --out docs/\n```");
    s
}

// ---- SVG -------------------------------------------------------------------

/// The reference sequential ramp (blue, steps 100 → 700).
const RAMP: [&str; 10] = ["#cde2fb", "#b7d3f6", "#9ec5f4", "#86b6ef", "#6da7ec", "#5598e7", "#3987e5", "#256abf", "#184f95", "#0d366b"];

fn lum(hex: &str) -> f64 {
    let c = |i: usize| {
        let v = u8::from_str_radix(&hex[i..i + 2], 16).unwrap() as f64 / 255.0;
        if v <= 0.03928 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * c(1) + 0.7152 * c(3) + 0.0722 * c(5)
}
fn contrast(a: &str, b: &str) -> f64 {
    let (x, y) = (lum(a), lum(b));
    (x.max(y) + 0.05) / (x.min(y) + 0.05)
}
fn ink(fill: &str) -> &'static str {
    if contrast(fill, "#0b0b0b") >= contrast(fill, "#ffffff") { "#0b0b0b" } else { "#ffffff" }
}

/// Bin a percentage: 0–9.9 → 0, …, 90–99.9 → 9, 100 → 9.
fn bin(p: f64) -> usize {
    ((p / 10.0).floor() as usize).min(9)
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn svg(rows: &[&str], names: &[&str], agg: &BTreeMap<(String, String), Agg>, results: &[CaseResult]) -> String {
    let label_w = 96.0;
    let p_w = 62.0;
    let gap = 8.0;
    let cw = 44.0;
    let ch = 26.0;
    let top = 112.0;
    let n_other = names.len() - 1;
    let grid_x = label_w + p_w + gap;
    let width = grid_x + cw * n_other as f64 + 44.0;
    let grid_h = ch * rows.len() as f64;
    let legend_y = top + grid_h + 22.0;
    let height = legend_y + 96.0;
    let selfc = self_counts(rows, results);
    let mut s = String::new();
    let _ = write!(
        s,
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {width} {height}" width="{width}" height="{height}" role="img" aria-labelledby="t d" font-family="system-ui, -apple-system, 'Segoe UI', Helvetica, Arial, sans-serif">
<title id="t">protoledger transfer matrix</title>
<desc id="d">Heatmap. Rows are source formats, columns target formats. Each cell is the percent of the source's facts preserved through source to protoledger to target to protoledger without carry. The protoledger column is the self trip, source to protoledger to source. The diagonal is the self trip.</desc>
<style>
.bg{{fill:#fcfcfb}} .ink{{fill:#0b0b0b}} .ink2{{fill:#52514e}} .mute{{fill:#898781}}
.pbox{{fill:none;stroke:#0b0b0b;stroke-width:1.5}} .diag{{fill:none;stroke:#0b0b0b;stroke-width:1.5}} .sep{{stroke:#c3c2b7;stroke-width:1}}
"##
    );
    for (i, h) in RAMP.iter().enumerate() {
        let _ = writeln!(s, ".f{i}{{fill:{h}}} .t{i}{{fill:{}}}", ink(h));
    }
    let _ = writeln!(
        s,
        "@media (prefers-color-scheme: dark) {{\n.bg{{fill:#1a1a19}} .ink{{fill:#ffffff}} .ink2{{fill:#c3c2b7}} .mute{{fill:#898781}} .pbox{{stroke:#ffffff}} .diag{{stroke:#ffffff}} .sep{{stroke:#383835}}"
    );
    for i in 0..RAMP.len() {
        // Dark mode flips the anchor: near zero recedes into the dark surface.
        let h = RAMP[RAMP.len() - 1 - i];
        let _ = writeln!(s, ".f{i}{{fill:{h}}} .t{i}{{fill:{}}}", ink(h));
    }
    let _ = writeln!(s, "}}\n.err{{fill:none;stroke:#d03b3b;stroke-width:2}}\n</style>");
    let _ = writeln!(s, r#"<rect class="bg" x="0" y="0" width="{width}" height="{height}" rx="6"/>"#);
    let _ = writeln!(
        s,
        r#"<text class="ink" x="8" y="20" font-size="14" font-weight="600">% of facts preserved, source → protoledger → target → protoledger (no carry)</text>"#
    );
    let _ = writeln!(
        s,
        r#"<text class="ink2" x="8" y="36" font-size="11">rows: source format · columns: target format · pooled over every fixture of the source</text>"#
    );
    // Column headers.
    let header = |s: &mut String, x: f64, name: &str, bold: bool| {
        let _ = writeln!(
            s,
            r#"<text class="{}" x="{x}" y="{}" font-size="11" {} transform="rotate(-45 {x} {})">{}</text>"#,
            if bold { "ink" } else { "ink2" },
            top - 6.0,
            if bold { r#"font-weight="700""# } else { "" },
            top - 6.0,
            esc(name)
        );
    };
    header(&mut s, label_w + p_w / 2.0 - 4.0, P, true);
    let others: Vec<&str> = names.iter().copied().filter(|n| *n != P).collect();
    for (j, y) in others.iter().enumerate() {
        header(&mut s, grid_x + cw * j as f64 + cw / 2.0 - 4.0, y, false);
    }
    let _ = writeln!(s, r#"<text class="mute" x="{}" y="{}" font-size="10" text-anchor="end">source ↓</text>"#, label_w - 6.0, top - 6.0);
    for (i, x) in rows.iter().enumerate() {
        let y0 = top + ch * i as f64;
        let _ = writeln!(
            s,
            r#"<text class="{}" x="{}" y="{}" font-size="11" text-anchor="end" {}>{}</text>"#,
            if *x == P { "ink" } else { "ink2" },
            label_w - 6.0,
            y0 + ch / 2.0 + 4.0,
            if *x == P { r#"font-weight="700""# } else { "" },
            esc(x)
        );
        // protoledger column.
        let (n, b, rw, fail) = &selfc[*x];
        let ok = fail.is_empty();
        let pa = &agg[&(x.to_string(), P.to_string())];
        let pct = pa.pct().unwrap_or(0.0);
        let k = if ok { 9 } else { bin(pct) };
        let px = label_w + 1.0;
        let tip = format!(
            "{x} → protoledger: {} of {n} fixtures unchanged on X → P → X ({b} byte-exact, {rw} SQLite row-identical); {} of facts preserved",
            b + rw,
            fmt_pct(pct)
        );
        let line1 = if pct >= 99.95 { "100%".to_string() } else { format!("{}%", fmt_pct(pct)) };
        let line2 = if ok { "lossless".to_string() } else { format!("{}/{n} exact", b + rw) };
        let _ = writeln!(
            s,
            r#"<g><title>{}</title><rect class="f{k}" x="{px}" y="{}" width="{}" height="{}" rx="2"/><text class="t{k}" x="{}" y="{}" font-size="10" font-weight="700" text-anchor="middle">{line1}</text><text class="t{k}" x="{}" y="{}" font-size="8.5" text-anchor="middle">{line2}</text></g>"#,
            esc(&tip),
            y0 + 1.0,
            p_w - 2.0,
            ch - 2.0,
            px + (p_w - 2.0) / 2.0,
            y0 + 11.5,
            px + (p_w - 2.0) / 2.0,
            y0 + 21.5,
        );
        for (j, y) in others.iter().enumerate() {
            let cx = grid_x + cw * j as f64;
            let a = &agg[&(x.to_string(), y.to_string())];
            let diag = *y == *x;
            let tip = if diag {
                format!(
                    "{x} → {x} (self trip): {} of {n} fixtures unchanged; {} preserved",
                    b + rw,
                    a.pct().map(|p| format!("{}%", fmt_pct(p))).unwrap_or("—".into())
                )
            } else {
                format!(
                    "{x} → {y}: {} preserved over {} fixtures ({} of {} facts lost; {} added, {} metadata changed){}; with carry {}/{} return exact",
                    a.pct().map(|p| format!("{}%", fmt_pct(p))).unwrap_or("—".into()),
                    a.fixtures,
                    a.lost,
                    a.facts,
                    a.added,
                    a.meta,
                    if a.errors > 0 { format!("; {} errored", a.errors) } else { String::new() },
                    a.carry_exact,
                    a.carry_runs
                )
            };
            match a.pct() {
                Some(p) => {
                    let k = bin(p);
                    let label = if p >= 99.95 { "100".to_string() } else { format!("{:.0}", p.floor().min(99.0)) };
                    let _ = writeln!(
                        s,
                        r#"<g><title>{}</title><rect class="f{k}" x="{}" y="{}" width="{}" height="{}" rx="2"/><text class="t{k}" x="{}" y="{}" font-size="{}" text-anchor="middle">{}</text></g>"#,
                        esc(&tip),
                        cx + 1.0,
                        y0 + 1.0,
                        cw - 2.0,
                        ch - 2.0,
                        cx + cw / 2.0,
                        if diag { y0 + ch / 2.0 + 1.0 } else { y0 + ch / 2.0 + 4.0 },
                        if diag { 9.5 } else { 11.0 },
                        label
                    );
                    if diag {
                        let _ = writeln!(
                            s,
                            r#"<rect class="diag" x="{}" y="{}" width="{}" height="{}" rx="2"/><text class="t{k}" x="{}" y="{}" font-size="7.5" text-anchor="middle">self</text>"#,
                            cx + 1.5,
                            y0 + 1.5,
                            cw - 3.0,
                            ch - 3.0,
                            cx + cw / 2.0,
                            y0 + ch - 4.0
                        );
                    }
                }
                None => {
                    let _ = writeln!(
                        s,
                        r#"<g><title>{}</title><text class="mute" x="{}" y="{}" font-size="11" text-anchor="middle">—</text></g>"#,
                        esc(&tip),
                        cx + cw / 2.0,
                        y0 + ch / 2.0 + 4.0
                    );
                }
            }
            if a.errors > 0 {
                let _ = writeln!(s, r#"<rect class="err" x="{}" y="{}" width="{}" height="{}" rx="2"/>"#, cx + 2.0, y0 + 2.0, cw - 4.0, ch - 4.0);
            }
        }
    }
    // Frame the protoledger column.
    let _ = writeln!(s, r#"<rect class="pbox" x="{}" y="{}" width="{}" height="{}" rx="3"/>"#, label_w, top, p_w, grid_h);
    // Legend: the binned scale.
    let lx = label_w;
    let _ = writeln!(s, r#"<text class="ink2" x="{lx}" y="{}" font-size="11">% preserved</text>"#, legend_y + 2.0);
    let sw = 30.0;
    for i in 0..10 {
        let x = lx + 76.0 + sw * i as f64;
        let _ = writeln!(s, r#"<rect class="f{i}" x="{x}" y="{}" width="{}" height="12" rx="2"/>"#, legend_y - 8.0, sw - 2.0);
        let _ = writeln!(s, r#"<text class="mute" x="{x}" y="{}" font-size="9">{}</text>"#, legend_y + 15.0, i * 10);
    }
    let _ = writeln!(s, r#"<text class="mute" x="{}" y="{}" font-size="9">100</text>"#, lx + 76.0 + sw * 10.0 - 6.0, legend_y + 15.0);
    let has_err = agg.values().any(|a| a.errors > 0);
    let mut kx = lx + 76.0 + sw * 10.0 + 22.0;
    let _ = writeln!(
        s,
        r#"<rect class="diag" x="{kx}" y="{}" width="16" height="12" rx="2"/><text class="ink2" x="{}" y="{}" font-size="10">self trip (X → P → X)</text>"#,
        legend_y - 8.0,
        kx + 21.0,
        legend_y + 2.0
    );
    kx += 140.0;
    if has_err {
        let _ = writeln!(
            s,
            r#"<rect class="err" x="{kx}" y="{}" width="16" height="12" rx="2"/><text class="ink2" x="{}" y="{}" font-size="10">some fixtures errored</text>"#,
            legend_y - 8.0,
            kx + 21.0,
            legend_y + 2.0
        );
    }
    let foot = legend_y + 38.0;
    let total: u64 = selfc.values().map(|v| v.0).sum();
    let exact: u64 = selfc.values().map(|v| v.1 + v.2).sum();
    let _ = writeln!(
        s,
        r#"<text class="ink2" x="8" y="{foot}" font-size="10.5">protoledger column: X → P → X returned {exact} of {total} fixtures unchanged (bytes; SQLite by schema and rows, a Cursor state.vscdb by its thread rows){}.</text>"#,
        if exact == total { ", so protoledger held all of it".to_string() } else { format!("; the other {}: see MATRIX.md", total - exact) }
    );
    let _ = writeln!(s, r#"<text class="ink2" x="8" y="{}" font-size="10.5">{}.</text>"#, foot + 16.0, esc(&carry_footnote(results)));
    let _ = writeln!(
        s,
        r#"<text class="mute" x="8" y="{}" font-size="10">A fact is an event plus each non-empty body field; lost = missing facts + changed body fields and kinds; additions and metadata changes are tabled apart in MATRIX.md. Generated by examples/matrix.rs.</text>"#,
        foot + 32.0
    );
    s.push_str("</svg>\n");
    s
}

/// The footnote's claim, worded from the data: the carriers through which
/// every carry trip of every fixture whose own self trip is exact came back
/// exact. A carrier that did not is named.
fn carry_footnote(results: &[CaseResult]) -> String {
    let label = |n: &str| {
        match n {
            "claude-code" => "Claude Code",
            "codex" => "Codex",
            "opencode" => "opencode",
            o => o,
        }
        .to_string()
    };
    let base: Vec<&CaseResult> = results.iter().filter(|r| r.selft.native.exact()).collect();
    let (mut ok, mut bad) = (Vec::new(), Vec::new());
    for y in CARRIERS {
        let all = base.iter().all(|r| r.format == y || r.cells.get(y).is_some_and(|c| c.carry_exact == Some(true)));
        if all { ok.push(label(y)) } else { bad.push(label(y)) }
    }
    let list = |v: &[String]| match v.len() {
        0 => String::new(),
        1 => v[0].clone(),
        2 => format!("{} and {}", v[0], v[1]),
        _ => format!("{}, {} and {}", v[0], v[1], v[2]),
    };
    let mut s = if ok.is_empty() {
        "With carry, no carrier returned every trip exact".to_string()
    } else {
        format!("With carry, every trip through {} returns exact", list(&ok))
    };
    s.push_str(&format!(" ({} fixtures whose own X → P → X is exact", base.len()));
    if !bad.is_empty() {
        s.push_str(&format!("; not every trip through {}", list(&bad)));
    }
    s.push(')');
    s
}

//! The conformance suite (SPEC §7). Every fixture under `fixtures/<format>/<case>/`
//! is run through:
//!
//! - a **self trip** X → P → X, which must reproduce the native files byte for
//!   byte unless the case's `expect.json` says otherwise;
//! - **cross trips** X → P → Y → P → X through every other implemented format,
//!   with carry (which must come back exact for the primary harnesses) and
//!   without (whose neutral loss is recorded and may not grow).
//!
//! Results are ratcheted against the reports COMMITTED on a base ref
//! (`RATCHET_BASE`, default `main`), read with `git cat-file`, never against
//! the working tree's copies: a branch cannot pass by rewriting its own
//! reports. A case or a via with no committed report on the base is a failure
//! unless named in `RATCHET_EXEMPT` (comma-separated `format/case` or
//! `format/case:via`), and every exemption used is printed and counted. A
//! via's neutral loss that grew passes when its `format/case:via` is named
//! (one key each; an import loss that appears or grows never does).
//! Ratcheted: each via's neutral and carried loss (may not grow), each via's
//! `carry_back_exact` (may not go from exact to not, nor disappear), the self
//! trip's exactness, and each `import_losses` code (may not appear or grow).
//!
//! Run with `UPDATE_REPORTS=1` (exactly `1`) to also rewrite
//! `tests/reports/<format>/<case>.json` and `tests/reports/MATRIX.md`; any other
//! value rewrites nothing. The ratchet runs either way.
//!
//! Run with `FIXTURE=<format>` or `FIXTURE=<format>/<case>` to run some fixtures;
//! a filter that runs no case fails. A case whose `expect.json` names
//! `requires_features` this build lacks is SKIPPED, and said so on stderr.

use protoledger::formats::{self, Format};
use protoledger::roundtrip::{self, CrossTrip, FileDiff, SelfTrip};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const PRIMARY: [&str; 3] = ["claude-code", "codex", "opencode"];
/// Sources crossed through every implemented format, not only the primaries.
const CROSS_ALL: [&str; 4] = ["claude-code", "codex", "opencode", "protoledger"];

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Expect {
    /// false when the self trip is known not to be byte-exact; `self_reason` says why.
    self_exact: Option<bool>,
    self_reason: Option<String>,
    /// Formats this fixture should not be crossed through, with the reason.
    skip_cross: BTreeMap<String, String>,
    /// Crate features the case cannot run without (`zstd` for a `.zst` rollout).
    requires_features: Vec<String>,
}

fn read_expect(case: &Path) -> Expect {
    std::fs::read_to_string(case.join("expect.json"))
        .ok()
        .map(|s| serde_json::from_str(&s).unwrap_or_else(|e| panic!("{}/expect.json: {e}", case.display())))
        .unwrap_or_default()
}

/// Whether this build has crate feature `name`; `None` for a name the harness does not know.
fn feature_on(name: &str) -> Option<bool> {
    Some(match name {
        "zstd" => cfg!(feature = "zstd"),
        "sqlite" => cfg!(feature = "sqlite"),
        "store" => cfg!(feature = "store"),
        _ => return None,
    })
}

/// A line the test harness does not capture: written to the process's stderr
/// directly, so a skipped case is visible in a passing run.
fn say(line: &str) {
    use std::io::Write;
    let _ = writeln!(std::io::stderr(), "{line}");
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct Report {
    format: String,
    case: String,
    variant: String,
    self_exact: bool,
    self_reimport_score: u64,
    self_summary: String,
    import_losses: Vec<String>,
    /// via-format → score
    carry_there: BTreeMap<String, u64>,
    carry_back_exact: BTreeMap<String, bool>,
    no_carry_there: BTreeMap<String, u64>,
    no_carry_summary: BTreeMap<String, String>,
    errors: BTreeMap<String, String>,
}

/// Formats whose exporter writes a SQLite file for a ledger it did not
/// import (deepagents' checkpoint store, hermes's state.db), so it cannot run
/// without the `sqlite` feature. Without it they are not crossed through (each
/// such via is skipped, said and counted), and their own fixtures must
/// require `sqlite` (their plain trips come back through that exporter).
/// Pinned by `export_needs_sqlite_population_is_exact`.
const EXPORT_NEEDS_SQLITE: [&str; 2] = ["deepagents", "hermes"];

/// Whether this build can export through `via` at all.
fn via_runs(via: &str) -> bool {
    cfg!(feature = "sqlite") || !EXPORT_NEEDS_SQLITE.contains(&via)
}

/// Whether a file starts with SQLite's header: a case holding one needs the
/// `sqlite` feature to be imported at all.
fn is_sqlite(path: &Path) -> bool {
    use std::io::Read;
    let mut head = [0u8; 16];
    std::fs::File::open(path).and_then(|mut f| f.read_exact(&mut head)).is_ok() && &head == b"SQLite format 3\0"
}

fn implemented(f: &dyn Format) -> bool {
    f.variants().iter().any(|v| v.import && v.export)
}

fn case_input(case: &Path) -> PathBuf {
    let native = case.join("native");
    let entries: Vec<_> = std::fs::read_dir(&native)
        .unwrap_or_else(|_| panic!("{} has no native/ directory", case.display()))
        .filter_map(|e| e.ok())
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .collect();
    if entries.len() == 1 && entries[0].path().is_file() { entries[0].path() } else { native }
}

/// What a self trip's report says about it. A trip that is not exact names
/// every file that kept it from being so, `altered` ones included: an altered
/// file is neither a difference nor exact, and a summary leaving it out would
/// fail the trip with empty lists. (`altered` is printed only when non-empty,
/// so a summary with none is spelled as it always was.)
fn self_summary(files: &FileDiff, reimport: &str) -> String {
    if files.is_exact() {
        return reimport.to_string();
    }
    let altered = if files.altered.is_empty() { String::new() } else { format!(" altered {:?}", files.altered) };
    format!("files differ {:?} missing {:?} extra {:?}{altered}; {reimport}", files.differ, files.missing, files.extra)
}

/// The features named in Cargo.toml's `[features]` table, read from the
/// manifest itself (not from `cfg!`).
fn declared_features() -> Vec<String> {
    let manifest = std::fs::read_to_string(root().join("Cargo.toml")).expect("Cargo.toml");
    let mut in_features = false;
    let mut out = Vec::new();
    for line in manifest.lines().map(str::trim) {
        if line.starts_with('[') {
            in_features = line == "[features]";
        } else if in_features
            && let Some((name, _)) = line.split_once('=')
            && !line.starts_with('#')
        {
            out.push(name.trim().to_string());
        }
    }
    out
}

/// `legacy-json` is gone: a fixture that still requires it is an unknown-feature
/// failure in `conformance`, never a silent skip. Each side is checked against
/// a source other than `feature_on` itself: which
/// names exist comes from the manifest, and whether `zstd` and `sqlite` are on
/// comes from what the importers can read.
#[test]
fn feature_on_agrees_with_the_manifest_and_with_what_imports() {
    let declared = declared_features();
    // The manifest read found the table at all (a search that finds nothing must be able to find something).
    assert!(declared.iter().any(|d| d == "default") && declared.len() >= 6, "{declared:?}");
    assert!(!declared.iter().any(|d| d == "legacy-json"), "legacy-json is back in Cargo.toml: {declared:?}");
    assert_eq!(feature_on("legacy-json"), None);
    for name in ["zstd", "sqlite", "store"] {
        assert!(declared.iter().any(|d| d == name), "{name} is not in Cargo.toml: {declared:?}");
        assert!(feature_on(name).is_some(), "the harness does not know {name}");
    }
    // Opposed, with genuinely different operands: whether the build can read
    // a `.zst` rollout and an opencode SQLite store.
    let reads = |format: &str, case: &str| {
        let case = root().join("fixtures").join(format).join(case);
        formats::by_name(format).unwrap().import(&case_input(&case), &protoledger::formats::ImportOptions::standard()).is_ok()
    };
    assert_eq!(feature_on("zstd"), Some(reads("codex", "zst-legacy-0.112")), "feature_on(zstd) disagrees with the codex importer");
    assert_eq!(feature_on("sqlite"), Some(reads("opencode", "v2-sqlite-2.0.31-generated")), "feature_on(sqlite) disagrees with the opencode importer");
}

#[test]
fn self_summary_names_altered_files() {
    use protoledger::formats::Output;
    use protoledger::roundtrip::AlteredFiles;
    let mut o = Output::default();
    o.put("s.jsonl", b"{\"k\":\"[redacted]\"}\n".to_vec());
    o.put("t.jsonl", b"{}\n".to_vec());
    // The same bytes back, but s.jsonl holds an altered record: not exact, and nothing differs.
    let altered = AlteredFiles { files: ["s.jsonl".to_string()].into_iter().collect(), unplaced: false };
    let d = FileDiff::of_altered(&o, &o.clone(), &altered);
    assert!(!d.is_exact() && d.differ.is_empty() && d.missing.is_empty() && d.extra.is_empty(), "{d:?}");
    let s = self_summary(&d, "clean");
    assert!(s.contains("altered [\"s.jsonl\"]"), "the summary does not name the altered file: {s}");
    assert!(!s.contains("t.jsonl"), "the summary names an exact file: {s}");
    // Opposed: with nothing altered the file is exact and the summary is the re-import's alone.
    let d = FileDiff::of_altered(&o, &o.clone(), &AlteredFiles::default());
    assert!(d.is_exact(), "{d:?}");
    assert_eq!(self_summary(&d, "clean"), "clean");
    // And a differing file with nothing altered is spelled as before, with no `altered` part.
    let mut p = o.clone();
    p.put("t.jsonl", b"{ }\n".to_vec());
    let d = FileDiff::of_altered(&o, &p, &AlteredFiles::default());
    assert_eq!(self_summary(&d, "clean"), "files differ [\"t.jsonl\"] missing [] extra []; clean");
}

/// `import_losses` as the report spells them (`"<code> ×<count>"`), by code.
fn loss_counts(losses: &[String]) -> BTreeMap<String, u64> {
    let mut m = BTreeMap::new();
    for l in losses {
        let (code, n) = l.rsplit_once(" ×").unwrap_or_else(|| panic!("import loss {l:?} is not \"<code> ×<count>\""));
        let n: u64 = n.parse().unwrap_or_else(|_| panic!("import loss {l:?} has no count"));
        *m.entry(code.to_string()).or_default() += n;
    }
    m
}

fn run_case(f: &'static dyn Format, case: &Path) -> (Report, Expect) {
    let expect = read_expect(case);
    let input = case_input(case);
    let name = case.file_name().unwrap().to_string_lossy().to_string();
    let mut r = Report {
        format: f.name().into(),
        case: name,
        variant: String::new(),
        self_exact: false,
        self_reimport_score: 0,
        self_summary: String::new(),
        import_losses: vec![],
        carry_there: BTreeMap::new(),
        carry_back_exact: BTreeMap::new(),
        no_carry_there: BTreeMap::new(),
        no_carry_summary: BTreeMap::new(),
        errors: BTreeMap::new(),
    };
    match roundtrip::self_trip(f, &input) {
        Ok(SelfTrip { variant, files, reimport, import_losses, .. }) => {
            r.variant = variant;
            r.self_exact = files.is_exact();
            r.self_reimport_score = reimport.score();
            r.self_summary = self_summary(&files, &reimport.summary());
            r.import_losses = import_losses.0.iter().map(|l| format!("{} ×{}", l.code, l.count)).collect();
        }
        Err(e) => {
            r.errors.insert("self".into(), format!("{e:#}"));
            return (r, expect);
        }
    }
    let vias: Vec<&'static dyn Format> = formats::all()
        .into_iter()
        .filter(|y| y.name() != f.name() && y.name() != "protoledger" && implemented(*y))
        .filter(|y| CROSS_ALL.contains(&f.name()) || PRIMARY.contains(&y.name()))
        .filter(|y| !expect.skip_cross.contains_key(y.name()))
        .filter(|y| via_runs(y.name()))
        .collect();
    for y in vias {
        match roundtrip::cross_trip(f, &input, y, true) {
            Ok(CrossTrip { there, back_files, .. }) => {
                r.carry_there.insert(y.name().into(), there.score());
                r.carry_back_exact.insert(y.name().into(), back_files.is_exact());
            }
            Err(e) => {
                r.errors.insert(format!("carry:{}", y.name()), format!("{e:#}"));
            }
        }
        match roundtrip::cross_trip(f, &input, y, false) {
            Ok(CrossTrip { there, .. }) => {
                r.no_carry_there.insert(y.name().into(), there.score());
                r.no_carry_summary.insert(y.name().into(), there.summary());
            }
            Err(e) => {
                r.errors.insert(format!("plain:{}", y.name()), format!("{e:#}"));
            }
        }
    }
    (r, expect)
}

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// The ratchet's base: committed reports read from a git ref.
struct Base {
    name: String,
    commit: String,
    /// `tests/reports/<format>/<case>.json` → its text on the base, for every case asked for.
    reports: BTreeMap<String, Option<String>>,
}

/// Read `paths` (relative to the repo root) at `base` in one `git cat-file --batch`.
/// A path the base does not have is `None`; anything else that goes wrong panics.
fn read_base(base: &str, paths: &[String]) -> Base {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let rev = Command::new("git")
        .arg("-C")
        .arg(root())
        .args(["rev-parse", "--verify", "--quiet"])
        .arg(format!("{base}^{{commit}}"))
        .output()
        .unwrap_or_else(|e| panic!("ratchet: cannot run git: {e}"));
    let commit = String::from_utf8_lossy(&rev.stdout).trim().to_string();
    if !rev.status.success() || commit.is_empty() {
        panic!("ratchet: base ref `{base}` (RATCHET_BASE) does not name a commit in {}", root().display());
    }
    let mut child = Command::new("git")
        .arg("-C")
        .arg(root())
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("ratchet: cannot run git cat-file: {e}"));
    let mut input = String::new();
    for p in paths {
        input.push_str(&format!("{commit}:{p}\n"));
    }
    let mut stdin = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
    let out = child.wait_with_output().expect("git cat-file runs");
    writer.join().unwrap().expect("git cat-file takes its input");
    assert!(out.status.success(), "ratchet: git cat-file --batch failed");
    let mut rest: &[u8] = &out.stdout;
    let mut reports = BTreeMap::new();
    for p in paths {
        let nl = rest.iter().position(|b| *b == b'\n').expect("git cat-file header line");
        let header = String::from_utf8_lossy(&rest[..nl]).to_string();
        rest = &rest[nl + 1..];
        if header.ends_with(" missing") {
            reports.insert(p.clone(), None);
            continue;
        }
        let size: usize = header.rsplit(' ').next().and_then(|n| n.parse().ok()).unwrap_or_else(|| panic!("ratchet: git cat-file said {header:?}"));
        reports.insert(p.clone(), Some(String::from_utf8(rest[..size].to_vec()).expect("a report is UTF-8")));
        rest = &rest[size + 1..];
    }
    Base { name: base.to_string(), commit, reports }
}

/// The commit checked out.
fn head_commit() -> String {
    let out = std::process::Command::new("git").arg("-C").arg(root()).args(["rev-parse", "HEAD"]).output().expect("git rev-parse HEAD runs");
    assert!(out.status.success(), "git rev-parse HEAD failed");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Why the ratchet base is unusable under CI, if it is: CI (`CI` set and not
/// `false`) must name `RATCHET_BASE`, and it must not resolve to HEAD.
/// Outside CI the default (`main`) stands.
fn ci_base_problem(ci: Option<&str>, ratchet_base: Option<&str>, base_commit: &str, head: &str) -> Option<String> {
    let ci = ci.is_some_and(|v| !v.is_empty() && v != "false" && v != "0");
    if !ci {
        return None;
    }
    match ratchet_base.filter(|b| !b.is_empty()) {
        None => Some("CI is set but RATCHET_BASE is not: the ratchet would compare against main, which on main is the commit itself".into()),
        Some(b) if base_commit == head => Some(format!("RATCHET_BASE={b} is HEAD ({head}): the ratchet would compare this commit with itself")),
        Some(_) => None,
    }
}

#[test]
fn ci_requires_a_ratchet_base_other_than_head() {
    let (a, b) = ("1111111111111111111111111111111111111111", "2222222222222222222222222222222222222222");
    assert_eq!(ci_base_problem(None, None, a, a), None, "outside CI the default stands");
    assert_eq!(ci_base_problem(Some("false"), None, a, a), None);
    assert!(ci_base_problem(Some("true"), None, a, b).is_some(), "CI without RATCHET_BASE passed");
    assert!(ci_base_problem(Some("true"), Some("HEAD"), a, a).is_some(), "a base equal to HEAD passed");
    assert_eq!(ci_base_problem(Some("true"), Some("HEAD~1"), a, b), None, "a base other than HEAD failed");
}

#[test]
fn conformance() {
    let update = match std::env::var("UPDATE_REPORTS") {
        Ok(v) if v == "1" => true,
        Ok(v) => {
            say(&format!("conformance: UPDATE_REPORTS={v:?} is not \"1\": no report is rewritten"));
            false
        }
        Err(_) => false,
    };
    let only = std::env::var("FIXTURE").ok().filter(|o| !o.is_empty());
    let base_name = std::env::var("RATCHET_BASE").ok().filter(|b| !b.is_empty()).unwrap_or_else(|| "main".into());
    let exempt: Vec<String> =
        std::env::var("RATCHET_EXEMPT").ok().map(|v| v.split(',').map(str::trim).filter(|x| !x.is_empty()).map(str::to_string).collect()).unwrap_or_default();
    let mut used_exempt: BTreeMap<String, usize> = BTreeMap::new();
    let mut failures: Vec<String> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    let mut skipped_vias = 0usize;
    let mut matrix: BTreeMap<(String, String), Report> = BTreeMap::new();
    let selected = |name: &str| only.as_deref().is_none_or(|o| o == name || o.split_once('/').is_some_and(|(f, _)| f == name));
    // Every fixtures/<dir> names a format, and every format has fixtures.
    let names: Vec<&str> = formats::all().iter().map(|f| f.name()).collect();
    for e in std::fs::read_dir(root().join("fixtures")).expect("fixtures/ exists").filter_map(|e| e.ok()) {
        let n = e.file_name().to_string_lossy().to_string();
        if e.path().is_dir() && !n.starts_with(['.', '_']) && !names.contains(&n.as_str()) && selected(&n) {
            failures.push(format!("fixtures/{n}/: no format of that name"));
        }
    }
    // Collect every case, run them in parallel, then judge them in order.
    let mut jobs: Vec<(&'static dyn Format, PathBuf, String)> = Vec::new();
    for f in formats::all() {
        if !selected(f.name()) {
            continue;
        }
        let dir = root().join("fixtures").join(f.name());
        let Ok(rd) = std::fs::read_dir(&dir) else {
            failures.push(format!("{}: no fixtures/{}/ directory, so nothing measures this format", f.name(), f.name()));
            continue;
        };
        let mut cases: Vec<PathBuf> = Vec::new();
        for p in rd.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.is_dir()) {
            let n = p.file_name().unwrap().to_string_lossy().to_string();
            if n.starts_with(['.', '_']) {
                continue; // generators and tools, not cases
            }
            if p.join("native").is_dir() {
                cases.push(p);
            } else {
                failures.push(format!("{}/{n}: a case directory with no native/", f.name()));
            }
        }
        cases.sort();
        for case in cases {
            let key = format!("{}/{}", f.name(), case.file_name().unwrap().to_string_lossy());
            if only.as_deref().is_some_and(|o| o != key && o != f.name()) {
                continue;
            }
            if !implemented(f) {
                failures.push(format!("{key}: format has fixtures but no adapter"));
                continue;
            }
            let expect = read_expect(&case);
            // A compressed rollout needs the zstd feature, and must say so.
            let zst = walkdir::WalkDir::new(case.join("native")).into_iter().filter_map(|e| e.ok()).any(|e| e.file_name().to_string_lossy().ends_with(".zst"));
            if zst && !expect.requires_features.iter().any(|x| x == "zstd") {
                failures.push(format!("{key}: holds .zst files but its expect.json does not list zstd in requires_features"));
            }
            // A SQLite file needs the sqlite feature, and must say so.
            let sqlite = walkdir::WalkDir::new(case.join("native")).into_iter().filter_map(|e| e.ok()).any(|e| e.file_type().is_file() && is_sqlite(e.path()));
            let needs_sqlite = expect.requires_features.iter().any(|x| x == "sqlite");
            if sqlite && !needs_sqlite {
                failures.push(format!("{key}: holds a SQLite file but its expect.json does not list sqlite in requires_features"));
            }
            if EXPORT_NEEDS_SQLITE.contains(&f.name()) && !needs_sqlite {
                failures.push(format!("{key}: {}'s exporter needs sqlite, so its expect.json must list sqlite in requires_features", f.name()));
            }
            let mut missing = Vec::new();
            for feat in &expect.requires_features {
                match feature_on(feat) {
                    Some(true) => {}
                    Some(false) => missing.push(feat.clone()),
                    None => failures.push(format!("{key}: requires_features names {feat:?}, which is not a crate feature the harness knows")),
                }
            }
            if !missing.is_empty() {
                say(&format!("conformance: SKIPPED {key}: this build lacks feature {}", missing.join(", ")));
                skipped.push(key);
                continue;
            }
            jobs.push((f, case, key));
        }
    }
    if let Some(o) = &only
        && jobs.is_empty()
    {
        failures.push(format!("FIXTURE={o:?} runs no fixture case ({} skipped): a filter that measures nothing is a failure", skipped.len()));
    }
    // The committed reports the ratchet compares against.
    let report_rel = |key: &str| format!("tests/reports/{key}.json");
    let base = read_base(&base_name, &jobs.iter().map(|(_, _, k)| report_rel(k)).collect::<Vec<_>>());
    // Under CI the base must be named and must not be the commit under test:
    // compared with itself, a commit that changes code and regenerates its
    // reports together passes by construction.
    if let Some(problem) = ci_base_problem(std::env::var("CI").ok().as_deref(), std::env::var("RATCHET_BASE").ok().as_deref(), &base.commit, &head_commit()) {
        failures.push(problem);
    }
    let mut exempted = |what: &str| -> bool {
        match exempt.iter().find(|x| x.as_str() == what) {
            Some(x) => {
                *used_exempt.entry(x.clone()).or_default() += 1;
                say(&format!("conformance: EXEMPT {what} (RATCHET_EXEMPT)"));
                true
            }
            None => false,
        }
    };
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results: std::sync::Mutex<Vec<Option<(Report, Expect)>>> = std::sync::Mutex::new((0..jobs.len()).map(|_| None).collect());
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    std::thread::scope(|sc| {
        for _ in 0..threads {
            sc.spawn(|| {
                loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let Some((f, case, _)) = jobs.get(i) else { break };
                    let out = run_case(*f, case);
                    results.lock().unwrap()[i] = Some(out);
                }
            });
        }
    });
    let results = results.into_inner().unwrap();
    let mut ratcheted = 0usize;
    for ((f, _case, key), res) in jobs.iter().zip(results) {
        let f = *f;
        let key = key.clone();
        let (r, expect) = res.expect("case ran");
        // Hard rules.
        for (k, e) in &r.errors {
            failures.push(format!("{key}: {k}: {e}"));
        }
        let want_exact = expect.self_exact.unwrap_or(true);
        if want_exact && !r.self_exact {
            failures.push(format!("{key}: self trip not exact: {}", r.self_summary));
        }
        if !want_exact && expect.self_reason.is_none() {
            failures.push(format!("{key}: self_exact=false needs a self_reason"));
        }
        if r.self_reimport_score != 0 {
            failures.push(format!("{key}: self re-import lost something: {}", r.self_summary));
        }
        // With carry, a trip through a primary harness loses nothing.
        for (via, score) in &r.carry_there {
            if PRIMARY.contains(&via.as_str()) && *score != 0 {
                failures.push(format!("{key}: carried trip via {via} lost {score}"));
            }
        }
        if PRIMARY.contains(&f.name()) {
            for (via, exact) in &r.carry_back_exact {
                if PRIMARY.contains(&via.as_str()) && !exact && want_exact {
                    failures.push(format!("{key}: carry trip via {via} did not come back exact"));
                }
            }
        }
        // Ratchet against the report committed on the base.
        let rel = report_rel(&key);
        match base.reports.get(&rel).cloned().flatten() {
            None => {
                if !exempted(&key) {
                    failures.push(format!("{key}: no report committed at {}:{rel}; the ratchet has nothing to compare against", base.name));
                }
            }
            Some(text) => {
                ratcheted += 1;
                let old: Report = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}:{rel}: {e}", base.name));
                let mut vias: Vec<&String> = r
                    .no_carry_there
                    .keys()
                    .chain(old.no_carry_there.keys())
                    .chain(r.carry_there.keys())
                    .chain(old.carry_there.keys())
                    .chain(r.carry_back_exact.keys())
                    .chain(old.carry_back_exact.keys())
                    .collect();
                vias.sort();
                vias.dedup();
                for via in vias {
                    if !via_runs(via) {
                        say(&format!("conformance: SKIPPED via {via} for {key}: this build lacks feature sqlite"));
                        skipped_vias += 1;
                        continue;
                    }
                    if r.errors.keys().any(|k| k.ends_with(&format!(":{via}"))) {
                        continue; // already a failure above
                    }
                    let pairs =
                        [("neutral", old.no_carry_there.get(via), r.no_carry_there.get(via)), ("carried", old.carry_there.get(via), r.carry_there.get(via))];
                    let mut gap = false;
                    for (what, prev, now) in pairs {
                        match (prev, now) {
                            // A neutral rise named in RATCHET_EXEMPT is passed (and counted); a carried one never is.
                            (Some(prev), Some(now)) if now > prev && !(what == "neutral" && exempted(&format!("{key}:{via}"))) => {
                                let why = if what == "neutral" { format!(": {}", r.no_carry_summary[via]) } else { String::new() };
                                failures.push(format!("{key}: {what} loss via {via} grew {prev} → {now}{why}"));
                            }
                            (Some(_), Some(_)) => {}
                            (None, Some(_)) | (Some(_), None) => gap = true,
                            (None, None) => {}
                        }
                    }
                    // A carried trip's via the base measured for exactness and this run does not:
                    // the exactness ratchet below would compare nothing. (A via newly measured
                    // here and absent on the base is an improvement, and passes.)
                    let back_gone = old.carry_back_exact.contains_key(via) && !r.carry_back_exact.contains_key(via);
                    if (gap || back_gone) && !exempted(&format!("{key}:{via}")) {
                        if gap {
                            let side = if old.no_carry_there.contains_key(via) || old.carry_there.contains_key(via) {
                                "is no longer measured"
                            } else {
                                "has no committed measurement"
                            };
                            failures.push(format!("{key}: via {via} {side} on {} (a via the ratchet cannot compare)", base.name));
                        }
                        if back_gone {
                            failures.push(format!(
                                "{key}: via {via} is in carry_back_exact on {} and no longer is (whether the carried trip comes back exact is no longer measured)",
                                base.name
                            ));
                        }
                    }
                    if old.carry_back_exact.get(via) == Some(&true) && r.carry_back_exact.get(via) == Some(&false) {
                        failures.push(format!("{key}: carry trip via {via} came back exact on {} and no longer does", base.name));
                    }
                }
                if old.self_exact && !r.self_exact {
                    failures.push(format!("{key}: self trip was exact and no longer is"));
                }
                // Import losses may shrink or vanish, never appear or grow.
                let (was, now) = (loss_counts(&old.import_losses), loss_counts(&r.import_losses));
                for (code, n) in &now {
                    match was.get(code) {
                        Some(w) if n <= w => {}
                        Some(w) => failures.push(format!("{key}: import loss {code} grew {w} → {n} (on {})", base.name)),
                        None => failures.push(format!("{key}: import loss {code} ×{n} is new (none on {})", base.name)),
                    }
                }
            }
        }
        if update {
            let path = root().join(&rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, serde_json::to_string_pretty(&r).unwrap() + "\n").unwrap();
        }
        matrix.insert((f.name().to_string(), r.case.clone()), r);
    }
    for x in &exempt {
        if !used_exempt.contains_key(x) {
            failures.push(format!("RATCHET_EXEMPT names {x:?}, which exempted nothing"));
        }
    }
    if update && only.is_none() {
        write_matrix(&matrix);
    }
    say(&format!(
        "conformance: {} cases run, {} ratcheted against {} ({}), {} skipped{}, {} ratcheted vias skipped, {} exemptions used",
        jobs.len(),
        ratcheted,
        base.name,
        &base.commit[..base.commit.len().min(10)],
        skipped.len(),
        if skipped.is_empty() { String::new() } else { format!(" [{}]", skipped.join(", ")) },
        skipped_vias,
        used_exempt.values().sum::<usize>(),
    ));
    if !failures.is_empty() {
        panic!("{} conformance failures:\n  {}", failures.len(), failures.join("\n  "));
    }
}

/// The fixtures whose self trip is allowed not to be byte-exact
/// (`expect.json` says `"self_exact": false`), pinned by name, so that a case
/// joining or leaving that population is a decision someone made on purpose.
const SELF_EXACT_FALSE: [&str; 19] = [
    "cursor/ide-agentsview-composers",
    "cursor/ide-cursor-history-v016",
    "cursor/ide-synthetic-inline-and-tools",
    "deepagents/handbuilt-overwrite",
    "deepagents/legacy-full-state-todos",
    "deepagents/long-thread-snapshot",
    "deepagents/reasoning-subagent",
    "deepagents/simple-chat",
    "deepagents/tool-calls-files",
    "hermes/state-db-v31-lineage",
    "hermes/state-db-v4",
    "openclaw/sqlite-real-transcripts",
    "openclaw/sqlite-subagent-store",
    "opencode/v1-sqlite-1.19.3-generated",
    "opencode/v1-sqlite-1.2.21-generated",
    "opencode/v1-sqlite-synthetic-1.2-todo",
    "opencode/v2-sqlite-2.0.31-generated",
    "opencode/v2-sqlite-beta-agentsview",
    "opencode/v2-sqlite-captured-1.18-read-tool",
];

#[test]
fn self_exact_false_population_is_pinned() {
    let mut found: Vec<String> = Vec::new();
    let mut cases = 0usize;
    for fmt in std::fs::read_dir(root().join("fixtures")).unwrap().filter_map(|e| e.ok()).filter(|e| e.path().is_dir()) {
        for case in std::fs::read_dir(fmt.path()).unwrap().filter_map(|e| e.ok()).filter(|e| e.path().join("native").is_dir()) {
            cases += 1;
            if read_expect(&case.path()).self_exact == Some(false) {
                found.push(format!("{}/{}", fmt.file_name().to_string_lossy(), case.file_name().to_string_lossy()));
            }
        }
    }
    found.sort();
    // The enumeration reached the cases at all (a search that finds nothing must be able to find something).
    assert!(cases > SELF_EXACT_FALSE.len(), "only {cases} fixture cases found under fixtures/");
    let pinned: Vec<String> = SELF_EXACT_FALSE.iter().map(|s| s.to_string()).collect();
    let added: Vec<&String> = found.iter().filter(|f| !pinned.contains(f)).collect();
    let removed: Vec<&String> = pinned.iter().filter(|p| !found.contains(p)).collect();
    assert!(
        added.is_empty() && removed.is_empty(),
        "the self_exact:false population changed: joined {added:?}, left {removed:?}; update SELF_EXACT_FALSE only on purpose"
    );
}

fn write_matrix(m: &BTreeMap<(String, String), Report>) {
    let mut vias: Vec<String> = m.values().flat_map(|r| r.no_carry_there.keys().cloned()).collect();
    vias.sort();
    vias.dedup();
    let mut s = String::from(
        "# Round-trip matrix\n\nGenerated by `UPDATE_REPORTS=1 cargo test --test fixtures`. Do not edit.\n\n\
         - **self**: X → protoledger → X reproduces the native files byte for byte (✓) or not (✗).\n\
         - Each other column is the **neutral loss score** of X → protoledger → Y → protoledger **without carry**: \
         how many facts did not survive the trip through Y (0 is lossless). With carry, every primary-harness trip \
         comes back exact.\n\n",
    );
    s.push_str("| fixture | variant | self |");
    for v in &vias {
        s.push_str(&format!(" {v} |"));
    }
    s.push_str("\n|---|---|---|");
    for _ in &vias {
        s.push_str("---|");
    }
    s.push('\n');
    for ((f, c), r) in m {
        s.push_str(&format!("| {f}/{c} | {} | {} |", r.variant, if r.self_exact { "✓" } else { "✗" }));
        for v in &vias {
            let cell = if f == v { "—".to_string() } else { r.no_carry_there.get(v).map(|n| n.to_string()).unwrap_or_else(|| "".into()) };
            s.push_str(&format!(" {cell} |"));
        }
        s.push('\n');
    }
    std::fs::write(root().join("tests/reports/MATRIX.md"), s).unwrap();
}

/// The population of `EXPORT_NEEDS_SQLITE` is exact: exporting a
/// ledger imported from elsewhere fails for want of `sqlite` through exactly
/// those formats in a build without it, and through none in a build with it.
#[test]
fn export_needs_sqlite_population_is_exact() {
    use protoledger::formats::{ExportOptions, ImportOptions};
    let src = formats::by_name("protoledger").unwrap();
    let set = src.import(&root().join("fixtures/protoledger/minimal/native"), &ImportOptions::standard()).unwrap().set;
    let mut needs: Vec<&str> = Vec::new();
    let mut exported = 0usize;
    for f in formats::all().into_iter().filter(|f| f.name() != "protoledger" && implemented(*f)) {
        match f.export(&set, &ExportOptions::no_carry()) {
            Ok(_) => exported += 1,
            Err(e) if format!("{e:#}").contains("`sqlite` feature") => needs.push(f.name()),
            Err(e) => panic!("{}: {e:#}", f.name()),
        }
    }
    let want: Vec<&str> = if cfg!(feature = "sqlite") { vec![] } else { EXPORT_NEEDS_SQLITE.to_vec() };
    needs.sort();
    assert_eq!(needs, want, "the formats whose export needs sqlite changed: update EXPORT_NEEDS_SQLITE");
    // The loop reached the exporters at all.
    assert!(exported >= 10, "only {exported} formats exported");
}

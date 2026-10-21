//! The mutation harness: no importer panics on malformed input.
//!
//! Every fixture under `fixtures/<format>/<case>/native` is copied and mutated
//! at least [`PER_FIXTURE`] times — truncated at many offsets, bytes flipped,
//! lines dropped, duplicated and swapped, numbers replaced by extreme
//! spellings, string values replaced by other JSON types, a file of a
//! directory case removed — and each mutated copy is run through detection
//! (every format's `detect`, and [`formats::detect`]), an import by the
//! fixture's own format and by whatever format detection picks, and, when the
//! fixture's own import succeeds, an export of what it made by the same format
//! (an exporter must not panic on what its importer produced). Every call runs
//! under `catch_unwind`. An `Err` is fine and is counted as one; a panic is a
//! failure, reported with the fixture, the mutation and the panic's location.
//!
//! Deterministic: the mutations are drawn from a xorshift stream seeded with
//! [`SEED`] (or `MUTATE_SEED`, hex or decimal), mixed with the case's name, and
//! the seed is printed. `MUTATE_FIXTURE=<format>` or `<format>/<case>` runs
//! some fixtures (a filter that runs none fails); `MUTATE_N` raises the count
//! per fixture (never below [`PER_FIXTURE`]).

use protoledger::formats::{self, ExportOptions, ImportOptions};
use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

const SEED: u64 = 0x5eed_7051_2026_1019;
const PER_FIXTURE: usize = 60;
/// Longer than any single mutated case has been measured to take (well
/// under a second each); a mutation still running then is a hang.
const HANG: Duration = Duration::from_secs(120);

fn xorshift(s: &mut u64) -> u64 {
    let mut x = *s;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *s = x;
    x
}

fn below(s: &mut u64, n: usize) -> usize {
    if n == 0 { 0 } else { (xorshift(s) % n as u64) as usize }
}

/// FNV-1a, so a case's stream depends on its name and not on enumeration order.
fn fnv(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

fn seed() -> u64 {
    match std::env::var("MUTATE_SEED") {
        Ok(v) => {
            let v = v.trim();
            match v.strip_prefix("0x") {
                Some(h) => u64::from_str_radix(h, 16).expect("MUTATE_SEED: hex"),
                None => v.parse().expect("MUTATE_SEED: a number"),
            }
        }
        Err(_) => SEED,
    }
}

struct Case {
    format: String,
    name: String,
    native: PathBuf,
    /// The import input, relative to `native`'s copy ("" for the directory itself).
    input: PathBuf,
    /// Every file under `native`, relative to it.
    files: Vec<PathBuf>,
}

/// The `requires_features` of a case's `expect.json` that this build lacks
/// (`--no-default-features --features store` runs without sqlite
/// and zstd). A name the harness does not know is a failure, as in fixtures.rs.
fn lacking_features(case: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(case.join("expect.json")) else { return vec![] };
    let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}/expect.json: {e}", case.display()));
    let names = v.get("requires_features").and_then(|r| r.as_array()).cloned().unwrap_or_default();
    names
        .iter()
        .map(|n| n.as_str().unwrap_or_else(|| panic!("{}: requires_features holds {n}", case.display())))
        .filter(|n| !match *n {
            "zstd" => cfg!(feature = "zstd"),
            "sqlite" => cfg!(feature = "sqlite"),
            "store" => cfg!(feature = "store"),
            other => panic!("{}: requires_features names {other:?}, which is not a crate feature the harness knows", case.display()),
        })
        .map(str::to_string)
        .collect()
}

/// Whether any file under `native` starts with SQLite's header.
fn holds_sqlite(native: &Path) -> bool {
    use std::io::Read;
    walkdir::WalkDir::new(native).into_iter().filter_map(|e| e.ok()).filter(|e| e.file_type().is_file()).any(|e| {
        let mut head = [0u8; 16];
        std::fs::File::open(e.path()).and_then(|mut f| f.read_exact(&mut head)).is_ok() && &head == b"SQLite format 3\0"
    })
}

fn cases() -> Vec<Case> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    let filter = std::env::var("MUTATE_FIXTURE").ok();
    let mut out = Vec::new();
    let mut formats: Vec<_> = std::fs::read_dir(&root).unwrap().filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.is_dir()).collect();
    formats.sort();
    for f in formats {
        let format = f.file_name().unwrap().to_string_lossy().into_owned();
        let mut dirs: Vec<_> = std::fs::read_dir(&f).unwrap().filter_map(|e| e.ok()).map(|e| e.path()).collect();
        dirs.sort();
        for case in dirs {
            let native = case.join("native");
            if !native.is_dir() {
                continue;
            }
            let name = case.file_name().unwrap().to_string_lossy().into_owned();
            if let Some(flt) = &filter
                && flt != &format
                && flt != &format!("{format}/{name}")
            {
                continue;
            }
            // As tests/fixtures.rs does: a case needing a feature this build lacks is skipped, and said so.
            let mut lacking = lacking_features(&case);
            // A SQLite file needs `sqlite` whether or not the case says so (the
            // `_live-*` cases have no expect.json).
            if !cfg!(feature = "sqlite") && !lacking.iter().any(|f| f == "sqlite") && holds_sqlite(&native) {
                lacking.push("sqlite".into());
            }
            if !lacking.is_empty() {
                eprintln!("mutate: SKIPPED {format}/{name}: this build lacks feature {}", lacking.join(", "));
                continue;
            }
            // As tests/fixtures.rs picks it: a lone file is the input, else the directory.
            let top: Vec<_> =
                std::fs::read_dir(&native).unwrap().filter_map(|e| e.ok()).filter(|e| !e.file_name().to_string_lossy().starts_with('.')).collect();
            let input = if top.len() == 1 && top[0].path().is_file() { PathBuf::from(top[0].file_name()) } else { PathBuf::new() };
            let mut files: Vec<PathBuf> = walkdir::WalkDir::new(&native)
                .sort_by_file_name()
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_file())
                .map(|e| e.path().strip_prefix(&native).unwrap().to_path_buf())
                .collect();
            files.sort();
            out.push(Case { format: format.clone(), name, native, input, files });
        }
    }
    out
}

/// Spellings of a number no fixture holds and every reader must survive.
const NUMBERS: &[&str] = &[
    "1e-2147483648",
    "1e2147483647",
    "-1e2147483647",
    "1.7976931348623157e308",
    "-1.7976931348623157e308",
    "18446744073709551616",
    "-9223372036854775809",
    "99999999999999999999999999999999999999999",
    "-0",
    "0.000000000000000000000000000000000000000000001",
    "4.9e-324",
    "-1",
    "1e400",
];

/// What a string value may become: another JSON type, or a hostile string.
const VALUES: &[&str] = &[
    "null",
    "{}",
    "[]",
    "-1",
    "1e400",
    "true",
    "\"\\ud800\"",
    "\"\"",
    "\"2026-13-45T99:99:99Z\"",
    "\"\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\"",
];

const KINDS: usize = 9;

/// Apply mutation `kind` to `bytes` (drawing from `s`), described.
fn mutate(kind: usize, bytes: &mut Vec<u8>, s: &mut u64) -> String {
    let lines = |b: &[u8]| -> Vec<(usize, usize)> {
        let mut v = Vec::new();
        let mut start = 0;
        for (i, c) in b.iter().enumerate() {
            if *c == b'\n' {
                v.push((start, i + 1));
                start = i + 1;
            }
        }
        if start < b.len() {
            v.push((start, b.len()));
        }
        v
    };
    match kind {
        0 => {
            let at = below(s, bytes.len());
            bytes.truncate(at);
            format!("truncate at byte {at}")
        }
        1 => {
            // Truncate just after or inside a line's end: a torn last record.
            let ls = lines(bytes);
            if ls.is_empty() {
                bytes.clear();
                return "truncate to empty".into();
            }
            let (_, end) = ls[below(s, ls.len())];
            let at = end.saturating_sub(below(s, 3));
            bytes.truncate(at);
            format!("truncate near a line end at byte {at}")
        }
        2 => {
            let n = 1 + below(s, 3);
            let mut at = Vec::new();
            for _ in 0..n {
                if bytes.is_empty() {
                    break;
                }
                let i = below(s, bytes.len());
                let mask = (1 + below(s, 255)) as u8;
                bytes[i] ^= mask;
                at.push(format!("{i}^{mask:#04x}"));
            }
            format!("flip bytes {}", at.join(","))
        }
        3..=5 => {
            let ls = lines(bytes);
            if ls.len() < 2 {
                // One line (a whole-document JSON file): the same three edits on
                // byte spans, so the case is not merely emptied.
                if bytes.len() < 2 {
                    bytes.clear();
                    return "under two bytes: emptied".into();
                }
                let a = below(s, bytes.len() - 1);
                let len = 1 + below(s, 64.min(bytes.len() - a - 1).max(1));
                let span = bytes[a..a + len].to_vec();
                return match kind {
                    3 => {
                        bytes.drain(a..a + len);
                        format!("one line: drop bytes {a}..{}", a + len)
                    }
                    4 => {
                        let at = below(s, bytes.len());
                        bytes.splice(at..at, span);
                        format!("one line: copy bytes {a}..{} to {at}", a + len)
                    }
                    _ => {
                        // Move the span to the end of the document (a reordered tail).
                        bytes.drain(a..a + len);
                        bytes.extend_from_slice(&span);
                        format!("one line: move bytes {a}..{} to the end", a + len)
                    }
                };
            }
            let i = below(s, ls.len());
            let (a, b) = ls[i];
            match kind {
                3 => {
                    bytes.drain(a..b);
                    format!("drop line {i}")
                }
                4 => {
                    let line = bytes[a..b].to_vec();
                    let at = ls[below(s, ls.len())].0;
                    bytes.splice(at..at, line);
                    format!("duplicate line {i}")
                }
                _ => {
                    let j = below(s, ls.len());
                    let (x, y) = if i < j { (ls[i], ls[j]) } else { (ls[j], ls[i]) };
                    if x == y {
                        return format!("swap line {i} with itself");
                    }
                    let (lx, ly) = (bytes[x.0..x.1].to_vec(), bytes[y.0..y.1].to_vec());
                    let mut out = bytes[..x.0].to_vec();
                    out.extend_from_slice(&ly);
                    out.extend_from_slice(&bytes[x.1..y.0]);
                    out.extend_from_slice(&lx);
                    out.extend_from_slice(&bytes[y.1..]);
                    *bytes = out;
                    format!("swap lines {} and {}", i.min(j), i.max(j))
                }
            }
        }
        6 => {
            // Replace a digit run (a number token, or part of one) with an extreme spelling.
            let runs: Vec<(usize, usize)> = {
                let mut v = Vec::new();
                let mut i = 0;
                while i < bytes.len() {
                    if bytes[i].is_ascii_digit() && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric()) {
                        let st = i;
                        while i < bytes.len() && matches!(bytes[i], b'0'..=b'9' | b'.' | b'e' | b'E' | b'-' | b'+') {
                            i += 1;
                        }
                        if i == bytes.len() || !bytes[i].is_ascii_alphanumeric() {
                            v.push((st, i));
                        }
                    } else {
                        i += 1;
                    }
                }
                v
            };
            if runs.is_empty() {
                return "no number to replace".into();
            }
            let (a, b) = runs[below(s, runs.len())];
            let n = NUMBERS[below(s, NUMBERS.len())];
            bytes.splice(a..b, n.bytes());
            format!("number at byte {a} -> {n}")
        }
        7 => {
            // Replace a string value (the string after `": `) with another type.
            let mut spots = Vec::new();
            let mut i = 0;
            while i + 1 < bytes.len() {
                if bytes[i] == b':' {
                    let mut j = i + 1;
                    while j < bytes.len() && bytes[j] == b' ' {
                        j += 1;
                    }
                    if j < bytes.len() && bytes[j] == b'"' {
                        let mut k = j + 1;
                        while k < bytes.len() && bytes[k] != b'"' && bytes[k] != b'\n' {
                            k += if bytes[k] == b'\\' { 2 } else { 1 };
                        }
                        if k < bytes.len() && bytes[k] == b'"' {
                            spots.push((j, k + 1));
                        }
                    }
                }
                i += 1;
            }
            if spots.is_empty() {
                return "no string value to replace".into();
            }
            let (a, b) = spots[below(s, spots.len())];
            let v = VALUES[below(s, VALUES.len())];
            bytes.splice(a..b, v.bytes());
            format!("string value at byte {a} -> {v}")
        }
        _ => {
            // A block of bytes repeated: a doubled write.
            if bytes.is_empty() {
                return "empty: nothing to repeat".into();
            }
            let a = below(s, bytes.len());
            let len = 1 + below(s, 64.min(bytes.len() - a));
            let block = bytes[a..a + len].to_vec();
            bytes.splice(a..a, block);
            format!("repeat {len} bytes at {a}")
        }
    }
}

/// Fixtures whose every native file is empty: no byte mutation changes them,
/// so they are exempt from the per-fixture reached floor, by name.
const NOTHING_TO_MUTATE: [&str; 1] = ["letta-code/empty"];

/// The panic currently unwinding on this thread: its message and location.
static LAST_PANIC: Mutex<BTreeMap<String, String>> = Mutex::new(BTreeMap::new());

/// The panic hook is process-wide: every test in this binary holds this lock,
/// so a sibling test's panic is never captured into [`LAST_PANIC`] (and so
/// never unprinted) while the harness's hook is installed.
static HOOK: Mutex<()> = Mutex::new(());

fn thread_key() -> String {
    format!("{:?}", std::thread::current().id())
}

#[derive(Default)]
struct Tally {
    mutations: usize,
    calls: usize,
    errs: usize,
    oks: usize,
    panics: Vec<String>,
    /// One mutation that made the fixture's own import fail with an `Err`, as a witness.
    err_example: Option<String>,
    /// Mutations whose own-format import is an `Err` or differs from the
    /// unmutated fixture's: the mutation reached the parser.
    reached: usize,
}

/// What an import made, as one number: every ledger's JSONL, hashed.
fn fingerprint(set: &protoledger::LedgerSet) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for l in set.iter() {
        l.to_jsonl().hash(&mut h);
    }
    h.finish()
}

/// Run `f` under `catch_unwind`; a panic is recorded with `what`.
fn guarded<T>(t: &mut Tally, what: &str, f: impl FnOnce() -> anyhow::Result<T>) -> Option<T> {
    t.calls += 1;
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(v)) => {
            t.oks += 1;
            Some(v)
        }
        Ok(Err(_)) => {
            t.errs += 1;
            None
        }
        Err(_) => {
            let msg = LAST_PANIC.lock().unwrap_or_else(|e| e.into_inner()).remove(&thread_key()).unwrap_or_default();
            t.panics.push(format!("{what}: {msg}"));
            None
        }
    }
}

/// One mutated copy of `case` through detect, import and export.
fn run_one(case: &Case, i: usize, s: &mut u64, t: &mut Tally, clean: Option<u64>) {
    let dir = tempfile::tempdir().unwrap();
    let native = dir.path().join("native");
    for f in &case.files {
        let to = native.join(f);
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        std::fs::copy(case.native.join(f), &to).unwrap();
    }
    let kind = i % KINDS;
    let fi = below(s, case.files.len());
    let target = &case.files[fi];
    let desc = if kind == KINDS - 1 && case.files.len() > 1 && below(s, 2) == 0 {
        std::fs::remove_file(native.join(target)).unwrap();
        format!("remove {}", target.display())
    } else {
        let path = native.join(target);
        let mut bytes = std::fs::read(&path).unwrap();
        let d = mutate(kind, &mut bytes, s);
        std::fs::write(&path, &bytes).unwrap();
        format!("{}: {d}", target.display())
    };
    let input = if case.input.as_os_str().is_empty() { native.clone() } else { native.join(&case.input) };
    let label = format!("{}/{} mutation {i} ({desc})", case.format, case.name);
    t.mutations += 1;
    for f in formats::all() {
        guarded(t, &format!("{label} detect by {}", f.name()), || Ok(f.detect(&input)));
    }
    let detected = guarded(t, &format!("{label} formats::detect"), || Ok(formats::detect(&input).map(|(f, _)| f.name()))).flatten();
    let own = formats::by_name(&case.format).unwrap_or_else(|| panic!("no format {}", case.format));
    let errs_before = t.errs;
    let imported = guarded(t, &format!("{label} import by {}", own.name()), || own.import(&input, &ImportOptions::standard()));
    if imported.is_none() && t.errs > errs_before && t.err_example.is_none() {
        let err = own.import(&input, &ImportOptions::standard()).err().map(|e| format!("{e:#}")).unwrap_or_default();
        t.err_example = Some(format!("{label}: {err}"));
    }
    if imported.as_ref().map(|i| fingerprint(&i.set)) != clean {
        t.reached += 1;
    }
    if let Some(imp) = imported
        && own.variants().iter().any(|v| v.export)
    {
        guarded(t, &format!("{label} export by {} (standard)", own.name()), || own.export(&imp.set, &ExportOptions::standard()).map(|_| ()));
        guarded(t, &format!("{label} export by {} (no carry)", own.name()), || own.export(&imp.set, &ExportOptions::no_carry()).map(|_| ()));
    }
    if let Some(other) = detected.filter(|n| *n != own.name()) {
        let f = formats::by_name(other).unwrap();
        guarded(t, &format!("{label} import by detected {other}"), || f.import(&input, &ImportOptions::standard()).map(|_| ()));
    }
}

fn run_case(case: &Case, seed: u64, n: usize) -> Tally {
    let mut t = Tally::default();
    let mut s = seed ^ fnv(&format!("{}/{}", case.format, case.name));
    if s == 0 {
        s = 1;
    }
    // The unmutated fixture, imported in place (an import writes nothing).
    let input = if case.input.as_os_str().is_empty() { case.native.clone() } else { case.native.join(&case.input) };
    let own = formats::by_name(&case.format).unwrap();
    let clean = own.import(&input, &ImportOptions::standard()).ok().map(|i| fingerprint(&i.set));
    for i in 0..n {
        run_one(case, i, &mut s, &mut t, clean);
    }
    t
}

#[test]
fn no_importer_panics_on_a_mutated_fixture() {
    let seed = seed();
    let n = std::env::var("MUTATE_N").ok().and_then(|v| v.parse().ok()).unwrap_or(PER_FIXTURE).max(PER_FIXTURE);
    let cases = cases();
    assert!(!cases.is_empty(), "no fixture matched MUTATE_FIXTURE={:?}", std::env::var("MUTATE_FIXTURE").ok());
    let _hook = HOOK.lock().unwrap_or_else(|e| e.into_inner());
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(|info| {
        let msg = info.payload().downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| info.payload().downcast_ref::<String>().cloned()).unwrap_or_default();
        let at = info.location().map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column())).unwrap_or_default();
        // With MUTATE_BACKTRACE set, the crate's own frames too (a panic inside a
        // dependency is located by the protoledger function that called it).
        let frames = if std::env::var_os("MUTATE_BACKTRACE").is_some() {
            let bt = std::backtrace::Backtrace::force_capture().to_string();
            let ours: Vec<&str> = bt.lines().map(str::trim).filter(|l| l.contains("protoledger::") && !l.contains("mutate::")).take(4).collect();
            format!(" [via {}]", ours.join(" <- "))
        } else {
            String::new()
        };
        LAST_PANIC.lock().unwrap_or_else(|e| e.into_inner()).insert(thread_key(), format!("panicked at {at}: {msg}{frames}"));
    }));
    let workers = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2).clamp(1, 4);
    let next = Mutex::new(0usize);
    let results: Mutex<Vec<(usize, Tally)>> = Mutex::new(Vec::new());
    let started: Mutex<BTreeMap<usize, std::time::Instant>> = Mutex::new(BTreeMap::new());
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    std::thread::scope(|sc| {
        for _ in 0..workers {
            let done_tx = done_tx.clone();
            let (next, results, started, cases) = (&next, &results, &started, &cases);
            sc.spawn(move || {
                loop {
                    let i = {
                        let mut g = next.lock().unwrap();
                        let i = *g;
                        *g += 1;
                        i
                    };
                    if i >= cases.len() {
                        break;
                    }
                    started.lock().unwrap().insert(i, std::time::Instant::now());
                    let t = run_case(&cases[i], seed, n);
                    started.lock().unwrap().remove(&i);
                    results.lock().unwrap().push((i, t));
                    let _ = done_tx.send(());
                }
            });
        }
        drop(done_tx);
        // Watch for a case that outlives HANG: report it rather than wait forever.
        loop {
            match done_rx.recv_timeout(Duration::from_secs(5)) {
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                _ => {
                    if let Some((i, _)) = started.lock().unwrap().iter().find(|(_, t0)| t0.elapsed() > HANG) {
                        let c = &cases[*i];
                        // A hung thread cannot be joined: say so and end the binary red.
                        eprintln!("mutate: HANG {}/{} still running after {HANG:?} (seed {seed:#x})", c.format, c.name);
                        std::process::exit(101);
                    }
                }
            }
        }
    });
    let _ = std::panic::take_hook();
    std::panic::set_hook(previous);
    let mut results = results.into_inner().unwrap();
    results.sort_by_key(|(i, _)| *i);
    let (mut mutations, mut calls, mut errs, mut oks) = (0, 0, 0, 0);
    let mut reached: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    let mut panics = Vec::new();
    let mut err_example = None;
    for (i, t) in &results {
        let r = reached.entry(cases[*i].format.as_str()).or_default();
        r.0 += t.reached;
        r.1 += t.mutations;
        mutations += t.mutations;
        calls += t.calls;
        errs += t.errs;
        oks += t.oks;
        panics.extend(t.panics.iter().cloned());
        if err_example.is_none() {
            err_example = t.err_example.clone();
        }
    }
    // Per fixture, so one fixture whose mutations never reach its parser is
    // not hidden in its format's total.
    let mut by_case: Vec<(f64, String)> = results
        .iter()
        .map(|(i, t)| (t.reached as f64 / t.mutations.max(1) as f64, format!("{}/{} {}/{}", cases[*i].format, cases[*i].name, t.reached, t.mutations)))
        .collect();
    by_case.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap().then(a.1.cmp(&b.1)));
    let formats_seen: std::collections::BTreeSet<&str> = cases.iter().map(|c| c.format.as_str()).collect();
    for p in &panics {
        eprintln!("mutate: PANIC {p}");
    }
    eprintln!(
        "mutate: seed={seed:#x} fixtures={} formats={} mutations={mutations} ({n} per fixture) calls={calls} ok={oks} err={errs} panics={}",
        cases.len(),
        formats_seen.len(),
        panics.len()
    );
    eprintln!("mutate: an Err is not a panic, e.g. {}", err_example.as_deref().unwrap_or("(none)"));
    eprintln!(
        "mutate: mutations that changed or failed the own-format import, per format: {}",
        reached.iter().map(|(f, (r, m))| format!("{f} {r}/{m}")).collect::<Vec<_>>().join(", ")
    );
    eprintln!(
        "mutate: the fixtures whose mutations least often changed or failed their own import: {}",
        by_case.iter().take(5).map(|(_, d)| d.as_str()).collect::<Vec<_>>().join(", ")
    );
    // Sentinels: every case ran its mutations, and in every format at least
    // half the mutations reached the parser (the import failed or came out
    // different; an importer that never errs still counts, by its output).
    // Measured at seed 0x5eed705120261019: 85% (letta-code) to 99%. Removing
    // a file alone, with every byte mutation a no-op, reaches about 2%.
    assert_eq!(mutations, cases.len() * n, "a case did not run all its mutations");
    assert_eq!(results.len(), cases.len());
    let unreached: Vec<String> = reached.iter().filter(|(_, (r, m))| r * 2 < *m).map(|(f, (r, m))| format!("{f} {r}/{m}")).collect();
    assert!(unreached.is_empty(), "under half the mutations changed what these formats import: {unreached:?}");
    // The same floor per fixture. A fixture whose every file is empty
    // has no byte to mutate; that population is pinned by name.
    let empty: Vec<String> = cases
        .iter()
        .filter(|c| c.files.iter().all(|f| std::fs::metadata(c.native.join(f)).map(|m| m.len() == 0).unwrap_or(false)))
        .map(|c| format!("{}/{}", c.format, c.name))
        .collect();
    if std::env::var_os("MUTATE_FIXTURE").is_none() {
        assert_eq!(empty, NOTHING_TO_MUTATE, "the fixtures with nothing to mutate changed");
    }
    let unreached: Vec<String> = results
        .iter()
        .filter(|(i, t)| !empty.contains(&format!("{}/{}", cases[*i].format, cases[*i].name)) && t.reached * 2 < t.mutations)
        .map(|(i, t)| format!("{}/{} {}/{}", cases[*i].format, cases[*i].name, t.reached, t.mutations))
        .collect();
    assert!(unreached.is_empty(), "under half the mutations changed what these fixtures import: {unreached:?}");
    assert!(panics.is_empty(), "{} panics on mutated fixtures (seed {seed:#x}); first: {}", panics.len(), panics[0]);
}

/// The mutations themselves are deterministic and do what they say.
#[test]
fn mutations_are_deterministic_and_change_the_input() {
    let _hook = HOOK.lock().unwrap_or_else(|e| e.into_inner());
    let src = b"{\"a\": \"x\", \"n\": 12}\n{\"b\": \"y\", \"n\": 3.5}\n{\"c\": \"z\"}\n".to_vec();
    for kind in 0..KINDS {
        let (mut a, mut b) = (src.clone(), src.clone());
        let (mut sa, mut sb) = (SEED, SEED);
        let da = mutate(kind, &mut a, &mut sa);
        let db = mutate(kind, &mut b, &mut sb);
        assert_eq!((&a, &da), (&b, &db), "kind {kind} is not deterministic");
        // Opposed: some seed changes the input for every kind.
        let changed = (1..50u64).any(|k| {
            let mut c = src.clone();
            let mut s = SEED ^ k.wrapping_mul(0x9e37_79b9_7f4a_7c15);
            mutate(kind, &mut c, &mut s);
            c != src
        });
        assert!(changed, "kind {kind} never changes the input");
    }
}

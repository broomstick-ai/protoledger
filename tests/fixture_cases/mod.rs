//! The fixture cases a fixture-driven suite runs over: every
//! `fixtures/<format>/<case>/native` whose `expect.json` asks for no crate
//! feature this build lacks (`requires_features`, which tests/fixtures.rs
//! checks is complete: a case holding a SQLite file must list `sqlite`, one
//! holding `.zst` files `zstd`). A build without `sqlite` (CI's "no default
//! features" and "store without sqlite") cannot read the SQLite-backed
//! cases (nor a `_live-*` case holding a SQLite file, which has no
//! `expect.json`), so they are left out here, once, and named on stderr,
//! rather than failing to import inside each suite.
//!
//! A suite never passes on an empty or shrunken population: [`readable`]
//! asserts the exact number of cases this feature set must read, and
//! [`FeatureSet::min_ledgers`] is the floor a suite's ledger count must
//! clear for it.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

/// The feature sets the fixture population depends on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeatureSet {
    /// `sqlite` and `zstd` both on (the default build, `--all-features`).
    Full,
    /// Neither `sqlite` nor `zstd` (`--no-default-features`, with or without
    /// `--features store,cli`).
    NoSqliteNoZstd,
    /// `sqlite` without `zstd`.
    SqliteNoZstd,
    /// `zstd` without `sqlite`.
    ZstdNoSqlite,
}

impl FeatureSet {
    pub fn this_build() -> FeatureSet {
        match (cfg!(feature = "sqlite"), cfg!(feature = "zstd")) {
            (true, true) => FeatureSet::Full,
            (false, false) => FeatureSet::NoSqliteNoZstd,
            (true, false) => FeatureSet::SqliteNoZstd,
            (false, true) => FeatureSet::ZstdNoSqlite,
        }
    }

    /// How many fixture cases a build with this feature set reads: 167 need
    /// no optional feature, 25 need `sqlite` (24 that say so and one
    /// `_live-*` SQLite case), 1 needs `zstd`. Adding or
    /// removing a fixture changes it, on purpose: a case that silently
    /// stopped being read would otherwise go unnoticed.
    pub fn cases(self) -> usize {
        match self {
            FeatureSet::Full => 193,
            FeatureSet::NoSqliteNoZstd => 167,
            FeatureSet::SqliteNoZstd => 192,
            FeatureSet::ZstdNoSqlite => 168,
        }
    }

    /// The fewest ledgers a suite importing every readable case into a store
    /// must read back. The SQLite-backed cases hold about 20 ledgers.
    pub fn min_ledgers(self) -> usize {
        match self {
            FeatureSet::Full | FeatureSet::SqliteNoZstd => 250,
            FeatureSet::NoSqliteNoZstd | FeatureSet::ZstdNoSqlite => 230,
        }
    }
}

/// The `requires_features` of a case's `expect.json` that this build lacks.
/// A name the harness does not know is a failure, as in tests/fixtures.rs.
pub fn lacking_features(case: &Path) -> Vec<String> {
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

/// The features a case needs that this build lacks: its `requires_features`,
/// and `sqlite` for any case holding a SQLite file whether or not it says so.
pub fn lacking(case: &Path) -> Vec<String> {
    let mut out = lacking_features(case);
    if !cfg!(feature = "sqlite") && !out.iter().any(|f| f == "sqlite") && holds_sqlite(&case.join("native")) {
        out.push("sqlite".into());
    }
    out
}

/// Whether any file under `native` starts with SQLite's header.
pub fn holds_sqlite(native: &Path) -> bool {
    use std::io::Read;
    walkdir::WalkDir::new(native).into_iter().filter_map(|e| e.ok()).filter(|e| e.file_type().is_file()).any(|e| {
        let mut head = [0u8; 16];
        std::fs::File::open(e.path()).and_then(|mut f| f.read_exact(&mut head)).is_ok() && &head == b"SQLite format 3\0"
    })
}

/// Every `fixtures/<format>/<case>/native` this build can read, with its
/// format's name, sorted. Asserts there are exactly
/// [`FeatureSet::cases`] of them.
pub fn readable() -> Vec<(String, PathBuf)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    let (mut out, mut left_out) = (Vec::new(), Vec::new());
    for f in std::fs::read_dir(&root).unwrap() {
        let f = f.unwrap().path();
        if !f.is_dir() {
            continue;
        }
        for c in std::fs::read_dir(&f).unwrap() {
            let case = c.unwrap().path();
            let native = case.join("native");
            if !native.is_dir() {
                continue;
            }
            let lacking = lacking(&case);
            if lacking.is_empty() {
                out.push((f.file_name().unwrap().to_string_lossy().into_owned(), native));
            } else {
                left_out.push(format!("{} (needs {})", case.strip_prefix(&root).unwrap().display(), lacking.join(", ")));
            }
        }
    }
    out.sort();
    left_out.sort();
    let set = FeatureSet::this_build();
    if !left_out.is_empty() {
        eprintln!("fixture cases this build ({set:?}) cannot read, left out: {}", left_out.join("; "));
    }
    assert_eq!(out.len(), set.cases(), "fixture cases readable with feature set {set:?} ({} left out)", left_out.len());
    out
}

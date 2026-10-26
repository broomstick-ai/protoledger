//! The database header values an application reads back from a SQLite file:
//! `PRAGMA application_id`, `user_version` and `encoding`.
//!
//! Every SQLite-backed adapter reads them on import with [`read`] and keeps
//! them, under the key [`KEY`], on every ledger of the file (in its source
//! `extra`), or, for OpenClaw, whose store cannot be written without its
//! snapshot, in the snapshot. The
//! object holds only the values that differ from a new database's (0, 0,
//! `UTF-8`), so a file with the defaults adds nothing to its ledger. On
//! export, [`write`] sets them on the new file before anything else is
//! written, and [`pick`] names the loss when ledgers from files with
//! different header values are written to one file.

use crate::formats::Losses;
use crate::model::Obj;
use anyhow::{Result, bail};
use rusqlite::Connection;
use serde_json::{Map, Value, json};

/// The key the header values are kept under.
pub(crate) const KEY: &str = "sqlite_header";

/// Export loss: one file written from ledgers of files whose header values
/// differ keeps the values of the ledger whose id sorts first (the set's
/// order).
pub(crate) const CONFLICT: &str = "sqlite-header-conflict";

/// The header values of `c` that differ from a new database's, or `None`.
pub(crate) fn read(c: &Connection) -> Result<Option<Value>> {
    let app: i64 = c.query_row("PRAGMA application_id", [], |r| r.get(0))?;
    let user: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let encoding: String = c.query_row("PRAGMA encoding", [], |r| r.get(0))?;
    let mut m = Map::new();
    if app != 0 {
        m.insert("application_id".into(), json!(app));
    }
    if user != 0 {
        m.insert("user_version".into(), json!(user));
    }
    if encoding != "UTF-8" {
        m.insert("encoding".into(), json!(encoding));
    }
    Ok((!m.is_empty()).then_some(Value::Object(m)))
}

/// Set `h` (from [`read`]) on `c`, a database nothing has been written to
/// yet: the encoding can only be chosen before the first table exists.
pub(crate) fn write(c: &Connection, h: Option<&Value>) -> Result<()> {
    let Some(h) = h else { return Ok(()) };
    if let Some(e) = h.get("encoding").and_then(Value::as_str) {
        c.pragma_update(None, "encoding", e)?;
        let got: String = c.query_row("PRAGMA encoding", [], |r| r.get(0))?;
        if !got.eq_ignore_ascii_case(e) {
            bail!("the database encoding {e} could not be set (the file already has {got})");
        }
    }
    if let Some(n) = h.get("application_id").and_then(Value::as_i64) {
        c.pragma_update(None, "application_id", n)?;
    }
    if let Some(n) = h.get("user_version").and_then(Value::as_i64) {
        c.pragma_update(None, "user_version", n)?;
    }
    Ok(())
}

/// The header to write for one file built from several source files, given
/// each source file's header (`None` for the defaults) in the set's order,
/// which sorts ledgers by id: the first one. Each
/// later one that differs is named [`CONFLICT`], since its values are not
/// written.
pub(crate) fn pick<'a>(headers: impl IntoIterator<Item = Option<&'a Value>>, losses: &mut Losses) -> Option<Value> {
    let mut it = headers.into_iter();
    let first = it.next().flatten();
    let differ = it.filter(|h| *h != first).count() as u64;
    losses.add_n(
        CONFLICT,
        None,
        differ,
        "ledgers read from SQLite files with different header values (PRAGMA application_id, user_version, encoding) are written to one file, which keeps the values of the ledger whose id sorts first",
    );
    first.cloned()
}

/// Export loss: ledgers of a SQLite file written without that file's
/// store-level data (its schema text, the rows of its other tables, its
/// sequence counters), which rides on one ledger of the file: that ledger is
/// not in the exported set, or another file's store is the one written.
pub(crate) const STORE_LEFT_OUT: &str = "sqlite-store-left-out";

/// One file written from ledgers read from SQLite files, given each such
/// ledger's (source file, source `extra`) in the set's order and the source
/// file whose store-level data is written: the header values to write. Each
/// source file counts once, with the values its ledgers carry, in the order
/// of its first ledger, so the file keeps the values of the ledger whose id
/// sorts first ([`pick`]). Every other source file's store-level data is not
/// written, and each is named [`STORE_LEFT_OUT`].
pub(crate) fn for_files<'a>(ledgers: impl IntoIterator<Item = (&'a str, &'a Obj)>, store_file: Option<&str>, losses: &mut Losses) -> Option<Value> {
    let mut files: Vec<(&str, Option<&Value>)> = Vec::new();
    for (file, extra) in ledgers {
        match files.iter_mut().find(|(f, _)| *f == file) {
            Some((_, h)) => *h = h.or(extra.get(KEY)),
            None => files.push((file, extra.get(KEY))),
        }
    }
    losses.add_n(
        STORE_LEFT_OUT,
        None,
        files.iter().filter(|(f, _)| Some(*f) != store_file).count() as u64,
        "ledgers of a SQLite file exported without the ledger that carries the file's store-level data (its schema text, the rows of its other tables, its sequence counters), or with another file's store written instead: the file written does not hold them",
    );
    pick(files.into_iter().map(|(_, h)| h), losses)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db(sql: &str) -> (tempfile::TempDir, Connection) {
        let d = tempfile::tempdir().unwrap();
        let c = Connection::open(d.path().join("x.db")).unwrap();
        c.execute_batch(sql).unwrap();
        (d, c)
    }

    /// Each value round-trips through [`read`] and [`write`]; a database with
    /// the defaults reads as `None`. Two different non-zero values, so a
    /// constant written for every file fails one of them.
    #[test]
    fn header_values_read_and_write_back() {
        let (_d, c) = db("CREATE TABLE t (x);");
        assert_eq!(read(&c).unwrap(), None, "a new database has the default header");
        for (app, user, enc) in [(2095157089i64, 24i64, "UTF-16le"), (7, 3, "UTF-16be"), (-5, 0, "UTF-8")] {
            let (_d, c) = db(&format!("PRAGMA encoding = '{enc}'; PRAGMA application_id = {app}; PRAGMA user_version = {user}; CREATE TABLE t (x);"));
            let h = read(&c).unwrap().unwrap();
            assert_eq!(h.get("application_id"), Some(&json!(app)));
            assert_eq!(h.get("user_version"), (user != 0).then(|| json!(user)).as_ref());
            assert_eq!(h.get("encoding"), (enc != "UTF-8").then(|| json!(enc)).as_ref());
            let (_e, out) = db("");
            write(&out, Some(&h)).unwrap();
            out.execute_batch("CREATE TABLE t (x); INSERT INTO t VALUES ('é');").unwrap();
            assert_eq!(read(&out).unwrap(), Some(h), "written back as read");
        }
    }

    /// Only a header that differs from the first is a loss; agreeing ones
    /// (and all-default ones) are not.
    #[test]
    fn pick_names_a_differing_header_only() {
        let a = json!({"user_version": 24});
        let b = json!({"user_version": 25});
        let mut l = Losses::default();
        assert_eq!(pick([Some(&a), Some(&a)], &mut l), Some(a.clone()));
        assert_eq!(pick([None, None], &mut l), None);
        assert!(l.is_empty(), "{l:?}");
        assert_eq!(pick([Some(&a), Some(&b), None, Some(&a)], &mut l), Some(a.clone()));
        assert_eq!(l.0.iter().find(|x| x.code == CONFLICT).map(|x| x.count), Some(2));
    }

    // ------------------------------------------------ every adapter, end to end
    //
    // Each test copies a fixture's native data, optionally sets header values
    // on the copy's database, imports it with the adapter, exports through the
    // export entry point, writes the output to another directory and reads the
    // PRAGMAs from both files: the source and the exported file, never the
    // ledger.

    use crate::formats::{ExportOptions, ImportOptions, by_name};
    use crate::ledger::LedgerSet;
    use std::path::Path;

    /// (application_id, user_version, encoding) as SQLite reports them for the file at `p`.
    type Pragmas = (i64, i64, String);

    fn pragmas(p: &Path) -> Pragmas {
        let c = Connection::open_with_flags(p, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let get = |q: &str| c.query_row(q, [], |r| r.get::<_, rusqlite::types::Value>(0)).unwrap();
        let int = |v| match v {
            rusqlite::types::Value::Integer(n) => n,
            v => panic!("{v:?}"),
        };
        let enc = match get("PRAGMA encoding") {
            rusqlite::types::Value::Text(s) => s,
            v => panic!("{v:?}"),
        };
        (int(get("PRAGMA application_id")), int(get("PRAGMA user_version")), enc)
    }

    fn copy_tree(from: &Path, to: &Path) {
        for e in walkdir::WalkDir::new(from) {
            let e = e.unwrap();
            let dst = to.join(e.path().strip_prefix(from).unwrap());
            if e.file_type().is_dir() {
                std::fs::create_dir_all(&dst).unwrap();
            } else {
                std::fs::copy(e.path(), &dst).unwrap();
            }
        }
    }

    /// A copy of `fixtures/<case>/native` in a new directory.
    fn native_copy(case: &str) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        copy_tree(&Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures").join(case).join("native"), d.path());
        d
    }

    fn set_header(db: &Path, app: i64, user: i64) {
        let c = Connection::open(db).unwrap();
        c.pragma_update(None, "application_id", app).unwrap();
        c.pragma_update(None, "user_version", user).unwrap();
    }

    /// Rewrite `db` (plain tables only) as a new file in `encoding`, keeping
    /// its header values: SQLite cannot change an existing file's encoding.
    fn re_encode(db: &Path, encoding: &str) {
        let (app, user, _) = pragmas(db);
        let tmp = db.with_extension("re-encoded");
        {
            let src = Connection::open(db).unwrap();
            let dst = Connection::open(&tmp).unwrap();
            dst.pragma_update(None, "encoding", encoding).unwrap();
            let objects: Vec<(String, String, String)> = src
                .prepare("SELECT type, name, sql FROM sqlite_master WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%' ORDER BY rowid")
                .unwrap()
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            for (ty, name, sql) in &objects {
                dst.execute_batch(sql).unwrap();
                if ty != "table" {
                    continue;
                }
                let mut st = src.prepare(&format!("SELECT * FROM \"{name}\"")).unwrap();
                let n = st.column_count();
                let rows: Vec<Vec<rusqlite::types::Value>> =
                    st.query_map([], |r| (0..n).map(|i| r.get(i)).collect()).unwrap().collect::<rusqlite::Result<_>>().unwrap();
                let ins = format!("INSERT INTO \"{name}\" VALUES ({})", vec!["?"; n].join(", "));
                for r in rows {
                    dst.execute(&ins, rusqlite::params_from_iter(r)).unwrap();
                }
            }
            dst.pragma_update(None, "application_id", app).unwrap();
            dst.pragma_update(None, "user_version", user).unwrap();
        }
        std::fs::rename(&tmp, db).unwrap();
        assert_eq!(pragmas(db).2, encoding, "the copy is re-encoded");
    }

    /// Import `dir` as `format`, let `edit` change the set, export through the
    /// entry point and write the output to a new directory.
    fn trip(format: &str, dir: &Path, edit: impl FnOnce(&mut LedgerSet)) -> (tempfile::TempDir, Losses) {
        let f = by_name(format).unwrap();
        let mut set = f.import(dir, &ImportOptions::standard()).unwrap().set;
        edit(&mut set);
        let out = crate::formats::export(f, &set, &ExportOptions::standard()).unwrap();
        let d = tempfile::tempdir().unwrap();
        out.output.write_to(d.path(), true).unwrap();
        (d, out.losses)
    }

    /// The self trip of `case` (its database at `db` under `native/`), after
    /// `prep` changed the copy: the exported file's header values equal the
    /// copy's, read from both files. Returns them.
    fn self_trip(format: &str, case: &str, db: &str, prep: impl FnOnce(&Path), edit: impl FnOnce(&mut LedgerSet)) -> Pragmas {
        let src = native_copy(case);
        prep(&src.path().join(db));
        let want = pragmas(&src.path().join(db));
        let (out, losses) = trip(format, src.path(), edit);
        let exported = out.path().join(db);
        assert_ne!(exported, src.path().join(db), "two different files are compared");
        assert!(exported.is_file(), "{case}: the export writes {db}");
        assert_eq!(pragmas(&exported), want, "{case}: the exported file's header values are the source's");
        assert!(!losses.0.iter().any(|l| l.code == CONFLICT), "{case}: {losses:?}");
        want
    }

    fn none(_: &Path) {}
    fn keep(_: &mut LedgerSet) {}

    /// The cases each test runs: the fixture as it is (asserting what it
    /// holds), then two different synthetic values, so a constant (a
    /// hard-coded value, or always 0) fails one of them.
    fn synthetic(format: &str, case: &str, db: &str, edit: fn(&mut LedgerSet)) {
        assert_eq!(self_trip(format, case, db, |p| set_header(p, 1_234_567, 7), edit), (1_234_567, 7, "UTF-8".into()));
        assert_eq!(self_trip(format, case, db, |p| set_header(p, -42, 25), edit), (-42, 25, "UTF-8".into()));
        assert_eq!(self_trip(format, case, db, |p| set_header(p, 0, 0), edit), (0, 0, "UTF-8".into()));
    }

    #[test]
    fn hermes_carries_the_sqlite_header() {
        assert_eq!(self_trip("hermes", "hermes/state-db-v31-lineage", "state.db", none, keep), (2095157089, 0, "UTF-8".into()));
        assert_eq!(self_trip("hermes", "hermes/state-db-v4", "state.db", none, keep), (0, 0, "UTF-8".into()));
        synthetic("hermes", "hermes/state-db-v4", "state.db", keep);
    }

    #[test]
    fn openclaw_carries_the_sqlite_header() {
        let db = "agents/main/agent/openclaw-agent.sqlite";
        assert_eq!(self_trip("openclaw", "openclaw/sqlite-real-transcripts", db, none, keep), (0, 24, "UTF-8".into()));
        assert_eq!(self_trip("openclaw", "openclaw/sqlite-subagent-store", db, none, keep), (0, 24, "UTF-8".into()));
        synthetic("openclaw", "openclaw/sqlite-subagent-store", db, keep);
    }

    /// A cursor-agent store whose ledger is unchanged is written back byte
    /// for byte; one with an appended event is rebuilt, which is the path
    /// these values have to survive.
    #[test]
    fn cursor_store_carries_the_sqlite_header() {
        fn append(set: &mut LedgerSet) {
            let mut l = set.iter().next().unwrap().clone();
            l.append_unique(crate::model::Event::new("appended", crate::model::Kind::Text).with("role", "user").with("text", "one more"));
            set.insert(l);
        }
        // The two paths are the two: only the appended trip rebuilds the store.
        for (edit, rebuilt) in [(keep as fn(&mut LedgerSet), false), (append, true)] {
            let src = native_copy("cursor/store-synthetic-tool-calls");
            let (_out, losses) = trip("cursor", src.path(), edit);
            assert_eq!(losses.0.iter().any(|l| l.code == "store-rebuilt"), rebuilt, "{losses:?}");
        }
        for edit in [keep as fn(&mut LedgerSet), append] {
            assert_eq!(self_trip("cursor", "cursor/store-synthetic-tool-calls", "store.db", none, edit), (0, 0, "UTF-8".into()));
            synthetic("cursor", "cursor/store-synthetic-tool-calls", "store.db", edit);
            let enc = self_trip(
                "cursor",
                "cursor/store-synthetic-tool-calls",
                "store.db",
                |p| {
                    set_header(p, 99, 3);
                    re_encode(p, "UTF-16le");
                },
                edit,
            );
            assert_eq!(enc, (99, 3, "UTF-16le".into()));
        }
    }

    #[test]
    fn cursor_ide_carries_the_sqlite_header() {
        let case = "cursor/ide-synthetic-inline-and-tools";
        assert_eq!(self_trip("cursor", case, "state.vscdb", none, keep), (0, 0, "UTF-8".into()));
        synthetic("cursor", case, "state.vscdb", keep);
        let enc = self_trip(
            "cursor",
            case,
            "state.vscdb",
            |p| {
                set_header(p, 5, 6);
                re_encode(p, "UTF-16be");
            },
            keep,
        );
        assert_eq!(enc, (5, 6, "UTF-16be".into()));
    }

    #[test]
    fn opencode_carries_the_sqlite_header() {
        for case in ["opencode/v1-sqlite-1.2.21-generated", "opencode/v2-sqlite-2.0.31-generated"] {
            assert_eq!(self_trip("opencode", case, "opencode.db", none, keep), (0, 0, "UTF-8".into()));
            synthetic("opencode", case, "opencode.db", keep);
        }
    }

    #[test]
    fn deepagents_carries_the_sqlite_header() {
        assert_eq!(self_trip("deepagents", "deepagents/simple-chat", "sessions.db", none, keep), (0, 0, "UTF-8".into()));
        synthetic("deepagents", "deepagents/simple-chat", "sessions.db", keep);
    }

    /// Two deepagents stores are exported as one file: when their header
    /// values differ, the file keeps the first ledger's (in the set's order) and the other is named
    /// a loss. Opposed: equal values are written with no loss.
    #[test]
    fn deepagents_names_the_header_it_cannot_write() {
        for (b_user, lost) in [(8i64, 1u64), (7, 0)] {
            let d = tempfile::tempdir().unwrap();
            for (sub, case) in [("a", "deepagents/simple-chat"), ("b", "deepagents/tool-calls-files")] {
                copy_tree(&Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures").join(case).join("native"), &d.path().join(sub));
            }
            set_header(&d.path().join("a/sessions.db"), 0, 7);
            set_header(&d.path().join("b/sessions.db"), 0, b_user);
            let (out, losses) = trip("deepagents", d.path(), keep);
            let files: Vec<_> = walkdir::WalkDir::new(out.path()).into_iter().filter_map(|e| e.ok()).filter(|e| e.file_type().is_file()).collect();
            assert_eq!(files.len(), 1, "one file is written: {files:?}");
            assert_eq!(pragmas(files[0].path()).1, 7, "the first store's value is written");
            let got = losses.0.iter().find(|l| l.code == CONFLICT).map(|l| l.count).unwrap_or(0);
            assert_eq!(got, lost, "b user_version {b_user}: {losses:?}");
        }
    }

    /// Two IDE `state.vscdb` files are exported as one: when their header
    /// values differ, the file keeps the first ledger's (in the set's order) and every ledger
    /// whose values are not written is named a loss. Opposed: equal values
    /// are written with no loss.
    #[test]
    fn cursor_ide_names_the_header_it_cannot_write() {
        for (b_user, lost) in [(8i64, 2u64), (7, 0)] {
            let d = tempfile::tempdir().unwrap();
            for (sub, case) in [("a", "cursor/ide-synthetic-inline-and-tools"), ("b", "cursor/ide-cursor-history-v016")] {
                copy_tree(&Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures").join(case).join("native"), &d.path().join(sub));
            }
            set_header(&d.path().join("a/state.vscdb"), 0, 7);
            set_header(&d.path().join("b/state.vscdb"), 0, b_user);
            let (out, losses) = trip("cursor", d.path(), keep);
            let files: Vec<_> = walkdir::WalkDir::new(out.path()).into_iter().filter_map(|e| e.ok()).filter(|e| e.file_type().is_file()).collect();
            assert_eq!(files.len(), 1, "one file is written: {files:?}");
            let got = losses.0.iter().find(|l| l.code == CONFLICT).map(|l| l.count).unwrap_or(0);
            assert_eq!(got, lost, "b user_version {b_user}: {losses:?}");
            let written = pragmas(files[0].path()).1;
            assert!(written == 7 || written == b_user, "one source's value is written, not a default: {written}");
            if lost == 0 {
                assert_eq!(written, 7);
            }
        }
    }

    /// Two hermes `state.db` files are exported as one: when their header
    /// values differ, the file keeps the first ledger's (in the set's order) and the other is named
    /// a loss. Opposed: equal values are written with no loss.
    #[test]
    fn hermes_names_the_header_it_cannot_write() {
        for (b_user, lost) in [(8i64, 1u64), (7, 0)] {
            let d = tempfile::tempdir().unwrap();
            // Two different stores: the same one twice would repeat its session ids.
            for (sub, case) in [("a", "hermes/state-db-v31-lineage"), ("b", "hermes/state-db-v4")] {
                copy_tree(&Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures").join(case).join("native"), &d.path().join(sub));
            }
            set_header(&d.path().join("a/state.db"), 0, 7);
            set_header(&d.path().join("b/state.db"), 0, b_user);
            // The two stores number their messages from 1; one file holds both.
            Connection::open(d.path().join("b/state.db")).unwrap().execute_batch("UPDATE messages SET id = id + 100000;").unwrap();
            let (out, losses) = trip("hermes", d.path(), keep);
            let files: Vec<_> = walkdir::WalkDir::new(out.path()).into_iter().filter_map(|e| e.ok()).filter(|e| e.file_type().is_file()).collect();
            assert_eq!(files.len(), 1, "one file is written: {files:?}");
            let got = losses.0.iter().find(|l| l.code == CONFLICT).map(|l| l.count).unwrap_or(0);
            assert_eq!(got, lost, "b user_version {b_user}: {losses:?}");
            // The ledger whose id sorts first is b's, the one whose id sorts
            // last a's, and a/ comes first in the files' order: keeping the
            // last ledger's or the first file's values writes 7.
            let set = by_name("hermes").unwrap().import(d.path(), &ImportOptions::standard()).unwrap().set;
            let file_of = |l: &crate::ledger::Ledger| l.agent().source.as_ref().unwrap().extra["file"].as_str().unwrap().to_string();
            assert!(file_of(set.iter().next().unwrap()).starts_with("b/"), "the lowest id is b's");
            assert!(file_of(set.iter().last().unwrap()).starts_with("a/"), "the highest id is a's");
            assert_eq!(pragmas(files[0].path()).1, b_user, "the values of the ledger whose id sorts first are written");
        }
    }

    /// An OpenClaw store whose snapshot rides on a ledger with an appended
    /// event is not rebuilt, so its header values are not written: named
    /// `sqlite-store-not-rebuilt`. Opposed: an event appended to a session
    /// that does not hold the snapshot leaves the store rebuilt with its
    /// header and no such loss.
    #[test]
    fn openclaw_names_the_store_it_does_not_rebuild() {
        let db = "agents/main/agent/openclaw-agent.sqlite";
        let holds = |l: &crate::ledger::Ledger| l.agent().source.as_ref().and_then(|s| s.extra.get("sqlite")).and_then(|x| x.get("store")).is_some();
        for to_holder in [true, false] {
            let src = native_copy("openclaw/sqlite-subagent-store");
            let (out, losses) = trip("openclaw", src.path(), |set| {
                let mut l = set.iter().find(|l| holds(l) == to_holder).expect("a ledger of each kind").clone();
                l.append_unique(crate::model::Event::new("appended", crate::model::Kind::Text).with("role", "user").with("text", "one more"));
                set.insert(l);
            });
            let got = losses.0.iter().find(|l| l.code == crate::formats::openclaw::sqlite::STORE_NOT_REBUILT).map(|l| l.count);
            if to_holder {
                assert_eq!(got, Some(1), "{losses:?}");
                assert!(!out.path().join(db).exists(), "the store file is not written");
            } else {
                assert_eq!(got, None, "{losses:?}");
                assert_eq!(pragmas(&out.path().join(db)), (0, 24, "UTF-8".into()), "the store is rebuilt with its header");
            }
        }
    }

    /// The set without the ledger that carries its file's store.
    fn without_store(set: &mut LedgerSet, holds: fn(&crate::ledger::Ledger) -> bool) {
        let n = set.len();
        let mut rest = LedgerSet::new();
        for l in set.iter().filter(|l| !holds(l)) {
            rest.insert(l.clone());
        }
        assert_eq!(rest.len(), n - 1, "exactly one ledger carries the store");
        assert!(!rest.is_empty(), "a subset is left");
        *set = rest;
    }

    fn holds_store(l: &crate::ledger::Ledger) -> bool {
        l.agent().source.as_ref().is_some_and(|s| s.extra.contains_key("store"))
    }

    /// A subset of a file's ledgers without the one that carries its store
    /// (and a full export, opposed): the exported file has the source's
    /// header values, read from both files, and the store-level data the
    /// subset cannot rebuild is named `sqlite-store-left-out` (and only then).
    /// The fixture's own values, then two synthetic ones, so a constant fails.
    fn subset_trip(format: &str, case: &str, db: &str, fixture: Pragmas) {
        let preps: [(Option<(i64, i64)>, Pragmas); 3] =
            [(None, fixture), (Some((1_234_567, 7)), (1_234_567, 7, "UTF-8".into())), (Some((-42, 25)), (-42, 25, "UTF-8".into()))];
        for (prep, want) in preps {
            for subset in [true, false] {
                let src = native_copy(case);
                if let Some((app, user)) = prep {
                    set_header(&src.path().join(db), app, user);
                }
                assert_eq!(pragmas(&src.path().join(db)), want, "{case}: the source's values");
                let (out, losses) = trip(format, src.path(), |set| {
                    if subset {
                        without_store(set, holds_store)
                    }
                });
                let exported = out.path().join(db);
                assert_ne!(exported, src.path().join(db), "two different files are compared");
                assert_eq!(pragmas(&exported), want, "{case} subset {subset}: the exported file's header values are the source's");
                let left = losses.0.iter().find(|l| l.code == STORE_LEFT_OUT).map(|l| l.count);
                assert_eq!(left, subset.then_some(1), "{case} subset {subset}: {losses:?}");
                assert!(!losses.0.iter().any(|l| l.code == CONFLICT), "{case}: {losses:?}");
            }
        }
    }

    #[test]
    fn hermes_subset_without_the_store_keeps_the_header() {
        subset_trip("hermes", "hermes/state-db-v31-lineage", "state.db", (2095157089, 0, "UTF-8".into()));
    }

    #[test]
    fn deepagents_subset_without_the_store_keeps_the_header() {
        subset_trip("deepagents", "deepagents/handbuilt-overwrite", "checkpoint.db", (0, 0, "UTF-8".into()));
    }

    /// An OpenClaw store cannot be rebuilt without its snapshot, which rides
    /// on its first session: a subset without that session writes its
    /// sessions as JSONL and names the store (with its header values)
    /// `sqlite-store-not-rebuilt`. Opposed: the full set rebuilds the store
    /// with its header and names nothing.
    #[test]
    fn openclaw_subset_without_the_store_names_it() {
        let db = "agents/main/agent/openclaw-agent.sqlite";
        let holds = |l: &crate::ledger::Ledger| l.agent().source.as_ref().and_then(|s| s.extra.get("sqlite")).and_then(|x| x.get("store")).is_some();
        for subset in [true, false] {
            let src = native_copy("openclaw/sqlite-subagent-store");
            let (out, losses) = trip("openclaw", src.path(), |set| {
                if subset {
                    without_store(set, holds)
                }
            });
            let got = losses.0.iter().find(|l| l.code == crate::formats::openclaw::sqlite::STORE_NOT_REBUILT).map(|l| l.count);
            let jsonl =
                walkdir::WalkDir::new(out.path()).into_iter().filter_map(|e| e.ok()).filter(|e| e.path().extension().is_some_and(|x| x == "jsonl")).count();
            if subset {
                assert_eq!(got, Some(1), "{losses:?}");
                assert!(!out.path().join(db).exists(), "the store file is not written");
                assert!(jsonl > 0, "the subset's sessions are written as JSONL");
            } else {
                assert_eq!(got, None, "{losses:?}");
                assert_eq!(jsonl, 0, "every session is in the store");
                assert_eq!(pragmas(&out.path().join(db)), (0, 24, "UTF-8".into()), "the store is rebuilt with its header");
            }
        }
    }

    /// A subset without the store's ledger, of a `state.db` below the root,
    /// is written to the file its ledgers were read from, with its header
    /// values. Opposed: the full set is written there too.
    #[test]
    fn hermes_subset_is_written_to_its_own_file() {
        for subset in [true, false] {
            let d = tempfile::tempdir().unwrap();
            copy_tree(&Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/hermes/state-db-v31-lineage/native"), &d.path().join("sub"));
            let (out, _) = trip("hermes", d.path(), |set| {
                if subset {
                    without_store(set, holds_store)
                }
            });
            let files: Vec<_> =
                walkdir::WalkDir::new(out.path()).into_iter().filter_map(|e| e.ok()).filter(|e| e.file_type().is_file()).map(|e| e.into_path()).collect();
            assert_eq!(files, vec![out.path().join("sub/state.db")], "subset {subset}: written where its ledgers came from");
            assert_eq!(pragmas(&files[0]), (2095157089, 0, "UTF-8".into()), "subset {subset}");
        }
    }
}

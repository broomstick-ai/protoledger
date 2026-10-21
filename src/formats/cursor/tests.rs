use super::*;
use crate::formats::ExportOptions;
#[cfg(feature = "sqlite")] // used only by the SQLite tests
use crate::loss::{CompareOptions, Diff, compare};
#[cfg(feature = "sqlite")]
use crate::roundtrip::materialise;

#[cfg(feature = "sqlite")] // used only by the SQLite tests
fn foreign() -> LedgerSet {
    let text = include_str!("../../../fixtures/cursor/_generators/foreign-ledger.jsonl");
    LedgerSet::single(Ledger::from_jsonl(text).unwrap().0)
}

#[cfg(feature = "sqlite")] // used only by the SQLite tests
fn trip(variant: &str, carry: bool) -> Diff {
    let p0 = foreign();
    let eo = ExportOptions { variant: Some(variant.into()), carry, target_version: None };
    let out = Cursor.export(&p0, &eo).unwrap();
    let t = tempfile::tempdir().unwrap();
    let path = materialise(&out.output, t.path()).unwrap();
    let io = ImportOptions { variant: None, restore_carried: carry };
    let p1 = Cursor.import(&path, &io).unwrap().set;
    compare(&p0, &p1, CompareOptions { ignore_native: true, ..Default::default() })
}

#[cfg(feature = "sqlite")] // reads or writes a SQLite store
#[test]
fn carry_comes_back_exact() {
    for v in [V_STREAM, V_TRANSCRIPT, V_IDE] {
        let d = trip(v, true);
        assert!(d.is_clean(), "{v}: {}", d.summary());
    }
}

#[cfg(feature = "sqlite")] // reads or writes a SQLite store
#[test]
fn plain_trips_keep_the_conversation() {
    for v in [V_STREAM, V_TRANSCRIPT, V_STORE, V_IDE] {
        let d = trip(v, false);
        eprintln!("{v}: {}", d.summary());
        // A composer bubble holds a call and its result together, so a second
        // parallel call is reordered behind the first one's result.
        let reordered = if v == V_IDE { 1 } else { 0 };
        assert!(d.missing.get("tool_call").copied().unwrap_or(0) <= reordered, "{v}: {}", d.summary());
        assert!(!d.missing.contains_key("tool_result"), "{v}: {}", d.summary());
        assert!(d.missing.get("text").copied().unwrap_or(0) <= 1, "{v}: {}", d.summary());
    }
}

#[test]
fn stream_groups_deltas_and_self_trips() {
    let text = concat!(
        "# a comment line\n",
        r#"{"type":"system","subtype":"init","apiKeySource":"login","cwd":"/w","session_id":"s1","model":"Auto","permissionMode":"default"}"#,
        "\n",
        r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"hi"}]},"session_id":"s1"}"#,
        "\n",
        r#"{"type":"thinking","subtype":"delta","text":"a","session_id":"s1","timestamp_ms":1788998267072}"#,
        "\n",
        r#"{"type":"thinking","subtype":"delta","text":"b","session_id":"s1","timestamp_ms":1788998267073}"#,
        "\n",
        r#"{"type":"thinking","subtype":"completed","session_id":"s1","timestamp_ms":1788998267074}"#,
        "\n",
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Fin"}]},"session_id":"s1","timestamp_ms":1788998267075}"#,
        "\n",
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"al."}]},"session_id":"s1","timestamp_ms":1788998267076}"#,
        "\n",
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Final."}]},"session_id":"s1"}"#,
        "\n",
        r#"{"type":"result","subtype":"success","duration_ms":10,"duration_api_ms":10,"is_error":false,"result":"Final.","session_id":"s1","usage":{"inputTokens":5,"outputTokens":2,"cacheReadTokens":0,"cacheWriteTokens":0}}"#,
        "\n",
    );
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("run.jsonl");
    std::fs::write(&p, text).unwrap();
    assert_eq!(Cursor.detect(&p).unwrap().variant, V_STREAM);
    let imp = Cursor.import(&p, &ImportOptions::standard()).unwrap();
    let l = imp.set.iter().next().unwrap();
    let kinds: Vec<&str> = l.events.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(kinds, ["native", "settings", "text", "reasoning", "text", "usage", "turn"]);
    assert_eq!(l.events[3].str("text"), Some("ab"));
    assert_eq!(l.events[4].str("text"), Some("Final."));
    let out = Cursor.export(&imp.set, &ExportOptions::standard()).unwrap();
    assert_eq!(out.output.files["run.jsonl"], text.as_bytes());
}

/// Lines spelled the way a parsed `Value` does not re-spell them under any
/// feature set (whitespace inside a line, the escape `\u00e9`, `1e3`,
/// `0.000005`) are held as their own text and written back byte for byte, in
/// the stream and transcript variants.
#[test]
fn lines_are_held_and_reemitted_as_spelled() {
    let cases: [(&str, &str, [&str; 2]); 2] = [
        (
            "run.jsonl",
            V_STREAM,
            [
                "{\"type\":\"system\",\"subtype\":\"init\", \"cwd\":\"/w\",\"session_id\":\"s1\",\"model\":\"Auto\",\"x\":1e3}",
                "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"caf\\u00e9\"}]},\"session_id\":\"s1\",\"y\":0.000005}",
            ],
        ),
        (
            "t.jsonl",
            V_TRANSCRIPT,
            [
                "{\"role\":\"user\", \"message\":{\"content\":\"caf\\u00e9\"},\"n\":1e3}",
                "{\"role\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"ok\"}]},\"x\":0.000005}",
            ],
        ),
    ];
    for (name, variant, lines) in cases {
        let text = lines.join("\n") + "\n";
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join(name);
        std::fs::write(&p, &text).unwrap();
        let imp = Cursor.import(&p, &ImportOptions { variant: Some(variant.into()), restore_carried: true }).unwrap();
        let l = imp.set.iter().next().unwrap();
        let held: Vec<&str> = l.events.iter().filter_map(|e| own(e, variant)).filter(|n| n.part == 0).filter_map(|n| n.record_text()).collect();
        assert_eq!(held, lines, "{variant}");
        // As imported, and from the records' own texts alone (no `raw`).
        for set in [imp.set.clone(), crate::formats::atif::jsonfmt::without_raw(&imp.set)] {
            let out = Cursor.export(&set, &ExportOptions::standard()).unwrap();
            assert_eq!(std::str::from_utf8(&out.output.files[name]).unwrap(), text, "{variant}");
        }
    }
}

/// Every row of every table in a `state.vscdb` (SQLite's own `sqlite_*`
/// tables included): (table, each column's storage class and bytes).
#[cfg(feature = "sqlite")]
type DbRows = std::collections::BTreeSet<(String, Vec<(String, Vec<u8>)>)>;

#[cfg(feature = "sqlite")]
fn vscdb_rows(p: &std::path::Path) -> DbRows {
    use rusqlite::types::ValueRef;
    let c = rusqlite::Connection::open_with_flags(p, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let tables: Vec<String> =
        c.prepare("SELECT name FROM sqlite_master WHERE type = 'table'").unwrap().query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
    let mut out = std::collections::BTreeSet::new();
    for table in tables {
        let mut st = c.prepare(&format!("SELECT * FROM \"{}\"", table.replace('"', "\"\""))).unwrap();
        let n = st.column_count();
        let rows = st
            .query_map([], |r| {
                let mut cols = Vec::with_capacity(n);
                for i in 0..n {
                    let (class, bytes) = match r.get_ref(i)? {
                        ValueRef::Null => ("null", vec![]),
                        ValueRef::Integer(i) => ("integer", i.to_string().into_bytes()),
                        ValueRef::Real(f) => ("real", f.to_string().into_bytes()),
                        ValueRef::Text(t) => ("text", t.to_vec()),
                        ValueRef::Blob(b) => ("blob", b.to_vec()),
                    };
                    cols.push((class.to_string(), bytes));
                }
                Ok((table.clone(), cols))
            })
            .unwrap();
        for r in rows {
            out.insert(r.unwrap());
        }
    }
    out
}

/// A row's table, first column (its key) and the storage class of each column.
#[cfg(feature = "sqlite")]
fn row_label(r: &(String, Vec<(String, Vec<u8>)>)) -> String {
    let key = r.1.first().map(|c| String::from_utf8_lossy(&c.1).into_owned()).unwrap_or_default();
    let classes: Vec<&str> = r.1.iter().map(|c| c.0.as_str()).collect();
    format!("{}:{key} {classes:?}", r.0)
}

/// IDE self trip of a fixture: (rows in, rows out, import loss codes).
#[cfg(feature = "sqlite")]
fn ide_self_trip(case: &str) -> (DbRows, DbRows, Vec<String>) {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/cursor").join(case).join("native/state.vscdb");
    let imp = Cursor.import(&src, &ImportOptions::standard()).unwrap();
    let out = Cursor.export(&imp.set, &ExportOptions::standard()).unwrap();
    let t = tempfile::tempdir().unwrap();
    let back = materialise(&out.output, t.path()).unwrap();
    (vscdb_rows(&src), vscdb_rows(&back), imp.losses.0.iter().map(|l| format!("{} ×{}", l.code, l.count)).collect())
}

/// agentsview's composers are stored as BLOBs: each row comes back with its
/// own storage class and bytes, not re-typed as TEXT.
#[cfg(feature = "sqlite")]
#[test]
fn ide_rows_keep_their_storage_class() {
    let (src, back, losses) = ide_self_trip("ide-agentsview-composers");
    assert!(src.iter().all(|r| r.1[1].0 == "blob"), "the fixture's values are BLOBs");
    assert_eq!(src.len(), 7, "the fixture's rows were read");
    assert_rows_eq(&src, &back, &losses);
}

/// The rows only one side holds, labelled, in the failure message.
#[cfg(feature = "sqlite")]
fn assert_rows_eq(src: &DbRows, back: &DbRows, losses: &[String]) {
    let only = |a: &DbRows, b: &DbRows| a.difference(b).map(row_label).collect::<Vec<_>>();
    assert!(src == back, "rows differ: source only {:?}; export only {:?}; import losses {losses:?}", only(src, back), only(back, src));
}

/// cursor-history's v0.16 store: a NULL bubble, a bubble listed under its
/// `bubbleId` field rather than its key, and one listed under an empty id.
/// All eight rows come back, and none is reported lost. The conformance
/// ratchet holds this floor only once a report with no loss is committed on
/// its base; until then this test is what holds it.
#[cfg(feature = "sqlite")]
#[test]
fn ide_rows_outside_the_headers_come_back() {
    let (src, back, losses) = ide_self_trip("ide-cursor-history-v016");
    assert_eq!(src.len(), 8, "the fixture's rows were read");
    assert_rows_eq(&src, &back, &losses);
    assert!(losses.is_empty(), "{losses:?}");
}

/// A thread with no events, exported to Cursor (stream-json by default), is
/// recognised by Cursor's importer as the same thread.
#[test]
fn empty_thread_export_reimports() {
    let a = crate::model::Agent { id: "empty-agent".into(), created_at: None, source: None, parent: None, links: vec![], meta: Obj::new() };
    let set = LedgerSet::single(Ledger::new(a));
    for carry in [true, false] {
        let eo = ExportOptions { variant: None, carry, target_version: None };
        let out = Cursor.export(&set, &eo).unwrap();
        let t = tempfile::tempdir().unwrap();
        out.output.write_scratch(t.path()).unwrap();
        let path = t.path().join(out.output.files.keys().next().unwrap());
        let back = Cursor.import(&path, &ImportOptions { variant: None, restore_carried: carry });
        let back = back.unwrap_or_else(|e| panic!("carry {carry}: {e:#}")).set;
        assert_eq!(back.len(), 1, "carry {carry}");
        if carry {
            let l = back.iter().next().unwrap();
            assert_eq!(l.id(), "empty-agent");
            assert!(l.events.is_empty(), "{:?}", l.events);
        }
    }
}

/// Every IDE fixture's self trip is row-identical: each row's key, storage class and bytes.
#[cfg(feature = "sqlite")]
#[test]
fn ide_fixtures_self_trip_row_identical() {
    for case in ["ide-agentsview-composers", "ide-cursor-history-v016", "ide-synthetic-inline-and-tools"] {
        let (src, back, losses) = ide_self_trip(case);
        assert!(!src.is_empty(), "{case}: no rows read");
        assert_rows_eq(&src, &back, &losses);
    }
}

/// Build a `state.vscdb` at `p` from `stmts`.
#[cfg(feature = "sqlite")]
fn vscdb(p: &std::path::Path, stmts: &str) {
    rusqlite::Connection::open(p).unwrap().execute_batch(stmts).unwrap();
}

/// A `state.vscdb` is the whole IDE's state store. Its threads come back row
/// for row; everything else is counted as skipped under its own code: two
/// `cursorDiskKV` rows tied to no composer (an orphan bubble, `agentKv:` data)
/// and five rows of other tables (`ItemTable` and a third table).
#[cfg(feature = "sqlite")]
#[test]
fn ide_state_outside_the_threads_is_named_as_skipped() {
    let t = tempfile::tempdir().unwrap();
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/cursor/ide-cursor-history-v016/native/state.vscdb");
    let thread_db = t.path().join("threads.vscdb");
    std::fs::copy(&fixture, &thread_db).unwrap();
    let threads = vscdb_rows(&thread_db);
    assert_eq!(threads.len(), 8, "the fixture's rows were read");
    let p = t.path().join("state.vscdb");
    std::fs::copy(&fixture, &p).unwrap();
    vscdb(
        &p,
        "CREATE TABLE ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);
         INSERT INTO ItemTable VALUES ('workbench.panel.width', '300'), ('cursor.theme', X'7B7D'), ('src.vs.platform.reactivestorage', NULL), ('history.recent', '[]');
         CREATE TABLE extensionState (id INTEGER PRIMARY KEY, blob BLOB);
         INSERT INTO extensionState (blob) VALUES (X'00FF');
         INSERT INTO cursorDiskKV VALUES ('bubbleId:99999999-0000-0000-0000-000000000000:orphan', '{\"type\":1,\"text\":\"orphan\"}');
         INSERT INTO cursorDiskKV VALUES ('agentKv:blob:abc', X'00FF10');",
    );
    assert_eq!(vscdb_rows(&p).len(), threads.len() + 5 + 2, "every row of every table of the store was read, the third table's included");
    let imp = Cursor.import(&p, &ImportOptions::standard()).unwrap();
    let losses: Vec<String> = imp.losses.0.iter().map(|l| format!("{} ×{}", l.code, l.count)).collect();
    assert_eq!(losses, ["ide-state-skipped ×5", "unlinked-row-skipped ×2"]);
    let out = Cursor.export(&imp.set, &ExportOptions::standard()).unwrap();
    let back = vscdb_rows(&materialise(&out.output, t.path().join("out").as_path()).unwrap());
    assert_rows_eq(&threads, &back, &losses);
}

/// A row belongs to the composer its key's second field names, exactly, for
/// a listed bubble and for every other row alike: composer `a:b`'s keys name
/// composer `a`, so composer `a:b` takes none of them (its listed bubble is
/// missing and both rows are unlinked), while composer `c` takes its own.
#[cfg(feature = "sqlite")]
#[test]
fn ide_rows_belong_to_the_composer_their_key_names_exactly() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("state.vscdb");
    vscdb(
        &p,
        r#"CREATE TABLE cursorDiskKV (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);
           INSERT INTO cursorDiskKV VALUES ('composerData:a:b', '{"_v":3,"composerId":"a:b","fullConversationHeadersOnly":[{"bubbleId":"X","type":1}]}');
           INSERT INTO cursorDiskKV VALUES ('bubbleId:a:b:X', '{"bubbleId":"X","type":1,"text":"hi"}');
           INSERT INTO cursorDiskKV VALUES ('checkpointId:a:b:cp', '{}');
           INSERT INTO cursorDiskKV VALUES ('composerData:c', '{"_v":3,"composerId":"c","fullConversationHeadersOnly":[{"bubbleId":"Y","type":1}]}');
           INSERT INTO cursorDiskKV VALUES ('bubbleId:c:Y', '{"bubbleId":"Y","type":1,"text":"yo"}');
           INSERT INTO cursorDiskKV VALUES ('checkpointId:c:cp', '{}');"#,
    );
    let imp = Cursor.import(&p, &ImportOptions::standard()).unwrap();
    let losses: Vec<String> = imp.losses.0.iter().map(|l| format!("{} ×{}", l.code, l.count)).collect();
    assert_eq!(losses, ["missing-bubble ×1", "unlinked-row-skipped ×2"]);
    let natives = |id: &str| -> usize { imp.set.iter().find(|l| l.id() == id).unwrap().events.iter().filter(|e| own(e, V_IDE).is_some()).count() };
    assert_eq!(natives("a:b"), 0, "composer a:b took a row its key does not name");
    assert_eq!(natives("c"), 2, "composer c's bubble and checkpoint");
}

/// A `cursorDiskKV` row whose key is not TEXT (a BLOB, a NULL) names no
/// composer: it is counted as unlinked, one each, and the store's threads
/// still import and come back row for row.
#[cfg(feature = "sqlite")]
#[test]
fn ide_rows_with_a_key_that_is_not_text_are_named_as_skipped() {
    let t = tempfile::tempdir().unwrap();
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/cursor/ide-cursor-history-v016/native/state.vscdb");
    let thread_db = t.path().join("threads.vscdb");
    std::fs::copy(&fixture, &thread_db).unwrap();
    let threads = vscdb_rows(&thread_db);
    assert_eq!(threads.len(), 8, "the fixture's rows were read");
    let p = t.path().join("state.vscdb");
    std::fs::copy(&fixture, &p).unwrap();
    // The fixture's key is `NOT NULL`; Cursor's own table is not, so the
    // copy is rebuilt with Cursor's schema, its rows moved over as they are.
    vscdb(
        &p,
        "CREATE TABLE kv (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);
         INSERT INTO kv SELECT key, value FROM cursorDiskKV;
         DROP TABLE cursorDiskKV;
         ALTER TABLE kv RENAME TO cursorDiskKV;
         INSERT INTO cursorDiskKV VALUES (X'6B6579', '{}'), (NULL, '{}');",
    );
    let all = vscdb_rows(&p);
    assert!(threads.is_subset(&all), "the rebuilt table holds the fixture's rows as they were");
    let keys: Vec<&str> = all.difference(&threads).map(|r| r.1[0].0.as_str()).collect();
    assert_eq!(keys, ["blob", "null"], "the two rows were written with a BLOB key and a NULL key");
    let imp = Cursor.import(&p, &ImportOptions::standard()).unwrap_or_else(|e| panic!("{e:#}"));
    let losses: Vec<String> = imp.losses.0.iter().map(|l| format!("{} ×{}", l.code, l.count)).collect();
    assert_eq!(losses, ["unlinked-row-skipped ×2"]);
    let out = Cursor.export(&imp.set, &ExportOptions::standard()).unwrap();
    let back = vscdb_rows(&materialise(&out.output, t.path().join("out").as_path()).unwrap());
    assert_rows_eq(&threads, &back, &losses);
}

/// A thread row whose TEXT value is not UTF-8 comes back as TEXT holding the
/// same bytes, not re-typed and not re-encoded.
#[cfg(feature = "sqlite")]
#[test]
fn ide_text_that_is_not_utf8_keeps_its_bytes() {
    let t = tempfile::tempdir().unwrap();
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/cursor/ide-cursor-history-v016/native/state.vscdb");
    let p = t.path().join("state.vscdb");
    std::fs::copy(&fixture, &p).unwrap();
    let cid: String = rusqlite::Connection::open(&p)
        .unwrap()
        .query_row("SELECT substr(key, 14) FROM cursorDiskKV WHERE key LIKE 'composerData:%' ORDER BY key LIMIT 1", [], |r| r.get(0))
        .unwrap();
    let key = format!("checkpointId:{cid}:bad");
    vscdb(&p, &format!("INSERT INTO cursorDiskKV VALUES ('{key}', CAST(X'41FF42' AS TEXT));"));
    let src = vscdb_rows(&p);
    let row = |rows: &DbRows| rows.iter().find(|r| r.1[0].1 == key.as_bytes()).map(|r| r.1[1].clone());
    assert_eq!(row(&src), Some(("text".to_string(), vec![0x41, 0xFF, 0x42])), "the row was written as TEXT holding 41 FF 42");
    let imp = Cursor.import(&p, &ImportOptions::standard()).unwrap();
    let losses: Vec<String> = imp.losses.0.iter().map(|l| format!("{} ×{}", l.code, l.count)).collect();
    let out = Cursor.export(&imp.set, &ExportOptions::standard()).unwrap();
    let back = vscdb_rows(&materialise(&out.output, t.path().join("out").as_path()).unwrap());
    assert_eq!(row(&back), Some(("text".to_string(), vec![0x41, 0xFF, 0x42])), "import losses {losses:?}");
    assert_rows_eq(&src, &back, &losses);
}

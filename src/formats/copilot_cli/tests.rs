use super::*;
use crate::formats::ExportOptions;
use crate::loss::{CompareOptions, Diff, compare};
use crate::roundtrip::materialise;

fn foreign() -> LedgerSet {
    let text = include_str!("../../../fixtures/copilot-cli/_generators/foreign-ledger.jsonl");
    LedgerSet::single(Ledger::from_jsonl(text).unwrap().0)
}

fn trip(variant: &str, carry: bool) -> Diff {
    let p0 = foreign();
    let eo = ExportOptions { variant: Some(variant.into()), carry, target_version: None };
    let out = CopilotCli.export(&p0, &eo).unwrap();
    let t = tempfile::tempdir().unwrap();
    let path = materialise(&out.output, t.path()).unwrap();
    let io = ImportOptions { variant: None, restore_carried: carry };
    let p1 = CopilotCli.import(&path, &io).unwrap().set;
    compare(&p0, &p1, CompareOptions { ignore_native: true, ..Default::default() })
}

#[test]
fn carry_comes_back_exact() {
    for v in [V_DIR, V_FLAT] {
        let d = trip(v, true);
        assert!(d.is_clean(), "{v}: {}", d.summary());
    }
}

#[test]
fn plain_trips_keep_the_conversation() {
    for v in [V_DIR, V_FLAT, V_LEGACY, V_INTERCHANGE] {
        let d = trip(v, false);
        eprintln!("{v}: {}", d.summary());
        assert!(!d.missing.contains_key("tool_call"), "{v}: {}", d.summary());
        assert!(!d.missing.contains_key("tool_result"), "{v}: {}", d.summary());
    }
}

/// The natives' part-0 record texts of the only ledger in `set`.
fn held(set: &LedgerSet) -> Vec<String> {
    let l = set.iter().next().unwrap();
    l.events.iter().filter_map(|e| e.native.as_ref()).filter(|n| n.part == 0).filter_map(|n| n.record_text().map(str::to_string)).collect()
}

/// The single file `set` exports to, as text.
fn exported(set: &LedgerSet) -> String {
    let out = CopilotCli.export(set, &ExportOptions::standard()).unwrap();
    assert_eq!(out.output.files.len(), 1, "{:?}", out.output.files.keys());
    String::from_utf8(out.output.files.values().next().unwrap().clone()).unwrap()
}

/// Records spelled the way a parsed `Value` does not re-spell them under any
/// feature set (the escape `\u00e9`, `1e3`, `0.000005`, whitespace inside a
/// line, keys out of order) are held as their own text and come back byte
/// for byte: event lines, and the legacy pretty document.
#[test]
fn records_are_held_and_reemitted_as_spelled() {
    let t = tempfile::tempdir().unwrap();
    let lines = [
        r#"{"type":"session.start","data":{"sessionId":"s1","version":1,"producer":"copilot-agent","copilotVersion":"1.0.77","startTime":"2025-12-18T21:32:04.182Z"},"id":"e1", "timestamp":"2025-12-18T21:32:05.000Z","parentId":null}"#,
        r#"{"type":"user.message","data":{"content":"caf\u00e9","n":1e3,"x":0.000005},"id":"e2","timestamp":"2025-12-18T21:32:06.000Z","parentId":"e1"}"#,
    ];
    let text = lines.join("\n") + "\n";
    let p = t.path().join("0aa3a1b2-0000-4000-8000-00000000c0f9.jsonl");
    std::fs::write(&p, &text).unwrap();
    let set = CopilotCli.import(&p, &ImportOptions::standard()).unwrap().set;
    assert_eq!(held(&set), lines);
    assert_eq!(exported(&set), text);
    assert_eq!(exported(&crate::formats::atif::jsonfmt::without_raw(&set)), text, "from the records' own texts alone");

    let doc = "{\n  \"sessionId\": \"s2\",\n  \"startTime\": \"2025-10-01T16:00:00.000Z\",\n  \"chatMessages\": [\n    {\n      \"role\": \"user\",\n      \"content\": \"caf\\u00e9\",\n      \"n\": 1e3\n    }\n  ],\n  \"timeline\": [\n    {\n      \"id\": \"t1\",\n      \"timestamp\": \"2025-10-01T16:00:01.000Z\",\n      \"type\": \"user\",\n      \"text\": \"caf\\u00e9\",\n      \"x\": 0.000005\n    }\n  ]\n}";
    let p = t.path().join("session_s2_1759334400000.json");
    std::fs::write(&p, doc).unwrap();
    let set = CopilotCli.import(&p, &ImportOptions::standard()).unwrap().set;
    assert_eq!(
        held(&set),
        [
            r#"{"role":"user","content":"caf\u00e9","n":1e3}"#,
            r#"{"id":"t1","timestamp":"2025-10-01T16:00:01.000Z","type":"user","text":"caf\u00e9","x":0.000005}"#,
        ]
    );
    assert!(set.iter().next().unwrap().agent().source.as_ref().unwrap().extra.get("raw_file").is_none(), "the layout is reproduced, not kept whole");
    assert_eq!(exported(&set), doc);
    assert_eq!(exported(&crate::formats::atif::jsonfmt::without_raw(&set)), doc, "from the records' own texts alone");
}

/// An unchanged record whose `raw` is a spelling within
/// `util::NUMBER_ULPS` of its record (so `current_raw` writes it) cannot be
/// laid back into the document it came from: the rebuilt text no longer
/// parses to the document's value, the exporter falls back to the value,
/// and that is named.
#[test]
fn an_unchanged_document_not_rebuilt_from_its_texts_names_the_fallback() {
    let t = tempfile::tempdir().unwrap();
    let doc = "{\n  \"sessionId\": \"s2\",\n  \"startTime\": \"2025-10-01T16:00:00.000Z\",\n  \"chatMessages\": [\n    {\n      \"role\": \"user\",\n      \"content\": \"hi\",\n      \"x\": 0.000005\n    }\n  ],\n  \"timeline\": []\n}";
    let p = t.path().join("session_s2_1759334400000.json");
    std::fs::write(&p, doc).unwrap();
    let set = CopilotCli.import(&p, &ImportOptions::standard()).unwrap().set;
    let has = |ex: &crate::formats::Exported| ex.losses.0.iter().any(|l| l.code == crate::formats::atif::jsonfmt::EXACT_TEXT_FALLBACK);
    // Opposed: as imported, it is rebuilt from its texts and names nothing.
    let ex = CopilotCli.export(&set, &ExportOptions::standard()).unwrap();
    assert_eq!(exported(&set), doc);
    assert!(!has(&ex), "{:?}", ex.losses);
    let mut nudged = set.clone();
    let id = nudged.iter().next().unwrap().id().to_string();
    let n = nudged
        .ledgers
        .get_mut(&id)
        .unwrap()
        .events
        .iter_mut()
        .filter_map(|e| e.native.as_mut())
        .find(|n| n.part == 0 && n.record_text().is_some_and(|t| t.contains("0.000005")))
        .expect("the record to nudge");
    n.raw = Some(n.record_text().unwrap().replace("0.000005", "5.000000000000001e-06"));
    assert_eq!(crate::util::current_raw(n), n.raw.as_deref(), "sentinel: the nudged raw still stands for the record");
    let ex = CopilotCli.export(&nudged, &ExportOptions::standard()).unwrap();
    assert!(has(&ex), "the fallback is named: {:?}", ex.losses);
}

/// SPEC §3.1.1: a session's running totals import with `input` excluding the
/// cache reads and writes Copilot counts in `inputTokens`.
#[test]
fn shutdown_totals_count_input_without_the_cache() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/copilot-cli/dir-real-1.0.68-toolpath/native");
    let set = CopilotCli.import(std::path::Path::new(dir), &ImportOptions::standard()).unwrap().set;
    let totals: Vec<(Value, Value, Value)> = set
        .iter()
        .flat_map(|l| l.events.iter())
        .filter(|e| e.kind == crate::model::Kind::Usage && e.body.get("cumulative") == Some(&Value::Bool(true)))
        .map(|e| (e.body["input"].clone(), e.body["cache_read"].clone(), e.body["cache_write"].clone()))
        .collect();
    // inputTokens 221936 = 54 uncached + 198884 read + 22998 written.
    assert_eq!(totals, [(serde_json::json!(54), serde_json::json!(198884), serde_json::json!(22998))]);
}

/// The `raw` rule for a legacy document kept whole (no layout reproduces it): its `raw`
/// is written back only while its records still say what it says.
#[test]
fn a_legacy_document_kept_whole_is_written_back_only_while_its_records_say_it() {
    let t = tempfile::tempdir().unwrap();
    let doc = "{\n    \"sessionId\": \"s3\",\n    \"startTime\": \"2025-10-01T16:00:00.000Z\",\n    \"chatMessages\": [\n        {\n            \"role\": \"user\",\n            \"content\": \"hello there\"\n        }\n    ],\n    \"timeline\": []\n}";
    let p = t.path().join("session_s3_1759334400000.json");
    std::fs::write(&p, doc).unwrap();
    let set = CopilotCli.import(&p, &ImportOptions::standard()).unwrap().set;
    assert!(set.iter().next().unwrap().agent().source.as_ref().unwrap().extra.get("raw_file").is_some(), "sentinel: kept whole");
    // Opposed: unchanged, the file as read.
    assert_eq!(exported(&set), doc);
    let mut edited = set.clone();
    let id = edited.iter().next().unwrap().id().to_string();
    let n = edited
        .ledgers
        .get_mut(&id)
        .unwrap()
        .events
        .iter_mut()
        .filter_map(|e| e.native.as_mut())
        .find(|n| n.part == 0 && n.record_text().is_some_and(|t| t.contains("hello there")))
        .expect("the record");
    let r = n.record_text().unwrap().replace("hello there", "EDITED");
    *n = n.clone().record_raw(&r).unwrap();
    let got = exported(&edited);
    assert!(got.contains("EDITED") && !got.contains("hello there"), "the edit is written: {got}");
}

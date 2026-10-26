//! The conformance fixtures and the index, through a store in every mode
//! (SPEC §10): plain, compressed with no dictionary, with a passed
//! dictionary, and after a recompression with a trained one. For every
//! fixture case, in each mode:
//!
//! - **conformance through the store**: the case imported into the store
//!   reads back, ledger by ledger, byte for byte the plain form of what its
//!   importer produced (the `P` every conformance trip starts from), and the
//!   case's own format exports from the ledgers read through the store
//!   exactly what it exports from that `P`, so the self trip's verdict is
//!   the conformance suite's. Every cross trip is a function of the same
//!   `P`, so it is unchanged too;
//! - every event reads back at its indexed offset;
//! - **reindex**: the index rebuilt from the files alone (index.db deleted)
//!   answers every query as before, its block rows included, and the block
//!   rows name the dictionary every sealed block's meta frame names.
//!
//! Each mode's cell checks, from the files, that its stores were in that
//! mode: compressed, with sealed blocks, each naming the mode's dictionary.
#![cfg(feature = "store")]

#[macro_use]
#[path = "modes/mod.rs"]
mod modes;

use modes::{Found, Mode};
use protoledger::formats::{self, ExportOptions, ImportOptions};
use protoledger::store::Store;
use std::path::{Path, PathBuf};

/// Every `fixtures/<format>/<case>/native` (or its one file), with its format.
fn fixture_cases() -> Vec<(String, PathBuf)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    let mut out = Vec::new();
    for f in std::fs::read_dir(&root).unwrap() {
        let f = f.unwrap().path();
        if !f.is_dir() {
            continue;
        }
        for c in std::fs::read_dir(&f).unwrap() {
            let native = c.unwrap().path().join("native");
            if native.is_dir() {
                out.push((f.file_name().unwrap().to_string_lossy().into_owned(), native));
            }
        }
    }
    out.sort();
    out
}

/// The input the conformance suite hands an importer: the one file of a
/// `native/` holding one, else the directory.
fn case_input(native: &Path) -> PathBuf {
    let entries: Vec<_> = std::fs::read_dir(native).unwrap().filter_map(|e| e.ok()).filter(|e| !e.file_name().to_string_lossy().starts_with('.')).collect();
    if entries.len() == 1 && entries[0].path().is_file() { entries[0].path() } else { native.to_path_buf() }
}

/// Everything the store's index answers: agents, events, blocks, the
/// dictionaries and their use, bindings.
fn answers(s: &Store) -> String {
    let mut o = String::new();
    for a in s.agents().unwrap() {
        o += &format!("{a:?}\n{:?}\n{:?}\n", s.events(&a.id, 0, usize::MAX).unwrap(), s.blocks(&a.id).unwrap());
    }
    o += &format!("{:?}\n{:?}\n{:?}\n", s.dictionaries().unwrap(), s.dictionary_usage().unwrap(), s.bindings().unwrap());
    o
}

/// Where the fixture stores seal: a few lines, so that most fixture ledgers
/// are several blocks and a hot tail, without a seal on every append.
const FIXTURE_SEAL_BYTES: u64 = 1024;

/// The cell: every fixture case through a store in `mode`.
fn fixtures_through_the_store(mode: Mode) {
    let dir = tempfile::tempdir().unwrap();
    let cases = fixture_cases();
    assert!(cases.len() > 150, "found {} fixture cases", cases.len());
    let (mut ledgers, mut events, mut exports, mut skipped, mut refused) = (0usize, 0u64, 0usize, 0usize, 0usize);
    let mut found = Found::default();
    for (i, (format, native)) in cases.iter().enumerate() {
        let f = formats::by_name(format).unwrap();
        let input = case_input(native);
        let what = format!("{mode:?}: {}", native.display());
        // P: what the importer produces, as the conformance suite has it.
        let direct = f.import(&input, &ImportOptions::standard());
        let root = dir.path().join(format!("s{i}"));
        mode.prepare(&root);
        let mut s = Store::open(&root).unwrap();
        if mode.compressed() {
            s.set_seal_bytes(FIXTURE_SEAL_BYTES).unwrap();
        }
        let imported = s.import(format, &input);
        let direct = match direct {
            Ok(d) => d,
            Err(e) => {
                // A case its importer refuses (a build without a feature it
                // needs) is refused through the store too.
                assert!(imported.is_err(), "{what}: imports through the store but not directly ({e:#})");
                skipped += 1;
                continue;
            }
        };
        if let Err(e) = imported {
            // A set a store cannot hold (a child whose parent the set does
            // not have, SPEC §2.1) is refused by a plain store just the same.
            let plain = Store::open(dir.path().join(format!("p{i}"))).unwrap().import(format, &input);
            let pe = plain.err().unwrap_or_else(|| panic!("{what}: a plain store imports it, this one refuses: {e:#}"));
            assert_eq!(format!("{pe:#}"), format!("{e:#}"), "{what}");
            refused += 1;
            continue;
        }
        mode.retrain(&mut s);
        drop(s);
        // A fresh store: every read goes through the files the writers left.
        let s = Store::open(&root).unwrap();
        assert!(s.damaged().is_empty(), "{what}: {:?}", s.damaged());
        let mut ids: Vec<&str> = direct.set.iter().map(|l| l.id()).collect();
        ids.sort();
        assert_eq!(s.agents().unwrap().iter().map(|a| a.id.as_str()).collect::<Vec<_>>(), ids, "{what}");
        for l in direct.set.iter() {
            // Byte for byte the plain form of P's ledger.
            let got = s.reader(l.id()).unwrap().stream().unwrap();
            assert!(got == l.to_jsonl().as_bytes(), "{what}: {}: {} bytes through the store, {} in P", l.id(), got.len(), l.to_jsonl().len());
            for e in &l.events {
                assert_eq!(s.event_at(l.id(), e.seq).unwrap().as_ref(), Some(e), "{what}: {} seq {}", l.id(), e.seq);
            }
            ledgers += 1;
            events += l.events.len() as u64;
        }
        // The self trip from the ledgers read through the store exports
        // exactly what it exports from P.
        let through = modes::read_set(&root);
        assert_eq!(through.iter().count(), direct.set.iter().count(), "{what}");
        match (f.export(&direct.set, &ExportOptions::standard()), f.export(&through, &ExportOptions::standard())) {
            (Ok(a), Ok(b)) => {
                assert!(a.output.files == b.output.files, "{what}: the export from the store differs from the export from P");
                exports += 1;
            }
            (Err(_), Err(_)) => {}
            (a, b) => panic!("{what}: export from P ok={}, from the store ok={}", a.is_ok(), b.is_ok()),
        }
        // The index names the dictionary of every sealed block, as its meta frame does.
        for a in s.agents().unwrap() {
            let rows = s.blocks(&a.id).unwrap();
            let zst = root.join(format!("{}.zst", a.path));
            let metas = if zst.exists() { modes::blocks_of(&std::fs::read(&zst).unwrap()) } else { Vec::new() };
            assert_eq!(rows.len(), metas.len(), "{what}: {}: block rows", a.id);
            for (r, m) in rows.iter().zip(&metas) {
                assert!(
                    r.dict == m.dict && r.dict == mode.dictionary(),
                    "{what}: {}: row {:?}, meta frame {:?}, mode {:?}",
                    a.id,
                    r.dict,
                    m.dict,
                    mode.dictionary()
                );
                assert_eq!((r.first_seq, r.last_seq, r.generation), (m.first_seq, m.last_seq, m.generation), "{what}: {}", a.id);
            }
        }
        // Reindex: the index rebuilt from the files alone answers the same.
        let before = answers(&s);
        drop(s);
        for x in ["index.db", "index.db-wal", "index.db-shm"] {
            let _ = std::fs::remove_file(root.join(x));
        }
        let mut s = Store::open(&root).unwrap();
        s.reindex().unwrap();
        assert!(answers(&s) == before, "{what}: the rebuilt index answers differently");
        drop(s);
        found.add(&modes::found(&root));
    }
    eprintln!(
        "{mode:?}: {} cases ({skipped} not importable, {refused} refused by every store), {ledgers} ledgers, {events} events, {exports} exports compared; {found:?}",
        cases.len()
    );
    assert!(ledgers > 250 && exports > 150, "{ledgers} ledgers, {exports} exports");
    assert!(refused < 10, "{refused} cases refused by the store");
    mode.check_found(&found, &format!("{} fixture stores", cases.len() - skipped - refused));
}

#[test]
fn fixtures_through_the_store_plain() {
    fixtures_through_the_store(Mode::Plain);
}

modes_tests!(fixtures_through_the_store);

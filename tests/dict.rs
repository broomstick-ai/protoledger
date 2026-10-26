//! Compression dictionaries in a store (SPEC §10.7–§10.9): passed, trained,
//! the default for new blocks, ledgers recompressed with one (checked byte
//! for byte before the rename), the index of which dictionary encoded each
//! block, and deletion only when nothing uses one. Every ledger reads back
//! byte for byte whatever its blocks were compressed with.
#![cfg(feature = "store")]

use protoledger::store::testing::{self, Point};
use protoledger::store::{BlockRow, DictChoice, Store};
use protoledger::{Agent, Event, Kind};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn agent(id: &str) -> Agent {
    Agent { id: id.into(), created_at: None, source: None, parent: None, links: vec![], meta: Default::default() }
}

/// An event whose line is the same length for every `n` in 10..=99.
fn fixed(n: u64) -> Event {
    Event::new(format!("e{n:02}"), Kind::Text).with("role", "user").with("text", "the same width every time")
}

struct Paths {
    plain: PathBuf,
    zst: PathBuf,
    tail: PathBuf,
}

fn paths(root: &Path, name: &str) -> Paths {
    let d = root.join("ledgers");
    Paths { plain: d.join(format!("{name}.jsonl")), zst: d.join(format!("{name}.jsonl.zst")), tail: d.join(format!("{name}.jsonl.tail")) }
}

fn len_of(p: &Path) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

fn sha(b: &[u8]) -> String {
    hex::encode(Sha256::digest(b))
}

fn dict_path(root: &Path, id: &str) -> PathBuf {
    root.join("dicts").join(format!("{id}.zdict"))
}

/// A dictionary a user passes: raw content zstd takes as one.
fn passed_dict() -> Vec<u8> {
    let mut d = Vec::new();
    for n in 10..40 {
        d.extend_from_slice(serde_json::to_string(&fixed(n).with("seq", n)).unwrap().as_bytes());
        d.push(b'\n');
    }
    d
}

/// A store whose new ledgers are compressed, sealing at `seal_bytes`.
fn compressed(root: &Path, seal_bytes: u64) -> Store {
    let mut s = Store::open(root).unwrap();
    s.set_compression(true).unwrap();
    s.set_seal_bytes(seal_bytes).unwrap();
    s
}

fn cat(s: &Store, agent: &str) -> Vec<u8> {
    s.reader(agent).unwrap().stream().unwrap()
}

fn set_recompress_bytes(root: &Path, n: u64) {
    let mut c = protoledger::store::Config::read(root).unwrap();
    c.compression.recompress_bytes = n;
    c.write(root).unwrap();
}

fn poke_index(root: &Path, sql: &str) {
    let db = rusqlite::Connection::open(root.join("index.db")).unwrap();
    db.execute_batch(sql).unwrap();
}

/// Meta payloads of a block file, parsed by this test (SPEC §10.3), with
/// each block's zstd frame.
fn parse_blocks(zst: &[u8]) -> Vec<(serde_json::Value, Vec<u8>)> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at < zst.len() {
        assert_eq!(&zst[at..at + 4], &[0x5C, 0x2A, 0x4D, 0x18], "a meta frame's magic at {at}");
        let plen = u32::from_le_bytes(zst[at + 4..at + 8].try_into().unwrap()) as usize;
        let meta: serde_json::Value = serde_json::from_slice(&zst[at + 8..at + 8 + plen]).unwrap();
        let zlen = meta["zlen"].as_u64().unwrap() as usize;
        out.push((meta, zst[at + 8 + plen..at + 8 + plen + zlen].to_vec()));
        at += 8 + plen + zlen;
    }
    assert_eq!(at, zst.len());
    out
}

/// The decompressed stream of a block file, decoded by this test with the
/// dictionaries in `dicts` (by id).
fn decode_file(zst: &[u8], dicts: &BTreeMap<String, Vec<u8>>) -> Vec<u8> {
    let mut out = Vec::new();
    for (meta, frame) in parse_blocks(zst) {
        let len = meta["len"].as_u64().unwrap() as usize;
        let bytes = match meta["dict"].as_str() {
            Some(id) => zstd::bulk::Decompressor::with_dictionary(&dicts[id]).unwrap().decompress(&frame, len).unwrap(),
            None => zstd::bulk::decompress(&frame, len).unwrap(),
        };
        out.extend_from_slice(&bytes);
    }
    out
}

/// A compressed ledger `a` with three sealed blocks (no dictionary) and a hot
/// tail of five lines. Returns the stream; the store is dropped.
fn three_blocks(root: &Path) -> Vec<u8> {
    let mut s = compressed(root, u64::MAX);
    s.create_agent(agent("a")).unwrap();
    for n in 10..=35 {
        if [10, 20, 30].contains(&n) {
            s.set_seal_bytes(1).unwrap();
        }
        s.append("a", fixed(n)).unwrap();
        s.set_seal_bytes(u64::MAX).unwrap();
    }
    let p = paths(root, "a");
    assert_eq!(parse_blocks(&std::fs::read(&p.zst).unwrap()).len(), 3);
    assert!(len_of(&p.tail) > 0);
    cat(&s, "a")
}

/// Every `fixtures/<format>/<case>/native`, with its format's name.
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

/// One plain store at `root` holding every fixture (a case whose agent ids
/// collide with an earlier case's is left out).
fn store_of_every_fixture(root: &Path, scratch: &Path) -> Store {
    let mut s = Store::open(root).unwrap();
    for (format, native) in fixture_cases() {
        let probe_root = scratch.join("probe");
        let _ = std::fs::remove_dir_all(&probe_root);
        let mut probe = Store::open(&probe_root).unwrap();
        if probe.import(&format, &native).is_err() {
            continue;
        }
        let ids: Vec<String> = probe.agents().unwrap().into_iter().map(|a| a.id).collect();
        if ids.iter().any(|id| s.agent(id).unwrap().is_some()) {
            continue;
        }
        s.import(&format, &native).unwrap();
    }
    s
}

/// Every block row and every dictionary row the index holds, and which
/// dictionaries each ledger uses.
type IndexDump = (Vec<BlockRow>, Vec<(String, u64, bool, Vec<(String, u64)>)>, Vec<(Option<String>, String, u64)>);

fn dump(s: &Store) -> IndexDump {
    let mut rows = Vec::new();
    for a in s.agents().unwrap() {
        rows.extend(s.blocks(&a.id).unwrap());
    }
    // The dictionary rows as index.db holds them, read without bringing the
    // index up to date first (`Store::dictionaries` would, rewriting them
    // from dicts/ before reading them).
    let db = rusqlite::Connection::open_with_flags(s.root().join("index.db"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let mut st = db.prepare("SELECT id, size FROM dicts ORDER BY id").unwrap();
    let table: Vec<(String, u64)> = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64))).unwrap().map(|r| r.unwrap()).collect();
    let usage = s.dictionary_usage().unwrap();
    let default = s.config().compression.dictionary.clone();
    let dicts = table
        .into_iter()
        .map(|(id, size)| {
            let ledgers = usage.iter().filter(|(d, _, _)| d.as_deref() == Some(id.as_str())).map(|(_, a, n)| (a.clone(), *n)).collect();
            let is_default = default.as_deref() == Some(id.as_str());
            (id, size, is_default, ledgers)
        })
        .collect();
    (rows, dicts, usage)
}

// ---- the core property ----

/// Train → recompress → `cat` and `event_at`, for every fixture, with no
/// dictionary, a trained one, a passed one, a retrained one and none again;
/// the index names each block's dictionary, and a rebuilt index (index.db
/// deleted) reproduces every row.
#[test]
fn every_fixture_reads_back_byte_for_byte_through_every_dictionary_and_reindex_reproduces_the_rows() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("s");
    let mut s = store_of_every_fixture(&root, dir.path());
    let mut originals: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    for a in s.agents().unwrap() {
        originals.insert(a.id.clone(), std::fs::read(root.join(&a.path)).unwrap());
    }
    assert!(originals.len() > 200, "{} ledgers", originals.len());
    // Small blocks, so most ledgers have several.
    set_recompress_bytes(&root, 4096);
    drop(s);
    s = Store::open(&root).unwrap();
    let on = s.set_compression(true).unwrap();
    assert_eq!((on.migrated.len(), on.failed.len()), (originals.len(), 0), "{:?}", on.failed);
    let check = |s: &Store, dict: Option<&str>, what: &str| {
        let mut blocks = 0;
        for (id, want) in &originals {
            let got = cat(s, id);
            assert!(got == *want, "{what}: {id}: cat gives {} bytes, the plain ledger holds {}", got.len(), want.len());
            for row in s.events(id, 1, usize::MAX).unwrap() {
                let e = s.event_at(id, row.seq).unwrap().unwrap();
                assert_eq!((e.seq, &e.id), (row.seq, &row.id), "{what}: {id}");
                let line = &want[row.offset as usize..(row.offset + row.len) as usize];
                assert_eq!(e, serde_json::from_slice::<Event>(line).unwrap(), "{what}: {id} seq {}", row.seq);
            }
            for r in s.blocks(id).unwrap() {
                assert_eq!(r.dict.as_deref(), dict, "{what}: {id}: the block at {} names its dictionary", r.offset);
                blocks += 1;
            }
        }
        assert!(blocks > originals.len(), "{what}: {blocks} blocks");
        blocks
    };
    check(&s, None, "no dictionary");
    // Trained from the store's own blocks.
    let trained = Store::train_dictionary(&root, protoledger::store::DEFAULT_TRAIN_SIZE).unwrap();
    assert_eq!(sha(&std::fs::read(dict_path(&root, &trained)).unwrap()), trained);
    let r = s.recompress(DictChoice::Id(trained.clone()), &[]).unwrap();
    assert_eq!((r.recompressed.len(), r.skipped.len(), r.failed.len()), (originals.len(), 0, 0), "{:?}", r.failed);
    let blocks = check(&s, Some(&trained), "trained");
    let ls = s.dictionaries().unwrap();
    assert_eq!(ls.len(), 1);
    assert_eq!((ls[0].blocks(), ls[0].ledgers.len()), (blocks as u64, originals.len()));
    // Passed.
    let passed = Store::add_dictionary(&root, &passed_dict()).unwrap();
    let r = s.recompress(DictChoice::Id(passed.clone()), &[]).unwrap();
    assert_eq!(r.recompressed.len(), originals.len(), "{:?}", r.failed);
    check(&s, Some(&passed), "passed");
    // Retrained: a dictionary of another size is another dictionary.
    let retrained = Store::train_dictionary(&root, 32 << 10).unwrap();
    assert!(retrained != trained && retrained != passed);
    s.set_dictionary(Some(&retrained)).unwrap();
    let r = s.recompress(DictChoice::Default, &[]).unwrap();
    assert_eq!(r.recompressed.len(), originals.len(), "{:?}", r.failed);
    check(&s, Some(&retrained), "retrained (the default)");
    // The rows, then the index thrown away: deleted, and rebuilt by reindex.
    let before = dump(&s);
    assert_eq!(before.1.len(), 3);
    assert!(before.1.iter().any(|d| d.0 == retrained && d.2 && !d.3.is_empty()));
    assert!(before.1.iter().any(|d| d.0 == trained && !d.2 && d.3.is_empty()));
    drop(s);
    std::fs::remove_file(root.join("index.db")).unwrap();
    let mut s = Store::open(&root).unwrap();
    assert!(dump(&s) == before, "a deleted index rebuilt from the files reproduces every block and dictionary row");
    s.reindex().unwrap();
    assert!(dump(&s) == before, "store reindex reproduces every block and dictionary row");
    check(&s, Some(&retrained), "after reindex");
    // And back to no dictionary.
    let r = s.recompress(DictChoice::None, &[]).unwrap();
    assert_eq!(r.recompressed.len(), originals.len());
    check(&s, None, "none again");
}

/// Plain bytes over compressed bytes of the fixture corpus, recompressed with
/// no dictionary, a passed dictionary and one trained from the store, at two
/// block sizes. Every physical byte of the block files counts; the
/// dictionary's own size is shown apart. Printed for the design notes;
/// asserted only to read back exactly.
#[test]
fn fixture_corpus_ratio_with_and_without_a_dictionary() {
    let dir = tempfile::tempdir().unwrap();
    let plain_root = dir.path().join("plain");
    let plain = store_of_every_fixture(&plain_root, dir.path());
    let agents = plain.agents().unwrap();
    let plain_bytes: u64 = agents.iter().map(|a| a.size).sum();
    let mut table = vec![format!("fixtures: {} ledgers, {plain_bytes} plain bytes", agents.len())];
    for (i, size) in [16u64 << 10, 1 << 20].into_iter().enumerate() {
        let root = dir.path().join(format!("c{i}"));
        std::fs::create_dir_all(root.join("ledgers")).unwrap();
        for a in &agents {
            std::fs::copy(plain_root.join(&a.path), root.join(&a.path)).unwrap();
        }
        set_recompress_bytes(&root, size);
        let mut s = Store::open(&root).unwrap();
        assert_eq!(s.set_compression(true).unwrap().migrated.len(), agents.len());
        let passed = Store::add_dictionary(&root, &passed_dict()).unwrap();
        let trained = Store::train_dictionary(&root, protoledger::store::DEFAULT_TRAIN_SIZE).unwrap();
        for (name, choice, id) in [
            ("none", DictChoice::None, None),
            ("passed", DictChoice::Id(passed.clone()), Some(&passed)),
            ("trained", DictChoice::Id(trained.clone()), Some(&trained)),
        ] {
            let r = s.recompress(choice, &[]).unwrap();
            assert_eq!(r.recompressed.len(), agents.len(), "{:?}", r.failed);
            let zst: u64 = agents.iter().map(|a| len_of(&root.join(format!("{}.zst", a.path)))).sum();
            let tails: u64 = agents.iter().map(|a| len_of(&root.join(format!("{}.tail", a.path)))).sum();
            assert_eq!(tails, 0);
            for a in &agents {
                assert!(cat(&s, &a.id) == std::fs::read(plain_root.join(&a.path)).unwrap(), "{name}: {}", a.id);
            }
            let dsize = id.map(|id| len_of(&dict_path(&root, id))).unwrap_or(0);
            let nblocks: usize = agents.iter().map(|a| s.blocks(&a.id).unwrap().len()).sum();
            table.push(format!(
                "block {:>4} KiB, {name:>7}: {zst} bytes in {nblocks} blocks, ratio {:.2}; with the dictionary ({dsize} bytes) counted, ratio {:.2}",
                size >> 10,
                plain_bytes as f64 / zst as f64,
                plain_bytes as f64 / (zst + dsize) as f64
            ));
        }
    }
    eprintln!("{}", table.join("\n"));
}

// ---- recompression's check and readers during it ----

/// Rewrite the temporary file of a recompression as whole blocks of a stream
/// that differs from the ledger's in one byte: every block is whole, so
/// only the byte-for-byte comparison can catch it.
fn corrupt_whole(tmp: &Path) {
    let blocks = parse_blocks(&std::fs::read(tmp).unwrap());
    let mut stream = decode_file(&std::fs::read(tmp).unwrap(), &BTreeMap::new());
    let at = stream.windows(4).rposition(|w| w == b"same").unwrap();
    stream[at] = b'S';
    let mut out = Vec::new();
    let mut offset = 0usize;
    for (meta, _) in blocks {
        let len = meta["len"].as_u64().unwrap() as usize;
        let data = &stream[offset..offset + len];
        let mut c = zstd::bulk::Compressor::new(3).unwrap();
        c.set_parameter(zstd::zstd_safe::CParameter::ChecksumFlag(true)).unwrap();
        let frame = c.compress(data).unwrap();
        let mut m = meta.clone();
        m["zlen"] = serde_json::json!(frame.len());
        let payload = serde_json::to_vec(&m).unwrap();
        out.extend_from_slice(&0x184D_2A5Cu32.to_le_bytes());
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&payload);
        out.extend_from_slice(&frame);
        offset += len;
    }
    std::fs::write(tmp, out).unwrap();
}

#[test]
fn a_recompression_whose_copy_does_not_read_back_identical_leaves_the_ledger_as_it_was() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let stream = three_blocks(root);
    let p = paths(root, "a");
    let (zst0, tail0) = (std::fs::read(&p.zst).unwrap(), std::fs::read(&p.tail).unwrap());
    let mut s = Store::open(root).unwrap();
    testing::set(Point::RecompressWritten, |tmp| {
        corrupt_whole(tmp);
        Ok(())
    });
    let r = s.recompress(DictChoice::None, &[]).unwrap();
    testing::clear();
    assert_eq!(r.recompressed, Vec::<String>::new(), "a copy that differs is never renamed into place");
    assert_eq!(r.failed.len(), 1);
    assert!(r.failed[0].1.contains("did not read back byte-identical"), "{}", r.failed[0].1);
    assert!(std::fs::read(&p.zst).unwrap() == zst0 && std::fs::read(&p.tail).unwrap() == tail0, "the ledger was left as it was");
    assert!(!std::fs::read_dir(root.join("ledgers")).unwrap().any(|e| e.unwrap().file_name().to_string_lossy().contains(".tmp-")));
    assert!(cat(&s, "a") == stream);
    // Opposed: the same recompression, nothing corrupted, is done.
    let r = s.recompress(DictChoice::None, &[]).unwrap();
    assert_eq!(r.recompressed, vec!["a".to_string()], "{:?}", r.failed);
    assert!(cat(&s, "a") == stream);
    assert_eq!(len_of(&p.tail), 0);
}

#[test]
fn a_reader_during_a_recompression_gets_one_consistent_stream_never_short() {
    for point in [Point::ReaderBeforeTail, Point::ReaderAfterTail] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let stream = three_blocks(&root);
        let s = Store::open(&root).unwrap();
        let reader = s.reader("a").unwrap();
        drop(s);
        let r2 = root.clone();
        let mut fired = false;
        testing::set(point, move |_| {
            if !fired {
                fired = true;
                let mut other = Store::open(&r2).unwrap();
                let r = other.recompress(DictChoice::None, &[]).unwrap();
                assert_eq!(r.recompressed, vec!["a".to_string()], "{:?} {:?}", r.skipped, r.failed);
            }
            Ok(())
        });
        let got = reader.stream().unwrap();
        testing::clear();
        assert!(got == stream, "{point:?}: the read gives {} bytes, the stream is {}", got.len(), stream.len());
        assert_eq!(len_of(&paths(&root, "a").tail), 0, "{point:?}: the recompression emptied the hot tail under the read");
    }
}

/// The index as it stood between a recompression's rename and its update
/// (a crash before step 5): rows of the previous generation, whose physical
/// positions land on real meta frames of the new file that describe other
/// bytes. Only checking each meta frame against its row catches them.
#[test]
fn a_stale_index_position_after_a_recompression_is_caught_by_the_meta_frame() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    three_blocks(root);
    set_recompress_bytes(root, 300);
    let mut s = Store::open(root).unwrap();
    assert_eq!(s.recompress(DictChoice::None, &[]).unwrap().recompressed.len(), 1);
    let old: Vec<BlockRow> = s.blocks("a").unwrap();
    for n in 36..=45 {
        s.append("a", fixed(n)).unwrap();
    }
    assert_eq!(s.recompress(DictChoice::None, &[]).unwrap().recompressed.len(), 1);
    let new = s.blocks("a").unwrap();
    let stream = cat(&s, "a");
    // Sentinel: the old rows are stale, and yet every one of them names the
    // position of a real meta frame in the new file.
    assert!(old != new[..old.len()], "the rows differ");
    let last = old.last().unwrap();
    let at_last = new.iter().find(|b| b.physical_offset == last.physical_offset).expect("the old last row lands on a new meta frame");
    assert!(at_last.generation == last.generation + 1 && at_last.last_seq > last.last_seq, "{at_last:?} {last:?}");
    for (o, n) in old.iter().zip(&new) {
        assert_eq!((o.offset, o.physical_offset, o.generation + 1), (n.offset, n.physical_offset, n.generation));
    }
    let mut sql = String::from("DELETE FROM blocks WHERE agent = 'a';");
    for b in &old {
        sql.push_str(&format!(
            "INSERT INTO blocks VALUES ('a', {}, {}, {}, {}, NULL, {}, {}, {});",
            b.offset, b.len, b.physical_offset, b.physical_len, b.first_seq, b.last_seq, b.generation
        ));
    }
    poke_index(root, &sql);
    assert_eq!(s.blocks("a").unwrap(), old);
    let want: Vec<String> = String::from_utf8(stream.clone()).unwrap().lines().map(str::to_string).collect();
    for row in s.events("a", 1, usize::MAX).unwrap() {
        let e = s.event_at("a", row.seq).unwrap().unwrap();
        assert_eq!(serde_json::to_string(&e).unwrap(), want[row.seq as usize], "seq {}", row.seq);
    }
    assert_eq!(want.len(), 37);
    assert!(cat(&s, "a") == stream);
}

#[test]
fn a_crash_mid_recompression_leaves_a_temporary_file_readers_and_reindex_ignore() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let stream = three_blocks(root);
    let p = paths(root, "a");
    let zst0 = std::fs::read(&p.zst).unwrap();
    let mut s = Store::open(root).unwrap();
    testing::set(Point::RecompressWritten, |_| anyhow::bail!("crash"));
    let r = s.recompress(DictChoice::None, &[]).unwrap();
    testing::clear();
    assert_eq!(r.failed.len(), 1);
    let tmps: Vec<String> = std::fs::read_dir(root.join("ledgers"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".tmp-"))
        .collect();
    assert_eq!(tmps, vec![format!("a.jsonl.zst.tmp-{}", std::process::id())]);
    assert!(std::fs::read(&p.zst).unwrap() == zst0);
    assert!(cat(&s, "a") == stream);
    let r = s.reindex().unwrap();
    assert_eq!((r.agents, r.events), (1, 26));
    assert_eq!(s.agents().unwrap().iter().map(|a| a.id.as_str()).collect::<Vec<_>>(), vec!["a"]);
    drop(s);
    std::fs::remove_file(root.join("index.db")).unwrap();
    let mut s = Store::open(root).unwrap();
    assert_eq!(s.agents().unwrap().len(), 1);
    assert!(cat(&s, "a") == stream);
    // The lock holder deletes it on open.
    s.append("a", fixed(36)).unwrap();
    assert!(!std::fs::read_dir(root.join("ledgers")).unwrap().any(|e| e.unwrap().file_name().to_string_lossy().contains(".tmp-")));
}

#[test]
fn a_leftover_tail_after_a_recompression_crash_equals_the_stream_end_and_is_cut() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let stream = three_blocks(root);
    let p = paths(root, "a");
    let tail = std::fs::read(&p.tail).unwrap();
    let mut s = Store::open(root).unwrap();
    testing::set(Point::RecompressRenamed, |_| anyhow::bail!("crash"));
    let r = s.recompress(DictChoice::None, &[]).unwrap();
    testing::clear();
    assert_eq!(r.failed.len(), 1);
    assert!(std::fs::read(&p.tail).unwrap() == tail, "the crash left the hot tail");
    let blocks = decode_file(&std::fs::read(&p.zst).unwrap(), &BTreeMap::new());
    assert!(blocks.ends_with(&tail), "the leftover tail is the last bytes of the new block file's stream");
    assert!(blocks == stream, "the new block file holds the whole stream");
    assert!(cat(&s, "a") == stream, "a reader skips the leftover tail");
    s.append("a", fixed(36)).unwrap();
    let t = std::fs::read_to_string(&p.tail).unwrap();
    assert!(t.lines().count() == 1 && t.contains("\"e36\""), "the writer cut the leftover tail, then appended: {t}");
    assert!(cat(&s, "a").starts_with(&stream));
}

// ---- deletion ----

#[test]
fn dict_rm_is_refused_while_a_dictionary_is_the_default_or_referenced_and_accepted_when_neither() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let stream = three_blocks(root);
    let d1 = Store::add_dictionary(root, &passed_dict()).unwrap();
    let d2 = Store::add_dictionary(root, &passed_dict().repeat(2)).unwrap();
    let mut s = Store::open(root).unwrap();
    s.set_dictionary(Some(&d1)).unwrap();
    assert_eq!(s.recompress(DictChoice::Id(d2.clone()), &[]).unwrap().recompressed.len(), 1);
    let e = Store::remove_dictionary(root, &d1).unwrap_err().to_string();
    assert!(e.contains("default"), "{e}");
    let e = Store::remove_dictionary(root, &d2).unwrap_err().to_string();
    assert!(e.contains("a.jsonl (3 blocks)") || e.contains("a.jsonl ("), "{e}");
    assert!(dict_path(root, &d1).exists() && dict_path(root, &d2).exists());
    // Neither the default nor referenced: deleted.
    s.set_dictionary(None).unwrap();
    Store::remove_dictionary(root, &d1).unwrap();
    assert!(!dict_path(root, &d1).exists());
    assert_eq!(s.recompress(DictChoice::None, &[]).unwrap().recompressed.len(), 1);
    Store::remove_dictionary(root, &d2).unwrap();
    assert!(!dict_path(root, &d2).exists());
    assert!(cat(&s, "a") == stream);
    assert!(s.dictionaries().unwrap().is_empty());
}

#[test]
fn dict_rm_counts_references_from_the_meta_frames_not_the_index() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let stream = three_blocks(root);
    let d = Store::add_dictionary(root, &passed_dict()).unwrap();
    let mut s = Store::open(root).unwrap();
    // A crash between the rename (step 3) and the index update (step 5).
    testing::set(Point::RecompressRenamed, |_| anyhow::bail!("crash"));
    let r = s.recompress(DictChoice::Id(d.clone()), &[]).unwrap();
    testing::clear();
    assert_eq!(r.failed.len(), 1);
    poke_index(root, "UPDATE blocks SET dict = NULL;");
    assert!(s.blocks("a").unwrap().iter().all(|b| b.dict.is_none()), "the index lags the rename");
    let e = Store::remove_dictionary(root, &d).unwrap_err().to_string();
    assert!(e.contains("used by 1 ledgers"), "{e}");
    assert!(dict_path(root, &d).exists());
    assert!(cat(&s, "a") == stream);
}

/// A deletion that lands between a recompression's check and its rename
/// (the walk does not see the temporary file) is undone before the rename,
/// so even a crash right after the rename leaves no block naming a deleted
/// dictionary.
#[test]
fn a_dictionary_deleted_between_the_check_and_the_rename_is_on_disk_before_the_rename_even_across_a_crash() {
    for crash in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let stream = three_blocks(&root);
        let bytes = passed_dict();
        let d = Store::add_dictionary(&root, &bytes).unwrap();
        let mut s = Store::open(&root).unwrap();
        let (r2, d2) = (root.clone(), d.clone());
        testing::set(Point::RecompressChecked, move |_| {
            Store::remove_dictionary(&r2, &d2).expect("nothing durable references it yet, so rm deletes it");
            assert!(!dict_path(&r2, &d2).exists());
            Ok(())
        });
        if crash {
            testing::set(Point::RecompressRenamed, |_| anyhow::bail!("crash"));
        }
        let r = s.recompress(DictChoice::Id(d.clone()), &[]).unwrap();
        testing::clear();
        assert_eq!((r.recompressed.len(), r.failed.len()), (usize::from(!crash), usize::from(crash)), "{:?}", r.failed);
        let p = paths(&root, "a");
        assert!(parse_blocks(&std::fs::read(&p.zst).unwrap()).iter().all(|(m, _)| m["dict"] == d.as_str()), "crash={crash}: the new blocks are in place");
        assert_eq!(std::fs::read(dict_path(&root, &d)).ok().map(|b| sha(&b)), Some(d.clone()), "crash={crash}: the dictionary is back");
        drop(s);
        let mut s = Store::open(&root).unwrap();
        assert!(cat(&s, "a") == stream, "crash={crash}");
        s.append("a", fixed(36)).unwrap();
    }
}

// ---- loading ----

#[test]
fn a_corrupt_dictionary_is_refused_on_load() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let d = Store::add_dictionary(root, &passed_dict()).unwrap();
    let mut s = compressed(root, 400);
    s.set_dictionary(Some(&d)).unwrap();
    s.create_agent(agent("a")).unwrap();
    for n in 10..=20 {
        s.append("a", fixed(n)).unwrap();
    }
    let blocks = s.blocks("a").unwrap();
    assert!(!blocks.is_empty() && blocks.iter().all(|b| b.dict.as_deref() == Some(d.as_str())), "{blocks:?}");
    let stream = cat(&s, "a");
    drop(s);
    // Same length, one byte changed: zstd still takes it as a dictionary.
    let mut bad = passed_dict();
    let at = bad.len() / 2;
    bad[at] ^= 0x20;
    std::fs::write(dict_path(root, &d), &bad).unwrap();
    let mut s = Store::open(root).unwrap();
    let e = format!("{:#}", s.reader("a").unwrap().stream().unwrap_err());
    assert!(e.contains("does not match its id"), "{e}");
    let e = format!("{:#}", s.set_dictionary(Some(&d)).unwrap_err());
    assert!(e.contains("does not match its id"), "{e}");
    // A writer never seals with it: it is refused before anything is written.
    let zst0 = std::fs::read(paths(root, "a").zst).unwrap();
    let e = format!("{:#}", s.append("a", fixed(21)).unwrap_err());
    assert!(e.contains("does not match its id"), "{e}");
    assert!(std::fs::read(paths(root, "a").zst).unwrap() == zst0);
    // Opposed: the right bytes back, and everything works.
    std::fs::write(dict_path(root, &d), passed_dict()).unwrap();
    assert!(cat(&s, "a") == stream);
    s.set_dictionary(Some(&d)).unwrap();
    s.append("a", fixed(21)).unwrap();
}

#[test]
fn dict_add_refuses_an_empty_or_rejected_file_and_takes_raw_content_once() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let e = Store::add_dictionary(root, b"").unwrap_err().to_string();
    assert!(e.contains("empty"), "{e}");
    // The magic number of a zstd dictionary, then nothing zstd can parse.
    let mut bad = vec![0x37, 0xA4, 0x30, 0xEC];
    bad.extend_from_slice(&[0xFF; 300]);
    assert!(zstd::bulk::Compressor::with_dictionary(3, &bad).and_then(|mut c| c.compress(b"x")).is_err(), "zstd itself rejects it");
    let e = Store::add_dictionary(root, &bad).unwrap_err().to_string();
    assert!(e.contains("zstd rejects"), "{e}");
    let count = || {
        std::fs::read_dir(root.join("dicts"))
            .map(|rd| rd.filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().ends_with(".zdict")).count())
            .unwrap_or(0)
    };
    assert_eq!(count(), 0);
    // Opposed: raw content is a dictionary; adding it again changes nothing.
    let id = Store::add_dictionary(root, &passed_dict()).unwrap();
    assert_eq!(id, sha(&passed_dict()));
    let m0 = std::fs::metadata(dict_path(root, &id)).unwrap().modified().unwrap();
    assert_eq!(Store::add_dictionary(root, &passed_dict()).unwrap(), id);
    assert_eq!(std::fs::metadata(dict_path(root, &id)).unwrap().modified().unwrap(), m0);
    assert_eq!(count(), 1);
}

#[test]
fn dict_train_fails_on_too_little_and_adds_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("small");
    let mut s = compressed(&root, 1);
    s.create_agent(agent("a")).unwrap();
    for n in 10..=14 {
        s.append("a", fixed(n)).unwrap();
    }
    drop(s);
    let e = Store::train_dictionary(&root, 4096).unwrap_err().to_string();
    assert!(e.contains("too little to train on"), "{e}");
    assert!(
        !root.join("dicts").exists() || std::fs::read_dir(root.join("dicts")).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().ends_with(".zdict"))
    );
    // Opposed: enough varied lines train one.
    let root = dir.path().join("big");
    let mut s = compressed(&root, 4096);
    s.create_agent(agent("b")).unwrap();
    for n in 0..2000u64 {
        s.append(
            "b",
            Event::new(format!("x{n}"), Kind::Text)
                .with("role", if n % 2 == 0 { "user" } else { "assistant" })
                .with("text", format!("line {n}: {}", n * 7919 % 1013)),
        )
        .unwrap();
    }
    drop(s);
    let id = Store::train_dictionary(&root, 4096).unwrap();
    assert_eq!(sha(&std::fs::read(dict_path(&root, &id)).unwrap()), id);
}

#[test]
fn config_dictionary_refuses_an_id_not_in_the_store_and_none_clears_it() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut s = Store::open(root).unwrap();
    let absent = sha(b"not in this store");
    let e = format!("{:#}", s.set_dictionary(Some(&absent)).unwrap_err());
    assert!(e.contains("missing"), "{e}");
    assert!(s.set_dictionary(Some("not-an-id")).is_err());
    assert_eq!(protoledger::store::Config::read(root).unwrap().compression.dictionary, None);
    let d = Store::add_dictionary(root, &passed_dict()).unwrap();
    s.set_dictionary(Some(&d)).unwrap();
    assert_eq!(protoledger::store::Config::read(root).unwrap().compression.dictionary.as_deref(), Some(d.as_str()));
    s.set_dictionary(None).unwrap();
    assert_eq!(protoledger::store::Config::read(root).unwrap().compression.dictionary, None);
}

/// `store config --dictionary <id>` checks the dictionary and writes the
/// default under the dictionary lock: a `dict rm` of that dictionary staged
/// between the check and the write is refused ("try again"), the default is
/// written naming a dictionary that is still there, and a compressed writer
/// opens and seals with it. Opposed in the same test: once the default is
/// cleared, the same `dict rm` deletes it.
#[test]
fn config_dictionary_holds_the_lock_so_a_dict_rm_between_its_check_and_its_write_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let mut s = compressed(&root, 1);
    s.create_agent(agent("a")).unwrap();
    s.append("a", fixed(10)).unwrap();
    let d = Store::add_dictionary(&root, &passed_dict()).unwrap();
    // Not the default yet, and no block uses it: an rm now would delete it.
    let rm: std::rc::Rc<std::cell::RefCell<Option<String>>> = Default::default();
    let (r, id) = (rm.clone(), d.clone());
    let at = root.clone();
    testing::set(Point::DictDefaulting, move |_| {
        *r.borrow_mut() = Some(match Store::remove_dictionary(&at, &id) {
            Ok(()) => "removed".to_string(),
            Err(e) => format!("{e:#}"),
        });
        Ok(())
    });
    s.set_dictionary(Some(&d)).unwrap();
    testing::clear();
    let rm = rm.borrow().clone().expect("sentinel: the rm was staged between the check and the write");
    assert!(rm.contains("try again"), "a dict rm between the check and the write was not refused: {rm}");
    assert!(dict_path(&root, &d).is_file(), "the default names a dictionary that was deleted");
    assert_eq!(protoledger::store::Config::read(&root).unwrap().compression.dictionary.as_deref(), Some(d.as_str()));
    drop(s);
    // Every compressed writer still opens, and seals with it.
    let mut s = Store::open(&root).unwrap();
    s.append("a", fixed(11)).unwrap();
    assert_eq!(s.blocks("a").unwrap().last().unwrap().dict.as_deref(), Some(d.as_str()));
    // Opposed: with the default cleared and no block using it after a
    // recompression with none, the same rm deletes it.
    s.set_dictionary(None).unwrap();
    s.recompress(DictChoice::None, &[]).unwrap();
    drop(s);
    Store::remove_dictionary(&root, &d).unwrap();
    assert!(!dict_path(&root, &d).exists());
}

// ---- seals and migrations re-check their dictionary ----

/// A deletion another process makes between a seal's compression and its
/// write (it had cleared the default first, so `dict rm` is allowed) is
/// undone before the block is written.
#[test]
fn a_seal_writes_its_dictionary_back_when_it_was_deleted_under_it() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let d = Store::add_dictionary(&root, &passed_dict()).unwrap();
    let mut s = compressed(&root, u64::MAX);
    s.set_dictionary(Some(&d)).unwrap();
    s.create_agent(agent("a")).unwrap();
    for n in 10..=14 {
        s.append("a", fixed(n)).unwrap();
    }
    let (r2, d2) = (root.clone(), d.clone());
    testing::set(Point::DictLocking, move |_| {
        let mut c = protoledger::store::Config::read(&r2).unwrap();
        c.compression.dictionary = None;
        c.write(&r2).unwrap();
        Store::remove_dictionary(&r2, &d2).expect("no block names it and it is not the default");
        Ok(())
    });
    s.set_seal_bytes(1).unwrap();
    s.append("a", fixed(15)).unwrap();
    testing::clear();
    let p = paths(&root, "a");
    let blocks = parse_blocks(&std::fs::read(&p.zst).unwrap());
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].0["dict"], d.as_str(), "the block reads back with the same dictionary id");
    assert_eq!(std::fs::read(dict_path(&root, &d)).ok().map(|b| sha(&b)), Some(d.clone()), "the dictionary was written back");
    let want = cat(&s, "a");
    drop(s);
    let s = Store::open(&root).unwrap();
    assert!(cat(&s, "a") == want);
    assert_eq!(want.iter().filter(|b| **b == b'\n').count(), 7);
}

/// A block whose dictionary is missing is damage, never a torn final
/// block and never decoded without it: `cat` fails naming the ledger and the
/// dictionary, the writer refuses, nothing is cut, and adding the dictionary
/// back repairs it.
#[test]
fn a_block_whose_dictionary_is_missing_is_damage_and_never_cut() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let d = Store::add_dictionary(root, &passed_dict()).unwrap();
    let mut s = compressed(root, u64::MAX);
    s.set_dictionary(Some(&d)).unwrap();
    s.create_agent(agent("a")).unwrap();
    for n in 10..=14 {
        s.append("a", fixed(n)).unwrap();
    }
    // A seal that stops after its block is checked, before the tail is
    // truncated: the hot tail still holds the final block's lines.
    testing::set(Point::SealChecked, |_| anyhow::bail!("crash"));
    s.set_seal_bytes(1).unwrap();
    assert!(s.append("a", fixed(15)).is_err());
    testing::clear();
    drop(s);
    // Indexed while the dictionary is there, so opening does not re-read it.
    drop(Store::open(root).unwrap());
    let p = paths(root, "a");
    let (zst0, tail0) = (std::fs::read(&p.zst).unwrap(), std::fs::read(&p.tail).unwrap());
    assert!(!zst0.is_empty() && !tail0.is_empty());
    std::fs::remove_file(dict_path(root, &d)).unwrap();
    let s = Store::open(root);
    let mut s = match s {
        Ok(s) => {
            let e = format!("{:#}", s.reader("a").unwrap().stream().unwrap_err());
            assert!(e.contains("a.jsonl") && e.contains("damaged") && e.contains(&format!("dictionary {d} is missing")), "{e}");
            s
        }
        Err(e) => panic!("{e:#}"),
    };
    let e = format!("{:#}", s.append("a", fixed(16)).unwrap_err());
    assert!(e.contains("damaged") && e.contains("is missing"), "{e}");
    assert!(std::fs::read(&p.zst).unwrap() == zst0 && std::fs::read(&p.tail).unwrap() == tail0, "nothing was cut or deleted");
    // Opposed: with its dictionary back, the same ledger opens and reads.
    Store::add_dictionary(root, &passed_dict()).unwrap();
    let mut s = Store::open(root).unwrap();
    s.append("a", fixed(16)).unwrap();
    assert_eq!(String::from_utf8(cat(&s, "a")).unwrap().lines().count(), 8);
}

/// A migration to the compressed form with a default dictionary writes the
/// dictionary back when it was deleted under it, before its rename; a
/// ledger whose blocks use a dictionary migrates back to the plain form byte
/// for byte, and not at all while that dictionary is missing.
#[test]
fn migrations_with_a_dictionary_write_it_back_and_never_decode_without_it() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let mut s = Store::open(&root).unwrap();
    s.create_agent(agent("m")).unwrap();
    for n in 10..=40 {
        s.append("m", fixed(n)).unwrap();
    }
    let p = paths(&root, "m");
    let original = std::fs::read(&p.plain).unwrap();
    let d = Store::add_dictionary(&root, &passed_dict()).unwrap();
    s.set_dictionary(Some(&d)).unwrap();
    let (r2, d2) = (root.clone(), d.clone());
    testing::set(Point::DictLocking, move |_| {
        let mut c = protoledger::store::Config::read(&r2).unwrap();
        c.compression.dictionary = None;
        c.write(&r2).unwrap();
        Store::remove_dictionary(&r2, &d2).expect("only a temporary file names it");
        Ok(())
    });
    let m = s.set_compression(true).unwrap();
    testing::clear();
    assert_eq!(m.migrated, vec!["m".to_string()], "{:?}", m.failed);
    assert!(parse_blocks(&std::fs::read(&p.zst).unwrap()).iter().all(|(meta, _)| meta["dict"] == d.as_str()));
    assert_eq!(std::fs::read(dict_path(&root, &d)).ok().map(|b| sha(&b)), Some(d.clone()), "the dictionary was written back");
    assert!(cat(&s, "m") == original);
    // Off, with the dictionary missing: refused, the ledger left as it was.
    std::fs::remove_file(dict_path(&root, &d)).unwrap();
    let zst0 = std::fs::read(&p.zst).unwrap();
    let off = s.set_compression(false).unwrap();
    assert_eq!(off.failed.len(), 1, "{off:?}");
    assert!(off.failed[0].1.contains(&format!("dictionary {d} is missing")), "{}", off.failed[0].1);
    assert!(!p.plain.exists() && std::fs::read(&p.zst).unwrap() == zst0);
    // Opposed: with it back, off is byte for byte.
    Store::add_dictionary(&root, &passed_dict()).unwrap();
    let off = s.set_compression(false).unwrap();
    assert_eq!(off.migrated, vec!["m".to_string()], "{:?}", off.failed);
    assert!(std::fs::read(&p.plain).unwrap() == original && !p.zst.exists());
}

// ---- the command line ----

mod cli {
    use super::*;
    use std::process::{Command, Output};

    fn run(args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_protoledger")).args(args).output().unwrap()
    }

    /// Standard output of a command that must succeed.
    fn ok(args: &[&str]) -> String {
        let o = run(args);
        assert!(o.status.success(), "{args:?}: {}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8(o.stdout).unwrap()
    }

    /// Standard error of a command that must succeed.
    fn ok_err(args: &[&str]) -> String {
        let o = run(args);
        assert!(o.status.success(), "{args:?}: {}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8(o.stderr).unwrap()
    }

    #[test]
    fn store_dict_add_ls_rm_recompress_and_config_dictionary() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("s");
        three_blocks(&root);
        let r = root.to_str().unwrap();
        let file = dir.path().join("d.zdict");
        std::fs::write(&file, passed_dict()).unwrap();
        let id = ok(&["store", "dict", "add", r, file.to_str().unwrap()]).trim().to_string();
        assert_eq!(id, sha(&passed_dict()));
        let ls = ok(&["store", "dict", "ls", r]);
        assert_eq!(ls, format!("{id}\tsize={}\tdefault=no\tblocks=0\n", passed_dict().len()));
        ok(&["store", "config", r, "--dictionary", &id]);
        assert!(std::fs::read_to_string(root.join("store.json")).unwrap().contains(&format!("\"dictionary\":\"{id}\"")));
        let out = ok_err(&["store", "recompress", r]);
        assert!(out.contains("recompressed\ta"), "{out}");
        let ls = ok(&["store", "dict", "ls", r]);
        assert!(ls.starts_with(&format!("{id}\tsize={}\tdefault=yes\tblocks=1\n\ta\tblocks=1\n", passed_dict().len())), "{ls}");
        assert!(!run(&["store", "dict", "rm", r, &id]).status.success(), "the default is not deleted");
        ok(&["store", "config", r, "--dictionary", "none"]);
        assert!(std::fs::read_to_string(root.join("store.json")).unwrap().contains("\"dictionary\":null"));
        let o = run(&["store", "dict", "rm", r, &id]);
        assert!(!o.status.success() && String::from_utf8_lossy(&o.stderr).contains("used by 1 ledgers"), "{}", String::from_utf8_lossy(&o.stderr));
        ok(&["store", "recompress", r, "--dictionary", "none", "a"]);
        ok(&["store", "dict", "rm", r, &id]);
        assert!(!dict_path(&root, &id).exists());
        assert!(!run(&["store", "config", r, "--dictionary", &id]).status.success(), "an id not in the store is refused");
        assert_eq!(ok(&["store", "dict", "ls", r]), "");
    }
}

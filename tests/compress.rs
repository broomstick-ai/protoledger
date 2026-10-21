//! Compressed ledgers in a store (SPEC §10): sealed zstd blocks with meta
//! frames plus a plain hot tail, read back byte for byte as the decompressed
//! stream; torn tails and interrupted seals and migrations repaired; damage
//! refused; readers consistent while a writer seals or a ledger migrates.
#![cfg(feature = "store")]

use protoledger::store::testing::{self, Point};
use protoledger::store::{BlockRow, Store};
use protoledger::{Agent, Event, Kind};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn agent(id: &str) -> Agent {
    Agent { id: id.into(), created_at: None, source: None, parent: None, links: vec![], meta: Default::default() }
}

/// An event whose line is the same length for every `n` in 10..=99.
fn fixed(n: u64) -> Event {
    Event::new(format!("e{n:02}"), Kind::Text).with("role", "user").with("text", "the same width every time")
}

fn text(id: &str, t: &str) -> Event {
    Event::new(id, Kind::Text).with("role", "user").with("text", t)
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

/// A store whose new ledgers are compressed, sealing at `seal_bytes`.
fn compressed(root: &Path, seal_bytes: u64) -> Store {
    let mut s = Store::open(root).unwrap();
    s.set_compression(true).unwrap();
    s.set_seal_bytes(seal_bytes).unwrap();
    s
}

/// The stream as the reader returns it.
fn cat(s: &Store, agent: &str) -> Vec<u8> {
    s.reader(agent).unwrap().stream().unwrap()
}

/// Every line of a stream, as text.
fn lines(stream: &[u8]) -> Vec<String> {
    String::from_utf8(stream.to_vec()).unwrap().lines().map(str::to_string).collect()
}

fn poke_index(root: &Path, sql: &str) {
    let db = rusqlite::Connection::open(root.join("index.db")).unwrap();
    db.execute_batch(sql).unwrap();
}

/// Block rows read from the block file by this test's own parser of the
/// format (SPEC §10.3), not the crate's: `(physical_offset, physical_len,
/// meta payload as JSON)`.
fn parse_blocks(zst: &[u8]) -> Vec<(u64, u64, serde_json::Value)> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at < zst.len() {
        assert_eq!(&zst[at..at + 4], &[0x5C, 0x2A, 0x4D, 0x18], "a meta frame's magic at {at}");
        let plen = u32::from_le_bytes(zst[at + 4..at + 8].try_into().unwrap()) as usize;
        let payload = &zst[at + 8..at + 8 + plen];
        let meta: serde_json::Value = serde_json::from_slice(payload).unwrap();
        let zlen = meta["zlen"].as_u64().unwrap() as usize;
        let total = 8 + plen + zlen;
        out.push((at as u64, total as u64, meta));
        at += total;
    }
    assert_eq!(at, zst.len(), "the block file holds blocks and nothing else");
    out
}

/// A zstd frame header, parsed by this test (RFC 8878 §3.1.1.1):
/// `(content size, checksum flag)`.
fn zstd_header(f: &[u8]) -> (Option<u64>, bool) {
    assert_eq!(&f[..4], &[0x28, 0xB5, 0x2F, 0xFD], "a zstd frame's magic");
    let fhd = f[4];
    let single = fhd & 0x20 != 0;
    let did = [0usize, 1, 2, 4][(fhd & 3) as usize];
    let at = 5 + usize::from(!single) + did;
    let size = match (fhd >> 6, single) {
        (0, false) => None,
        (0, true) => Some(f[at] as u64),
        (1, _) => Some(u16::from_le_bytes(f[at..at + 2].try_into().unwrap()) as u64 + 256),
        (2, _) => Some(u32::from_le_bytes(f[at..at + 4].try_into().unwrap()) as u64),
        _ => Some(u64::from_le_bytes(f[at..at + 8].try_into().unwrap())),
    };
    (size, fhd & 4 != 0)
}

/// One block (meta frame + zstd frame with checksum) holding `data`, with a
/// meta payload made by `meta` from the zstd frame's length.
fn craft_block(data: &[u8], meta: impl Fn(usize) -> String) -> Vec<u8> {
    use zstd::zstd_safe::CParameter;
    let mut c = zstd::bulk::Compressor::new(3).unwrap();
    c.set_parameter(CParameter::ChecksumFlag(true)).unwrap();
    let frame = c.compress(data).unwrap();
    let payload = meta(frame.len());
    let mut out = Vec::new();
    out.extend_from_slice(&0x184D_2A5Cu32.to_le_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload.as_bytes());
    out.extend_from_slice(&frame);
    out
}

/// A compressed ledger `a` with three sealed blocks (seqs 0..=1, 2..=11,
/// 12..=21) and a hot tail holding seqs 22..=26; event `eN` is seq `N - 9`,
/// and every event line is the same length. Returns the store (holding the
/// writer) and the stream.
fn three_blocks(root: &Path) -> (Store, Vec<u8>) {
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
    assert!(!p.plain.exists());
    assert_eq!(parse_blocks(&std::fs::read(&p.zst).unwrap()).len(), 3);
    assert_eq!(lines(&std::fs::read(&p.tail).unwrap()).len(), 5);
    let stream = cat(&s, "a");
    (s, stream)
}

/// The `n` of every event `eN` in a stream of [`fixed`] events, in order
/// (the header skipped).
fn fixed_numbers(stream: &[u8]) -> Vec<u64> {
    lines(stream)[1..].iter().map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap()["id"].as_str().unwrap()[1..].parse::<u64>().unwrap()).collect()
}

// ---- the core property: every fixture ----

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

const FIXTURE_SEAL_BYTES: u64 = 256;

#[test]
fn every_fixture_in_a_compressed_store_reads_back_byte_for_byte() {
    let dir = tempfile::tempdir().unwrap();
    let cases = fixture_cases();
    assert!(cases.len() > 150, "found {} fixture cases", cases.len());
    let (mut ledgers, mut events, mut skipped) = (0usize, 0u64, Vec::new());
    let mut block_counts: Vec<(usize, String)> = Vec::new();
    let mut with_tail = 0usize;
    for (i, (format, native)) in cases.iter().enumerate() {
        let plain_root = dir.path().join(format!("p{i}"));
        let packed_root = dir.path().join(format!("c{i}"));
        let mut p = Store::open(&plain_root).unwrap();
        let mut c = compressed(&packed_root, FIXTURE_SEAL_BYTES);
        let rp = p.import(format, native);
        let rc = c.import(format, native);
        if let Err(e) = &rp {
            assert!(rc.is_err(), "{}: imports plain ({e}) but not compressed", native.display());
            skipped.push(native.display().to_string());
            continue;
        }
        rc.unwrap();
        drop(c);
        // A fresh store: every read goes through files a writer left behind.
        let c = Store::open(&packed_root).unwrap();
        let agents = p.agents().unwrap();
        assert_eq!(agents.iter().map(|a| &a.id).collect::<Vec<_>>(), c.agents().unwrap().iter().map(|a| &a.id).collect::<Vec<_>>());
        for a in &agents {
            let pa = paths(&packed_root, Path::new(&a.path).file_stem().unwrap().to_str().unwrap());
            // The probe must not witness itself: the compressed store really
            // holds this ledger compressed, and the plain one plain.
            assert!(!pa.plain.exists() && pa.tail.exists(), "{}: {} is not compressed", native.display(), a.id);
            assert!(plain_root.join(&a.path).is_file());
            let want = std::fs::read(plain_root.join(&a.path)).unwrap();
            let got = cat(&c, &a.id);
            assert!(got == want, "{}: {}: cat gives {} bytes, the plain ledger holds {}", native.display(), a.id, got.len(), want.len());
            let blocks = if pa.zst.exists() { parse_blocks(&std::fs::read(&pa.zst).unwrap()).len() } else { 0 };
            assert_eq!(c.blocks(&a.id).unwrap().len(), blocks, "{}: block rows", a.id);
            block_counts.push((blocks, format!("{format}/{}", a.id)));
            with_tail += usize::from(len_of(&pa.tail) > 0);
            for seq in 1..=a.last_seq {
                let (x, y) = (p.event_at(&a.id, seq).unwrap(), c.event_at(&a.id, seq).unwrap());
                assert!(x.is_some() && x == y, "{}: {} seq {seq}", native.display(), a.id);
            }
            assert!(c.event_at(&a.id, a.last_seq + 1).unwrap().is_none());
            ledgers += 1;
            events += a.last_seq;
        }
        drop(p);
    }
    block_counts.sort();
    let (min, max) = (block_counts.first().unwrap(), block_counts.last().unwrap());
    let several = block_counts.iter().filter(|(n, _)| *n >= 2).count();
    eprintln!(
        "fixtures: {} cases, {} skipped, {ledgers} ledgers, {events} events; seal_bytes {FIXTURE_SEAL_BYTES}: blocks per ledger min {} ({}), max {} ({}); {several} ledgers with 2+ blocks; {with_tail} with a non-empty hot tail",
        cases.len(),
        skipped.len(),
        min.0,
        min.1,
        max.0,
        max.1
    );
    assert!(ledgers > 250 && max.0 > 10 && several > ledgers / 2, "the fixtures must exercise several blocks");
}

/// One plain store at `root` holding every fixture (cases whose agent ids
/// collide with an earlier case's are left out: an import would extend that
/// ledger). `scratch` is a directory it may use.
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

/// Total bytes of the files in `<root>/ledgers` whose names end in one of `suffixes`.
fn bytes_of(root: &Path, suffixes: &[&str]) -> u64 {
    std::fs::read_dir(root.join("ledgers"))
        .unwrap()
        .map(|e| e.unwrap())
        .filter(|e| suffixes.iter().any(|x| e.file_name().to_string_lossy().ends_with(x)))
        .map(|e| e.metadata().unwrap().len())
        .sum()
}

#[test]
fn migration_on_off_on_is_byte_identical_for_every_fixture() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("s");
    let mut s = store_of_every_fixture(&root, dir.path());
    let mut originals: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    for a in s.agents().unwrap() {
        originals.insert(a.id.clone(), std::fs::read(root.join(&a.path)).unwrap());
    }
    assert!(originals.len() > 200, "{} ledgers", originals.len());
    let check = |s: &Store, compressed: bool| {
        for a in s.agents().unwrap() {
            let p = paths(&root, Path::new(&a.path).file_stem().unwrap().to_str().unwrap());
            assert_eq!((p.plain.exists(), p.zst.exists()), (!compressed, compressed), "{}", a.id);
            let got = cat(s, &a.id);
            assert!(got == originals[&a.id], "{}: {} bytes, {} expected", a.id, got.len(), originals[&a.id].len());
            if !compressed {
                assert!(std::fs::read(&p.plain).unwrap() == originals[&a.id], "{}: the plain file", a.id);
            }
        }
    };
    let on = s.set_compression(true).unwrap();
    assert_eq!((on.migrated.len(), on.skipped.len(), on.failed.len()), (originals.len(), 0, 0), "{:?}", on.failed);
    check(&s, true);
    let off = s.set_compression(false).unwrap();
    assert_eq!((off.migrated.len(), off.skipped.len(), off.failed.len()), (originals.len(), 0, 0), "{:?}", off.failed);
    check(&s, false);
    let again = s.set_compression(true).unwrap();
    assert_eq!(again.migrated.len(), originals.len());
    check(&s, true);
    // The store reopened, and the index rebuilt from the files alone.
    drop(s);
    let mut s = Store::open(&root).unwrap();
    s.reindex().unwrap();
    check(&s, true);
    assert!(std::fs::read_dir(root.join("ledgers")).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().contains(".tmp-")));
}

/// The ratio of the fixture corpus, compressed by this crate, by block size
/// and no dictionary: blocks a migration writes (`recompress_bytes`, zstd
/// level 19) and blocks a writer seals as it appends (`seal_bytes`, level 3).
/// Every physical byte counts: meta frames, zstd frame headers, checksums,
/// and the hot tails. Printed for the design notes; asserted only to shrink
/// the corpus and to read back exactly.
#[test]
fn fixture_corpus_ratio_by_block_size() {
    let dir = tempfile::tempdir().unwrap();
    let plain_root = dir.path().join("plain");
    let plain = store_of_every_fixture(&plain_root, dir.path());
    let agents = plain.agents().unwrap();
    let plain_bytes = bytes_of(&plain_root, &[".jsonl"]);
    assert_eq!(plain_bytes, agents.iter().map(|a| a.size).sum::<u64>());
    let mut table = vec![format!("fixtures: {} ledgers, {} events, {plain_bytes} plain bytes", agents.len(), agents.iter().map(|a| a.last_seq).sum::<u64>())];
    for (i, size) in [16u64 << 10, 64 << 10, 256 << 10, 1 << 20].into_iter().enumerate() {
        // Migrated: a copy of the plain store, compressed in blocks of `size`.
        let root = dir.path().join(format!("m{i}"));
        std::fs::create_dir_all(root.join("ledgers")).unwrap();
        for a in &agents {
            std::fs::copy(plain_root.join(&a.path), root.join(&a.path)).unwrap();
        }
        let mut c = protoledger::store::Config::default();
        c.compression.recompress_bytes = size;
        c.write(&root).unwrap();
        let mut s = Store::open(&root).unwrap();
        let m = s.set_compression(true).unwrap();
        assert_eq!(m.migrated.len(), agents.len());
        let migrated = bytes_of(&root, &[".jsonl.zst", ".jsonl.tail"]);
        let blocks: usize = agents.iter().map(|a| s.blocks(&a.id).unwrap().len()).sum();
        // Sealed: every ledger appended afresh to a compressed store.
        let root = dir.path().join(format!("s{i}"));
        let mut s = compressed(&root, size);
        s.import("protoledger", &plain_root.join("ledgers")).unwrap();
        let sealed = bytes_of(&root, &[".jsonl.zst", ".jsonl.tail"]);
        let sealed_rows: Vec<BlockRow> = agents.iter().flat_map(|a| s.blocks(&a.id).unwrap()).collect();
        let sealed_blocks = sealed_rows.len();
        // The sealed part alone: the stream bytes now in blocks, over the block files.
        let (in_blocks, block_files) = (sealed_rows.iter().map(|r| r.len).sum::<u64>(), bytes_of(&root, &[".jsonl.zst"]));
        for a in &agents {
            assert!(cat(&s, &a.id) == std::fs::read(plain_root.join(&a.path)).unwrap(), "{}", a.id);
        }
        // A seal size above most fixtures leaves them whole in hot tails.
        assert!(migrated < plain_bytes && sealed <= plain_bytes, "{migrated} {sealed} {plain_bytes}");
        table.push(format!(
            "block {:>4} KiB: migrated (L19) {migrated} bytes, {blocks} blocks, ratio {:.2}; sealed (L3) {sealed} bytes with tails, {sealed_blocks} blocks, ratio {:.2}; sealed part alone {in_blocks} -> {block_files} bytes, ratio {}",
            size >> 10,
            plain_bytes as f64 / migrated as f64,
            plain_bytes as f64 / sealed as f64,
            if block_files > 0 { format!("{:.2}", in_blocks as f64 / block_files as f64) } else { "-".into() }
        ));
    }
    // The two ceilings a per-ledger block scheme gives up: each ledger as one
    // zstd frame (no random access, no append), and the whole corpus as one
    // (no per-ledger deletion either). Level 19, no dictionary.
    let all: Vec<Vec<u8>> = agents.iter().map(|a| std::fs::read(plain_root.join(&a.path)).unwrap()).collect();
    let whole_files: usize = all.iter().map(|b| zstd::bulk::compress(b, 19).unwrap().len()).sum();
    let one_stream = zstd::bulk::compress(&all.concat(), 19).unwrap().len();
    table.push(format!("each ledger as one frame (L19): {whole_files} bytes, ratio {:.2}", plain_bytes as f64 / whole_files as f64));
    table.push(format!("the corpus as one frame (L19): {one_stream} bytes, ratio {:.2}", plain_bytes as f64 / one_stream as f64));
    eprintln!("{}", table.join("\n"));
}

// ---- sealing ----

#[test]
fn a_seal_moves_the_whole_tail_into_one_checked_block() {
    let dir = tempfile::tempdir().unwrap();
    let (s, stream) = three_blocks(dir.path());
    let p = paths(dir.path(), "a");
    let zst = std::fs::read(&p.zst).unwrap();
    let blocks = parse_blocks(&zst);
    let mut decoded = Vec::new();
    let (mut offset, mut next) = (0u64, 0u64);
    for (at, total, meta) in &blocks {
        // The meta frame: exactly the fields of SPEC §10.3, canonical JSON.
        let payload = &zst[*at as usize + 8..(*at + *total - meta["zlen"].as_u64().unwrap()) as usize];
        let keys: Vec<&str> = meta.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, ["block", "dict", "first_seq", "generation", "last_seq", "len", "offset", "zlen"]);
        assert_eq!(std::str::from_utf8(payload).unwrap(), serde_json::to_string(meta).unwrap(), "sorted keys, no whitespace");
        assert_eq!((meta["block"].as_u64(), meta["dict"].is_null(), meta["generation"].as_u64()), (Some(1), true, Some(0)));
        assert_eq!((meta["offset"].as_u64(), meta["first_seq"].as_u64()), (Some(offset), Some(next)));
        // The zstd frame: content size equal to len, checksum flag set.
        let frame = &zst[(*at + *total - meta["zlen"].as_u64().unwrap()) as usize..(*at + *total) as usize];
        assert_eq!(zstd_header(frame), (meta["len"].as_u64(), true));
        let bytes = zstd::bulk::decompress(frame, meta["len"].as_u64().unwrap() as usize).unwrap();
        assert_eq!(bytes.len() as u64, meta["len"].as_u64().unwrap());
        decoded.extend_from_slice(&bytes);
        offset += meta["len"].as_u64().unwrap();
        next = meta["last_seq"].as_u64().unwrap() + 1;
    }
    assert_eq!(blocks.iter().map(|b| (b.2["first_seq"].as_u64().unwrap(), b.2["last_seq"].as_u64().unwrap())).collect::<Vec<_>>(), [(0, 1), (2, 11), (12, 21)]);
    // The stream is the blocks' bytes, then the hot tail.
    let tail = std::fs::read(&p.tail).unwrap();
    assert_eq!([decoded.as_slice(), tail.as_slice()].concat(), stream);
    // The index's block rows are the meta frames and their positions.
    let rows: Vec<BlockRow> = s.blocks("a").unwrap();
    let want: Vec<(u64, u64, u64, u64, u64, u64, u64)> = blocks
        .iter()
        .map(|(at, total, m)| {
            (m["offset"].as_u64().unwrap(), m["len"].as_u64().unwrap(), *at, *total, m["first_seq"].as_u64().unwrap(), m["last_seq"].as_u64().unwrap(), 0)
        })
        .collect();
    let got: Vec<_> = rows.iter().map(|r| (r.offset, r.len, r.physical_offset, r.physical_len, r.first_seq, r.last_seq, r.generation)).collect();
    assert_eq!(got, want);
    assert!(rows.iter().all(|r| r.dict.is_none() && r.agent == "a"));
    // Rebuilt from the meta frames alone: reindex, and an index deleted.
    drop(s);
    let mut s = Store::open(dir.path()).unwrap();
    s.reindex().unwrap();
    assert_eq!(s.blocks("a").unwrap(), rows, "reindex");
    drop(s);
    std::fs::remove_file(dir.path().join("index.db")).unwrap();
    let s = Store::open(dir.path()).unwrap();
    assert_eq!(s.blocks("a").unwrap(), rows, "an index rebuilt from nothing");
    assert!(cat(&s, "a") == stream);
}

#[test]
fn a_seal_whose_block_does_not_read_back_is_cut_off_and_the_tail_kept() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, _) = three_blocks(dir.path());
    let p = paths(dir.path(), "a");
    let (zst_before, tail_before) = (len_of(&p.zst), len_of(&p.tail));
    // Corrupt the new block between its write and its read-back.
    testing::set(Point::SealWritten, move |zst| {
        let mut b = std::fs::read(zst)?;
        let last = b.len() - 3;
        b[last] ^= 0xff;
        std::fs::write(zst, b)?;
        Ok(())
    });
    s.set_seal_bytes(1).unwrap();
    s.append("a", fixed(36)).unwrap();
    testing::clear();
    assert_eq!(len_of(&p.zst), zst_before, "the bad block was cut off");
    assert!(len_of(&p.tail) > tail_before, "the hot tail was kept, with the new line");
    assert_eq!(fixed_numbers(&cat(&s, "a")), (10..=36).collect::<Vec<_>>());
    // Opposed: the same seal with nothing corrupted moves the tail.
    s.append("a", fixed(37)).unwrap();
    assert!(len_of(&p.zst) > zst_before && len_of(&p.tail) == 0);
    assert_eq!(fixed_numbers(&cat(&s, "a")), (10..=37).collect::<Vec<_>>());
    drop(s);
    let s = Store::open(dir.path()).unwrap();
    assert_eq!(fixed_numbers(&cat(&s, "a")), (10..=37).collect::<Vec<_>>());
}

#[test]
fn a_writer_reopening_a_sealed_ledger_loads_it_through_its_blocks() {
    let dir = tempfile::tempdir().unwrap();
    let (s, stream) = three_blocks(dir.path());
    drop(s);
    let mut s = Store::open(dir.path()).unwrap();
    assert_eq!(s.held("a").unwrap().events.len(), 26);
    let at = s.append("a", fixed(36)).unwrap();
    assert_eq!((at.seq, at.offset), (27, stream.len() as u64));
    assert_eq!(fixed_numbers(&cat(&s, "a")), (10..=36).collect::<Vec<_>>());
    assert!(s.append("a", fixed(20)).is_err(), "an id the sealed blocks hold is refused");
}

#[test]
fn catch_up_after_a_seal_indexes_only_the_new_events() {
    let dir = tempfile::tempdir().unwrap();
    let (mut w, stream) = three_blocks(dir.path());
    let other = Store::open(dir.path()).unwrap();
    let rx = other.subscribe(protoledger::store::Since::Agents(vec![("a".into(), 26)])).unwrap();
    // Two appends that seal, whose index updates are lost (a crash between
    // the append and the index): the block file grows, the hot tail empties,
    // and the index still says seq 26 at the old decompressed length.
    w.set_seal_bytes(1).unwrap();
    w.append("a", fixed(36)).unwrap();
    w.append("a", fixed(37)).unwrap();
    poke_index(
        dir.path(),
        &format!(
            "DELETE FROM events WHERE agent = 'a' AND seq > 26;
             UPDATE agents SET last_seq = 26, byte_len = {}, size = 1, mtime_ns = 1, tail_size = 1, tail_mtime_ns = 1 WHERE id = 'a';",
            stream.len()
        ),
    );
    let p = paths(dir.path(), "a");
    assert!(len_of(&p.tail) == 0 && len_of(&p.zst) < stream.len() as u64, "every file is shorter than the indexed stream");
    assert_eq!(other.refresh().unwrap(), 2, "only the two new events are fresh");
    let got: Vec<u64> = rx.try_iter().map(|p| p.seq).collect();
    assert_eq!(got, [27, 28]);
    let a = other.agent("a").unwrap().unwrap();
    assert_eq!((a.last_seq, a.byte_len), (28, cat(&other, "a").len() as u64));
    assert_eq!(other.blocks("a").unwrap().len(), 5);
}

// ---- torn tails ----

/// Restore a ledger's two files.
fn put(p: &Paths, zst: &[u8], tail: &[u8]) {
    std::fs::write(&p.zst, zst).unwrap();
    std::fs::write(&p.tail, tail).unwrap();
}

#[test]
fn a_torn_hot_tail_at_every_byte_of_its_last_line_repairs_to_the_last_whole_line() {
    let dir = tempfile::tempdir().unwrap();
    let (s, stream) = three_blocks(dir.path());
    drop(s);
    let p = paths(dir.path(), "a");
    let (zst, tail) = (std::fs::read(&p.zst).unwrap(), std::fs::read(&p.tail).unwrap());
    let last_start = tail[..tail.len() - 1].iter().rposition(|b| *b == b'\n').unwrap() + 1;
    let before = &stream[..stream.len() - (tail.len() - last_start)];
    for cut in last_start + 1..tail.len() {
        put(&p, &zst, &tail[..cut]);
        let mut s = Store::open(dir.path()).unwrap();
        assert!(cat(&s, "a") == before, "cut {cut}: a reader ignores the torn line");
        s.held("a").unwrap();
        assert_eq!(len_of(&p.tail), last_start as u64, "cut {cut}: the writer cut the tail to its last whole line");
        assert!(cat(&s, "a") == before, "cut {cut}");
        s.append("a", fixed(35)).unwrap();
        assert!(cat(&s, "a") == stream, "cut {cut}: the line appended again lands on a line boundary");
    }
}

/// A seal that crashed after writing its block: the block file ends in the
/// new block (whole), and the hot tail still holds its lines.
fn crashed_seal(root: &Path) -> (Vec<u8>, Vec<u8>, usize, Vec<u8>) {
    let (mut s, _) = three_blocks(root);
    let p = paths(root, "a");
    let zst_before = len_of(&p.zst) as usize;
    testing::set(Point::SealWritten, |_| anyhow::bail!("crash"));
    s.set_seal_bytes(1).unwrap();
    assert!(s.append("a", fixed(36)).is_err());
    testing::clear();
    drop(s);
    let (zst, tail) = (std::fs::read(&p.zst).unwrap(), std::fs::read(&p.tail).unwrap());
    assert!(zst.len() > zst_before && lines(&tail).len() == 6);
    let stream = cat(&Store::open(root).unwrap(), "a");
    (zst, tail, zst_before, stream)
}

#[test]
fn a_torn_final_block_at_every_byte_repairs_to_the_last_whole_block() {
    let dir = tempfile::tempdir().unwrap();
    let (zst, tail, whole, stream) = crashed_seal(dir.path());
    assert_eq!(fixed_numbers(&stream), (10..=36).collect::<Vec<_>>());
    let p = paths(dir.path(), "a");
    for cut in whole + 1..zst.len() {
        put(&p, &zst[..cut], &tail);
        let mut s = Store::open(dir.path()).unwrap();
        assert!(cat(&s, "a") == stream, "cut {cut}: a reader ignores the torn block");
        assert_eq!(s.event_at("a", 27).unwrap().unwrap().id, "e36", "cut {cut}");
        s.held("a").unwrap_or_else(|e| panic!("cut {cut}: {e:#}"));
        assert_eq!(len_of(&p.zst), whole as u64, "cut {cut}: the writer cut the block file to its last whole block");
        assert_eq!(std::fs::read(&p.tail).unwrap(), tail, "cut {cut}: the hot tail is untouched");
        assert!(cat(&s, "a") == stream, "cut {cut}");
        assert_eq!(s.blocks("a").unwrap().len(), 3, "cut {cut}");
    }
}

#[test]
fn a_leftover_tail_after_a_seal_crash_equals_the_stream_end_and_is_cut() {
    let dir = tempfile::tempdir().unwrap();
    let (zst, tail, _, stream) = crashed_seal(dir.path());
    let p = paths(dir.path(), "a");
    put(&p, &zst, &tail);
    let mut s = Store::open(dir.path()).unwrap();
    // The block holds the tail's lines: the tail is a leftover, skipped.
    assert!(cat(&s, "a") == stream, "no line twice");
    assert!(stream.ends_with(&tail), "the leftover is the stream's last bytes");
    let r = s.reader("a").unwrap();
    assert_eq!(r.len().unwrap(), stream.len() as u64);
    let (evs, end) = r.tail(stream.len() as u64).unwrap();
    assert_eq!((evs.len(), end), (0, stream.len() as u64), "a read at the end of the stream ends");
    assert!(r.read_at(stream.len() as u64).is_err());
    s.held("a").unwrap();
    assert_eq!((len_of(&p.zst), len_of(&p.tail)), (zst.len() as u64, 0), "the writer kept the block and cut the tail");
    assert!(cat(&s, "a") == stream);
    // A leftover that does not match the block is damage: a writer refuses
    // it, a read fails naming the ledger, and nothing is changed.
    drop(s);
    put(&p, &zst, &tail);
    drop(Store::open(dir.path()).unwrap());
    let mut bad = tail.clone();
    let i = bad.len() - 5;
    bad[i] = b'X';
    // Same length and modification time: the index sees nothing new, so
    // the store opens without reading the ledger and the writer meets it.
    let mtime = std::fs::metadata(&p.tail).unwrap().modified().unwrap();
    std::fs::write(&p.tail, &bad).unwrap();
    std::fs::File::options().write(true).open(&p.tail).unwrap().set_modified(mtime).unwrap();
    let mut s = Store::open(dir.path()).unwrap();
    let e = format!("{:#}", s.held("a").map(|_| ()).unwrap_err());
    assert!(e.contains("a.jsonl") && e.contains("damaged"), "{e}");
    let e = format!("{:#}", s.reader("a").unwrap().stream().unwrap_err());
    assert!(e.contains("a.jsonl") && e.contains("damaged"), "cat fails naming the ledger: {e}");
    assert_eq!((std::fs::read(&p.zst).unwrap(), std::fs::read(&p.tail).unwrap()), (zst.clone(), bad.clone()), "nothing was changed");
    drop(s);
    // And a store that has to read it to open says so.
    std::fs::write(&p.tail, &bad).unwrap();
    let e = format!("{:#}", Store::open(dir.path()).err().expect("the index cannot read a"));
    assert!(e.contains("a.jsonl") && e.contains("damaged"), "{e}");
}

// ---- damage ----

#[test]
fn a_corrupt_block_that_is_not_the_final_one_is_damage() {
    let dir = tempfile::tempdir().unwrap();
    let (s, _) = three_blocks(dir.path());
    drop(s);
    let p = paths(dir.path(), "a");
    let mut zst = std::fs::read(&p.zst).unwrap();
    let blocks = parse_blocks(&zst);
    let (at, total, _) = &blocks[1];
    let i = (*at + *total - 6) as usize;
    zst[i] ^= 0x55;
    std::fs::write(&p.zst, &zst).unwrap();
    let tail = std::fs::read(&p.tail).unwrap();
    let mut s = Store::open(dir.path()).unwrap();
    let e = format!("{:#}", s.reader("a").unwrap().stream().unwrap_err());
    assert!(e.contains("a.jsonl") && e.contains("damaged"), "{e}");
    let e = format!("{:#}", s.event_at("a", 5).unwrap_err());
    assert!(e.contains("a.jsonl"), "{e}");
    // A read that does not need the damaged block still works.
    assert_eq!(s.event_at("a", 1).unwrap().unwrap().id, "e10");
    assert_eq!(s.event_at("a", 25).unwrap().unwrap().id, "e34");
    let e = format!("{:#}", s.append("a", fixed(36)).unwrap_err());
    assert!(e.contains("a.jsonl") && e.contains("not the final block"), "{e}");
    assert_eq!((std::fs::read(&p.zst).unwrap(), std::fs::read(&p.tail).unwrap()), (zst, tail), "nothing was repaired");
}

#[test]
fn a_block_of_an_unknown_version_is_refused_and_unknown_meta_fields_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let header = "{\"protoledger\":\"0.1\",\"agent\":{\"id\":\"v\"}}\n";
    let ev = "{\"id\":\"v1\",\"seq\":1,\"kind\":\"text\",\"body\":{\"role\":\"user\",\"text\":\"one\"}}\n";
    let data = format!("{header}{ev}");
    let meta = |version: u64, extra: &'static str| {
        let len = data.len();
        move |zlen: usize| {
            format!("{{\"block\":{version},\"dict\":null,\"first_seq\":0,\"generation\":0,\"last_seq\":1,\"len\":{len},\"offset\":0{extra},\"zlen\":{zlen}}}")
        }
    };
    let ledgers = dir.path().join("ledgers");
    std::fs::create_dir_all(&ledgers).unwrap();
    // Unknown fields: ignored.
    std::fs::write(ledgers.join("v.jsonl.zst"), craft_block(data.as_bytes(), meta(1, ",\"zz_future\":{\"x\":[1,2]}"))).unwrap();
    let mut s = Store::open(dir.path()).unwrap();
    assert_eq!(cat(&s, "v"), data.as_bytes());
    s.append("v", text("v2", "two")).unwrap();
    assert_eq!(lines(&cat(&s, "v")).len(), 3);
    drop(s);
    // An unknown version: refused, by a reader and by the store's index.
    std::fs::write(ledgers.join("w.jsonl.zst"), craft_block(data.replace("\"v\"", "\"w\"").as_bytes(), meta(2, ""))).unwrap();
    let e = format!("{:#}", protoledger::store::LedgerReader::plain(ledgers.join("w.jsonl")).stream().unwrap_err());
    assert!(e.contains("w.jsonl") && e.contains("version 2"), "{e}");
    let e = format!("{:#}", Store::open(dir.path()).err().expect("the index cannot read w"));
    assert!(e.contains("w.jsonl") && e.contains("version 2"), "{e}");
    // Opposed: the same block at version 1 is read.
    std::fs::write(ledgers.join("w.jsonl.zst"), craft_block(data.replace("\"v\"", "\"w\"").as_bytes(), meta(1, ""))).unwrap();
    assert_eq!(lines(&protoledger::store::LedgerReader::plain(ledgers.join("w.jsonl")).stream().unwrap()).len(), 2);
}

// ---- reading while a writer seals ----

#[test]
fn a_reader_during_a_seal_gets_every_line_once() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, stream) = three_blocks(dir.path());
    let root = dir.path().to_path_buf();
    let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    for point in [Point::SealWritten, Point::SealChecked] {
        let (seen, root) = (seen.clone(), root.clone());
        testing::set(point, move |_| {
            let r = Store::open(&root)?.reader("a")?;
            seen.borrow_mut().push((r.stream()?, r.len()?, r.tail(0)?.0.len()));
            Ok(())
        });
    }
    s.set_seal_bytes(1).unwrap();
    s.append("a", fixed(36)).unwrap();
    testing::clear();
    let after = cat(&s, "a");
    assert_eq!(fixed_numbers(&after), (10..=36).collect::<Vec<_>>());
    let seen = seen.borrow();
    assert_eq!(seen.len(), 2, "both points ran");
    for (got, len, events) in seen.iter() {
        assert!(*got == after, "mid-seal: {} bytes, {} expected", got.len(), after.len());
        assert_eq!((*len, *events), (after.len() as u64, 27));
    }
    assert!(after.starts_with(&stream));
}

/// A store holding `a` (three blocks and a tail of seqs 31..=35) whose
/// writer, in a hook, seals the tail and appends `then` more lines.
fn sealing_hook(root: &Path, point: Point, then: Vec<u64>) -> Store {
    let (w, _) = three_blocks(root);
    // Opened before the hook is set: opening reads every ledger.
    let r = Store::open(root).unwrap();
    let mut w = Some(w);
    testing::set(point, move |_| {
        if let Some(mut s) = w.take() {
            s.set_seal_bytes(1)?;
            s.append("a", fixed(36))?;
            s.set_seal_bytes(u64::MAX)?;
            for n in then.clone() {
                s.append("a", fixed(n))?;
            }
        }
        Ok(())
    });
    r
}

#[test]
fn a_seal_between_taking_l_and_reading_the_tail_never_yields_another_line() {
    let dir = tempfile::tempdir().unwrap();
    // Seq 23 (e32) is the tail's second line. The hook seals e31..=e36 into
    // a block and appends e37..=e40, so the tail's second line is then e38:
    // the old position is a line boundary holding a later line.
    let r = sealing_hook(dir.path(), Point::ReaderBeforeTail, (37..=40).collect());
    let row = r.events("a", 23, 1).unwrap().pop().unwrap();
    let e = r.reader("a").unwrap().read_seq_at(row.offset, 23).unwrap();
    testing::clear();
    assert_eq!((e.seq, e.id.as_str()), (23, "e32"));

    // Not knowing the seq it wants (a sequential read from an offset), the
    // reader still never returns another line.
    let dir = tempfile::tempdir().unwrap();
    let r = sealing_hook(dir.path(), Point::ReaderBeforeTail, (37..=40).collect());
    let row = r.events("a", 23, 1).unwrap().pop().unwrap();
    let e = r.reader("a").unwrap().read_at(row.offset).unwrap();
    let (evs, _) = r.reader("a").unwrap().tail(row.offset).unwrap();
    testing::clear();
    assert_eq!(e.id, "e32");
    assert_eq!(evs.iter().map(|(_, e)| e.id.as_str()).collect::<Vec<_>>(), ["e32", "e33", "e34", "e35", "e36", "e37", "e38", "e39", "e40"]);
}

#[test]
fn a_read_whose_tail_was_sealed_away_reopens_and_is_never_short() {
    let dir = tempfile::tempdir().unwrap();
    // The hook seals the whole tail away and appends nothing: the reader
    // finds an empty tail where it expected lines.
    let r = sealing_hook(dir.path(), Point::ReaderBeforeTail, vec![]);
    let got = r.reader("a").unwrap().stream().unwrap();
    testing::clear();
    assert_eq!(fixed_numbers(&got), (10..=36).collect::<Vec<_>>());
    // Opposed: with nothing sealed under it, the same read ends where the
    // stream ends.
    let dir = tempfile::tempdir().unwrap();
    let (s, stream) = three_blocks(dir.path());
    assert!(s.reader("a").unwrap().stream().unwrap() == stream);
}

#[test]
fn a_line_read_for_a_seq_is_that_seq_or_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let (s, _) = three_blocks(dir.path());
    let r = s.reader("a").unwrap();
    // Seq 23 is in the hot tail, seq 5 in a block.
    for seq in [23u64, 5] {
        let row = s.events("a", seq, 1).unwrap().pop().unwrap();
        assert_eq!(r.read_seq_at(row.offset, seq).unwrap().seq, seq);
        let e = format!("{:#}", r.read_seq_at(row.offset, seq + 1).unwrap_err());
        assert!(e.contains("a.jsonl") && e.contains(&format!("not seq {}", seq + 1)), "{e}");
    }
}

#[test]
fn a_read_at_the_end_of_the_stream_ends_and_never_loops() {
    let dir = tempfile::tempdir().unwrap();
    let (s, stream) = three_blocks(dir.path());
    let r = s.reader("a").unwrap();
    let end = stream.len() as u64;
    let t = std::time::Instant::now();
    assert_eq!(r.tail(end).unwrap(), (vec![], end));
    assert_eq!(r.tail(end + 500).unwrap(), (vec![], end + 500));
    let e = format!("{:#}", r.read_at(end).unwrap_err());
    assert!(e.contains("a.jsonl") && e.contains("no complete line"), "{e}");
    // A position in the middle of a tail line, at rest: an error, not a retry.
    let e = format!("{:#}", r.read_at(end - 3).unwrap_err());
    assert!(e.contains("not the start of a line"), "{e}");
    assert!(t.elapsed() < std::time::Duration::from_secs(5), "{:?}", t.elapsed());
}

#[test]
fn l_is_taken_from_the_block_file_when_the_index_lags_a_seal() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, _) = three_blocks(dir.path());
    s.set_seal_bytes(1).unwrap();
    s.append("a", fixed(36)).unwrap();
    s.set_seal_bytes(u64::MAX).unwrap();
    s.append("a", fixed(37)).unwrap();
    s.append("a", fixed(38)).unwrap();
    assert_eq!(s.blocks("a").unwrap().len(), 4);
    // The index forgets the newest block, as if the seal had not reached it.
    poke_index(dir.path(), "DELETE FROM blocks WHERE agent = 'a' AND first_seq = 22;");
    assert_eq!(s.blocks("a").unwrap().len(), 3);
    // An event in the block the index misses, and one in the tail past it.
    assert_eq!(s.event_at("a", 25).unwrap().unwrap().id, "e34");
    assert_eq!(s.event_at("a", 27).unwrap().unwrap().id, "e36");
    assert_eq!(s.event_at("a", 29).unwrap().unwrap().id, "e38");
    assert_eq!(fixed_numbers(&cat(&s, "a")), (10..=38).collect::<Vec<_>>());
    // A row missing in the middle: the meta frames are walked instead.
    poke_index(dir.path(), "DELETE FROM blocks WHERE agent = 'a' AND first_seq = 2;");
    assert_eq!(s.blocks("a").unwrap().iter().map(|b| b.first_seq).collect::<Vec<_>>(), [0, 12]);
    assert_eq!(s.event_at("a", 5).unwrap().unwrap().id, "e14");
    assert_eq!(s.event_at("a", 15).unwrap().unwrap().id, "e24");
    assert_eq!(fixed_numbers(&cat(&s, "a")), (10..=38).collect::<Vec<_>>());
}

#[test]
fn a_block_row_that_does_not_match_its_meta_frame_is_re_resolved_from_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let (s, stream) = three_blocks(dir.path());
    let rows = s.blocks("a").unwrap();
    // Move the boundary between the first two rows back by one line (e10,
    // seq 1), keeping the rows contiguous so the reader takes them: the
    // stale second row now claims e10, and its meta frame says otherwise.
    let h = stream.iter().position(|b| *b == b'\n').unwrap() as u64 + 1;
    let w = rows[0].len - h;
    assert!(w > 0 && rows[1].offset == h + w);
    poke_index(
        dir.path(),
        &format!(
            "UPDATE blocks SET len = {h}, last_seq = 0 WHERE agent = 'a' AND offset = 0;
             UPDATE blocks SET offset = {h}, len = len + {w}, first_seq = 1 WHERE agent = 'a' AND offset = {};",
            rows[1].offset
        ),
    );
    let poked = s.blocks("a").unwrap();
    assert_eq!((poked[0].len, poked[1].offset, poked[1].first_seq), (h, h, 1), "the stale rows are in place");
    assert_eq!(s.event_at("a", 1).unwrap().unwrap().id, "e10");
    assert_eq!(s.event_at("a", 15).unwrap().unwrap().id, "e24");
    assert_eq!(fixed_numbers(&cat(&s, "a")), (10..=35).collect::<Vec<_>>());
}

#[test]
fn a_read_past_the_rows_starts_at_the_last_row_and_never_walks_the_meta_frames_before_it() {
    // Damage the first block's meta frame in place, same length: a walk
    // from the start of the file meets it, a read placed by the rows does not.
    let dir = tempfile::tempdir().unwrap();
    let (s, _) = three_blocks(dir.path());
    let zst = paths(dir.path(), "a").zst;
    let mut bytes = std::fs::read(&zst).unwrap();
    let at = 8 + bytes[8..].windows(9).position(|x| x == b"\"block\":1").unwrap();
    assert_eq!(at, 9, "the first meta frame's payload opens with its version");
    bytes[at + 8] = b'9';
    std::fs::write(&zst, &bytes).unwrap();
    // Sentinel: the damage is met by anything that reads the first block.
    assert!(s.event_at("a", 1).is_err(), "the first block's meta frame is damaged");
    // A read in the last block and one in the tail, through the rows.
    assert_eq!(s.event_at("a", 15).unwrap().unwrap().id, "e24");
    assert_eq!(s.event_at("a", 25).unwrap().unwrap().id, "e34");
}

#[cfg(unix)]
#[test]
fn a_block_file_rewritten_in_place_to_the_same_length_is_caught_by_its_generation() {
    // As if two rewrites reused the block file's inode: same file, same
    // length, a new generation, and lines moved from the tail into blocks.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let ledgers = root.join("ledgers");
    std::fs::create_dir_all(&ledgers).unwrap();
    let header = "{\"protoledger\":\"0.1\",\"agent\":{\"id\":\"g\"}}\n";
    let line = |n: u64| format!("{{\"id\":\"e{n:02}\",\"seq\":{n},\"kind\":\"text\",\"body\":{{\"role\":\"user\",\"text\":\"the same width every time\"}}}}\n");
    let upto = |last: u64| format!("{header}{}", (1..=last).map(line).collect::<String>());
    let block = |data: &str, last: u64, generation: u64, pad: usize| {
        let len = data.len();
        craft_block(data.as_bytes(), move |zlen| {
            format!(
                "{{\"block\":1,\"dict\":null,\"first_seq\":0,\"generation\":{generation},\"last_seq\":{last},\"len\":{len},\"offset\":0,\"pad\":\"{}\",\"zlen\":{zlen}}}",
                "p".repeat(pad)
            )
        })
    };
    // Old: one block of seqs 0..=10, the tail 11..=16. New: one block of
    // 0..=16 (generation 1), the tail 17..=20. Pad the shorter to equal length.
    let (old0, new0) = (block(&upto(10), 10, 0, 0), block(&upto(16), 16, 1, 0));
    let (old, new) = if old0.len() <= new0.len() {
        (block(&upto(10), 10, 0, new0.len() - old0.len()), new0)
    } else {
        (old0.clone(), block(&upto(16), 16, 1, old0.len() - new0.len()))
    };
    assert_eq!(old.len(), new.len());
    let p = paths(root, "g");
    std::fs::write(&p.zst, &old).unwrap();
    std::fs::write(&p.tail, (11..=16).map(line).collect::<String>()).unwrap();
    let s = Store::open(root).unwrap();
    let row = s.events("g", 13, 1).unwrap().pop().unwrap();
    let ino = |f: &Path| {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(f).unwrap().ino()
    };
    let before = ino(&p.zst);
    let (zst, tail) = (p.zst.clone(), p.tail.clone());
    let mut once = Some((new, (17..=20).map(line).collect::<String>()));
    testing::set(Point::ReaderBeforeTail, move |_| {
        if let Some((new, t)) = once.take() {
            use std::io::{Seek, Write};
            let mut f = std::fs::OpenOptions::new().write(true).open(&zst)?;
            f.seek(std::io::SeekFrom::Start(0))?;
            f.write_all(&new)?;
            std::fs::write(&tail, t)?;
        }
        Ok(())
    });
    let e = s.reader("g").unwrap().read_at(row.offset).unwrap();
    testing::clear();
    assert_eq!(ino(&p.zst), before, "the same inode");
    assert_eq!(len_of(&p.zst), old.len() as u64, "the same length");
    assert_eq!(e.id, "e13");
}

// ---- migration ----

#[test]
fn a_migration_whose_copy_does_not_read_back_identical_leaves_the_ledger_as_it_was() {
    for on in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut s = Store::open(root).unwrap();
        if !on {
            s.set_compression(true).unwrap();
            s.set_seal_bytes(300).unwrap();
        }
        s.create_agent(agent("m")).unwrap();
        for n in 10..=30 {
            s.append("m", fixed(n)).unwrap();
        }
        let stream = cat(&s, "m");
        let p = paths(root, "m");
        let files = |p: &Paths| [&p.plain, &p.zst, &p.tail].map(|f| std::fs::read(f).ok());
        let before = files(&p);
        // Corrupt the temporary file between its write and its check.
        testing::set(Point::MigrationWritten, |tmp| {
            let mut b = std::fs::read(tmp)?;
            let i = b.len() - 4;
            b[i] ^= 0x21;
            std::fs::write(tmp, b)?;
            Ok(())
        });
        let m = s.set_compression(on).unwrap();
        testing::clear();
        assert_eq!((m.migrated.len(), m.failed.len()), (0, 1), "on={on}: {m:?}");
        assert!(m.failed[0].1.contains("byte-identical") || m.failed[0].1.contains("did not read back"), "{m:?}");
        assert_eq!(files(&p), before, "on={on}: the ledger was left as it was");
        assert!(cat(&s, "m") == stream);
        assert!(std::fs::read_dir(root.join("ledgers")).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().contains(".tmp-")), "on={on}");
        // Opposed: run again with nothing corrupted, and it migrates.
        let m = s.set_compression(on).unwrap();
        assert_eq!(m.migrated, ["m"], "on={on}");
        assert_eq!((p.plain.exists(), p.zst.exists()), (!on, on));
        assert!(cat(&s, "m") == stream);
    }
}

#[test]
fn a_crash_between_rename_and_delete_leaves_both_forms_equal_and_the_writer_keeps_the_configured_one() {
    for on in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut s = Store::open(root).unwrap();
        if !on {
            s.set_compression(true).unwrap();
            s.set_seal_bytes(300).unwrap();
        }
        s.create_agent(agent("m")).unwrap();
        for n in 10..=30 {
            s.append("m", fixed(n)).unwrap();
        }
        let stream = cat(&s, "m");
        testing::set(Point::MigrationRenamed, |_| anyhow::bail!("crash"));
        let m = s.set_compression(on).unwrap();
        testing::clear();
        assert_eq!(m.failed.len(), 1, "on={on}");
        drop(s);
        let p = paths(root, "m");
        assert!(p.plain.exists() && p.zst.exists(), "on={on}: both forms");
        let mut s = Store::open(root).unwrap();
        assert!(cat(&s, "m") == stream, "on={on}: a reader reads one consistent stream");
        s.append("m", fixed(31)).unwrap();
        assert_eq!((p.plain.exists(), p.zst.exists() || p.tail.exists()), (!on, on), "on={on}: the form store.json names was kept");
        assert_eq!(fixed_numbers(&cat(&s, "m")), (10..=31).collect::<Vec<_>>());
    }
    // Two forms that differ are damage: refused, nothing deleted.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut s = Store::open(root).unwrap();
    s.create_agent(agent("m")).unwrap();
    s.append("m", fixed(10)).unwrap();
    testing::set(Point::MigrationRenamed, |_| anyhow::bail!("crash"));
    s.set_compression(true).unwrap();
    testing::clear();
    drop(s);
    let p = paths(root, "m");
    let plain = std::fs::read_to_string(&p.plain).unwrap().replace("the same width", "the SAME width");
    std::fs::write(&p.plain, &plain).unwrap();
    let mut s = Store::open(root).unwrap();
    let e = format!("{:#}", s.append("m", fixed(11)).unwrap_err());
    assert!(e.contains("m.jsonl") && e.contains("both forms"), "{e}");
    assert!(p.plain.exists() && p.zst.exists());
}

#[test]
fn a_reader_during_a_migration_gets_one_consistent_stream() {
    for on in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut s = Store::open(&root).unwrap();
        if !on {
            s.set_compression(true).unwrap();
            s.set_seal_bytes(300).unwrap();
        }
        s.create_agent(agent("m")).unwrap();
        for n in 10..=30 {
            s.append("m", fixed(n)).unwrap();
        }
        let stream = cat(&s, "m");
        let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        for point in [Point::MigrationWritten, Point::MigrationRenamed] {
            let (seen, root) = (seen.clone(), root.clone());
            testing::set(point, move |_| {
                seen.borrow_mut().push(Store::open(&root)?.reader("m")?.stream()?);
                Ok(())
            });
        }
        s.set_compression(on).unwrap();
        testing::clear();
        assert_eq!(seen.borrow().len(), 2);
        for got in seen.borrow().iter() {
            assert!(*got == stream, "on={on}");
        }
    }
}

#[test]
fn a_reader_whose_ledger_migrates_under_it_reopens_in_the_new_form() {
    // Mid-read: the reader has taken L from the block file when the ledger
    // migrates to plain, unlinking the block file and the hot tail.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (s, stream) = three_blocks(&root);
    drop(s);
    let r = Store::open(&root).unwrap().reader("a").unwrap();
    let mut once = Some(root.clone());
    testing::set(Point::ReaderBeforeTail, move |_| {
        if let Some(root) = once.take() {
            let m = Store::open(&root)?.set_compression(false)?;
            anyhow::ensure!(m.migrated == ["a"], "{m:?}");
        }
        Ok(())
    });
    let got = r.stream().unwrap();
    testing::clear();
    assert!(paths(&root, "a").plain.exists() && !paths(&root, "a").zst.exists());
    assert!(got == stream, "{} bytes, {} expected", got.len(), stream.len());

    // A tailer: each read resumes at the same decompressed offset in
    // whichever form the ledger is in now.
    let (r, (evs, mut at)) = {
        let s = Store::open(&root).unwrap();
        let r = s.reader("a").unwrap();
        let t = r.tail(0).unwrap();
        (r, t)
    };
    assert_eq!(evs.len(), 26);
    for (on, n) in [(true, 36), (false, 37), (true, 38)] {
        let mut s = Store::open(&root).unwrap();
        s.set_compression(on).unwrap();
        s.append("a", fixed(n)).unwrap();
        let (evs, end) = r.tail(at).unwrap();
        assert_eq!(evs.iter().map(|(_, e)| e.id.clone()).collect::<Vec<_>>(), [format!("e{n}")], "on={on}");
        assert_eq!(paths(&root, "a").plain.exists(), !on);
        at = end;
    }
}

#[test]
fn a_writer_that_locks_a_ledger_migrated_under_it_opens_it_again() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let mut s = Store::open(&root).unwrap();
    s.create_agent(agent("m")).unwrap();
    for n in 10..=20 {
        s.append("m", fixed(n)).unwrap();
    }
    drop(s);
    let mut w = Store::open(&root).unwrap();
    // Between opening the plain file and locking it, the ledger migrates.
    let mut once = Some(root.clone());
    testing::set(Point::WriterLocking, move |_| {
        if let Some(root) = once.take() {
            let m = Store::open(&root)?.set_compression(true)?;
            anyhow::ensure!(m.migrated == ["m"], "{m:?}");
        }
        Ok(())
    });
    w.append("m", fixed(21)).unwrap();
    testing::clear();
    let p = paths(&root, "m");
    assert!(!p.plain.exists() && p.zst.exists());
    assert_eq!(lines(&std::fs::read(&p.tail).unwrap()).len(), 1, "the append went to the hot tail");
    let fresh = Store::open(&root).unwrap();
    assert_eq!(fixed_numbers(&cat(&fresh, "m")), (10..=21).collect::<Vec<_>>());
}

#[test]
fn a_ledger_another_writer_holds_is_skipped_by_a_migration_and_migrated_by_the_next() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut holder = Store::open(root).unwrap();
    holder.create_agent(agent("h")).unwrap();
    holder.create_agent(agent("free")).unwrap();
    holder.append("h", fixed(10)).unwrap();
    drop(holder);
    let mut holder = Store::open(root).unwrap();
    holder.held("h").unwrap();
    let mut other = Store::open(root).unwrap();
    let m = other.set_compression(true).unwrap();
    assert_eq!(m.migrated, ["free"]);
    assert_eq!(m.skipped.iter().map(|(a, _)| a.as_str()).collect::<Vec<_>>(), ["h"]);
    assert!(m.skipped[0].1.contains("locked"), "{m:?}");
    drop(holder);
    let m = other.set_compression(true).unwrap();
    assert_eq!(m.migrated, ["h"]);
}

// ---- temporary files ----

#[test]
fn temporary_files_are_named_exactly_and_only_the_lock_holders_own_are_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut s = compressed(root, 1);
    for id in ["foo", "x.tmp-1", "foo.jsonl.zst.tmp-1", "bar"] {
        s.create_agent(agent(id)).unwrap();
        s.append(id, fixed(10)).unwrap();
    }
    drop(s);
    let ledgers = root.join("ledgers");
    for t in ["foo.jsonl.zst.tmp-123", "foo.jsonl.tmp-9", "bar.jsonl.tmp-5"] {
        std::fs::write(ledgers.join(t), "half a rewrite").unwrap();
    }
    let names = || {
        let mut n: Vec<String> = std::fs::read_dir(&ledgers).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        n.sort();
        n
    };
    let mut s = Store::open(root).unwrap();
    s.reindex().unwrap();
    let ids: Vec<String> = s.agents().unwrap().into_iter().map(|a| a.id).collect();
    assert_eq!(ids, ["bar", "foo", "foo.jsonl.zst.tmp-1", "x.tmp-1"], "temporary files are no ledgers; ledgers named like them are");
    let before = names();
    s.append("foo", fixed(11)).unwrap();
    let gone: Vec<&String> = before.iter().filter(|n| !names().contains(n)).collect();
    assert_eq!(gone, ["foo.jsonl.tmp-9", "foo.jsonl.zst.tmp-123"], "foo's writer deleted its own temporary files and no other file");
    for id in ["x.tmp-1", "foo.jsonl.zst.tmp-1", "bar"] {
        s.append(id, fixed(11)).unwrap();
        assert_eq!(fixed_numbers(&cat(&s, id)), [10, 11], "{id}");
    }
    assert!(ledgers.join("foo.jsonl.zst.tmp-1.jsonl.zst").exists() && ledgers.join("x.tmp-1.jsonl.zst").exists());
    assert!(!ledgers.join("bar.jsonl.tmp-5").exists(), "bar's own writer deleted bar's");
}

// ---- configuration, CLI and garbage collection ----

#[cfg(feature = "cli")]
mod cli {
    use super::*;
    use std::process::{Command, Output};

    fn run(args: &[&str]) -> Output {
        let out = Command::new(env!("CARGO_BIN_EXE_protoledger")).args(args).output().unwrap();
        assert!(out.status.success(), "protoledger {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
        out
    }

    #[test]
    fn store_config_records_the_settings_and_migrates_both_ways() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("s");
        let st = store.to_str().unwrap();
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/protoledger/orchestrated-peers/native");
        run(&["store", "import", st, "protoledger", src.to_str().unwrap()]);
        let originals: Vec<(String, Vec<u8>)> =
            ["coord-1", "peer-a", "peer-b"].iter().map(|a| (a.to_string(), std::fs::read(store.join(format!("ledgers/{a}.jsonl"))).unwrap())).collect();
        assert!(!store.join("store.json").exists());
        let out = run(&["store", "config", st]);
        assert_eq!(
            String::from_utf8(out.stdout).unwrap(),
            "{\"compression\":{\"dictionary\":null,\"enabled\":false,\"recompress_bytes\":1048576,\"seal_bytes\":262144}}\n"
        );
        assert!(!store.join("store.json").exists(), "showing it writes nothing");

        // --seal-bytes alone is recorded in store.json.
        run(&["store", "config", st, "--seal-bytes", "300"]);
        assert_eq!(
            std::fs::read_to_string(store.join("store.json")).unwrap(),
            "{\"compression\":{\"dictionary\":null,\"enabled\":false,\"recompress_bytes\":1048576,\"seal_bytes\":300}}\n"
        );
        let out = run(&["store", "config", st, "--seal-bytes", "512", "--compression", "on"]);
        let json = "{\"compression\":{\"dictionary\":null,\"enabled\":true,\"recompress_bytes\":1048576,\"seal_bytes\":512}}\n";
        assert_eq!(String::from_utf8(out.stdout).unwrap(), json);
        assert_eq!(std::fs::read_to_string(store.join("store.json")).unwrap(), json);
        assert!(String::from_utf8_lossy(&out.stderr).contains("3 migrated, 0 skipped, 0 failed"));
        for (a, bytes) in &originals {
            assert!(!store.join(format!("ledgers/{a}.jsonl")).exists() && store.join(format!("ledgers/{a}.jsonl.zst")).exists());
            assert!(run(&["cat", st, a]).stdout == *bytes, "cat {a}");
        }
        // A new ledger is created compressed, and seals at 512 bytes.
        let more = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/protoledger/branch-rewind/native");
        run(&["store", "import", st, "protoledger", more.to_str().unwrap()]);
        let rewind = std::fs::read(more.join("rewind-1.jsonl")).unwrap();
        assert!(store.join("ledgers/rewind-1.jsonl.zst").exists() && !store.join("ledgers/rewind-1.jsonl").exists());
        assert!(rewind.len() > 1024 && len_of(&store.join("ledgers/rewind-1.jsonl.tail")) < 1024);
        assert!(run(&["cat", st, "rewind-1"]).stdout == rewind);

        let out = run(&["store", "config", st, "--compression", "off"]);
        assert!(String::from_utf8_lossy(&out.stderr).contains("4 migrated"), "{}", String::from_utf8_lossy(&out.stderr));
        for (a, bytes) in &originals {
            assert!(std::fs::read(store.join(format!("ledgers/{a}.jsonl"))).unwrap() == *bytes, "{a}: the plain file is byte-identical");
            assert!(!store.join(format!("ledgers/{a}.jsonl.zst")).exists() && !store.join(format!("ledgers/{a}.jsonl.tail")).exists());
        }
        assert!(std::fs::read(store.join("ledgers/rewind-1.jsonl")).unwrap() == rewind);
        let bad = Command::new(env!("CARGO_BIN_EXE_protoledger")).args(["store", "config", st, "--compression", "maybe"]).output().unwrap();
        assert!(!bad.status.success());
        let bad = Command::new(env!("CARGO_BIN_EXE_protoledger")).args(["store", "config", st, "--seal-bytes", "0"]).output().unwrap();
        assert!(!bad.status.success());
    }
}

#[test]
fn gc_removes_every_file_of_a_compressed_ledger_and_sizes_it_by_its_stream() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let (mut s, stream) = three_blocks(root);
    s.create_agent(agent("small")).unwrap();
    drop(s);
    std::fs::write(root.join("ledgers/a.jsonl.zst.tmp-77"), "x").unwrap();
    let p = paths(root, "a");
    assert!(len_of(&p.zst) + len_of(&p.tail) < stream.len() as u64, "compressed smaller than its stream");
    let threshold = stream.len() as u64 - 1;
    let policy = protoledger::store::GcPolicy { larger_than: Some(threshold), ..Default::default() };
    let mut s = Store::open(root).unwrap();
    // Another store holds a's writer: the lock on its hot tail stops the gc.
    let mut other = Store::open(root).unwrap();
    other.held("a").unwrap();
    let e = format!("{:#}", Store::open(root).unwrap().held("a").map(|_| ()).unwrap_err());
    assert!(e.contains("a.jsonl.tail") && e.contains("locked by another writer"), "{e}");
    assert!(s.gc(&policy, std::time::SystemTime::now()).is_err(), "a ledger another writer holds stops the gc");
    drop(other);
    let plan = s.gc(&policy, std::time::SystemTime::now()).unwrap();
    assert_eq!(plan.agents(), ["a"], "selected by its decompressed size");
    assert!(!p.zst.exists() && !p.tail.exists() && !root.join("ledgers/a.jsonl.zst.tmp-77").exists());
    assert!(s.agent("a").unwrap().is_none() && s.blocks("a").unwrap().is_empty());
}

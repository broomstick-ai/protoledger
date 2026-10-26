//! The store modes a suite runs in (SPEC §10): a plain store, and a
//! compressed one with no dictionary, with a dictionary the user passes, and
//! after its ledgers were recompressed with a dictionary trained from a
//! store (a retrain). A suite runs its tests once per mode
//! ([`modes_tests!`]); each mode's cell checks that the store really was in
//! that mode ([`Mode::check`]), so a cell cannot pass by running plain.
//!
//! A mode is set before the test writes anything, by writing the store's
//! configuration (`store.json`) through the store itself: every
//! `Store::open` of that directory, in the test or in the binary, then
//! creates compressed ledgers and seals at [`SEAL_BYTES`]. A retrain happens
//! where the test calls [`Mode::retrain`], once its store holds ledgers.
#![allow(dead_code)]

use protoledger::store::{DictChoice, LedgerReader, Store};
use protoledger::{Ledger, LedgerSet};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The hot tail's size at which a compressed store's writers seal: below
/// one event line, so that every append seals and the suites' small
/// ledgers are sealed into several blocks.
pub const SEAL_BYTES: u64 = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Not compressed (the suites as they always ran).
    Plain,
    /// Compressed, no dictionary (the default once compression is on).
    NoDictionary,
    /// Compressed with a dictionary the user passed (`store dict add`,
    /// `store config --dictionary`): raw content, not trained.
    PassedDictionary,
    /// Compressed with no dictionary, then retrained: a dictionary trained
    /// from a store (`store dict train`) is added, made the default, and
    /// every ledger is recompressed with it (`store recompress`).
    Retrained,
}

/// Generate `#[test]`s running `body(Mode)` in every compressed mode, in a
/// module named after the body: `<body>::no_dictionary`,
/// `<body>::passed_dictionary`, `<body>::retrained`. The plain mode keeps
/// its own test (the original name). Declare the module `#[macro_use]`.
macro_rules! modes_tests {
    ($($body:ident),* $(,)?) => {$(
        mod $body {
            #[test]
            fn no_dictionary() {
                super::$body($crate::modes::Mode::NoDictionary)
            }
            #[test]
            fn passed_dictionary() {
                super::$body($crate::modes::Mode::PassedDictionary)
            }
            #[test]
            fn retrained() {
                super::$body($crate::modes::Mode::Retrained)
            }
        }
    )*};
}

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

/// The dictionary [`Mode::PassedDictionary`] passes: raw content (the
/// protoledger fixtures' ledgers), which zstd takes as a dictionary.
pub fn passed_dictionary() -> &'static [u8] {
    static D: OnceLock<Vec<u8>> = OnceLock::new();
    D.get_or_init(|| {
        let mut d = Vec::new();
        for case in ["minimal", "native-subagent-compaction", "orchestrated-peers", "branch-rewind"] {
            let dir = fixtures().join("protoledger").join(case).join("native");
            let mut files: Vec<PathBuf> = walkdir::WalkDir::new(&dir).into_iter().map(|e| e.unwrap().into_path()).filter(|p| p.is_file()).collect();
            files.sort();
            for f in files {
                d.extend(std::fs::read(f).unwrap());
            }
        }
        assert!(d.len() > 4096, "the passed dictionary is {} bytes", d.len());
        d
    })
}

/// The dictionary [`Mode::Retrained`] retrains with: trained by
/// `Store::train_dictionary` from a store holding the Claude Code and Codex
/// fixtures. Trained once per test binary.
pub fn trained_dictionary() -> &'static [u8] {
    static D: OnceLock<Vec<u8>> = OnceLock::new();
    D.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("corpus");
        let mut s = Store::open(&root).unwrap();
        for format in ["claude-code", "codex"] {
            let mut cases: Vec<PathBuf> =
                std::fs::read_dir(fixtures().join(format)).unwrap().map(|e| e.unwrap().path().join("native")).filter(|p| p.is_dir()).collect();
            cases.sort();
            for native in cases {
                // A case whose agents an earlier case already put in the
                // store is left out: importing it would extend that ledger.
                let probe_root = dir.path().join("probe");
                let _ = std::fs::remove_dir_all(&probe_root);
                let mut probe = Store::open(&probe_root).unwrap();
                if probe.import(format, &native).is_err() {
                    continue;
                }
                if probe.agents().unwrap().iter().any(|a| s.agent(&a.id).unwrap().is_some()) {
                    continue;
                }
                s.import(format, &native).unwrap();
            }
        }
        drop(s);
        let id = Store::train_dictionary(&root, 16 * 1024).unwrap();
        let bytes = std::fs::read(root.join("dicts").join(format!("{id}.zdict"))).unwrap();
        assert_ne!(bytes, passed_dictionary(), "the trained and the passed dictionaries are different things");
        bytes
    })
}

/// A dictionary's id: the hex SHA-256 of its bytes (SPEC §10.7), computed
/// here, not by the store.
pub fn id_of(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

/// One block's meta frame, read from a block file by this module, not by the store.
#[derive(Debug, Clone)]
pub struct BlockMeta {
    pub dict: Option<String>,
    pub generation: u64,
    pub first_seq: u64,
    pub last_seq: u64,
}

/// Every block of a block file, from its meta frames (zstd skippable frames,
/// magic `0x184D2A5C`), each followed by the zstd frame of `zlen` bytes.
pub fn blocks_of(zst: &[u8]) -> Vec<BlockMeta> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < zst.len() {
        assert!(zst.len() >= i + 8 && zst[i..i + 4] == [0x5C, 0x2A, 0x4D, 0x18], "no meta frame at {i}");
        let n = u32::from_le_bytes(zst[i + 4..i + 8].try_into().unwrap()) as usize;
        let meta: serde_json::Value = serde_json::from_slice(&zst[i + 8..i + 8 + n]).unwrap();
        let zlen = meta["zlen"].as_u64().unwrap() as usize;
        out.push(BlockMeta {
            dict: meta["dict"].as_str().map(str::to_string),
            generation: meta["generation"].as_u64().unwrap(),
            first_seq: meta["first_seq"].as_u64().unwrap(),
            last_seq: meta["last_seq"].as_u64().unwrap(),
        });
        i += 8 + n + zlen;
    }
    out
}

/// What [`Mode::check`] found in a store.
#[derive(Debug, Clone, Default)]
pub struct Found {
    /// Ledgers, by name, in each form.
    pub plain: usize,
    pub compressed: usize,
    /// Sealed blocks, in all.
    pub blocks: usize,
    /// Blocks by the dictionary they name (`None`: no dictionary).
    pub by_dict: BTreeMap<Option<String>, usize>,
    /// The highest `generation` of any block.
    pub max_generation: u64,
}

impl Mode {
    pub fn compressed(self) -> bool {
        self != Mode::Plain
    }

    /// The dictionary every block of a store in this mode names.
    pub fn dictionary(self) -> Option<String> {
        match self {
            Mode::Plain | Mode::NoDictionary => None,
            Mode::PassedDictionary => Some(id_of(passed_dictionary())),
            Mode::Retrained => Some(id_of(trained_dictionary())),
        }
    }

    /// Make `root` a store in this mode, before anything is written to it,
    /// sealing at [`SEAL_BYTES`]. Plain: nothing (the directory is left as
    /// the test finds it).
    pub fn prepare(self, root: &Path) {
        if self == Mode::Plain {
            return;
        }
        let mut s = Store::open(root).unwrap();
        s.set_compression(true).unwrap();
        s.set_seal_bytes(SEAL_BYTES).unwrap();
        if self == Mode::PassedDictionary {
            let id = Store::add_dictionary(root, passed_dictionary()).unwrap();
            assert_eq!(id, id_of(passed_dictionary()));
            s.set_dictionary(Some(&id)).unwrap();
        }
        if self == Mode::Retrained {
            // Training needs the corpus; do it before the test's clock starts.
            trained_dictionary();
        }
    }

    /// [`Mode::Retrained`]: retrain the store `s` holds: add the trained
    /// dictionary, make it the default for new blocks, and recompress every
    /// compressed ledger with it; every one must be recompressed. Any other
    /// mode: nothing.
    pub fn retrain(self, s: &mut Store) {
        if self != Mode::Retrained {
            return;
        }
        let id = Store::add_dictionary(s.root(), trained_dictionary()).unwrap();
        s.set_dictionary(Some(&id)).unwrap();
        let r = s.recompress(DictChoice::Default, &[]).unwrap();
        assert!(r.skipped.is_empty() && r.failed.is_empty(), "retrain: skipped {:?}, failed {:?}", r.skipped, r.failed);
        assert!(!r.recompressed.is_empty(), "retrain: nothing to recompress");
    }

    /// [`Mode::retrain`] on the store at `root`, opened for it and closed after.
    pub fn retrain_at(self, root: &Path) {
        if self == Mode::Retrained {
            self.retrain(&mut Store::open(root).unwrap());
        }
    }

    /// The cell's guard: the store at `root` is in this mode. Every ledger is
    /// in this mode's form (plain: no block file or hot tail; compressed: no
    /// plain file); a compressed store holds at least one sealed block, and
    /// every block names this mode's dictionary; a retrained store's blocks
    /// were written by a recompression (some `generation` > 0: a ledger that
    /// had blocks before it was recompressed). Read from the files, not
    /// through the store.
    pub fn check(self, root: &Path) -> Found {
        let f = found(root);
        self.check_found(&f, &root.display().to_string());
        f
    }

    /// [`Mode::check`] on what [`found`] found in one or more stores.
    pub fn check_found(self, f: &Found, at: &str) {
        if self.compressed() {
            assert_eq!(f.plain, 0, "{self:?} at {at}: a plain ledger in a compressed store: {f:?}");
            assert!(f.compressed > 0 && f.blocks > 0, "{self:?} at {at}: no sealed block ({f:?}): the cell ran on hot tails alone");
            let want = self.dictionary();
            assert!(f.by_dict.keys().all(|d| *d == want), "{self:?} at {at}: blocks name {:?}, every one should name {want:?}", f.by_dict);
            if self == Mode::Retrained {
                assert!(f.max_generation > 0, "{self:?} at {at}: no block was written by a recompression");
            }
        } else {
            assert_eq!((f.compressed, f.blocks), (0, 0), "{self:?} at {at}: a compressed ledger in a plain store: {f:?}");
            assert!(f.plain > 0, "{self:?} at {at}: no ledger");
        }
    }
}

/// The ledgers and blocks of the store at `root`, from its files.
pub fn found(root: &Path) -> Found {
    let mut f = Found::default();
    let mut names: Vec<String> = std::fs::read_dir(root.join("ledgers")).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    for n in &names {
        if n.ends_with(".jsonl") {
            f.plain += 1;
        } else if n.ends_with(".jsonl.tail") || (n.ends_with(".jsonl.zst") && !names.contains(&n.replace(".zst", ".tail"))) {
            f.compressed += 1;
        }
        if n.ends_with(".jsonl.zst") {
            for b in blocks_of(&std::fs::read(root.join("ledgers").join(n)).unwrap()) {
                f.blocks += 1;
                *f.by_dict.entry(b.dict).or_default() += 1;
                f.max_generation = f.max_generation.max(b.generation);
            }
        }
    }
    f
}

impl Found {
    pub fn add(&mut self, o: &Found) {
        self.plain += o.plain;
        self.compressed += o.compressed;
        self.blocks += o.blocks;
        for (d, n) in &o.by_dict {
            *self.by_dict.entry(d.clone()).or_default() += n;
        }
        self.max_generation = self.max_generation.max(o.max_generation);
    }
}

/// Every ledger of the store at `root`, read through the store's reader (so
/// in either form), not by opening its files.
pub fn read_set(root: &Path) -> LedgerSet {
    let dir = root.join("ledgers");
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| {
            let n = e.unwrap().file_name().to_string_lossy().into_owned();
            ["", ".zst", ".tail"].iter().find_map(|x| n.strip_suffix(&format!(".jsonl{x}")).map(str::to_string))
        })
        .collect();
    names.sort();
    names.dedup();
    let mut set = LedgerSet::new();
    for n in names {
        let l: Ledger = LedgerReader::plain(dir.join(format!("{n}.jsonl"))).ledger().unwrap();
        set.insert(l);
    }
    set
}

/// Every file a ledger may be stored in, relative to the store root, in
/// either form: `ledgers/<name>.jsonl`, its block file and its hot tail.
pub fn ledger_files(name: &str) -> Vec<String> {
    ["", ".zst", ".tail"].iter().map(|x| format!("ledgers/{name}.jsonl{x}")).collect()
}

/// The file a ledger's writer locks: the plain file, or the hot tail.
pub fn locked_file(mode: Mode, root: &Path, name: &str) -> PathBuf {
    root.join(if mode.compressed() { format!("ledgers/{name}.jsonl.tail") } else { format!("ledgers/{name}.jsonl") })
}

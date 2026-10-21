//! A store's compression dictionaries (SPEC §10.7): `<store>/dicts/<id>.zdict`,
//! each named by the lowercase hex SHA-256 of its bytes, written atomically,
//! and checked against that id whenever it is loaded.
//!
//! Deleting one races with the writers that compress with it: a seal, a
//! recompression or a migration that has the dictionary's bytes in memory
//! and is about to make a block that names it durable. The race is closed by
//! one lock, `<store>/dicts/.lock` (SPEC §10.7). Such a writer holds it
//! shared from just before it makes sure the dictionary is on disk (writing
//! it back if it is gone) until the block naming it is durable under its
//! final name; `store dict rm` holds it exclusively while it counts
//! references and deletes. So a dictionary is always durable before any
//! block naming it is, and a deletion either sees that block or happened
//! before the writer's check.

use super::layout;
use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

/// The directory of a store's dictionaries, under its root.
pub const DICTS: &str = "dicts";
/// The lock that orders deletion against the writers that use a dictionary.
pub const LOCK: &str = ".lock";
/// `store dict train`'s default size: 110 KiB (SPEC §10.7).
pub const DEFAULT_TRAIN_SIZE: usize = 112_640;
/// zstd's smallest trainable dictionary.
pub const MIN_TRAIN_SIZE: usize = 256;
/// Training stops sampling at this many times the dictionary's size, as
/// zstd's own guidance suggests (about 100 times).
const SAMPLE_FACTOR: usize = 100;
/// Fewer sampled lines than this is too little to train on.
const MIN_SAMPLES: usize = 16;

/// A dictionary's id: the lowercase hex SHA-256 of its bytes.
pub fn id_of(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Whether `s` is spelled as an id is (64 lowercase hex digits).
pub fn is_id(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Why a dictionary could not be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DictError {
    Missing(String),
    /// Its bytes do not hash to its id.
    Mismatch(String),
    Unreadable(String, String),
}

impl std::fmt::Display for DictError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DictError::Missing(id) => write!(f, "dictionary {id} is missing"),
            DictError::Mismatch(id) => write!(f, "dictionary {id} does not match its id (its bytes hash to something else)"),
            DictError::Unreadable(id, e) => write!(f, "dictionary {id} cannot be read: {e}"),
        }
    }
}

impl std::error::Error for DictError {}

/// The dictionaries of one store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dicts {
    dir: PathBuf,
}

/// Loaded dictionaries, by path, kept while the file is the one that was
/// checked (same device, inode, length and modification time).
type Cache = Mutex<HashMap<PathBuf, ((u64, u64, u64, i64), Arc<Vec<u8>>)>>;

fn cache() -> &'static Cache {
    static C: OnceLock<Cache> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

fn key_of(m: &fs::Metadata) -> (u64, u64, u64, i64) {
    let id = layout::Ident::of(m);
    let mtime = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_nanos() as i64).unwrap_or(0);
    (id.dev, id.ino, id.len, mtime)
}

impl Dicts {
    /// The dictionaries of the store at `root`.
    pub fn of_store(root: &Path) -> Dicts {
        Dicts { dir: root.join(DICTS) }
    }

    /// The dictionaries of the store whose ledgers directory is `ledgers`
    /// (`<store>/ledgers`).
    pub fn of_ledgers_dir(ledgers: &Path) -> Dicts {
        let root = ledgers.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
        Dicts::of_store(root)
    }

    /// `dicts/<id>.zdict`.
    pub fn path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.zdict"))
    }

    /// Load dictionary `id`, checking that its bytes hash to `id`.
    pub fn load(&self, id: &str) -> Result<Arc<Vec<u8>>, DictError> {
        if !is_id(id) {
            return Err(DictError::Missing(id.to_string()));
        }
        let p = self.path(id);
        let m = match fs::metadata(&p) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(DictError::Missing(id.to_string())),
            Err(e) => return Err(DictError::Unreadable(id.to_string(), e.to_string())),
        };
        let key = key_of(&m);
        if let Some((k, bytes)) = cache().lock().expect("dictionary cache").get(&p)
            && *k == key
        {
            return Ok(bytes.clone());
        }
        let f = match File::open(&p) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(DictError::Missing(id.to_string())),
            Err(e) => return Err(DictError::Unreadable(id.to_string(), e.to_string())),
        };
        // The key of the file actually read, so a replacement between the
        // stat and the open is not cached under the old key.
        let key = f.metadata().map(|m| key_of(&m)).map_err(|e| DictError::Unreadable(id.to_string(), e.to_string()))?;
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut &f, &mut bytes).map_err(|e| DictError::Unreadable(id.to_string(), e.to_string()))?;
        if id_of(&bytes) != id {
            return Err(DictError::Mismatch(id.to_string()));
        }
        let bytes = Arc::new(bytes);
        cache().lock().expect("dictionary cache").insert(p, (key, bytes.clone()));
        Ok(bytes)
    }

    /// Whether dictionary `id` is in the store (its file exists).
    pub fn contains(&self, id: &str) -> Result<bool> {
        layout::exists(&self.path(id))
    }

    /// Every dictionary file in the store: `(id, size)`, by id. Only names
    /// that are exactly `<id>.zdict` count.
    pub fn list(&self) -> Result<Vec<(String, u64)>> {
        let rd = match fs::read_dir(&self.dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", self.dir.display())),
        };
        let mut out = Vec::new();
        for e in rd {
            let e = e?;
            let Ok(n) = e.file_name().into_string() else { continue };
            let Some(id) = n.strip_suffix(".zdict").filter(|id| is_id(id)) else { continue };
            out.push((id.to_string(), e.metadata()?.len()));
        }
        out.sort();
        Ok(out)
    }

    /// Add a dictionary (SPEC §10.7 `store dict add`): any bytes zstd accepts
    /// as a dictionary. Refused when empty or rejected by zstd. Adding an id
    /// already present (and whole) changes nothing. Returns its id.
    pub fn add(&self, bytes: &[u8]) -> Result<String> {
        check_usable(bytes)?;
        let id = id_of(bytes);
        match self.load(&id) {
            Ok(_) => return Ok(id),
            Err(DictError::Missing(_)) | Err(DictError::Mismatch(_)) => {}
            Err(e) => return Err(e.into()),
        }
        self.write(&id, bytes)?;
        Ok(id)
    }

    /// Write `bytes` as dictionary `id`, atomically and durably.
    fn write(&self, id: &str, bytes: &[u8]) -> Result<()> {
        fs::create_dir_all(&self.dir).with_context(|| format!("creating {}", self.dir.display()))?;
        crate::ledger::write_atomic(&self.path(id), bytes).with_context(|| format!("writing dictionary {id}"))?;
        layout::sync_dir(&self.dir)
    }

    /// Make sure dictionary `id` (whose bytes the caller compressed with) is
    /// on disk and whole, writing it back if it is not. The caller holds the
    /// lock shared ([`Dicts::lock_shared`]). Returns whether it wrote it.
    pub fn ensure(&self, id: &str, bytes: &[u8]) -> Result<bool> {
        debug_assert_eq!(id_of(bytes), id);
        match self.load(id) {
            Ok(_) => Ok(false),
            Err(DictError::Missing(_)) | Err(DictError::Mismatch(_)) => {
                self.write(id, bytes)?;
                Ok(true)
            }
            Err(e) => Err(e.into()),
        }
    }

    fn lock_file(&self) -> Result<File> {
        fs::create_dir_all(&self.dir).with_context(|| format!("creating {}", self.dir.display()))?;
        let p = self.dir.join(LOCK);
        OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&p).with_context(|| format!("opening {}", p.display()))
    }

    /// Hold the dictionary lock shared: a writer about to make a block that
    /// names a dictionary durable. Waits for a deletion in progress.
    pub fn lock_shared(&self) -> Result<File> {
        let f = self.lock_file()?;
        f.lock_shared().with_context(|| format!("locking {}", self.dir.join(LOCK).display()))?;
        Ok(f)
    }

    /// Hold the dictionary lock exclusively, for a deletion; refused while a
    /// writer holds it.
    pub fn try_lock_exclusive(&self) -> Result<File> {
        let f = self.lock_file()?;
        f.try_lock().map_err(|e| anyhow!("{}: a writer is using a dictionary right now ({e}); try again", self.dir.join(LOCK).display()))?;
        Ok(f)
    }

    /// Delete dictionary `id`; the caller holds the lock exclusively and has
    /// checked that nothing references it.
    pub fn remove(&self, id: &str) -> Result<()> {
        let p = self.path(id);
        if !layout::exists(&p)? {
            bail!("no dictionary {id} in the store");
        }
        layout::remove_if_present(&p)?;
        layout::sync_dir(&self.dir)
    }
}

/// Check that zstd accepts `bytes` as a dictionary: not empty, and a sample
/// compressed with it decompresses with it to the sample again.
pub fn check_usable(bytes: &[u8]) -> Result<()> {
    if bytes.is_empty() {
        bail!("the dictionary is empty");
    }
    let sample = b"{\"id\":\"probe\",\"kind\":\"text\",\"seq\":1}\n".repeat(4);
    let frame = compress_with(&sample, 3, Some(bytes)).map_err(|e| anyhow!("zstd rejects the dictionary: {e}"))?;
    let back = decompress_with(&frame, sample.len(), Some(bytes)).map_err(|e| anyhow!("zstd rejects the dictionary: {e}"))?;
    if back != sample {
        bail!("zstd rejects the dictionary: a sample does not round-trip through it");
    }
    Ok(())
}

/// One zstd frame of `data` with its content size and checksum, compressed
/// with `dict` (none: no dictionary). The frame does not record a
/// dictionary id: a reader selects the dictionary by the meta frame's `dict`
/// (SPEC §10.3).
pub fn compress_with(data: &[u8], level: i32, dict: Option<&[u8]>) -> std::io::Result<Vec<u8>> {
    use zstd::zstd_safe::CParameter;
    let mut c = match dict {
        Some(d) => zstd::bulk::Compressor::with_dictionary(level, d)?,
        None => zstd::bulk::Compressor::new(level)?,
    };
    c.set_parameter(CParameter::ChecksumFlag(true))?;
    c.set_parameter(CParameter::ContentSizeFlag(true))?;
    c.set_parameter(CParameter::DictIdFlag(false))?;
    c.compress(data)
}

/// Decompress one frame of at most `cap` bytes with `dict`.
pub fn decompress_with(frame: &[u8], cap: usize, dict: Option<&[u8]>) -> std::io::Result<Vec<u8>> {
    match dict {
        Some(d) => zstd::bulk::Decompressor::with_dictionary(d)?.decompress(frame, cap),
        None => zstd::bulk::decompress(frame, cap),
    }
}

/// Train a dictionary of at most `size` bytes from `samples` (SPEC §10.7
/// `store dict train`). Fails when there is too little to train on.
pub fn train(samples: &[Vec<u8>], size: usize) -> Result<Vec<u8>> {
    if size < MIN_TRAIN_SIZE {
        bail!("--size {size}: a dictionary must be at least {MIN_TRAIN_SIZE} bytes");
    }
    let total: usize = samples.iter().map(Vec::len).sum();
    if samples.len() < MIN_SAMPLES || total < size {
        bail!("too little to train on: {} lines, {total} bytes (at least {MIN_SAMPLES} lines and {size} bytes are needed)", samples.len());
    }
    let dict = zstd::dict::from_samples(samples, size)
        .map_err(|e| anyhow!("too little to train on: zstd could not train a dictionary of {size} bytes from {} lines, {total} bytes: {e}", samples.len()))?;
    check_usable(&dict)?;
    Ok(dict)
}

/// Lines of `streams` (each a decompressed stream, newest ledger first),
/// each stream's newest line first, until about `SAMPLE_FACTOR × size`
/// bytes.
pub fn sample(streams: &[Vec<u8>], size: usize) -> Vec<Vec<u8>> {
    let cap = size.saturating_mul(SAMPLE_FACTOR);
    let mut out = Vec::new();
    let mut total = 0usize;
    for s in streams {
        for line in s.split_inclusive(|b| *b == b'\n').rev() {
            if total >= cap {
                return out;
            }
            total += line.len();
            out.push(line.to_vec());
        }
    }
    out
}

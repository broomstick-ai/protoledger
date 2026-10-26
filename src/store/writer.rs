//! The store's writer of one ledger, plain or compressed (SPEC §10.4), and
//! migration between the two forms (SPEC §10.10).
//!
//! A plain ledger's writer is an [`Appender`] on `<name>.jsonl`. A compressed
//! ledger's writer appends to its hot tail `<name>.jsonl.tail`, on which it
//! holds the exclusive lock, and seals the whole tail into a block of
//! `<name>.jsonl.zst` once the tail holds `seal_bytes`: one write of the block
//! and an fsync, a read-back that checks the block is whole and decodes to
//! the tail byte for byte (else the block is cut off again and the tail
//! kept), then the tail truncated to zero. A writer that has taken its lock
//! checks that the ledger's files are still the ones it opened, and opens
//! again if not; then it repairs what a crash can leave (SPEC §10.6) and
//! refuses a damaged ledger.

use super::blocks::{self, Located};
use super::dicts::Dicts;
use super::layout::{self, Files, Form, Ident};
use super::reader;
use crate::Ledger;
use crate::ledger::{Appended, Appender, redaction_event};
use crate::model::{Agent, Event};
use anyhow::{Context, Result, anyhow, bail};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, Write};
use std::sync::Arc;

/// How many times opening may find the files changed under it before it
/// gives up. Each one is a migration or a removal that landed meanwhile.
const MAX_OPENS: usize = 100;

/// What a writer needs from the store's configuration.
#[derive(Debug, Clone)]
pub(crate) struct Opts {
    /// `compression.enabled`: new ledgers are compressed, and a ledger found
    /// in both forms keeps the compressed one.
    pub compress: bool,
    pub seal_bytes: u64,
    /// `compression.dictionary`: the default dictionary for new blocks.
    pub dictionary: Option<String>,
}

/// A dictionary loaded (and checked against its id) for compressing with.
#[derive(Debug, Clone)]
pub(crate) struct Loaded {
    pub id: String,
    pub bytes: Arc<Vec<u8>>,
}

impl Loaded {
    /// Load dictionary `id` of the store whose ledgers are in `dir`.
    pub fn load(dir: &std::path::Path, id: &str) -> Result<Loaded> {
        let bytes = Dicts::of_ledgers_dir(dir).load(id)?;
        Ok(Loaded { id: id.to_string(), bytes })
    }

    fn as_dict(&self) -> blocks::Dict<'_> {
        blocks::Dict { id: &self.id, bytes: &self.bytes }
    }
}

/// Load the dictionary `id` names, if any.
fn load_opt(files: &Files, id: Option<&str>) -> Result<Option<Loaded>> {
    id.map(|id| Loaded::load(files.dir(), id).with_context(|| format!("{}: the dictionary for new blocks", files.plain.display()))).transpose()
}

/// Make the block naming `dict` durable: with the store's dictionary lock
/// held shared, make sure the dictionary is on disk (writing it back from
/// the bytes compressed with if it was deleted meanwhile), then run `durable`
/// (the block's write and fsync, or the rename of a file of blocks into
/// place). A deletion of the dictionary either happened before, and is
/// undone here, or comes after and sees the block (SPEC §10.7).
fn with_dict_on_disk<T>(files: &Files, dict: Option<&Loaded>, durable: impl FnOnce() -> Result<T>) -> Result<T> {
    let Some(d) = dict else { return durable() };
    let ds = Dicts::of_ledgers_dir(files.dir());
    hook!(DictLocking, &ds.path(&d.id));
    let _held = ds.lock_shared()?;
    ds.ensure(&d.id, &d.bytes).with_context(|| format!("{}: writing dictionary {} back", files.plain.display(), d.id))?;
    durable()
}

/// The one writer of a ledger.
pub(crate) enum Writer {
    Plain(Appender),
    Packed(Packed),
}

impl Writer {
    pub fn ledger(&self) -> &Ledger {
        match self {
            Writer::Plain(a) => a.ledger(),
            Writer::Packed(p) => &p.ledger,
        }
    }

    /// The length of the ledger's decompressed stream.
    pub fn len(&self) -> u64 {
        match self {
            Writer::Plain(a) => a.len(),
            Writer::Packed(p) => p.l + p.tail_len,
        }
    }

    /// Append and fsync one event; a compressed ledger then seals when its
    /// hot tail has reached `seal_bytes`. Returns where it landed and
    /// whether a block was sealed.
    pub fn append(&mut self, e: Event) -> Result<(Appended, bool)> {
        match self {
            Writer::Plain(a) => Ok((a.append(e)?, false)),
            Writer::Packed(p) => p.append(e),
        }
    }

    pub fn redaction(&mut self, unredact: bool, ids: &[String], reason: &str) -> Result<(Appended, bool)> {
        match self {
            Writer::Plain(a) => Ok((a.redaction(unredact, ids, reason)?, false)),
            Writer::Packed(p) => {
                p.ledger.check_redaction(unredact, ids)?;
                p.append(redaction_event(unredact, ids, reason))
            }
        }
    }

    pub fn set_seal_bytes(&mut self, n: u64) {
        if let Writer::Packed(p) = self {
            p.seal_bytes = n;
        }
    }

    /// Create a new ledger holding `agent`'s header, compressed when
    /// `opts.compress`.
    pub fn create(files: &Files, agent: Agent, opts: Opts) -> Result<Writer> {
        if files.form()? != Form::Absent {
            bail!("{} already exists", files.plain.display());
        }
        if !opts.compress {
            return Ok(Writer::Plain(Appender::open(&files.plain, Some(agent))?));
        }
        let tail =
            OpenOptions::new().read(true).append(true).create_new(true).open(&files.tail).with_context(|| format!("creating {}", files.tail.display()))?;
        tail.try_lock().map_err(|e| anyhow!("{} is locked by another writer: {e}", files.tail.display()))?;
        layout::sync_dir(files.dir())?;
        Ok(Writer::Packed(Packed::open_locked(files, tail, Some(agent), &opts)?))
    }

    /// Open an existing ledger's writer, in whichever form it is stored.
    pub fn open(files: &Files, opts: Opts) -> Result<Writer> {
        for _ in 0..MAX_OPENS {
            match files.form()? {
                Form::Absent => bail!("{}: the ledger has no files", files.plain.display()),
                Form::Plain => {
                    let Some(f) = open_rw(&files.plain, false)? else { continue };
                    hook!(WriterLocking, &files.plain);
                    lock(&f, &files.plain)?;
                    // Under the lock: is this still the ledger's one file?
                    if !is_at(&f, &files.plain)? || files.form()? != Form::Plain {
                        continue;
                    }
                    files.remove_temporaries()?;
                    return Ok(Writer::Plain(Appender::locked(f, &files.plain, None, true)?));
                }
                Form::Compressed => {
                    let existed = layout::exists(&files.tail)?;
                    let Some(t) = open_rw(&files.tail, true)? else { continue };
                    hook!(WriterLocking, &files.tail);
                    lock(&t, &files.tail)?;
                    let here = is_at(&t, &files.tail)?;
                    if !here || layout::exists(&files.plain)? {
                        // A tail this open created for a ledger that has gone
                        // plain meanwhile is removed again.
                        if here && !existed && t.metadata()?.len() == 0 {
                            layout::remove_if_present(&files.tail)?;
                        }
                        continue;
                    }
                    if !existed {
                        layout::sync_dir(files.dir())?;
                    }
                    files.remove_temporaries()?;
                    return Ok(Writer::Packed(Packed::open_locked(files, t, None, &opts)?));
                }
                Form::Both => {
                    let Some(pf) = open_rw(&files.plain, false)? else { continue };
                    let Some(tf) = open_rw(&files.tail, true)? else { continue };
                    hook!(WriterLocking, &files.plain);
                    lock(&pf, &files.plain)?;
                    lock(&tf, &files.tail)?;
                    if !is_at(&pf, &files.plain)? || !is_at(&tf, &files.tail)? {
                        continue;
                    }
                    files.remove_temporaries()?;
                    return both(files, pf, tf, &opts);
                }
            }
        }
        bail!("{}: its files kept changing while a writer opened it", files.plain.display())
    }
}

/// A ledger found in both forms (an interrupted migration, SPEC §10.6),
/// with both locks held: check both hold the same stream, keep the form
/// `store.json` names, delete the other.
fn both(files: &Files, pf: File, tf: File, opts: &Opts) -> Result<Writer> {
    let mut plain = Vec::new();
    (&pf).read_to_end(&mut plain)?;
    plain.truncate(plain.iter().rposition(|b| *b == b'\n').map(|i| i + 1).unwrap_or(0));
    let packed = reader::packed_stream(files)?;
    if plain != packed {
        bail!(
            "{}: damaged: the ledger is in both forms and they differ ({} plain bytes, {} compressed); nothing was changed",
            files.plain.display(),
            plain.len(),
            packed.len()
        );
    }
    if opts.compress {
        layout::remove_if_present(&files.plain)?;
        layout::sync_dir(files.dir())?;
        drop(pf);
        Ok(Writer::Packed(Packed::open_locked(files, tf, None, opts)?))
    } else {
        layout::remove_if_present(&files.zst)?;
        layout::remove_if_present(&files.tail)?;
        layout::sync_dir(files.dir())?;
        drop(tf);
        // The comparison read `pf` to its end; the appender reads it again.
        (&pf).seek(std::io::SeekFrom::Start(0))?;
        Ok(Writer::Plain(Appender::locked(pf, &files.plain, None, true)?))
    }
}

/// Open `p` for reading and appending; `None` when it does not exist and
/// `create` is false.
fn open_rw(p: &std::path::Path, create: bool) -> Result<Option<File>> {
    match OpenOptions::new().read(true).append(true).create(create).open(p) {
        Ok(f) => Ok(Some(f)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("opening {}", p.display())),
    }
}

fn lock(f: &File, p: &std::path::Path) -> Result<()> {
    f.try_lock().map_err(|e| anyhow!("{} is locked by another writer: {e}", p.display()))
}

/// Is `f` the file the path `p` names now?
fn is_at(f: &File, p: &std::path::Path) -> Result<bool> {
    let held = Ident::of(&f.metadata()?);
    Ok(layout::ident_of(p)?.is_some_and(|now| now.same_file(&held)))
}

/// A compressed ledger's writer: the locked hot tail, and what the block
/// file holds.
pub(crate) struct Packed {
    files: Files,
    tail: File,
    ledger: Ledger,
    /// `L`: the blocks' length in the stream.
    l: u64,
    /// The seq of the last sealed line (`None`: no block yet).
    last_sealed: Option<u64>,
    /// The block file's `generation`.
    generation: u64,
    tail_len: u64,
    seal_bytes: u64,
    /// The dictionary new blocks are sealed with, loaded and checked.
    dict: Option<Loaded>,
}

impl Packed {
    /// Open the compressed ledger whose hot tail `tail` the caller has
    /// opened and locked: repair what a crash left, refuse damage, and load
    /// the ledger. With `agent`, an empty ledger gets its header.
    fn open_locked(files: &Files, tail: File, agent: Option<Agent>, opts: &Opts) -> Result<Packed> {
        let name = files.plain.display().to_string();
        let ds = Dicts::of_ledgers_dir(files.dir());
        // The hot tail's torn final line is cut (SPEC §10.6).
        let mut tail_bytes = Vec::new();
        (&tail).read_to_end(&mut tail_bytes).with_context(|| format!("reading {}", files.tail.display()))?;
        let keep = tail_bytes.iter().rposition(|b| *b == b'\n').map(|i| i + 1).unwrap_or(0);
        if keep < tail_bytes.len() {
            tail.set_len(keep as u64)?;
            tail.sync_data()?;
            tail_bytes.truncate(keep);
        }
        // Every block, decoded and checked whole.
        let mut stream = Vec::new();
        let mut whole: Vec<Located> = Vec::new();
        let mut torn: Option<(u64, String)> = None;
        if let Some(zf) = layout::open_if_present(&files.zst)? {
            let len = zf.metadata()?.len();
            let walked = blocks::walk(&zf, len, 0, 0, 0).with_context(|| name.clone())?;
            let n = walked.blocks.len();
            for (i, b) in walked.blocks.into_iter().enumerate() {
                match blocks::read_block(&zf, &b, &ds) {
                    Ok(bytes) => {
                        stream.extend_from_slice(&bytes);
                        whole.push(b);
                    }
                    // A block whose dictionary is missing or does not match
                    // is damage, never a torn block (SPEC §10.6).
                    Err(blocks::BlockError::Dict(e)) => {
                        bail!("{name}: damaged: the block at {} (offset {}): {e}; nothing was changed", b.physical_offset, b.meta.offset)
                    }
                    Err(why) if i + 1 == n && walked.rest.is_none() => {
                        torn = Some((b.physical_offset, format!("the block at {}: {why}", b.physical_offset)));
                    }
                    Err(why) => {
                        bail!("{name}: damaged: the block at {} (offset {}) is not whole and is not the final block: {why}", b.physical_offset, b.meta.offset)
                    }
                }
            }
            if let Some(why) = walked.rest {
                torn = Some((whole.last().map(Located::physical_end).unwrap_or(0), why));
            }
        }
        let last_seq = whole.last().map(|b| b.meta.last_seq);
        let first = match blocks::first_last(&tail_bytes) {
            Some((f, _)) => Some(blocks::line_seq(f).with_context(|| format!("{name}: the hot tail's first line"))?),
            None => None,
        };
        let continues = |first: Option<Option<u64>>| match first {
            Some(None) => last_seq.is_none(),
            Some(Some(s)) => last_seq.is_some_and(|last| s == last + 1),
            None => false,
        };
        // A torn final block whose lines the hot tail still holds is cut off.
        if let Some((cut, why)) = torn {
            if !continues(first) {
                bail!("{name}: damaged: the final block is not whole ({why}) and the hot tail does not hold its lines");
            }
            let zf = OpenOptions::new().write(true).open(&files.zst)?;
            zf.set_len(cut)?;
            zf.sync_all()?;
        }
        // A leftover tail (an interrupted seal) must be the stream's last
        // bytes; then it is cut.
        let leftover = match (first, last_seq) {
            (Some(None), Some(_)) => true,
            (Some(Some(s)), Some(last)) => s <= last,
            _ => false,
        };
        if leftover {
            if !stream.ends_with(&tail_bytes) {
                bail!("{name}: damaged: the hot tail holds lines the block file already holds, and they differ from it");
            }
            tail.set_len(0)?;
            tail.sync_data()?;
            tail_bytes.clear();
        } else if !tail_bytes.is_empty() && !continues(first) {
            bail!("{name}: damaged: the hot tail does not continue the block file (its first line is seq {first:?}, the blocks end at {last_seq:?})");
        }
        if stream.is_empty() && tail_bytes.is_empty() {
            let agent = agent.ok_or_else(|| anyhow!("{name} is empty and no agent header was given"))?;
            let mut line = serde_json::to_vec(&Ledger::new(agent).header)?;
            line.push(b'\n');
            (&tail).write_all(&line)?;
            tail.sync_data()?;
            tail_bytes = line;
        } else if agent.is_some() {
            bail!("{name} already exists");
        }
        let l = stream.len() as u64;
        stream.extend_from_slice(&tail_bytes);
        let text = std::str::from_utf8(&stream).with_context(|| name.clone())?;
        let (ledger, _) = Ledger::from_jsonl(text).with_context(|| name.clone())?;
        Ok(Packed {
            files: files.clone(),
            tail,
            ledger,
            l,
            last_sealed: last_seq,
            generation: whole.last().map(|b| b.meta.generation).unwrap_or(0),
            tail_len: tail_bytes.len() as u64,
            seal_bytes: opts.seal_bytes,
            dict: load_opt(files, opts.dictionary.as_deref())?,
        })
    }

    fn append(&mut self, e: Event) -> Result<(Appended, bool)> {
        let e = self.ledger.append(e)?.clone();
        let mut line = serde_json::to_string(&e)?;
        line.push('\n');
        // One write per line, so a crash leaves at most one torn line.
        (&self.tail).write_all(line.as_bytes())?;
        self.tail.sync_data()?;
        let at = Appended { seq: e.seq, offset: self.l + self.tail_len, len: line.len() as u64 };
        self.tail_len += at.len;
        let sealed = if self.tail_len >= self.seal_bytes { self.seal()? } else { false };
        Ok((at, sealed))
    }

    /// Seal the whole hot tail into one block (SPEC §10.4). `false`: the
    /// block did not read back whole and equal to the tail, so it was cut off
    /// again and the tail kept.
    fn seal(&mut self) -> Result<bool> {
        if self.tail_len == 0 {
            return Ok(false);
        }
        let data = blocks::read_exact_at(&self.tail, 0, self.tail_len as usize)?;
        let first_seq = self.last_sealed.map(|s| s + 1).unwrap_or(0);
        let last_seq = self.ledger.last_seq();
        let (meta, bytes) = blocks::encode(&data, self.l, first_seq, last_seq, self.generation, blocks::SEAL_LEVEL, self.dict.as_ref().map(Loaded::as_dict))?;
        // 1. One write of the block, then fsync (and the directory's, when
        //    the block file is new); its dictionary is on disk first.
        let (zf, p0) = with_dict_on_disk(&self.files, self.dict.as_ref(), || {
            let created = !layout::exists(&self.files.zst)?;
            let mut zf = OpenOptions::new().append(true).create(true).open(&self.files.zst)?;
            let p0 = zf.metadata()?.len();
            let written = (|| -> Result<()> {
                zf.write_all(&bytes)?;
                zf.sync_all()?;
                if created {
                    layout::sync_dir(self.files.dir())?;
                }
                Ok(())
            })();
            if let Err(e) = written {
                let _ = zf.set_len(p0);
                let _ = zf.sync_all();
                return Err(e.context(format!("{}: sealing", self.files.plain.display())));
            }
            Ok((zf, p0))
        })?;
        hook!(SealWritten, &self.files.zst);
        // 2. Read it back: whole, and the tail's bytes exactly.
        if !self.reads_back(p0, &meta, bytes.len() as u64, &data)? {
            zf.set_len(p0)?;
            zf.sync_all()?;
            return Ok(false);
        }
        hook!(SealChecked, &self.files.tail);
        // 3. Only now is the tail emptied.
        self.tail.set_len(0)?;
        self.tail.sync_all()?;
        self.l += self.tail_len;
        self.tail_len = 0;
        self.last_sealed = Some(last_seq);
        Ok(true)
    }

    fn reads_back(&self, at: u64, meta: &blocks::Meta, physical_len: u64, data: &[u8]) -> Result<bool> {
        let f = File::open(&self.files.zst)?;
        let len = f.metadata()?.len();
        let b = match blocks::meta_at(&f, at, len) {
            Ok(blocks::At::Block(b)) => b,
            _ => return Ok(false),
        };
        if b.meta != *meta || b.physical_len != physical_len || b.physical_end() != len {
            return Ok(false);
        }
        // Decoded with the bytes it was compressed with.
        let frame = blocks::read_exact_at(&f, b.physical_end() - b.meta.zlen, b.meta.zlen as usize)?;
        Ok(blocks::decode(&b.meta, &frame, self.dict.as_ref().map(|d| d.bytes.as_slice())).is_ok_and(|bytes| bytes == data))
    }
}

/// What a migration did with one ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Migrated {
    Done,
    /// Already in the target form (or holding no complete line yet).
    NotNeeded,
    /// Held by another writer, or changed under the migration.
    Skipped(String),
}

/// Migrate one ledger to the compressed form (`on`) or the plain form,
/// under its writer lock (SPEC §10.10). A ledger in both forms is first
/// resolved as a writer resolves it. Blocks are written with the default
/// dictionary `opts.dictionary`.
pub(crate) fn migrate(files: &Files, on: bool, opts: &Opts, recompress_bytes: u64) -> Result<Migrated> {
    if files.form()? == Form::Both {
        match Writer::open(files, opts.clone()) {
            Ok(w) => drop(w),
            Err(e) if e.to_string().contains("locked by another writer") => return Ok(Migrated::Skipped(e.to_string())),
            Err(e) => return Err(e),
        }
    }
    match (on, files.form()?) {
        (true, Form::Plain) => to_compressed(files, recompress_bytes, opts.dictionary.as_deref()),
        (false, Form::Compressed) => to_plain(files),
        _ => Ok(Migrated::NotNeeded),
    }
}

/// `stream` (whole lines) as blocks of about `target` bytes, from offset 0,
/// all of `generation`, compressed with `dict`.
fn blocks_of(files: &Files, stream: &[u8], target: u64, generation: u64, dict: Option<&Loaded>) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for (s, e, first, last) in blocks::split(stream, target).with_context(|| files.plain.display().to_string())? {
        out.extend_from_slice(&blocks::encode(&stream[s..e], s as u64, first, last, generation, blocks::REWRITE_LEVEL, dict.map(Loaded::as_dict))?.1);
    }
    Ok(out)
}

/// Check that the block file `tmp` reads back from disk, every block whole,
/// as exactly `stream` (SPEC §10.9 / §10.10 step 2); on any difference delete
/// it and say why.
fn check_copy(files: &Files, tmp: &std::path::Path, stream: &[u8], dict: Option<&Loaded>) -> Result<()> {
    let why = match packed_file_stream(tmp, dict) {
        Ok(back) if back == stream => return Ok(()),
        Ok(back) => format!("did not read back byte-identical ({} bytes, {} expected)", back.len(), stream.len()),
        Err(e) => format!("did not read back ({e})"),
    };
    layout::remove_if_present(tmp)?;
    bail!("{}: the compressed copy {why}; the ledger was left as it was", files.plain.display())
}

fn to_compressed(files: &Files, recompress_bytes: u64, dictionary: Option<&str>) -> Result<Migrated> {
    let Some(f) = open_rw(&files.plain, false)? else { return Ok(Migrated::NotNeeded) };
    hook!(WriterLocking, &files.plain);
    if let Err(e) = lock(&f, &files.plain) {
        return Ok(Migrated::Skipped(e.to_string()));
    }
    if !is_at(&f, &files.plain)? || files.form()? != Form::Plain {
        return Ok(Migrated::Skipped("its files changed while it was locked".into()));
    }
    files.remove_temporaries()?;
    let dict = load_opt(files, dictionary)?;
    let mut stream = Vec::new();
    (&f).read_to_end(&mut stream)?;
    stream.truncate(stream.iter().rposition(|b| *b == b'\n').map(|i| i + 1).unwrap_or(0));
    if stream.is_empty() {
        return Ok(Migrated::NotNeeded);
    }
    // 1. The blocks, under the temporary name.
    let tmp = files.zst_tmp();
    write_synced(&tmp, &blocks_of(files, &stream, recompress_bytes, 0, dict.as_ref())?)?;
    hook!(MigrationWritten, &tmp);
    // 2. Check: read back from disk, decompressed, byte for byte.
    check_copy(files, &tmp, &stream, dict.as_ref())?;
    // 3. Rename into place, its dictionary on disk first.
    with_dict_on_disk(files, dict.as_ref(), || {
        fs::rename(&tmp, &files.zst).with_context(|| format!("renaming {} into place", tmp.display()))?;
        layout::sync_dir(files.dir())
    })?;
    hook!(MigrationRenamed, &files.zst);
    // 4. The plain file goes.
    layout::remove_if_present(&files.plain)?;
    layout::sync_dir(files.dir())?;
    drop(f);
    Ok(Migrated::Done)
}

/// Run `body` with the hot tail `t` locked; when `t` was created only to be
/// locked (`!existed`) and `body` did not get the work done, the empty tail
/// is removed again while still locked, so the ledger's files are as they
/// were.
fn keeping_files(files: &Files, t: &File, existed: bool, body: impl FnOnce() -> Result<Migrated>) -> Result<Migrated> {
    let r = body();
    if !existed && !matches!(r, Ok(Migrated::Done)) && is_at(t, &files.tail).unwrap_or(false) && t.metadata()?.len() == 0 {
        layout::remove_if_present(&files.tail)?;
    }
    r
}

fn to_plain(files: &Files) -> Result<Migrated> {
    let existed = layout::exists(&files.tail)?;
    let Some(t) = open_rw(&files.tail, true)? else { return Ok(Migrated::NotNeeded) };
    hook!(WriterLocking, &files.tail);
    if let Err(e) = lock(&t, &files.tail) {
        return Ok(Migrated::Skipped(e.to_string()));
    }
    if !is_at(&t, &files.tail)? || layout::exists(&files.plain)? {
        if !existed && t.metadata()?.len() == 0 {
            layout::remove_if_present(&files.tail)?;
        }
        return Ok(Migrated::Skipped("its files changed while it was locked".into()));
    }
    keeping_files(files, &t, existed, || {
        files.remove_temporaries()?;
        let stream = reader::packed_stream(files)?;
        if stream.is_empty() {
            return Ok(Migrated::NotNeeded);
        }
        // 1. The plain file, under the temporary name.
        let tmp = files.plain_tmp();
        write_synced(&tmp, &stream)?;
        hook!(MigrationWritten, &tmp);
        // 2. Check.
        let back = fs::read(&tmp)?;
        if back != stream {
            layout::remove_if_present(&tmp)?;
            bail!(
                "{}: the plain copy did not read back byte-identical ({} bytes, {} expected); the ledger was left as it was",
                files.plain.display(),
                back.len(),
                stream.len()
            );
        }
        // 3. Rename into place.
        fs::rename(&tmp, &files.plain).with_context(|| format!("renaming {} into place", tmp.display()))?;
        layout::sync_dir(files.dir())?;
        hook!(MigrationRenamed, &files.plain);
        // 4. The compressed files go.
        layout::remove_if_present(&files.zst)?;
        layout::remove_if_present(&files.tail)?;
        layout::sync_dir(files.dir())?;
        Ok(Migrated::Done)
    })
}

/// Recompress one compressed ledger with `dictionary` (none: no
/// dictionary), under its writer lock (SPEC §10.9): its whole stream, the
/// hot tail's lines included, as new blocks of about `recompress_bytes`
/// under the temporary name; checked from disk byte for byte; renamed over
/// the block file; then the hot tail emptied. The caller updates the index.
pub(crate) fn recompress(files: &Files, dictionary: Option<&str>, recompress_bytes: u64) -> Result<Migrated> {
    match files.form()? {
        Form::Compressed => {}
        Form::Absent => return Ok(Migrated::NotNeeded),
        Form::Plain | Form::Both => return Ok(Migrated::Skipped("it is not compressed".into())),
    }
    let existed = layout::exists(&files.tail)?;
    let Some(t) = open_rw(&files.tail, true)? else { return Ok(Migrated::NotNeeded) };
    hook!(WriterLocking, &files.tail);
    if let Err(e) = lock(&t, &files.tail) {
        return Ok(Migrated::Skipped(e.to_string()));
    }
    if !is_at(&t, &files.tail)? || layout::exists(&files.plain)? {
        if !existed && t.metadata()?.len() == 0 {
            layout::remove_if_present(&files.tail)?;
        }
        return Ok(Migrated::Skipped("its files changed while it was locked".into()));
    }
    keeping_files(files, &t, existed, || {
        files.remove_temporaries()?;
        let dict = load_opt(files, dictionary)?;
        // What a writer would repair on open is repaired first (a torn line or
        // block, a leftover tail), and damage refused.
        if !layout::exists(&files.zst)? && t.metadata()?.len() == 0 {
            return Ok(Migrated::NotNeeded);
        }
        let w = Packed::open_locked(files, t.try_clone()?, None, &Opts { compress: true, seal_bytes: u64::MAX, dictionary: None })?;
        let stream = reader::packed_stream(files)?;
        if stream.is_empty() {
            return Ok(Migrated::NotNeeded);
        }
        let generation = match layout::open_if_present(&files.zst)? {
            Some(zf) => {
                let len = zf.metadata()?.len();
                blocks::walk(&zf, len, 0, 0, 0)?.blocks.iter().map(|b| b.meta.generation).max().map(|g| g + 1).unwrap_or(0)
            }
            None => 0,
        };
        // 1. The new block file, under the temporary name.
        let tmp = files.zst_tmp();
        write_synced(&tmp, &blocks_of(files, &stream, recompress_bytes, generation, dict.as_ref())?)?;
        hook!(RecompressWritten, &tmp);
        // 2. Check: read back from disk, every block whole, byte for byte.
        check_copy(files, &tmp, &stream, dict.as_ref())?;
        hook!(RecompressChecked, &tmp);
        // 3. Rename over the block file, its dictionary on disk first.
        with_dict_on_disk(files, dict.as_ref(), || {
            fs::rename(&tmp, &files.zst).with_context(|| format!("renaming {} into place", tmp.display()))?;
            layout::sync_dir(files.dir())
        })?;
        hook!(RecompressRenamed, &files.zst);
        // 4. The hot tail's lines are in the blocks now.
        w.tail.set_len(0)?;
        w.tail.sync_all()?;
        Ok(Migrated::Done)
    })
}

fn write_synced(p: &std::path::Path, bytes: &[u8]) -> Result<()> {
    let mut f = File::create(p).with_context(|| format!("creating {}", p.display()))?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
}

/// The decompressed stream of a block file holding only whole blocks. A
/// block naming `own`'s id is decoded with `own`'s bytes (the ones it was
/// just compressed with); any other dictionary is loaded from the store.
fn packed_file_stream(p: &std::path::Path, own: Option<&Loaded>) -> Result<Vec<u8>> {
    let f = File::open(p)?;
    let len = f.metadata()?.len();
    let walked = blocks::walk(&f, len, 0, 0, 0)?;
    if let Some(why) = walked.rest {
        bail!("{why}");
    }
    let ds = Dicts::of_ledgers_dir(p.parent().unwrap_or(std::path::Path::new(".")));
    let mut out = Vec::new();
    for b in &walked.blocks {
        let bytes = match (&b.meta.dict, own) {
            (Some(id), Some(o)) if *id == o.id => {
                let frame = blocks::read_exact_at(&f, b.physical_end() - b.meta.zlen, b.meta.zlen as usize)?;
                blocks::decode(&b.meta, &frame, Some(&o.bytes)).map_err(blocks::BlockError::NotWhole)
            }
            _ => blocks::read_block(&f, b, &ds),
        };
        out.extend_from_slice(&bytes.map_err(|why| anyhow!("the block at {}: {why}", b.physical_offset))?);
    }
    Ok(out)
}

/// Lock every file of a ledger a writer would lock, for its removal
/// (SPEC §10.11): nothing is written to any of them.
pub(crate) fn lock_for_removal(files: &Files) -> Result<Vec<File>> {
    let mut held = Vec::new();
    let form = files.form()?;
    if matches!(form, Form::Plain | Form::Both)
        && let Some(f) = layout::open_if_present(&files.plain)?
    {
        lock(&f, &files.plain)?;
        held.push(f);
    }
    if matches!(form, Form::Compressed | Form::Both)
        && let Some(f) = open_rw(&files.tail, true)?
    {
        lock(&f, &files.tail)?;
        held.push(f);
    }
    Ok(held)
}

/// Remove every file of a ledger: its plain file, block file, hot tail and
/// temporary files (SPEC §10.11).
pub(crate) fn remove(files: &Files) -> Result<()> {
    layout::remove_if_present(&files.plain)?;
    layout::remove_if_present(&files.zst)?;
    layout::remove_if_present(&files.tail)?;
    files.remove_temporaries()?;
    Ok(())
}

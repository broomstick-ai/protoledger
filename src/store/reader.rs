//! The store's one reader of a ledger: its decompressed stream (SPEC §10.1).
//!
//! Every read a store makes of a ledger's bytes goes through a
//! [`LedgerReader`]: looking an event up at its indexed offset, catching the
//! index up, a subscriber's backlog, sync's reads of the ledgers it follows,
//! and `protoledger cat`. Every offset it takes or returns is an offset in the
//! ledger's decompressed stream (SPEC §10.5), so a caller never knows how the
//! ledger is stored.
//!
//! A reader is named by the ledger's plain path (`<dir>/<name>.jsonl`) and
//! tells the ledger's form from which files exist, afresh on every read: the
//! plain file when it exists (also when both forms do, SPEC §10.6), else the
//! block file `<name>.jsonl.zst` and the hot tail `<name>.jsonl.tail`
//! (SPEC §10.2). It takes no lock, and gets a consistent stream while a writer
//! seals and while a ledger migrates (SPEC §10.5):
//!
//! - `L`, the block file's end in the stream, is taken from the block file
//!   itself, opened by path and held open across the read of the hot tail,
//!   never from the index;
//! - what is read from the hot tail is kept only when it starts on a line of
//!   the tail, continues the blocks, has the seq the reader wants, and the
//!   block file re-opened by path afterwards is the file it held (same file,
//!   same length, same last `generation`), and when the plain form has not
//!   appeared meanwhile; otherwise the reader starts again from the same
//!   decompressed offset, which never moves;
//! - it starts again only when the block file or the form did change; at rest
//!   a short tail is the end of the stream, a leftover tail (SPEC §10.6) is
//!   skipped, and a line that is not where it should be is an error naming the
//!   ledger.

use super::blocks::{self, Located};
use super::dicts::Dicts;
use super::layout::{self, Files, Form, Ident};
use super::testing::{self, Point};
use crate::Ledger;
use crate::model::{Event, Header};
use anyhow::{Context, Result, anyhow, bail};
use std::fmt;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Rounds a read may start again because the block file changed under it.
/// Each one is a seal, a rewrite or a migration that landed during the read.
const MAX_ROUNDS: usize = 10_000;

/// A reader of one ledger's decompressed stream (SPEC §10.1): the header
/// line and every complete event line, each ending in `\n`. A torn final
/// line is not part of the stream. Cheap to clone; it holds no file open,
/// so each read opens the ledger afresh and needs no lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerReader {
    path: PathBuf,
    /// Where the index last saw the ledger's sealed blocks (SPEC §10.8): a
    /// shortcut past walking their meta frames, checked against the file
    /// before it is used. Never the source of `L`.
    hints: Option<Arc<Vec<Located>>>,
}

/// What a read wants.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Want {
    /// The stream's length.
    Len,
    /// Every byte from the offset on.
    Rest,
    /// The one line at the offset, of this seq when known (`Some(0)`: the header).
    Line(Option<u64>),
}

impl LedgerReader {
    /// The reader of the ledger whose plain file is `path`
    /// (`<dir>/<name>.jsonl`), in whichever form it is stored.
    pub fn plain(path: impl Into<PathBuf>) -> LedgerReader {
        LedgerReader { path: path.into(), hints: None }
    }

    /// The same reader, told where the index last saw the ledger's blocks.
    pub(crate) fn with_hints(mut self, hints: Vec<Located>) -> LedgerReader {
        self.hints = (!hints.is_empty()).then(|| Arc::new(hints));
        self
    }

    /// The ledger's plain path, which names it.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The length of the decompressed stream: every byte up to and
    /// including its last `\n`.
    pub fn len(&self) -> Result<u64> {
        Ok(self.read(0, Want::Len)?.1)
    }

    /// Whether the stream holds no complete line yet.
    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }

    /// The event whose line starts at decompressed offset `offset`.
    pub fn read_at(&self, offset: u64) -> Result<Event> {
        self.read_event(offset, None)
    }

    /// The event of seq `seq`, whose line starts at decompressed offset
    /// `offset` (both from the index): the reader checks it is that line.
    pub fn read_seq_at(&self, offset: u64, seq: u64) -> Result<Event> {
        self.read_event(offset, Some(seq))
    }

    fn read_event(&self, offset: u64, seq: Option<u64>) -> Result<Event> {
        let (line, _) = self.read(offset, Want::Line(seq))?;
        let text = std::str::from_utf8(&line).with_context(|| format!("{self}: offset {offset}"))?;
        serde_json::from_str(text.trim_end()).with_context(|| format!("{self}: offset {offset}"))
    }

    /// The events in complete lines from decompressed offset `offset` on
    /// (0 = the start; the header is skipped), each with its own offset,
    /// plus the offset to resume from. A torn final line is left for the
    /// next call.
    pub fn tail(&self, offset: u64) -> Result<(Vec<(u64, Event)>, u64)> {
        let (bytes, end) = self.read(offset, Want::Rest)?;
        let mut out = Vec::new();
        let mut at = offset;
        for line in bytes.split_inclusive(|b| *b == b'\n') {
            let start = at;
            at += line.len() as u64;
            let text = std::str::from_utf8(line).with_context(|| format!("{self}: offset {start}"))?.trim_end();
            if text.is_empty() {
                continue;
            }
            if start == 0 {
                serde_json::from_str::<Header>(text).with_context(|| format!("{self}: line 1 is not a protoledger header"))?;
                continue;
            }
            out.push((start, serde_json::from_str(text).with_context(|| format!("{self}: offset {start}"))?));
        }
        debug_assert_eq!(at, end);
        Ok((out, end))
    }

    /// The header, or `None` while the stream holds no complete first line.
    pub fn header(&self) -> Result<Option<Header>> {
        if self.is_empty()? {
            return Ok(None);
        }
        let (line, _) = self.read(0, Want::Line(Some(0)))?;
        Ok(Some(serde_json::from_slice(line.trim_ascii_end()).with_context(|| format!("{self}: line 1 is not a protoledger header"))?))
    }

    /// Write the whole decompressed stream to `out`, exactly, and return how
    /// many bytes were written: everything that was durable when it began.
    pub fn copy_to(&self, out: &mut dyn Write) -> Result<u64> {
        if let Some(path) = self.plain_now()? {
            let len = complete_len(&path)?;
            let f = File::open(&path).with_context(|| format!("reading {}", path.display()))?;
            let n = std::io::copy(&mut f.take(len), out)?;
            if n < len {
                bail!("{} shrank while it was read", path.display());
            }
            return Ok(n);
        }
        let (bytes, _) = self.read(0, Want::Rest)?;
        out.write_all(&bytes)?;
        Ok(bytes.len() as u64)
    }

    /// The whole decompressed stream.
    pub fn stream(&self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        self.copy_to(&mut out)?;
        Ok(out)
    }

    /// The whole ledger, parsed from the stream.
    pub fn ledger(&self) -> Result<Ledger> {
        let bytes = self.stream()?;
        let text = std::str::from_utf8(&bytes).with_context(|| format!("reading {self}"))?;
        Ok(Ledger::from_jsonl(text).with_context(|| self.to_string())?.0)
    }

    /// The plain file, when the ledger is plain right now.
    fn plain_now(&self) -> Result<Option<PathBuf>> {
        Ok(matches!(Files::of(&self.path).form()?, Form::Plain | Form::Both).then(|| self.path.clone()))
    }

    /// Read from decompressed offset `from`: the bytes `want` asks for, and
    /// the offset just past them (for [`Want::Len`], the stream's length).
    fn read(&self, from: u64, want: Want) -> Result<(Vec<u8>, u64)> {
        self.read_as(from, want, false)
    }

    /// [`LedgerReader::read`]; with `packed`, the compressed form's stream
    /// even while a plain file exists too (a writer resolving both forms).
    fn read_as(&self, from: u64, want: Want, packed: bool) -> Result<(Vec<u8>, u64)> {
        let files = Files::of(&self.path);
        let mut out = Vec::new();
        let mut pos = from;
        for _ in 0..MAX_ROUNDS {
            match files.form()? {
                _ if packed => {}
                Form::Plain | Form::Both => return plain_read(&files.plain, pos, want, out),
                Form::Absent => bail!("{self}: the ledger has no files"),
                Form::Compressed => {}
            }
            match self.compressed_round(&files, &mut pos, want, &mut out, packed)? {
                Some(end) => return Ok((out, end)),
                None => continue,
            }
        }
        bail!("{self}: the block file changed under the reader {MAX_ROUNDS} times")
    }

    /// One attempt at a compressed read from `*pos`. Bytes of whole blocks are
    /// added to `out` and `*pos` moves past them as they are read (they never
    /// change); bytes of the hot tail are added only once the block file is
    /// known not to have changed. `None`: the block file or the form changed,
    /// start again from `*pos`.
    fn compressed_round(&self, files: &Files, pos: &mut u64, want: Want, out: &mut Vec<u8>, packed: bool) -> Result<Option<u64>> {
        let mut snap = Snap::open(&files.zst, self.hints.as_deref()).map_err(|e| anyhow!("{self}: {e}"))?;
        let wants_line = matches!(want, Want::Line(_));
        // The final block is checked whole before anything relies on L.
        if !(wants_line && snap.inside_unchecked(*pos)) {
            snap.check_final().map_err(|e| anyhow!("{self}: {e}"))?;
        }
        let l = snap.l();
        let last_seq = snap.last_seq();
        if *pos < l {
            match want {
                Want::Len => *pos = l,
                Want::Rest => {
                    snap.read_blocks(*pos, l, &mut |bytes| out.extend_from_slice(bytes)).map_err(|e| anyhow!("{self}: {e}"))?;
                    *pos = l;
                }
                Want::Line(seq) => {
                    let line = snap.line_at(*pos, seq).map_err(|e| anyhow!("{self}: {e}"))?;
                    out.extend_from_slice(&line);
                    return Ok(Some(*pos + line.len() as u64));
                }
            }
        }
        // The hot tail, at physical offset pos - L.
        testing::fire(Point::ReaderBeforeTail, &files.zst)?;
        let tail = read_complete(&files.tail)?;
        testing::fire(Point::ReaderAfterTail, &files.tail)?;
        if snap.changed(files, packed)? {
            return Ok(None);
        }
        let first = match blocks::first_last(&tail) {
            Some((first, _)) => Some(blocks::line_seq(first).map_err(|e| anyhow!("{self}: the hot tail's first line: {e}"))?),
            None => None,
        };
        // A leftover tail (SPEC §10.6): its lines are already in the block
        // file. A reader skips it.
        let leftover = match (first, last_seq) {
            (Some(None), Some(_)) => true,
            (Some(Some(s)), Some(last)) => s <= last,
            _ => false,
        };
        if leftover {
            // Its whole content must be the last bytes of the blocks' stream;
            // a leftover that contradicts a whole block is damage (SPEC §10.6).
            let n = tail.len() as u64;
            let mut end = Vec::with_capacity(tail.len());
            if n <= l {
                snap.read_blocks(l - n, l, &mut |bytes| end.extend_from_slice(bytes)).map_err(|e| anyhow!("{self}: {e}"))?;
            }
            if end != tail {
                // A seal cutting its block back lands between the two checks
                // only when the block file changed: then read again.
                if snap.changed(files, packed)? {
                    return Ok(None);
                }
                bail!("{self}: damaged: the hot tail holds lines the block file already holds, and they differ from it");
            }
        }
        let tail: &[u8] = if leftover { &[] } else { &tail };
        let continues = match first {
            _ if tail.is_empty() => true,
            Some(None) => last_seq.is_none(),
            Some(Some(s)) => last_seq.is_some_and(|last| s == last + 1),
            None => true,
        };
        if snap.torn.is_some() && (tail.is_empty() || !continues) {
            bail!("{self}: damaged: the final block is not whole ({}) and the hot tail does not hold its lines", snap.torn.as_deref().unwrap_or(""));
        }
        if !continues {
            bail!("{self}: damaged: the hot tail does not continue the block file (its first line is seq {first:?}, the blocks end at {last_seq:?})");
        }
        let p = (*pos - l) as usize;
        if p > tail.len() || (wants_line && p == tail.len()) {
            // Past the end of the stream, with nothing changed under the
            // reader: not written yet.
            return match want {
                Want::Line(_) => bail!("{self}: no complete line at offset {}", *pos),
                _ => Ok(Some(*pos)),
            };
        }
        if p > 0 && tail[p - 1] != b'\n' {
            bail!("{self}: offset {} is not the start of a line", *pos);
        }
        match want {
            Want::Len => Ok(Some(*pos + (tail.len() - p) as u64)),
            Want::Rest => {
                out.extend_from_slice(&tail[p..]);
                Ok(Some(*pos + (tail.len() - p) as u64))
            }
            Want::Line(seq) => {
                let end = tail[p..].iter().position(|b| *b == b'\n').map(|i| p + i + 1).expect("the tail ends in a newline");
                let line = &tail[p..end];
                let got = blocks::line_seq(line.trim_ascii_end()).map_err(|e| anyhow!("{self}: offset {}: {e}", *pos))?;
                check_line(*pos, got, seq, last_seq).map_err(|e| anyhow!("{self}: {e}"))?;
                out.extend_from_slice(line);
                Ok(Some(*pos + line.len() as u64))
            }
        }
    }
}

impl fmt::Display for LedgerReader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.path.display())
    }
}

/// Check the line found at `pos`: the header at 0; otherwise an event past
/// the blocks' `last_seq`, and the one wanted when that is known.
fn check_line(pos: u64, got: Option<u64>, want: Option<u64>, last_seq: Option<u64>) -> Result<()> {
    match (pos, got) {
        (0, None) => return Ok(()),
        (0, Some(s)) => bail!("offset 0 holds seq {s}, not the header"),
        (_, None) => bail!("offset {pos} holds a header"),
        (_, Some(s)) => {
            if last_seq.is_some_and(|last| s <= last) {
                bail!("offset {pos} holds seq {s}, which the block file already holds");
            }
            if want.is_some_and(|w| w != s) {
                bail!("offset {pos} holds seq {s}, not seq {}", want.unwrap_or(0));
            }
        }
    }
    Ok(())
}

/// The block file as one read sees it: held open, its blocks, and whether
/// its final block is whole.
struct Snap {
    file: Option<File>,
    ident: Option<Ident>,
    blocks: Vec<Located>,
    /// Which blocks came from the index and have not been checked against
    /// the file yet.
    hinted: usize,
    /// Why the bytes after the last block are not a block, if there are any.
    rest: Option<String>,
    final_checked: bool,
    /// The final block is not whole, and why: a reader ignores it (SPEC §10.6).
    torn: Option<String>,
    /// The decoded final block, kept from its check.
    final_bytes: Option<Vec<u8>>,
    /// The store's dictionaries, for blocks that name one.
    dicts: Dicts,
}

impl Snap {
    fn open(zst: &Path, hints: Option<&Vec<Located>>) -> Result<Snap> {
        let dicts = Dicts::of_ledgers_dir(zst.parent().unwrap_or(Path::new(".")));
        let Some(file) = layout::open_if_present(zst)? else {
            return Ok(Snap { file: None, ident: None, blocks: Vec::new(), hinted: 0, rest: None, final_checked: true, torn: None, final_bytes: None, dicts });
        };
        let ident = Ident::of(&file.metadata()?);
        let len = ident.len;
        // Start past the blocks the index knows, when the last of them is
        // still where the index saw it; walk the rest.
        let mut blocks = Vec::new();
        let mut hinted = 0;
        if let Some(h) = hints.filter(|h| !h.is_empty() && contiguous(h)) {
            let last = h.last().expect("non-empty");
            if let blocks::At::Block(b) = blocks::meta_at(&file, last.physical_offset, len)?
                && b.matches_hint(last)
            {
                blocks = h.clone();
                hinted = blocks.len() - 1;
                *blocks.last_mut().expect("non-empty") = b;
            }
        }
        let (at, offset, next) = match blocks.last() {
            Some(b) => (b.physical_end(), b.end(), b.meta.last_seq + 1),
            None => (0, 0, 0),
        };
        let walked = blocks::walk(&file, len, at, offset, next)?;
        blocks.extend(walked.blocks);
        Ok(Snap { file: Some(file), ident: Some(ident), blocks, hinted, rest: walked.rest, final_checked: false, torn: None, final_bytes: None, dicts })
    }

    /// Whether `pos` lies in a block other than the final one, so the final
    /// block need not be checked to read it.
    fn inside_unchecked(&self, pos: u64) -> bool {
        self.blocks.len() > 1 && pos < self.blocks[self.blocks.len() - 2].end()
    }

    /// Check the final block is whole; if it is not, drop it (a torn final
    /// block, SPEC §10.6). Bytes after the last block are such a block too.
    fn check_final(&mut self) -> Result<()> {
        if self.final_checked {
            return Ok(());
        }
        self.final_checked = true;
        if let Some(why) = &self.rest {
            self.torn = Some(why.clone());
            return Ok(());
        }
        let Some(last) = self.blocks.last().cloned() else { return Ok(()) };
        match blocks::read_block(self.file.as_ref().expect("blocks come from a file"), &last, &self.dicts) {
            Ok(bytes) => self.final_bytes = Some(bytes),
            // A missing or mismatched dictionary is damage, never a torn
            // final block (SPEC §10.6).
            Err(blocks::BlockError::Dict(e)) => bail!("damaged: the block at {} (offset {}): {e}", last.physical_offset, last.meta.offset),
            Err(why) => {
                self.blocks.pop();
                self.hinted = self.hinted.min(self.blocks.len());
                self.torn = Some(format!("the block at {}: {why}", last.physical_offset));
            }
        }
        Ok(())
    }

    /// `L`: where the blocks end in the stream.
    fn l(&self) -> u64 {
        self.blocks.last().map(Located::end).unwrap_or(0)
    }

    fn last_seq(&self) -> Option<u64> {
        self.blocks.last().map(|b| b.meta.last_seq)
    }

    /// The index of the block holding decompressed offset `o`.
    fn find(&self, o: u64) -> Option<usize> {
        let i = self.blocks.partition_point(|b| b.end() <= o);
        (i < self.blocks.len() && self.blocks[i].meta.offset <= o).then_some(i)
    }

    /// Decode block `i`. A block placed by the index is checked against the
    /// meta frame found there first (SPEC §10.5); on a mismatch the blocks
    /// are walked again from the file and `Ok(None)` says so.
    fn decode(&mut self, i: usize) -> Result<Option<Vec<u8>>> {
        let file = self.file.as_ref().expect("blocks come from a file");
        if i < self.hinted && self.blocks[i].meta.zlen == 0 {
            // Placed by the index: check the meta frame found there (magic,
            // offset, generation and the rest of the row) before using it.
            match blocks::meta_at(file, self.blocks[i].physical_offset, self.ident.expect("a file").len)? {
                blocks::At::Block(found) if found.matches_hint(&self.blocks[i]) => self.blocks[i] = found,
                _ => {
                    self.rewalk()?;
                    return Ok(None);
                }
            }
        }
        if i + 1 == self.blocks.len()
            && let Some(bytes) = &self.final_bytes
        {
            return Ok(Some(bytes.clone()));
        }
        let b = &self.blocks[i];
        blocks::read_block(file, b, &self.dicts)
            .map(Some)
            .map_err(|why| anyhow!("damaged: the block at {} (offset {}) is not whole: {why}", b.physical_offset, b.meta.offset))
    }

    /// Forget what the index said and walk the meta frames from the start.
    fn rewalk(&mut self) -> Result<()> {
        let file = self.file.as_ref().expect("blocks come from a file");
        let walked = blocks::walk(file, self.ident.expect("a file").len, 0, 0, 0)?;
        self.blocks = walked.blocks;
        self.rest = walked.rest;
        self.hinted = 0;
        self.final_checked = false;
        self.final_bytes = None;
        self.torn = None;
        self.check_final()
    }

    /// Feed `sink` the stream's bytes `from .. to` (both within the blocks).
    fn read_blocks(&mut self, from: u64, to: u64, sink: &mut dyn FnMut(&[u8])) -> Result<()> {
        let mut at = from;
        while at < to {
            let i = self.find(at).ok_or_else(|| anyhow!("no block holds offset {at}"))?;
            let Some(bytes) = self.decode(i)? else { continue };
            let b = &self.blocks[i];
            let start = (at - b.meta.offset) as usize;
            let end = (to.min(b.end()) - b.meta.offset) as usize;
            sink(&bytes[start..end]);
            at = b.meta.offset + end as u64;
        }
        Ok(())
    }

    /// The line starting at `pos`, inside a block, checked to be seq `seq`
    /// when that is known.
    fn line_at(&mut self, pos: u64, seq: Option<u64>) -> Result<Vec<u8>> {
        loop {
            let i = self.find(pos).ok_or_else(|| anyhow!("no block holds offset {pos}"))?;
            let Some(bytes) = self.decode(i)? else { continue };
            let b = &self.blocks[i];
            let p = (pos - b.meta.offset) as usize;
            if p > 0 && bytes[p - 1] != b'\n' {
                bail!("offset {pos} is not the start of a line");
            }
            let end = bytes[p..].iter().position(|c| *c == b'\n').map(|e| p + e + 1).expect("a whole block ends in a newline");
            let line = bytes[p..end].to_vec();
            let got = blocks::line_seq(line.trim_ascii_end())?;
            let want = if pos == 0 { None } else { seq };
            match (pos, got) {
                (0, None) => {}
                (0, Some(s)) => bail!("offset 0 holds seq {s}, not the header"),
                (_, None) => bail!("offset {pos} holds a header"),
                (_, Some(s)) if want.is_some_and(|w| w != s) => bail!("offset {pos} holds seq {s}, not seq {}", want.unwrap_or(0)),
                _ => {}
            }
            return Ok(line);
        }
    }

    /// Re-open the block file by path: is it still the file this read holds,
    /// at the same length, with the same last `generation`, and is the
    /// ledger still compressed?
    fn changed(&self, files: &Files, packed: bool) -> Result<bool> {
        if !packed && layout::exists(&files.plain)? {
            return Ok(true);
        }
        let now = layout::ident_of(&files.zst)?;
        match (self.ident, now) {
            (None, None) => Ok(false),
            (Some(a), Some(b)) if a.same_file(&b) && a.len == b.len => {
                // The descriptor is held open, so the file cannot have been
                // freed and its inode reused; the generation is checked too.
                let Some(last) = self.blocks.last() else { return Ok(false) };
                let f = File::open(&files.zst)?;
                match blocks::meta_at(&f, last.physical_offset, b.len)? {
                    blocks::At::Block(m) => Ok(m.meta.generation != last.meta.generation),
                    _ => Ok(true),
                }
            }
            _ => Ok(true),
        }
    }
}

/// Whether index rows describe a block file from its start with no block
/// missing between them. Rows that do not are not used: the reader walks the
/// meta frames instead (an index miss, SPEC §10.5).
fn contiguous(rows: &[Located]) -> bool {
    let (mut offset, mut next_seq, mut at) = (0u64, 0u64, 0u64);
    for r in rows {
        if r.meta.offset != offset || r.meta.first_seq != next_seq || r.physical_offset != at || r.meta.len == 0 || r.meta.last_seq < r.meta.first_seq {
            return false;
        }
        (offset, next_seq, at) = (r.end(), r.meta.last_seq + 1, r.physical_end());
    }
    true
}

/// The bytes of the plain file at `path` up to its last `\n`; absent is empty.
fn read_complete(path: &Path) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    match File::open(path) {
        Ok(mut f) => {
            f.read_to_end(&mut buf).with_context(|| format!("reading {}", path.display()))?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    }
    let complete = buf.iter().rposition(|b| *b == b'\n').map(|i| i + 1).unwrap_or(0);
    buf.truncate(complete);
    Ok(buf)
}

/// A read of a plain file, continuing `out` (bytes already read from the
/// compressed form, when the ledger migrated during the read).
fn plain_read(path: &Path, pos: u64, want: Want, mut out: Vec<u8>) -> Result<(Vec<u8>, u64)> {
    match want {
        Want::Len => Ok((out, complete_len(path)?)),
        Want::Line(_) => {
            let mut f = File::open(path).with_context(|| format!("reading {}", path.display()))?;
            f.seek(SeekFrom::Start(pos))?;
            let mut line = Vec::new();
            BufReader::new(f).read_until(b'\n', &mut line)?;
            if !line.ends_with(b"\n") {
                bail!("{}: no complete line at offset {pos}", path.display());
            }
            out.extend_from_slice(&line);
            Ok((out, pos + line.len() as u64))
        }
        Want::Rest => {
            let mut f = File::open(path).with_context(|| format!("reading {}", path.display()))?;
            f.seek(SeekFrom::Start(pos))?;
            let mut buf = Vec::new();
            f.read_to_end(&mut buf)?;
            let complete = buf.iter().rposition(|b| *b == b'\n').map(|i| i + 1).unwrap_or(0);
            out.extend_from_slice(&buf[..complete]);
            Ok((out, pos + complete as u64))
        }
    }
}

/// Bytes of a plain file up to and including its last `\n`.
fn complete_len(path: &Path) -> Result<u64> {
    let mut f = File::open(path).with_context(|| format!("reading {}", path.display()))?;
    let mut end = f.metadata()?.len();
    let mut buf = vec![0u8; 64 * 1024];
    while end > 0 {
        let start = end.saturating_sub(buf.len() as u64);
        let chunk = &mut buf[..(end - start) as usize];
        f.seek(SeekFrom::Start(start))?;
        f.read_exact(chunk)?;
        if let Some(i) = chunk.iter().rposition(|b| *b == b'\n') {
            return Ok(start + i as u64 + 1);
        }
        end = start;
    }
    Ok(0)
}

/// The whole decompressed stream of a ledger's compressed form, read under
/// its writer lock: every whole block, then the hot tail, whether or not a
/// plain file exists too. Damage is an error.
pub(crate) fn packed_stream(files: &Files) -> Result<Vec<u8>> {
    Ok(LedgerReader::plain(&files.plain).read_as(0, Want::Rest, true)?.0)
}

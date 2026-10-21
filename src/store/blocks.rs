//! Sealed blocks (SPEC §10.3): a meta frame (a zstd skippable frame holding
//! one canonical JSON object) followed by one zstd frame. A block file holds
//! blocks and nothing else.

use crate::model::Header;
use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

/// The magic number of the meta frame: a zstd skippable frame.
pub const META_MAGIC: u32 = 0x184D_2A5C;
/// The magic number of a zstd frame.
pub const ZSTD_MAGIC: u32 = 0xFD2F_B528;
/// The meta frame's format version (`block`).
pub const BLOCK_VERSION: u64 = 1;
/// zstd level for sealing: keeps the append path fast (SPEC §10.4).
pub const SEAL_LEVEL: i32 = 3;
/// zstd level for whole-ledger rewrites (migration, SPEC §10.10).
pub const REWRITE_LEVEL: i32 = 19;
/// A meta payload longer than this is not a meta frame this reader wrote.
const MAX_META: u32 = 64 * 1024;

/// The meta frame's fields, in canonical (sorted) order (SPEC §10.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    pub block: u64,
    pub dict: Option<String>,
    pub first_seq: u64,
    pub generation: u64,
    pub last_seq: u64,
    pub len: u64,
    pub offset: u64,
    pub zlen: u64,
}

/// A block and where it sits in its file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Located {
    pub meta: Meta,
    /// Position of the meta frame in the block file.
    pub physical_offset: u64,
    /// Length of the two frames.
    pub physical_len: u64,
}

impl Located {
    /// The decompressed offset just past the block.
    pub fn end(&self) -> u64 {
        self.meta.offset + self.meta.len
    }

    /// The physical position just past the block.
    pub fn physical_end(&self) -> u64 {
        self.physical_offset + self.physical_len
    }

    /// Whether `self`, read from a block file, is the block an index row
    /// `hint` describes (every field the row holds; the row has no `zlen`).
    pub fn matches_hint(&self, hint: &Located) -> bool {
        let (a, b) = (&self.meta, &hint.meta);
        (self.physical_offset, self.physical_len, a.offset, a.len, a.first_seq, a.last_seq, a.generation, &a.dict)
            == (hint.physical_offset, hint.physical_len, b.offset, b.len, b.first_seq, b.last_seq, b.generation, &b.dict)
    }

    fn zstd_at(&self) -> u64 {
        self.physical_len - self.meta.zlen + self.physical_offset
    }
}

/// The meta frame's bytes for `m`.
pub fn meta_frame(m: &Meta) -> Vec<u8> {
    let payload = serde_json::to_vec(m).expect("meta serialises");
    let mut out = Vec::with_capacity(8 + payload.len());
    out.extend_from_slice(&META_MAGIC.to_le_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&payload);
    out
}

/// Compress `data` into one zstd frame carrying its content size and a
/// content checksum (SPEC §10.3), with no dictionary.
#[cfg(test)]
pub fn compress(data: &[u8], level: i32) -> Result<Vec<u8>> {
    Ok(super::dicts::compress_with(data, level, None)?)
}

/// The dictionary a block is compressed with: its id and its bytes.
#[derive(Debug, Clone, Copy)]
pub struct Dict<'a> {
    pub id: &'a str,
    pub bytes: &'a [u8],
}

/// One block's bytes (meta frame + zstd frame) holding `data`, the lines
/// `first_seq ..= last_seq` starting at decompressed `offset`, compressed
/// with `dict` (none: no dictionary).
pub fn encode(data: &[u8], offset: u64, first_seq: u64, last_seq: u64, generation: u64, level: i32, dict: Option<Dict>) -> Result<(Meta, Vec<u8>)> {
    let frame = super::dicts::compress_with(data, level, dict.map(|d| d.bytes))?;
    let meta = Meta {
        block: BLOCK_VERSION,
        dict: dict.map(|d| d.id.to_string()),
        first_seq,
        generation,
        last_seq,
        len: data.len() as u64,
        offset,
        zlen: frame.len() as u64,
    };
    let mut out = meta_frame(&meta);
    out.extend_from_slice(&frame);
    Ok((meta, out))
}

/// What a zstd frame header says (RFC 8878 §3.1.1.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub content_size: Option<u64>,
    pub checksum: bool,
    pub dict_id: Option<u32>,
}

/// Parse the header of the zstd frame at the start of `b`.
pub fn frame_header(b: &[u8]) -> Option<FrameHeader> {
    if b.len() < 5 || u32::from_le_bytes(b[..4].try_into().ok()?) != ZSTD_MAGIC {
        return None;
    }
    let fhd = b[4];
    let fcs_flag = fhd >> 6;
    let single = fhd & 0x20 != 0;
    let checksum = fhd & 0x04 != 0;
    let did_size = [0usize, 1, 2, 4][(fhd & 0x03) as usize];
    let mut at = 5 + usize::from(!single);
    let dict_id = match did_size {
        0 => None,
        n => {
            let mut v = [0u8; 4];
            v[..n].copy_from_slice(b.get(at..at + n)?);
            at += n;
            Some(u32::from_le_bytes(v))
        }
    };
    let fcs_size = match fcs_flag {
        0 if single => 1,
        0 => 0,
        1 => 2,
        2 => 4,
        _ => 8,
    };
    let content_size = match fcs_size {
        0 => None,
        n => {
            let mut v = [0u8; 8];
            v[..n].copy_from_slice(b.get(at..at + n)?);
            let raw = u64::from_le_bytes(v);
            Some(if n == 2 { raw + 256 } else { raw })
        }
    };
    Some(FrameHeader { content_size, checksum, dict_id })
}

/// The seq of one line: `None` for the header (SPEC §2.1).
pub fn line_seq(line: &[u8]) -> Result<Option<u64>> {
    #[derive(Deserialize)]
    struct Probe {
        seq: Option<u64>,
        protoledger: Option<serde::de::IgnoredAny>,
    }
    let p: Probe = serde_json::from_slice(line).map_err(|e| anyhow!("not a ledger line: {e}"))?;
    match (p.seq, p.protoledger) {
        (Some(s), _) => Ok(Some(s)),
        (None, Some(_)) => {
            serde_json::from_slice::<Header>(line).map_err(|e| anyhow!("not a protoledger header: {e}"))?;
            Ok(None)
        }
        (None, None) => bail!("a line with neither a seq nor a header"),
    }
}

/// The first and last complete line of `bytes` (which ends in `\n`).
pub fn first_last(bytes: &[u8]) -> Option<(&[u8], &[u8])> {
    let body = bytes.strip_suffix(b"\n")?;
    let first = &body[..body.iter().position(|b| *b == b'\n').unwrap_or(body.len())];
    let last = &body[body.iter().rposition(|b| *b == b'\n').map(|i| i + 1).unwrap_or(0)..];
    Some((first, last))
}

/// Check that `bytes` are whole lines from `first_seq` (`0` = the header) to
/// `last_seq` (SPEC §10.3).
pub fn check_lines(bytes: &[u8], first_seq: u64, last_seq: u64) -> Result<(), String> {
    let (first, last) = first_last(bytes).ok_or("its bytes do not end in a whole line")?;
    let fs = line_seq(first).map_err(|e| format!("its first line: {e}"))?.unwrap_or(0);
    let ls = line_seq(last).map_err(|e| format!("its last line: {e}"))?.unwrap_or(0);
    if fs != first_seq {
        return Err(format!("its first line has seq {fs}, its meta frame says {first_seq}"));
    }
    if ls != last_seq {
        return Err(format!("its last line has seq {ls}, its meta frame says {last_seq}"));
    }
    Ok(())
}

/// Why a block cannot be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockError {
    /// The block is not whole (SPEC §10.3): a torn final block, or damage.
    NotWhole(String),
    /// The dictionary its meta frame names is missing or does not match its
    /// id. Never a torn block: always damage (SPEC §10.6).
    Dict(super::dicts::DictError),
}

impl std::fmt::Display for BlockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BlockError::NotWhole(why) => f.write_str(why),
            BlockError::Dict(e) => write!(f, "{e}"),
        }
    }
}

impl From<String> for BlockError {
    fn from(s: String) -> Self {
        BlockError::NotWhole(s)
    }
}

impl From<&str> for BlockError {
    fn from(s: &str) -> Self {
        BlockError::NotWhole(s.to_string())
    }
}

/// Decode a zstd frame `frame` of the block `m` with `dict`, the bytes of
/// the dictionary `m.dict` names (none when it names none), and check that it
/// is whole (SPEC §10.3): one frame, content size present and equal to `len`,
/// checksum flag set and matching, whole lines from `first_seq` to
/// `last_seq`.
pub fn decode(m: &Meta, frame: &[u8], dict: Option<&[u8]>) -> Result<Vec<u8>, String> {
    if m.dict.is_some() != dict.is_some() {
        return Err(format!("it names dictionary {:?}, and it was given {}", m.dict, if dict.is_some() { "one" } else { "none" }));
    }
    let h = frame_header(frame).ok_or("its zstd frame has no valid header")?;
    if h.content_size != Some(m.len) {
        return Err(format!("its zstd frame's content size is {:?}, its meta frame says {}", h.content_size, m.len));
    }
    if !h.checksum {
        return Err("its zstd frame has no content checksum".into());
    }
    match zstd::zstd_safe::find_frame_compressed_size(frame) {
        Ok(n) if n as u64 == m.zlen && n == frame.len() => {}
        Ok(n) => return Err(format!("its zstd frame is {n} bytes, its meta frame says {}", m.zlen)),
        Err(_) => return Err("its zstd frame is cut short".into()),
    }
    let len = usize::try_from(m.len).map_err(|_| "its length does not fit in memory")?;
    let out = super::dicts::decompress_with(frame, len, dict).map_err(|e| format!("its zstd frame does not decode: {e}"))?;
    if out.len() != len {
        return Err(format!("it decodes to {} bytes, its meta frame says {}", out.len(), m.len));
    }
    check_lines(&out, m.first_seq, m.last_seq)?;
    Ok(out)
}

/// Read exactly `len` bytes at `at`.
pub fn read_exact_at(f: &File, at: u64, len: usize) -> std::io::Result<Vec<u8>> {
    let mut f = f;
    f.seek(SeekFrom::Start(at))?;
    let mut buf = vec![0u8; len];
    f.read_exact(&mut buf)?;
    Ok(buf)
}

/// Read one block's two frames from `f` and decode it (see [`decode`]),
/// with the dictionary its meta frame names, loaded from `dicts` and checked
/// against its id.
pub fn read_block(f: &File, b: &Located, dicts: &super::dicts::Dicts) -> Result<Vec<u8>, BlockError> {
    let dict = match &b.meta.dict {
        None => None,
        Some(id) => Some(dicts.load(id).map_err(BlockError::Dict)?),
    };
    let zlen = usize::try_from(b.meta.zlen).map_err(|_| "its frame does not fit in memory")?;
    let frame = read_exact_at(f, b.zstd_at(), zlen).map_err(|e| format!("reading its zstd frame: {e}"))?;
    Ok(decode(&b.meta, &frame, dict.as_deref().map(|d| d.as_slice()))?)
}

/// What is at a position in a block file.
#[derive(Debug)]
pub enum At {
    /// A block whose two frames lie within the file.
    Block(Located),
    /// Nothing: the position is the file's end.
    End,
    /// Bytes that are not a complete block (cut short, or not a meta frame).
    NotABlock(String),
}

/// Read the meta frame at `at` of a block file `len` bytes long. A meta frame
/// naming a `block` version this reader does not know is an error (SPEC
/// §10.3: refused), as is a read error.
pub fn meta_at(f: &File, at: u64, len: u64) -> Result<At> {
    if at == len {
        return Ok(At::End);
    }
    if at > len {
        // A position from the index, past the end of a file replaced since.
        return Ok(At::NotABlock(format!("{at} is past the file's end {len}")));
    }
    if len - at < 8 {
        return Ok(At::NotABlock(format!("{} bytes at {at} are too short for a meta frame", len - at)));
    }
    let head = read_exact_at(f, at, 8)?;
    let magic = u32::from_le_bytes(head[..4].try_into().expect("4 bytes"));
    let plen = u32::from_le_bytes(head[4..].try_into().expect("4 bytes"));
    if magic != META_MAGIC {
        return Ok(At::NotABlock(format!("no meta frame at {at} (magic {magic:#010x})")));
    }
    if plen > MAX_META {
        return Ok(At::NotABlock(format!("the meta frame at {at} claims {plen} bytes")));
    }
    if len - at - 8 < plen as u64 {
        return Ok(At::NotABlock(format!("the meta frame at {at} is cut short")));
    }
    let payload = read_exact_at(f, at + 8, plen as usize)?;
    #[derive(Deserialize)]
    struct Version {
        block: u64,
    }
    let v: Version = match serde_json::from_slice(&payload) {
        Ok(v) => v,
        Err(e) => return Ok(At::NotABlock(format!("the meta frame at {at} does not parse: {e}"))),
    };
    if v.block != BLOCK_VERSION {
        bail!("the block at {at} has meta frame version {}, which this reader does not know", v.block);
    }
    let meta: Meta = match serde_json::from_slice(&payload) {
        Ok(m) => m,
        Err(e) => return Ok(At::NotABlock(format!("the meta frame at {at} does not parse: {e}"))),
    };
    let physical_len = 8 + plen as u64 + meta.zlen;
    if len - at < physical_len {
        return Ok(At::NotABlock(format!("the block at {at} is cut short ({} of {physical_len} bytes)", len - at)));
    }
    Ok(At::Block(Located { meta, physical_offset: at, physical_len }))
}

/// The blocks of a block file read by walking its meta frames from physical
/// `at`, continuing a file whose blocks so far end at decompressed `offset`
/// with seq `next_seq` next (`0` = the header). Walking stops at the first
/// position that does not hold a complete block continuing the previous one;
/// what follows it, if anything, is returned as `rest`.
pub struct Walked {
    pub blocks: Vec<Located>,
    /// Why the bytes after the last block are not one, when there are any.
    pub rest: Option<String>,
}

pub fn walk(f: &File, len: u64, mut at: u64, mut offset: u64, mut next_seq: u64) -> Result<Walked> {
    let mut blocks = Vec::new();
    loop {
        match meta_at(f, at, len)? {
            At::End => return Ok(Walked { blocks, rest: None }),
            At::NotABlock(why) => return Ok(Walked { blocks, rest: Some(why) }),
            At::Block(b) => {
                let m = &b.meta;
                if m.offset != offset || m.first_seq != next_seq || m.last_seq < m.first_seq || m.len == 0 {
                    let why = format!(
                        "the block at {at} does not continue the one before it (offset {} first_seq {}, expected {offset} and {next_seq})",
                        m.offset, m.first_seq
                    );
                    return Ok(Walked { blocks, rest: Some(why) });
                }
                at = b.physical_end();
                offset = b.end();
                next_seq = m.last_seq + 1;
                blocks.push(b);
            }
        }
    }
}

/// Split `stream` (whole lines) into blocks of about `target` bytes each,
/// never splitting a line. Returns `(start, end, first_seq, last_seq)` per
/// block.
pub fn split(stream: &[u8], target: u64) -> Result<Vec<(usize, usize, u64, u64)>> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut first: Option<u64> = None;
    let mut at = 0usize;
    while at < stream.len() {
        let end = stream[at..].iter().position(|b| *b == b'\n').map(|i| at + i + 1).ok_or_else(|| anyhow!("the stream does not end in a whole line"))?;
        let seq = line_seq(&stream[at..end - 1])?.unwrap_or(0);
        if first.is_none() {
            first = Some(seq);
        }
        at = end;
        if (at - start) as u64 >= target || at == stream.len() {
            out.push((start, at, first.take().expect("set"), seq));
            start = at;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sealed_frame_carries_content_size_and_checksum() {
        let data = b"{\"protoledger\":\"0.1\",\"agent\":{\"id\":\"a\"}}\n".repeat(3);
        let f = compress(&data, SEAL_LEVEL).unwrap();
        let h = frame_header(&f).unwrap();
        assert_eq!(h, FrameHeader { content_size: Some(data.len() as u64), checksum: true, dict_id: None });
        let plain = zstd::bulk::Compressor::new(3).unwrap().compress(&data).unwrap();
        assert!(!frame_header(&plain).unwrap().checksum, "zstd's own default carries no checksum, so the flag is ours");
    }

    #[test]
    fn the_meta_frame_is_canonical_json_with_sorted_keys() {
        let m = Meta { block: 1, dict: None, first_seq: 0, generation: 0, last_seq: 4, len: 10, offset: 0, zlen: 7 };
        let f = meta_frame(&m);
        assert_eq!(&f[..4], &[0x5C, 0x2A, 0x4D, 0x18]);
        let payload = &f[8..];
        assert_eq!(u32::from_le_bytes(f[4..8].try_into().unwrap()) as usize, payload.len());
        assert_eq!(std::str::from_utf8(payload).unwrap(), r#"{"block":1,"dict":null,"first_seq":0,"generation":0,"last_seq":4,"len":10,"offset":0,"zlen":7}"#);
    }
}

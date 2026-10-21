//! A ledger's files in `<store>/ledgers/` (SPEC §10.2): its plain file, or its
//! block file and hot tail, and the temporary files of a rewrite.

use anyhow::{Context, Result};
use std::fs::{self, File};
use std::path::{Path, PathBuf};

/// The files of one ledger, named by its plain path `<dir>/<name>.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Files {
    pub plain: PathBuf,
    pub zst: PathBuf,
    pub tail: PathBuf,
}

impl Files {
    /// The files of the ledger whose plain file is `plain` (`<name>.jsonl`).
    pub fn of(plain: &Path) -> Files {
        let s = plain.as_os_str().to_owned();
        let with = |suffix: &str| {
            let mut p = s.clone();
            p.push(suffix);
            PathBuf::from(p)
        };
        Files { plain: plain.to_path_buf(), zst: with(".zst"), tail: with(".tail") }
    }

    pub fn dir(&self) -> &Path {
        self.plain.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."))
    }

    /// `<name>.jsonl`.
    pub fn file_name(&self) -> String {
        self.plain.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    }

    /// Which form the ledger is stored in, told by which files exist.
    pub fn form(&self) -> Result<Form> {
        let plain = exists(&self.plain)?;
        let packed = exists(&self.zst)? || exists(&self.tail)?;
        Ok(match (plain, packed) {
            (true, false) => Form::Plain,
            (false, true) => Form::Compressed,
            (true, true) => Form::Both,
            (false, false) => Form::Absent,
        })
    }

    /// The temporary file of a rewrite into the compressed form, by this process.
    pub fn zst_tmp(&self) -> PathBuf {
        self.dir().join(format!("{}.zst.tmp-{}", self.file_name(), std::process::id()))
    }

    /// The temporary file of a rewrite into the plain form, by this process.
    pub fn plain_tmp(&self) -> PathBuf {
        self.dir().join(format!("{}.tmp-{}", self.file_name(), std::process::id()))
    }

    /// This ledger's temporary files, by any process (SPEC §10.2): exactly
    /// `<name>.jsonl.zst.tmp-<pid>` and `<name>.jsonl.tmp-<pid>`.
    pub fn temporaries(&self) -> Result<Vec<PathBuf>> {
        let me = self.file_name();
        let mut out = Vec::new();
        let rd = match fs::read_dir(self.dir()) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(e).with_context(|| format!("reading {}", self.dir().display())),
        };
        for e in rd {
            let e = e?;
            let Ok(n) = e.file_name().into_string() else { continue };
            if temporary_of(&n).is_some_and(|(ledger, _)| ledger == me) {
                out.push(e.path());
            }
        }
        out.sort();
        Ok(out)
    }

    /// Delete this ledger's temporary files (the lock holder's duty on open,
    /// SPEC §10.6) and no other file.
    pub fn remove_temporaries(&self) -> Result<()> {
        for p in self.temporaries()? {
            remove_if_present(&p)?;
        }
        Ok(())
    }
}

/// How a ledger is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Form {
    Plain,
    Compressed,
    /// Both forms: an interrupted migration (SPEC §10.6).
    Both,
    Absent,
}

/// The plain file name (`<name>.jsonl`) of the ledger a file in
/// `<store>/ledgers/` belongs to: a name ending `.jsonl`, `.jsonl.zst` or
/// `.jsonl.tail`. A temporary file, and anything else, belongs to none.
pub fn ledger_of(file_name: &str) -> Option<String> {
    if file_name.ends_with(".jsonl") {
        return Some(file_name.to_string());
    }
    for suffix in [".zst", ".tail"] {
        if let Some(stem) = file_name.strip_suffix(suffix)
            && stem.ends_with(".jsonl")
        {
            return Some(stem.to_string());
        }
    }
    None
}

/// When `file_name` is a temporary file (SPEC §10.2), the plain file name of
/// its ledger and whether it is the compressed form's: the name is exactly
/// `<name>.jsonl.zst.tmp-<digits>` or `<name>.jsonl.tmp-<digits>`.
pub fn temporary_of(file_name: &str) -> Option<(String, bool)> {
    let (stem, pid) = file_name.rsplit_once(".tmp-")?;
    if pid.is_empty() || !pid.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if let Some(plain) = stem.strip_suffix(".zst")
        && plain.ends_with(".jsonl")
    {
        return Some((plain.to_string(), true));
    }
    stem.ends_with(".jsonl").then(|| (stem.to_string(), false))
}

pub fn exists(p: &Path) -> Result<bool> {
    match fs::symlink_metadata(p) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("stat {}", p.display())),
    }
}

pub fn remove_if_present(p: &Path) -> Result<()> {
    match fs::remove_file(p) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", p.display())),
    }
}

/// Open `p` for reading, or `None` when it does not exist.
pub fn open_if_present(p: &Path) -> Result<Option<File>> {
    match File::open(p) {
        Ok(f) => Ok(Some(f)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("opening {}", p.display())),
    }
}

/// fsync a directory, so the names in it are durable.
pub fn sync_dir(dir: &Path) -> Result<()> {
    File::open(dir).and_then(|d| d.sync_all()).with_context(|| format!("syncing {}", dir.display()))
}

/// Which file a path or an open handle is: device, inode and length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ident {
    pub dev: u64,
    pub ino: u64,
    pub len: u64,
}

impl Ident {
    pub fn of(m: &fs::Metadata) -> Ident {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Ident { dev: m.dev(), ino: m.ino(), len: m.len() }
        }
        #[cfg(not(unix))]
        {
            Ident { dev: 0, ino: 0, len: m.len() }
        }
    }

    /// The same file, whatever its length.
    pub fn same_file(&self, other: &Ident) -> bool {
        (self.dev, self.ino) == (other.dev, other.ino)
    }
}

/// The ident of the file at `p` now, or `None` when there is none.
pub fn ident_of(p: &Path) -> Result<Option<Ident>> {
    match fs::metadata(p) {
        Ok(m) => Ok(Some(Ident::of(&m))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("stat {}", p.display())),
    }
}

/// Size and modification time (ns) of a file, `(0, 0)` when it is absent.
pub fn stat(p: &Path) -> Result<(u64, i64)> {
    match fs::metadata(p) {
        Ok(m) => {
            let mtime = m.modified()?.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as i64).unwrap_or(0);
            Ok((m.len(), mtime))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((0, 0)),
        Err(e) => Err(e).with_context(|| format!("stat {}", p.display())),
    }
}

/// What the index records of a ledger's files (SPEC §9.1): for a plain
/// ledger its file's size and time; for a compressed one, its block file's
/// and its hot tail's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Fingerprint {
    pub compressed: bool,
    pub size: u64,
    pub mtime_ns: i64,
    pub tail_size: u64,
    pub tail_mtime_ns: i64,
}

impl Fingerprint {
    /// Stat a ledger's files. A ledger in both forms reads as plain, as a
    /// reader reads it (SPEC §10.6).
    pub fn of(files: &Files) -> Result<Fingerprint> {
        if exists(&files.plain)? {
            let (size, mtime_ns) = stat(&files.plain)?;
            return Ok(Fingerprint { compressed: false, size, mtime_ns, tail_size: 0, tail_mtime_ns: 0 });
        }
        let (size, mtime_ns) = stat(&files.zst)?;
        let (tail_size, tail_mtime_ns) = stat(&files.tail)?;
        Ok(Fingerprint { compressed: true, size, mtime_ns, tail_size, tail_mtime_ns })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temporary_names_are_exact() {
        assert_eq!(temporary_of("a.jsonl.zst.tmp-12"), Some(("a.jsonl".into(), true)));
        assert_eq!(temporary_of("a.jsonl.tmp-7"), Some(("a.jsonl".into(), false)));
        for not in ["x.tmp-1.jsonl", "foo.jsonl.zst.tmp-1.jsonl", "a.jsonl.tmp-", "a.jsonl.tmp-1x", "a.tmp-1", "a.jsonl.zst", "a.jsonl.tail"] {
            assert_eq!(temporary_of(not), None, "{not}");
        }
        assert_eq!(ledger_of("x.tmp-1.jsonl").as_deref(), Some("x.tmp-1.jsonl"));
        assert_eq!(ledger_of("foo.jsonl.zst.tmp-1.jsonl").as_deref(), Some("foo.jsonl.zst.tmp-1.jsonl"));
        assert_eq!(ledger_of("a.jsonl.zst").as_deref(), Some("a.jsonl"));
        assert_eq!(ledger_of("a.jsonl.tail").as_deref(), Some("a.jsonl"));
        assert_eq!(ledger_of("a.jsonl.zst.tmp-3"), None);
        assert_eq!(ledger_of("a.zst"), None);
    }
}

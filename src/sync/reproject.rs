//! Re-projection (SPEC §11.2): a projection binding's location written again
//! from the projected view, and what sync measures such a location against.
//!
//! [`reproject`] follows §11.2's steps:
//!
//! 1. It refuses a binding that is not active (unbound, or stopped by
//!    divergence), an attachment, and a binding whose location is not a
//!    single file its harness only appends to ([`reprojectable`]: Claude
//!    Code's and Codex's `.jsonl` transcripts; opencode's SQLite store, a
//!    directory, a Codex `.json` or `.jsonl.zst` file are refused). It then
//!    deletes the temporary files a crashed re-projection of the binding left.
//! 2. It syncs the binding, reading the location whole, and keeps a
//!    fingerprint of the location's files as that sync read them: the
//!    location file's length and SHA-256, and those of every other file of
//!    the location (its sidecar directory). A sync that finds the location
//!    diverged, or cannot read it (a torn final line, a missing file),
//!    refuses the re-projection and leaves the location as it is.
//! 3. It renders the projected view of the bound agent and its structural
//!    descendants as the binding's format (with carry, as `project` does, so
//!    an offset-named line that lands away from its offset keeps its id), and
//!    imports the render back from a scratch copy: it must yield only agents
//!    and event ids the store holds, or nothing is written and nothing
//!    appended.
//! 4. It writes every file of the render that differs from the file at its
//!    place to a temporary file beside it (`.<name>.protoledger-tmp`),
//!    fsynced, and just before step 5 checks the fingerprint again: if a
//!    harness wrote since step 2, the temporary files are deleted and it goes
//!    back to step 2, at most [`ATTEMPTS`] times, then refuses.
//! 5. It appends the `protoledger/projected` fact step 6 owes, then renames
//!    the sidecar files into place, then the location file (each directory
//!    fsynced), so the fact naming the new bytes is durable before they are
//!    in place, and the location file's rename is the last write.
//! 6. The fact is owed when its `bytes`, `sha256` or `through` differ from
//!    the binding's last one ([`Store::append_projected`], checked under the
//!    writer's lock): one fact per change. When every file of the render
//!    equals the file at its place, nothing is written and a fact, if owed,
//!    is still appended.
//! 7. It re-baselines the binding's cursor on the render it wrote.
//!
//! A shared append-only index of the render (Codex's `session_index.jsonl`)
//! is not a file of the location: like `project`, re-projection appends the
//! lines it lacks and never rewrites it, so a title that is redacted stays in
//! Codex's index (an append-only file loses nothing), and sync never reads
//! it (a Codex binding's location is its rollout file), so its offset-named
//! lines never reach a binding.
//!
//! Re-projection is defined only between harness runs: the application must
//! not run it while a harness process that may write the location is
//! running (SPEC §11.2). The step-4 check is defence in depth, not a lock.

use super::{Cursor, Follows, files_of, projection_set, stamp, strangers, sync_with};
use crate::formats::{self, ExportOptions, Format, ImportOptions, Output};
use crate::ledger::{Appended, LedgerSet};
use crate::store::bindings::{Projected, Projections, sha256_hex};
use crate::store::{BindingRow, Store};
use anyhow::{Context, Result, anyhow, bail};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};

use super::Synced;

/// How many times re-projection goes back to step 2 when the location
/// changed since its sync, before it refuses.
pub const ATTEMPTS: u32 = 3;

/// The suffix of a re-projection's temporary file.
const TEMP: &str = ".protoledger-tmp";

/// Where [`reproject_with`] (or [`super::project_with`]) is, for a hook that
/// stops it there as a crash would, or changes what it is about to check.
#[non_exhaustive]
pub enum Stage<'a> {
    /// `project`: the projection's files are written and durable; no fact
    /// is appended yet.
    Written,
    /// `project`: the `protoledger/bound` fact is appended, the
    /// `protoledger/projected` fact not yet.
    Bound,
    /// Step 2 is done (`attempt` counts from 1).
    Synced { attempt: u32 },
    /// Step 3: the render, before the check that it imports back as itself.
    Rendered(&'a mut Output),
    /// Step 4: every temporary file is written and fsynced; the location's
    /// fingerprint is checked next.
    TempsWritten,
    /// Step 5: the fact is appended (or none was owed); nothing is renamed yet.
    FactAppended,
    /// Step 5: the sidecar files are renamed into place, the location file not yet.
    SidecarsRenamed,
    /// Step 5: the location file is renamed into place; step 7 is next.
    Renamed,
}

/// What one re-projection did.
#[derive(Debug, Clone)]
pub struct Reprojected {
    pub binding: String,
    /// The step-2 sync of the attempt that wrote.
    pub synced: Synced,
    /// The files replaced, the location file last; empty when the render
    /// equalled what was there.
    pub written: Vec<PathBuf>,
    /// The `protoledger/projected` fact appended, when one was owed.
    pub fact: Option<Appended>,
    /// The fact's data: the one appended, or the binding's last one, which
    /// names the same render.
    pub projected: Projected,
    /// How many times step 2 ran.
    pub attempts: u32,
    /// Temporary files a crashed re-projection had left, deleted.
    pub removed_temps: Vec<PathBuf>,
}

/// Can `format`'s binding at `location` be re-projected (SPEC §11.2 step 1:
/// a single file the harness only appends to)? `Err` says why not.
pub fn reprojectable(format: &str, location: &Path) -> std::result::Result<(), String> {
    if !matches!(format, "claude-code" | "codex") {
        return Err(format!(
            "{format}'s location is not a single file its harness only appends to (a SQLite store, a directory, or records rewritten in place)"
        ));
    }
    if location.is_dir() || location.extension().is_none_or(|x| x != "jsonl") {
        return Err(format!("{} is not a .jsonl transcript its harness only appends to", location.display()));
    }
    Ok(())
}

/// Re-project `binding` (SPEC §11.2; the module docs).
pub fn reproject(store: &mut Store, binding: &str) -> Result<Reprojected> {
    reproject_with(store, binding, &mut |_| Ok(()))
}

/// [`reproject`], calling `hook` at every [`Stage`]; an error from it stops
/// the re-projection there, leaving whatever was written as a crash would
/// (for tests).
pub fn reproject_with(store: &mut Store, binding: &str, hook: &mut dyn FnMut(Stage<'_>) -> Result<()>) -> Result<Reprojected> {
    let row = store.binding(binding)?.ok_or_else(|| anyhow!("no binding {binding:?}"))?;
    refuse(&row)?;
    let loc = PathBuf::from(&row.location);
    let f = formats::by_name(&row.format).ok_or_else(|| anyhow!("binding {binding}: unknown format {:?}", row.format))?;
    let mut removed_temps = remove_temps_of(&loc)?;
    for attempt in 1..=ATTEMPTS {
        // Step 2.
        let before = fingerprint(&loc)?;
        let synced = sync_with(store, binding, &mut Follows::default())?;
        if let Some(d) = &synced.diverged {
            bail!("binding {binding}: its location is diverged ({d}); refused, nothing written");
        }
        if let Some(d) = &synced.deferred {
            bail!("binding {binding}: its location could not be read ({d}); refused, nothing written");
        }
        let kept = fingerprint(&loc)?;
        if kept != before {
            continue; // written during the sync: read it again
        }
        hook(Stage::Synced { attempt })?;

        // Step 3.
        let set = projection_set(store, &row.agent)?;
        let mut ex = f.export(&set, &ExportOptions::standard())?;
        hook(Stage::Rendered(&mut ex.output))?;
        let (dir, rel) = locate(&loc, &ex.output)?;
        let scratch = tempfile::tempdir()?;
        ex.output.write_scratch(scratch.path())?;
        let scratch_loc = scratch.path().join(&rel);
        let back = f.import(&scratch_loc, &ImportOptions::standard()).with_context(|| format!("binding {binding}: reading the render back"))?;
        let mut strange = strangers(store, &back.set)?;
        if !strange.is_empty() {
            let n = strange.len();
            strange.truncate(5);
            bail!(
                "binding {binding}: the render does not import back as itself ({n} events or agents the store does not hold, e.g. {}); refused, nothing written",
                strange.join(", ")
            );
        }
        let render = ex.output.files[&rel].clone();
        let through = set.get(&row.agent).and_then(|l| l.projected_view().last().map(|e| e.id.clone()));
        let projected = Projected::of(binding, &render, through);

        // Step 4: the files that differ, the location file last.
        let mut targets: Vec<(PathBuf, &[u8])> = Vec::new();
        for (k, bytes) in &ex.output.files {
            if ex.output.appends.contains(k) || *k == rel {
                continue;
            }
            let p = dir.join(k);
            removed_temps.extend(remove_temp(&p)?);
            if std::fs::read(&p).ok().as_deref() != Some(bytes.as_slice()) {
                targets.push((p, bytes));
            }
        }
        if std::fs::read(&loc)? != render {
            targets.push((loc.clone(), &render));
        }
        for (p, bytes) in &targets {
            write_temp(p, bytes)?;
        }
        hook(Stage::TempsWritten)?;
        if !targets.is_empty() && fingerprint(&loc)? != kept {
            for (p, _) in &targets {
                let _ = std::fs::remove_file(temp_of(p));
            }
            continue; // the harness wrote since step 2: back to step 2
        }

        // Steps 5 and 6.
        let fact = store.append_projected(&row.agent, &projected)?;
        hook(Stage::FactAppended)?;
        let mut written = Vec::new();
        let (side, main): (Vec<_>, Vec<_>) = targets.iter().partition(|(p, _)| *p != loc);
        for (p, _) in &side {
            rename_into_place(p)?;
            written.push(p.clone());
        }
        hook(Stage::SidecarsRenamed)?;
        for (p, _) in &main {
            rename_into_place(p)?;
            written.push(p.clone());
        }
        hook(Stage::Renamed)?;
        // A shared append-only index gains the lines it lacks (as `project`).
        let mut appends = Output::default();
        for k in &ex.output.appends {
            appends.put_append(k.clone(), ex.output.files[k].clone());
        }
        appends.write_to(&dir, false)?;

        // Step 7.
        let facts = store.projections(&row.agent, binding)?;
        let prev = store.cursor(binding)?.and_then(|c| Cursor::parse(&c));
        let cursor = baseline(f, &scratch_loc, &render, &facts, &back.set, stamp(&loc)?, prev.as_ref());
        store.set_cursor(binding, &cursor.text())?;
        return Ok(Reprojected { binding: binding.to_string(), synced, written, fact, projected, attempts: attempt, removed_temps });
    }
    bail!("binding {binding}: its location changed during each of {ATTEMPTS} attempts (is a harness writing it?); refused, nothing written")
}

/// SPEC §11.2 step 1.
fn refuse(row: &BindingRow) -> Result<()> {
    let b = &row.binding;
    if !row.active {
        let why = row.ended.as_deref().unwrap_or("inactive");
        bail!("binding {b} is not active ({why}); refused: a stopped binding is never written again (unbind it and project to a new location)");
    }
    if row.mode != "projection" {
        bail!("binding {b} is an {}: protoledger never writes a location the harness owns; refused", row.mode);
    }
    reprojectable(&row.format, Path::new(&row.location)).map_err(|why| anyhow!("binding {b}: {why}; refused"))
}

/// The render's file that is the location file, and the directory the
/// render's paths are relative to.
fn locate(loc: &Path, out: &Output) -> Result<(PathBuf, String)> {
    for k in out.files.keys().filter(|k| !out.appends.contains(*k)) {
        let rel = Path::new(k);
        if loc.ends_with(rel) {
            let mut dir = loc.to_path_buf();
            for _ in rel.components() {
                dir.pop();
            }
            return Ok((dir, k.clone()));
        }
    }
    bail!("the render holds no file at {} (the projection's files are named otherwise now); refused, nothing written", loc.display())
}

/// The temporary file that stands for `p` until it is renamed over it.
pub(crate) fn temp_of(p: &Path) -> PathBuf {
    let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    p.with_file_name(format!(".{name}{TEMP}"))
}

/// Is `p` a re-projection's temporary file?
pub(crate) fn is_temp(p: &Path) -> bool {
    p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with('.') && n.ends_with(TEMP))
}

fn remove_temp(p: &Path) -> Result<Option<PathBuf>> {
    let t = temp_of(p);
    if t.is_file() {
        std::fs::remove_file(&t).with_context(|| format!("removing {}", t.display()))?;
        return Ok(Some(t));
    }
    Ok(None)
}

/// Delete the temporary files of `loc` and of every file under its sidecar
/// directory.
fn remove_temps_of(loc: &Path) -> Result<Vec<PathBuf>> {
    let mut out: Vec<PathBuf> = remove_temp(loc)?.into_iter().collect();
    if let Some(side) = super::sidecar(loc).filter(|s| s.is_dir()) {
        for e in walkdir::WalkDir::new(&side).into_iter().filter_map(|e| e.ok()) {
            if e.file_type().is_file() && is_temp(e.path()) {
                std::fs::remove_file(e.path())?;
                out.push(e.into_path());
            }
        }
    }
    Ok(out)
}

fn write_temp(p: &Path, bytes: &[u8]) -> Result<()> {
    let dir = p.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let t = temp_of(p);
    let mut f = std::fs::File::create(&t).with_context(|| format!("writing {}", t.display()))?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
}

fn rename_into_place(p: &Path) -> Result<()> {
    std::fs::rename(temp_of(p), p).with_context(|| format!("renaming into {}", p.display()))?;
    if let Some(d) = p.parent()
        && let Ok(d) = std::fs::File::open(d)
    {
        d.sync_all()?;
    }
    Ok(())
}

/// The length and SHA-256 of every file of the location (the location file
/// first, then its sidecar files), as one string.
fn fingerprint(loc: &Path) -> Result<String> {
    let mut out = String::new();
    for p in files_of(loc)? {
        let Ok(bytes) = std::fs::read(&p) else { continue };
        out.push_str(&format!("{}\0{}\0{}\n", p.display(), bytes.len(), sha256_hex(&bytes)));
    }
    Ok(out)
}

/// A location file measured against a binding's facts (SPEC §11.2).
pub(crate) struct Measure {
    /// The latest fact the file matches (its event id).
    pub fact: String,
    /// Per agent of the import, the ids past that fact's bytes, in order:
    /// every event of the import that an import of the file's first
    /// `bytes` bytes alone does not yield (the importer is prefix-stable,
    /// SPEC §9.3), which is also every event of the location's other files.
    pub past: BTreeMap<String, Vec<String>>,
}

/// Measure `file` (the bytes of `loc`, which imported as `whole`) against
/// `facts`: `None` when no fact matches.
pub(crate) fn measure(f: &dyn Format, loc: &Path, file: &[u8], facts: &Projections, whole: &LedgerSet) -> Option<Measure> {
    let fact = facts.matching(file)?;
    let pre = prefix_import(f, loc, &file[..fact.data.bytes as usize]);
    let past = whole
        .iter()
        .filter_map(|l| {
            let held: HashSet<&str> = pre.get(l.id()).map(|p| p.events.iter().map(|e| e.id.as_str()).collect()).unwrap_or_default();
            let ids: Vec<String> = l.events.iter().filter(|e| !held.contains(e.id.as_str())).map(|e| e.id.clone()).collect();
            (!ids.is_empty()).then(|| (l.id().to_string(), ids))
        })
        .collect();
    Some(Measure { fact: fact.id.clone(), past })
}

/// What importing `bytes` as the file `loc` (on its own, without the files
/// beside it) yields; nothing when the importer refuses it.
fn prefix_import(f: &dyn Format, loc: &Path, bytes: &[u8]) -> LedgerSet {
    let read = || -> Result<LedgerSet> {
        let dir = tempfile::tempdir()?;
        let p = dir.path().join(loc.file_name().ok_or_else(|| anyhow!("no file name"))?);
        std::fs::write(&p, bytes)?;
        Ok(f.import(&p, &ImportOptions::standard())?.set)
    };
    if bytes.is_empty() { LedgerSet::new() } else { read().unwrap_or_default() }
}

/// SPEC §11.2, beyond the matching fact's bytes: an event `read` there by a
/// sync matching the same fact that is now gone from `now`, or no longer in
/// the same order, makes the location diverged, unless the ledger holds it
/// redacted (`redacted(agent)`): re-projection leaves a redacted event out,
/// and a location without it lost nothing.
pub(crate) fn past_divergence(
    read: &BTreeMap<String, Vec<String>>,
    now: &BTreeMap<String, Vec<String>>,
    redacted: impl Fn(&str) -> HashSet<String>,
) -> Option<String> {
    let mut cache: HashMap<&str, HashSet<String>> = HashMap::new();
    for (agent, ids) in read {
        let here: &[String] = now.get(agent).map(Vec::as_slice).unwrap_or(&[]);
        let present: HashSet<&str> = here.iter().map(String::as_str).collect();
        let gone: Vec<&String> = ids.iter().filter(|id| !present.contains(id.as_str())).collect();
        if !gone.is_empty() {
            let red = cache.entry(agent).or_insert_with(|| redacted(agent));
            let lost: Vec<&&String> = gone.iter().filter(|id| !red.contains(id.as_str())).collect();
            if !lost.is_empty() {
                return Some(format!("agent {agent}: {} of {} events read past the projected bytes are gone (first: {})", lost.len(), ids.len(), lost[0]));
            }
        }
        let mut it = here.iter();
        if !ids.iter().filter(|id| present.contains(id.as_str())).all(|id| it.any(|x| x == id)) {
            return Some(format!("agent {agent}: events read past the projected bytes are no longer in the same order"));
        }
    }
    None
}

/// The cursor of a location file whose bytes are `file` and whose import is
/// `whole`, measured against `facts` (one of which it matches: it is the
/// render just written), keeping what `prev` recorded for other facts.
pub(crate) fn baseline(f: &dyn Format, loc: &Path, file: &[u8], facts: &Projections, whole: &LedgerSet, stamp: String, prev: Option<&Cursor>) -> Cursor {
    let mut c = Cursor::of(whole, stamp);
    if let Some(p) = prev {
        c.past = p.past.clone();
        c.ever_matched = p.ever_matched;
    }
    if let Some(m) = measure(f, loc, file, facts, whole) {
        c.ever_matched = true;
        c.past.insert(m.fact.clone(), m.past);
        c.matched = Some(m.fact);
    }
    c
}
